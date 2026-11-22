//! End-to-end tests for the local daemon.
//!
//! Each test boots a real axum server on an ephemeral port with `FakeHarness`,
//! connects a real `tokio-tungstenite` client and drives the wire protocol.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

/// Default event budget. Generous because CI and shared dev machines are
/// slow; the tests assert on events, never on this timeout elapsing.
const EVENT_TIMEOUT: Duration = Duration::from_secs(20);

/// Hang guard for one websocket frame; slow-harness tests rely on it being
/// longer than their configured delay.
const FRAME_TIMEOUT: Duration = Duration::from_secs(30);

use futures::{SinkExt, StreamExt};
use h4rn_core::LifecycleSettings;
use h4rn_daemon::auth::TokenStore;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::threads::ThreadManagerOptions;
use h4rn_harness::FakeHarness;
use h4rn_protocol::{
    AgentState, ApprovalDecision, AutoResumePolicy, ClientFrame, ClientRequest, DeviceId,
    DeviceInfo, Event, HarnessKind, HostId, Item, MessageRole, ProtoError, ProtoErrorCode,
    ProxyProfileDraft, ResponseBody, ResumeState, ServerFrame, ThreadFilter, ThreadId, ThreadTab,
    ToolCallStatus, Topic, TurnStatus, UsageGroupBy, UsageRange, UsageScope,
};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

struct TestDaemon {
    addr: std::net::SocketAddr,
    token: String,
    _dir: Option<tempfile::TempDir>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl TestDaemon {
    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

/// A registry that is unreachable, so tests never touch the network.
struct OfflineRegistry;

#[async_trait::async_trait]
impl h4rn_harness::RegistryClient for OfflineRegistry {
    async fn latest(&self, _package: &str) -> Option<String> {
        None
    }
}

/// An update runner that fails, so a test can never touch a real global CLI.
struct RefusingRunner;

#[async_trait::async_trait]
impl h4rn_harness::UpdateRunner for RefusingRunner {
    async fn run(
        &self,
        _program: &str,
        _args: &[String],
        _on_line: &(dyn Fn(h4rn_harness::UpdateLine) + Send + Sync),
    ) -> Result<i32, String> {
        Err("tests never run a real update".to_owned())
    }
}

async fn boot() -> TestDaemon {
    boot_with(
        ThreadManagerOptions {
            settings: LifecycleSettings::default()
                .with_working_exit_grace(chrono::TimeDelta::milliseconds(80))
                .with_stale_bypass(chrono::TimeDelta::seconds(60)),
            tick_interval: Duration::from_millis(20),
            clock: None,
        },
        FakeHarness::new(),
    )
    .await
}

async fn boot_with(options: ThreadManagerOptions, harness: FakeHarness) -> TestDaemon {
    boot_inner(options, harness, false).await
}

/// A daemon whose catalog holds only the fake harness, so the host's real
/// global CLIs are never detected, listed or updated.
async fn boot_isolated() -> TestDaemon {
    boot_inner(ThreadManagerOptions::default(), FakeHarness::new(), true).await
}

async fn boot_inner(
    options: ThreadManagerOptions,
    harness: FakeHarness,
    isolated: bool,
) -> TestDaemon {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Auto,
        network_access: None,
        network_port: 0,
    };

    let mut builder = DaemonBuilder::new(config)
        .with_harness_registry(Arc::new(OfflineRegistry))
        // No test may run a package manager against the machine's real CLIs.
        .with_update_runner(Arc::new(RefusingRunner))
        .with_lifecycle(options);
    builder = if isolated {
        builder.with_catalog(Arc::new(
            h4rn_daemon::harnesses::StaticHarnessCatalog::with(Arc::new(harness)),
        ))
    } else {
        builder.with_harness(Arc::new(harness))
    };
    let daemon = builder.build().await.expect("build daemon");
    let token = TokenStore::new(dir.path())
        .load()
        .expect("read token")
        .expect("token exists");
    let bound = daemon.bind().await.expect("bind daemon");
    let addr = bound.local_addr();
    let (shutdown, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        bound
            .serve(async move {
                let _ = rx.await;
            })
            .await
    });

    TestDaemon {
        addr,
        token,
        _dir: Some(dir),
        shutdown: Some(shutdown),
        task,
    }
}

/// A clock the test moves by hand.
///
/// Lifecycle windows (post-turn grace, auto-settle, snooze, a provider reset)
/// are then crossed on the test's step, so a slow CI machine cannot skip a tab
/// or expire a snooze before the assertion runs.
#[derive(Clone, Debug)]
struct ManualClock {
    millis: Arc<std::sync::atomic::AtomicI64>,
}

impl ManualClock {
    fn new() -> Self {
        Self {
            millis: Arc::new(std::sync::atomic::AtomicI64::new(
                chrono::Utc::now().timestamp_millis(),
            )),
        }
    }

    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(
            self.millis.load(std::sync::atomic::Ordering::SeqCst),
        )
        .expect("a valid timestamp")
    }

    fn advance(&self, delta: chrono::TimeDelta) {
        self.millis.fetch_add(
            delta.num_milliseconds(),
            std::sync::atomic::Ordering::SeqCst,
        );
    }
}

impl h4rn_daemon::threads::Clock for ManualClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        ManualClock::now(self)
    }
}

/// Options that read `clock`, so every derived placement is the test's call.
fn clocked_options(clock: &ManualClock, settings: LifecycleSettings) -> ThreadManagerOptions {
    ThreadManagerOptions {
        settings,
        tick_interval: Duration::from_millis(20),
        clock: Some(Arc::new(clock.clone())),
    }
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    pending: VecDeque<(u64, Event)>,
}

impl Client {
    async fn connect(daemon: &TestDaemon) -> Self {
        let url = format!("ws://{}/ws?token={}", daemon.addr, daemon.token);
        let (ws, _) = connect_async(url).await.expect("connect");
        Self {
            ws,
            next_id: 1,
            pending: VecDeque::new(),
        }
    }

    async fn connect_with_protocol(daemon: &TestDaemon, protocol: &str) -> Self {
        let mut request = format!("ws://{}/ws", daemon.addr)
            .into_client_request()
            .expect("request");
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_str(protocol).expect("header value"),
        );
        let (ws, _) = connect_async(request).await.expect("connect with protocol");
        Self {
            ws,
            next_id: 1,
            pending: VecDeque::new(),
        }
    }

    async fn send(&mut self, body: ClientRequest) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let frame = ClientFrame { id, body };
        self.ws
            .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
            .await
            .expect("send frame");
        id
    }

    async fn send_raw(&mut self, text: &str) {
        self.ws
            .send(Message::Text(text.to_owned().into()))
            .await
            .expect("send raw");
    }

    async fn send_binary(&mut self, bytes: Vec<u8>) {
        self.ws
            .send(Message::Binary(bytes.into()))
            .await
            .expect("send binary");
    }

    async fn read_frame(&mut self) -> ServerFrame {
        loop {
            // A hang guard only: tests that configure a slow harness send need
            // a single frame to arrive later than the usual event budget.
            let message = tokio::time::timeout(FRAME_TIMEOUT, self.ws.next())
                .await
                .expect("timed out waiting for a frame")
                .expect("socket closed")
                .expect("websocket error");
            match message {
                Message::Text(text) => {
                    return serde_json::from_str(&text).expect("parse ServerFrame");
                }
                Message::Close(_) => panic!("server closed the connection unexpectedly"),
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
    }

    async fn response(&mut self, id: u64) -> Result<ResponseBody, ProtoError> {
        loop {
            match self.read_frame().await {
                ServerFrame::Response {
                    id: response_id,
                    result,
                } if response_id == id => return result,
                ServerFrame::Event { seq, event } => self.pending.push_back((seq, event)),
                _ => {}
            }
        }
    }

    async fn request(&mut self, body: ClientRequest) -> Result<ResponseBody, ProtoError> {
        let id = self.send(body).await;
        self.response(id).await
    }

    async fn next_event(&mut self) -> (u64, Event) {
        if let Some(event) = self.pending.pop_front() {
            return event;
        }
        loop {
            match self.read_frame().await {
                ServerFrame::Event { seq, event } => return (seq, event),
                ServerFrame::Response { .. }
                | ServerFrame::TerminalEvent { .. }
                | ServerFrame::ProxyEvent { .. }
                | ServerFrame::UsageEvent { .. }
                | ServerFrame::HarnessEvent { .. }
                | ServerFrame::TerminalResponder { .. } => {}
            }
        }
    }

    async fn wait_event(&mut self, predicate: impl Fn(&Event) -> bool) -> (u64, Event) {
        self.wait_event_within(EVENT_TIMEOUT, predicate).await
    }

    /// Like `wait_event` with an explicit budget. A budget must exceed any
    /// harness delay the test configures, or the wait races the delay.
    async fn wait_event_within(
        &mut self,
        budget: Duration,
        predicate: impl Fn(&Event) -> bool,
    ) -> (u64, Event) {
        tokio::time::timeout(budget, async {
            loop {
                let (seq, event) = self.next_event().await;
                if predicate(&event) {
                    return (seq, event);
                }
            }
        })
        .await
        .expect("timed out waiting for an event")
    }

    async fn hello(&mut self) -> ResponseBody {
        self.request(ClientRequest::Hello {
            protocol_version: h4rn_protocol::PROTOCOL_VERSION,
            device: device(),
        })
        .await
        .expect("hello")
    }

    async fn wait_for_turn_completed(&mut self) -> (u64, TurnStatus) {
        let (seq, event) = self
            .wait_event(|event| matches!(event, Event::TurnCompleted { .. }))
            .await;
        match event {
            Event::TurnCompleted { status, .. } => (seq, status),
            _ => unreachable!(),
        }
    }

    async fn wait_for_tab(&mut self, tab: ThreadTab) -> (u64, h4rn_protocol::ThreadSummary) {
        let (seq, event) = self
            .wait_event(
                |event| matches!(event, Event::ThreadUpserted(summary) if summary.tab == tab),
            )
            .await;
        match event {
            Event::ThreadUpserted(summary) => (seq, summary),
            _ => unreachable!(),
        }
    }
}

fn device() -> DeviceInfo {
    DeviceInfo {
        device_id: DeviceId::new("dev_test"),
        name: "integration test".into(),
        platform: "linux".into(),
        app_version: "0.1.0".into(),
    }
}

/// The daemon exposes its id through Hello/ListHosts; probe with a fresh client
/// so the test's own event buffer stays clean.
async fn host_id_for(daemon: &TestDaemon) -> HostId {
    let mut probe = Client::connect(daemon).await;
    probe.hello().await;
    match probe.request(ClientRequest::ListHosts).await.unwrap() {
        ResponseBody::Hosts { hosts } => hosts[0].host_id.clone(),
        other => panic!("unexpected host response: {other:?}"),
    }
}

async fn create_with_host(
    client: &mut Client,
    host: HostId,
    prompt: &str,
    harness: HarnessKind,
) -> h4rn_protocol::ThreadSummary {
    create_with_model(client, host, prompt, harness, "fake-model").await
}

async fn create_with_model(
    client: &mut Client,
    host: HostId,
    prompt: &str,
    harness: HarnessKind,
    model: &str,
) -> h4rn_protocol::ThreadSummary {
    let result = client
        .request(ClientRequest::CreateThread {
            host,
            harness,
            workspace_path: "/tmp/h4rn-workspace".into(),
            model: model.into(),
            prompt: prompt.into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            worktree: None,
        })
        .await
        .expect("create thread");
    match result {
        ResponseBody::ThreadCreated { summary, .. } => summary,
        other => panic!("unexpected CreateThread response: {other:?}"),
    }
}

#[tokio::test]
async fn rejects_missing_and_wrong_tokens() {
    let daemon = boot().await;
    let url = format!("ws://{}/ws", daemon.addr);
    assert!(connect_async(url).await.is_err(), "no token must fail");

    let wrong = format!("ws://{}/ws?token=wrong", daemon.addr);
    assert!(connect_async(wrong).await.is_err(), "wrong token must fail");

    let good = format!("ws://{}/ws?token={}", daemon.addr, daemon.token);
    assert!(connect_async(good).await.is_ok(), "query token must work");

    let mut header_client =
        Client::connect_with_protocol(&daemon, &format!("bearer.{}", daemon.token)).await;
    header_client.hello().await;

    daemon.stop().await;
}

#[tokio::test]
async fn creates_streams_and_moves_working_done_settled() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    let hello = client.hello().await;
    match hello {
        ResponseBody::Hello {
            protocol_version,
            hosts,
            ..
        } => {
            assert_eq!(protocol_version, h4rn_protocol::PROTOCOL_VERSION);
            assert!(hosts[0].online);
            assert_eq!(hosts[0].host_id, host);
            assert!(hosts[0].harnesses.contains(&HarnessKind::ClaudeCode));
            assert!(hosts[0].harnesses.contains(&HarnessKind::Codex));
            assert!(hosts[0].harnesses.contains(&HarnessKind::Antigravity));
        }
        other => panic!("unexpected hello: {other:?}"),
    }

    let summary = create_with_host(
        &mut client,
        host.clone(),
        "ship the daemon",
        HarnessKind::ClaudeCode,
    )
    .await;
    assert_eq!(summary.tab, ThreadTab::Working);
    assert_eq!(summary.title, "ship the daemon");
    assert_eq!(summary.host, host);
    let thread_id = summary.thread_id.clone();

    let (_, event) = client
        .wait_event(|event| matches!(event, Event::MessageDelta { .. }))
        .await;
    match event {
        Event::MessageDelta { text, .. } => assert_eq!(text, "echo: ship the daemon"),
        _ => unreachable!(),
    }

    let (_, status) = client.wait_for_turn_completed().await;
    assert_eq!(status, TurnStatus::Completed);

    // Working -> Done is a clock-derived transition emitted by the ticker.
    let (_, done) = client.wait_for_tab(ThreadTab::Done).await;
    assert_eq!(done.thread_id, thread_id);
    assert!(done.unread);
    assert!(done.needs_attention);

    // Manual Settled sticks.
    let result = client
        .request(ClientRequest::SetThreadTab {
            thread_id: thread_id.clone(),
            tab: ThreadTab::Settled,
            pinned: false,
        })
        .await
        .expect("settle");
    assert_eq!(result, ResponseBody::Ack);
    let (_, settled) = client.wait_for_tab(ThreadTab::Settled).await;
    assert!(!settled.unread);

    let threads = client
        .request(ClientRequest::ListThreads {
            filter: ThreadFilter {
                tabs: vec![ThreadTab::Settled],
                ..ThreadFilter::default()
            },
        })
        .await
        .expect("list");
    match threads {
        ResponseBody::Threads { threads } => {
            assert_eq!(threads.len(), 1);
            assert_eq!(threads[0].thread_id, thread_id);
            assert_eq!(threads[0].tab, ThreadTab::Settled);
        }
        other => panic!("unexpected list response: {other:?}"),
    }

    daemon.stop().await;
}

#[tokio::test]
async fn reconnecting_with_since_seq_replays_without_gaps() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let summary = create_with_host(&mut client, host, "replay me", HarnessKind::ClaudeCode).await;
    let thread_id = summary.thread_id.clone();

    let mut live_seqs = Vec::new();
    let mut saw_delta = false;
    loop {
        let (seq, event) = client.next_event().await;
        live_seqs.push(seq);
        saw_delta |=
            matches!(&event, Event::MessageDelta { text, .. } if text.contains("replay me"));
        if matches!(event, Event::TurnCompleted { .. }) {
            break;
        }
    }
    assert!(saw_delta);
    assert!(
        live_seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "live sequences must strictly increase: {live_seqs:?}"
    );
    let last_seq = *live_seqs.last().unwrap();
    drop(client);

    let mut resumed = Client::connect(&daemon).await;
    resumed.hello().await;

    let all = resumed
        .request(ClientRequest::GetThread {
            thread_id: thread_id.clone(),
            since_seq: 0,
        })
        .await
        .expect("get thread");
    let (next_seq, has_delta, has_completed) = match all {
        ResponseBody::Thread {
            events, next_seq, ..
        } => {
            // The replay response orders events by global sequence; verify the
            // content we care about is present.
            let has_delta = events.iter().any(|event| {
                matches!(&event.event, Event::MessageDelta { text, .. } if text.contains("replay me"))
            });
            let has_completed = events
                .iter()
                .any(|event| matches!(event.event, Event::TurnCompleted { .. }));
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event.event, Event::ThreadUpserted(_))),
                "the snapshot event must replay too"
            );
            (next_seq, has_delta, has_completed)
        }
        other => panic!("unexpected get response: {other:?}"),
    };
    assert!(has_delta, "the streamed delta must replay");
    assert!(has_completed, "the turn completion must replay");
    assert!(
        next_seq >= last_seq,
        "{next_seq} must cover the last live sequence {last_seq}"
    );

    let tail = resumed
        .request(ClientRequest::GetThread {
            thread_id: thread_id.clone(),
            since_seq: last_seq,
        })
        .await
        .expect("get thread tail");
    match tail {
        ResponseBody::Thread {
            events, next_seq, ..
        } => {
            // Nothing at or before `last_seq` may replay. Events after it (such
            // as the ticker's Done transition) may legitimately appear.
            assert!(
                !events.iter().any(|event| {
                    matches!(&event.event, Event::MessageDelta { text, .. } if text.contains("replay me"))
                }),
                "seen deltas must not replay"
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event.event, Event::TurnCompleted { .. })),
                "the seen turn completion must not replay"
            );
            assert!(next_seq >= last_seq);
        }
        other => panic!("unexpected tail response: {other:?}"),
    }

    daemon.stop().await;
}

#[tokio::test]
async fn send_message_starts_a_new_turn_and_clears_a_pin() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let summary = create_with_host(&mut client, host, "first", HarnessKind::ClaudeCode).await;
    let thread_id = summary.thread_id.clone();
    client.wait_for_turn_completed().await;
    client.wait_for_tab(ThreadTab::Done).await;

    client
        .request(ClientRequest::SetThreadTab {
            thread_id: thread_id.clone(),
            tab: ThreadTab::Working,
            pinned: true,
        })
        .await
        .expect("pin");
    let (_, pinned) = client.wait_for_tab(ThreadTab::Working).await;
    assert!(pinned.pinned);

    client
        .request(ClientRequest::SendMessage {
            thread_id: thread_id.clone(),
            text: "second".into(),
            attachments: Vec::new(),
        })
        .await
        .expect("send");

    let (_, event) = client
        .wait_event(
            |event| matches!(event, Event::MessageDelta { text, .. } if text == "echo: second"),
        )
        .await;
    assert!(matches!(event, Event::MessageDelta { .. }));
    let (_, summary) = client.wait_for_tab(ThreadTab::Working).await;
    assert!(!summary.pinned, "a new turn clears the manual pin");
    let (_, status) = client.wait_for_turn_completed().await;
    assert_eq!(status, TurnStatus::Completed);

    daemon.stop().await;
}

#[tokio::test]
async fn auto_settle_moves_a_done_thread_to_settled() {
    let clock = ManualClock::new();
    let options = clocked_options(
        &clock,
        LifecycleSettings::default()
            .with_working_exit_grace(chrono::TimeDelta::seconds(1))
            .with_auto_settle(chrono::TimeDelta::seconds(2))
            .with_stale_bypass(chrono::TimeDelta::seconds(60)),
    );
    let daemon = boot_with(options, FakeHarness::new()).await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    create_with_host(&mut client, host, "settle me", HarnessKind::ClaudeCode).await;
    client.wait_for_turn_completed().await;

    // Cross the post-turn grace: Done is the tab, and moves only on a tick.
    clock.advance(chrono::TimeDelta::seconds(1) + chrono::TimeDelta::milliseconds(1));
    client.wait_for_tab(ThreadTab::Done).await;

    // Cross the auto-settle window. Virtual time cannot jump over the tab, so
    // the Done row above is always observed first.
    clock.advance(chrono::TimeDelta::seconds(2) + chrono::TimeDelta::milliseconds(1));
    let (_, settled) = client.wait_for_tab(ThreadTab::Settled).await;
    assert!(!settled.unread, "settling clears the unread marker");

    daemon.stop().await;
}

#[tokio::test]
async fn snooze_hides_a_thread_and_the_ticker_wakes_it() {
    let clock = ManualClock::new();
    let options = clocked_options(
        &clock,
        LifecycleSettings::default()
            .with_working_exit_grace(chrono::TimeDelta::seconds(1))
            .with_stale_bypass(chrono::TimeDelta::seconds(60)),
    );
    let daemon = boot_with(options, FakeHarness::new()).await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let summary = create_with_host(&mut client, host, "snooze me", HarnessKind::ClaudeCode).await;
    let thread_id = summary.thread_id.clone();
    client.wait_for_turn_completed().await;
    clock.advance(chrono::TimeDelta::seconds(1) + chrono::TimeDelta::milliseconds(1));
    client.wait_for_tab(ThreadTab::Done).await;

    let until = clock.now() + chrono::TimeDelta::seconds(5);
    client
        .request(ClientRequest::Snooze {
            thread_id: thread_id.clone(),
            until: Some(until),
        })
        .await
        .expect("snooze");

    let (_, snoozed_event) = client
        .wait_event(|event| {
            matches!(event, Event::ThreadUpserted(summary) if summary.snoozed_until.is_some())
        })
        .await;
    match snoozed_event {
        Event::ThreadUpserted(summary) => assert!(summary.snoozed_until.is_some()),
        _ => unreachable!(),
    }

    let listed = client
        .request(ClientRequest::ListThreads {
            filter: ThreadFilter::default(),
        })
        .await
        .expect("list");
    match listed {
        ResponseBody::Threads { threads } => {
            assert!(
                threads.iter().all(|thread| thread.thread_id != thread_id),
                "a snoozed thread is outside the three tabs by default"
            );
        }
        other => panic!("unexpected list response: {other:?}"),
    }

    // The ticker clears the snooze when it expires and re-emits the row. Time
    // only moves here, so the row above cannot expire before it is read.
    clock.advance(chrono::TimeDelta::seconds(6));
    client
        .wait_event(|event| {
            matches!(event, Event::ThreadUpserted(summary) if summary.snoozed_until.is_none())
        })
        .await;

    daemon.stop().await;
}

#[tokio::test]
async fn unimplemented_requests_and_bad_hello_have_clear_errors() {
    let daemon = boot().await;
    let mut client = Client::connect(&daemon).await;

    let error = client
        .request(ClientRequest::McpList)
        .await
        .expect_err("requests before Hello are rejected");
    assert_eq!(error.code, ProtoErrorCode::Unauthorized);

    client.hello().await;

    // MCP list is implemented in phase 4 and starts empty. Real coverage of
    // the MCP operations lives in `tests/mcp.rs`.
    match client.request(ClientRequest::McpList).await.expect("list") {
        ResponseBody::McpServers { servers } => assert!(servers.is_empty()),
        other => panic!("unexpected MCP response: {other:?}"),
    }

    // A fresh connection with a newer protocol major is rejected and stays
    // unauthenticated for later requests.
    let mut future = Client::connect(&daemon).await;
    let error = future
        .request(ClientRequest::Hello {
            protocol_version: 99,
            device: device(),
        })
        .await
        .expect_err("version 99 must be rejected");
    assert_eq!(error.code, ProtoErrorCode::Unsupported);
    assert!(error.message.contains("99"));
    let error = future
        .request(ClientRequest::ListHosts)
        .await
        .expect_err("Hello was rejected");
    assert_eq!(error.code, ProtoErrorCode::Unauthorized);

    daemon.stop().await;
}

#[tokio::test]
async fn get_thread_reports_not_found_and_binary_frames_are_rejected() {
    let daemon = boot().await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let error = client
        .request(ClientRequest::GetThread {
            thread_id: ThreadId::new("th_missing"),
            since_seq: 0,
        })
        .await
        .expect_err("missing thread");
    assert_eq!(error.code, ProtoErrorCode::NotFound);

    let bytes =
        h4rn_protocol::BinaryFrame::new(h4rn_protocol::BinaryKind::PtyOutput, 1, 1).encode();
    client.send_binary(bytes).await;
    let (_, event) = client
        .wait_event(|event| matches!(event, Event::Error { .. }))
        .await;
    match event {
        Event::Error { message, .. } => assert!(message.contains("not accepted from clients")),
        _ => unreachable!(),
    }

    // Malformed JSON gets an InvalidRequest response rather than killing the
    // connection.
    client.send_raw("{not json").await;
    let mut invalid = None;
    for _ in 0..4 {
        match client.read_frame().await {
            ServerFrame::Response {
                result: Err(error), ..
            } if error.code == ProtoErrorCode::InvalidRequest => {
                invalid = Some(error);
                break;
            }
            _ => continue,
        }
    }
    assert!(invalid.is_some());

    daemon.stop().await;
}

#[tokio::test]
async fn approvals_are_answered_through_the_stream() {
    use h4rn_harness::HarnessEvent;
    use h4rn_protocol::{ApprovalKind, ApprovalOption};

    let script = vec![
        HarnessEvent::ApprovalRequested {
            approval_id: "appr_1".into(),
            kind: ApprovalKind::Command,
            prompt: "Run `git status`?".into(),
            options: vec![ApprovalOption {
                id: "go".into(),
                label: "Go".into(),
            }],
        },
        HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Default::default(),
        },
    ];
    let daemon = boot_with(
        ThreadManagerOptions {
            settings: LifecycleSettings::default()
                .with_working_exit_grace(chrono::TimeDelta::milliseconds(30)),
            tick_interval: Duration::from_millis(20),
            clock: None,
        },
        FakeHarness::with_events(script),
    )
    .await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    // A scripted session emits approval + completion immediately, so the turn
    // may already be complete by the time the response arrives.
    let summary = create_with_host(&mut client, host, "approve me", HarnessKind::ClaudeCode).await;
    let thread_id = summary.thread_id;

    let (_, event) = client
        .wait_event(|event| matches!(event, Event::ApprovalRequested { .. }))
        .await;
    match event {
        Event::ApprovalRequested {
            approval_id,
            prompt,
            ..
        } => {
            assert_eq!(approval_id, "appr_1");
            assert!(prompt.contains("git status"));
        }
        _ => unreachable!(),
    }

    client
        .request(ClientRequest::RespondApproval {
            thread_id,
            approval_id: "appr_1".into(),
            decision: ApprovalDecision::Approve,
            option_id: None,
        })
        .await
        .expect("respond approval");

    daemon.stop().await;
}

#[tokio::test]
async fn subscribe_without_threads_suppresses_thread_events() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    client
        .request(ClientRequest::Subscribe {
            topics: vec![Topic::Usage],
            since_seq: None,
        })
        .await
        .expect("subscribe");

    // Create a thread from a second connection; the first should not receive
    // thread events after narrowing its subscription.
    let mut creator = Client::connect(&daemon).await;
    creator.hello().await;
    create_with_host(&mut creator, host, "subscribed", HarnessKind::ClaudeCode).await;

    let quiet = tokio::time::timeout(Duration::from_millis(300), client.next_event()).await;
    assert!(
        quiet.is_err(),
        "a Usage-only subscription must not receive thread events, got {quiet:?}"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn list_threads_filters_by_tab_and_search() {
    // Keep both turns running for the whole test. The default harness echoes
    // and completes immediately, so a thread only stays in Working for the
    // 80ms exit grace; under load the two creates could outlast it and the
    // tab filter then legitimately saw Done rows. An empty script pins the
    // phase at Running, making the tab assertion independent of wall time.
    let daemon = boot_with(
        ThreadManagerOptions {
            settings: LifecycleSettings::default()
                .with_working_exit_grace(chrono::TimeDelta::milliseconds(80))
                .with_stale_bypass(chrono::TimeDelta::seconds(60)),
            tick_interval: Duration::from_millis(20),
            clock: None,
        },
        FakeHarness::with_events(Vec::new()),
    )
    .await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let first = create_with_host(
        &mut client,
        host.clone(),
        "alpha task",
        HarnessKind::ClaudeCode,
    )
    .await;
    let second = create_with_host(&mut client, host, "beta task", HarnessKind::ClaudeCode).await;

    let by_search = client
        .request(ClientRequest::ListThreads {
            filter: ThreadFilter {
                search: Some("alpha".into()),
                ..ThreadFilter::default()
            },
        })
        .await
        .expect("list by search");
    match by_search {
        ResponseBody::Threads { threads } => {
            assert_eq!(threads.len(), 1);
            assert_eq!(threads[0].thread_id, first.thread_id);
        }
        other => panic!("unexpected list response: {other:?}"),
    }

    let by_tab = client
        .request(ClientRequest::ListThreads {
            filter: ThreadFilter {
                tabs: vec![ThreadTab::Working],
                ..ThreadFilter::default()
            },
        })
        .await
        .expect("list working");
    match by_tab {
        ResponseBody::Threads { threads } => {
            assert!(
                threads
                    .iter()
                    .any(|thread| thread.thread_id == first.thread_id)
            );
            assert!(
                threads
                    .iter()
                    .any(|thread| thread.thread_id == second.thread_id)
            );
        }
        other => panic!("unexpected list response: {other:?}"),
    }

    daemon.stop().await;
}

#[tokio::test]
async fn restored_threads_replay_after_a_daemon_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Auto,
        network_access: None,
        network_port: 0,
    };
    let options = ThreadManagerOptions {
        settings: LifecycleSettings::default()
            .with_working_exit_grace(chrono::TimeDelta::milliseconds(40)),
        tick_interval: Duration::from_millis(20),
        clock: None,
    };

    let daemon = DaemonBuilder::new(config.clone())
        .with_harness(Arc::new(FakeHarness::new()))
        .with_lifecycle(options.clone())
        .build()
        .await
        .expect("first daemon");
    let token = TokenStore::new(dir.path()).load().unwrap().unwrap();
    let bound = daemon.bind().await.expect("bind");
    let addr = bound.local_addr();
    let (shutdown, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        bound
            .serve(async move {
                let _ = rx.await;
            })
            .await
    });
    let first_daemon = TestDaemon {
        addr,
        token,
        _dir: None,
        shutdown: Some(shutdown),
        task,
    };

    let host = host_id_for(&first_daemon).await;
    let mut client = Client::connect(&first_daemon).await;
    client.hello().await;
    let summary = create_with_host(&mut client, host, "persist me", HarnessKind::ClaudeCode).await;
    let thread_id = summary.thread_id;
    client.wait_for_turn_completed().await;
    first_daemon.stop().await;

    // Reboot against the same data directory.
    let rebooted = DaemonBuilder::new(config)
        .with_harness(Arc::new(FakeHarness::new()))
        .with_lifecycle(options)
        .build()
        .await
        .expect("second daemon");
    let token = TokenStore::new(dir.path()).load().unwrap().unwrap();
    let bound = rebooted.bind().await.expect("bind again");
    let addr = bound.local_addr();
    let (shutdown, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        bound
            .serve(async move {
                let _ = rx.await;
            })
            .await
    });
    let second_daemon = TestDaemon {
        addr,
        token,
        _dir: None,
        shutdown: Some(shutdown),
        task,
    };

    let mut client = Client::connect(&second_daemon).await;
    client.hello().await;
    let listed = client
        .request(ClientRequest::ListThreads {
            filter: ThreadFilter::default(),
        })
        .await
        .expect("list after restart");
    match listed {
        ResponseBody::Threads { threads } => {
            assert_eq!(threads.len(), 1);
            assert_eq!(threads[0].thread_id, thread_id);
        }
        other => panic!("unexpected list response: {other:?}"),
    }

    let replayed = client
        .request(ClientRequest::GetThread {
            thread_id,
            since_seq: 0,
        })
        .await
        .expect("replay after restart");
    match replayed {
        ResponseBody::Thread { events, .. } => {
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event.event, Event::MessageDelta { .. }))
            );
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event.event, Event::TurnCompleted { .. }))
            );
        }
        other => panic!("unexpected get response: {other:?}"),
    }

    second_daemon.stop().await;
    // Keep the original tempdir alive until both daemons have stopped.
    drop(dir);
}

#[tokio::test]
async fn create_and_send_report_durable_ids_and_worktrees_are_not_implemented() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let error = client
        .request(ClientRequest::CreateThread {
            host: host.clone(),
            harness: HarnessKind::ClaudeCode,
            workspace_path: "/tmp/h4rn-workspace".into(),
            model: "fake-model".into(),
            prompt: "needs a worktree".into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::FullAccess,
            worktree: Some(h4rn_protocol::WorktreeRequest::Generated(true)),
        })
        .await
        .expect_err("worktree creation is phase 2");
    assert_eq!(error.code, ProtoErrorCode::NotImplemented);
    assert!(error.message.contains("worktree"));

    let created = client
        .request(ClientRequest::CreateThread {
            host,
            harness: HarnessKind::ClaudeCode,
            workspace_path: "/tmp/h4rn-workspace".into(),
            model: "fake-model".into(),
            prompt: "report ids".into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::FullAccess,
            worktree: None,
        })
        .await
        .expect("create thread");
    let (thread_id, first_turn_id) = match created {
        ResponseBody::ThreadCreated {
            thread_id,
            turn_id,
            summary,
        } => {
            assert_eq!(summary.thread_id, thread_id);
            (thread_id, turn_id)
        }
        other => panic!("unexpected create response: {other:?}"),
    };

    client.wait_for_turn_completed().await;

    let accepted = client
        .request(ClientRequest::SendMessage {
            thread_id: thread_id.clone(),
            text: "second turn".into(),
            attachments: Vec::new(),
        })
        .await
        .expect("send message");
    match accepted {
        ResponseBody::MessageAccepted {
            thread_id: accepted_thread,
            turn_id: accepted_turn,
            seq,
        } => {
            assert_eq!(accepted_thread, thread_id);
            assert_ne!(accepted_turn, first_turn_id, "the next turn gets a new id");
            assert!(seq > 0);
        }
        other => panic!("unexpected send response: {other:?}"),
    }

    daemon.stop().await;
}

#[tokio::test]
async fn skills_list_returns_the_install_matrix_and_conflicts() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    match client
        .request(ClientRequest::SkillsList { host: host.clone() })
        .await
        .expect("list skills")
    {
        ResponseBody::Skills { inventory } => {
            assert!(inventory.skills.is_empty());
            assert_eq!(inventory.harnesses.len(), 4);
            let harnesses: Vec<_> = inventory
                .harnesses
                .iter()
                .map(|entry| entry.harness)
                .collect();
            for harness in [
                HarnessKind::ClaudeCode,
                HarnessKind::Codex,
                HarnessKind::Opencode,
                HarnessKind::Antigravity,
            ] {
                assert!(harnesses.contains(&harness), "missing {harness:?}");
            }
        }
        other => panic!("unexpected skills response: {other:?}"),
    }

    match client
        .request(ClientRequest::SkillsConflicts { host })
        .await
        .expect("list conflicts")
    {
        ResponseBody::SkillConflicts { conflicts } => assert!(conflicts.is_empty()),
        other => panic!("unexpected conflicts response: {other:?}"),
    }

    daemon.stop().await;
}

#[tokio::test]
async fn harness_versions_list_every_harness_and_treat_offline_as_unknown() {
    let daemon = boot_isolated().await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    match client
        .request(ClientRequest::ListHarnessVersions)
        .await
        .expect("list harness versions")
    {
        ResponseBody::HarnessVersions { harnesses } => {
            assert_eq!(harnesses.len(), 4);
            // The offline registry is "unknown" for every row, not an error.
            assert!(harnesses.iter().all(|row| row.latest.is_none()));
            assert!(harnesses.iter().all(|row| !row.update_available));
            let claude = harnesses
                .iter()
                .find(|row| row.harness == HarnessKind::ClaudeCode)
                .expect("claude row");
            assert!(claude.installed.is_some(), "the fake harness is installed");
        }
        other => panic!("unexpected harness versions response: {other:?}"),
    }

    // Codex is not registered in this daemon, so there is nothing to update.
    let error = client
        .request(ClientRequest::UpdateHarness {
            harness: HarnessKind::Codex,
        })
        .await
        .expect_err("an uninstalled harness cannot be updated");
    assert_eq!(error.code, h4rn_protocol::ProtoErrorCode::NotFound);

    daemon.stop().await;
}

// ---------------------------------------------------------------------------
// Phase 5: metering, limits and auto-resume (ADR-024).
// ---------------------------------------------------------------------------

fn limit_script(
    reset_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Vec<h4rn_harness::HarnessEvent> {
    use h4rn_harness::HarnessEvent;
    use h4rn_protocol::Usage;

    vec![
        HarnessEvent::Usage {
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Usage::default()
            },
        },
        HarnessEvent::LimitHit {
            reset_at,
            auto_resume: true,
        },
        HarnessEvent::TurnCompleted {
            status: TurnStatus::Failed,
            usage: Usage::default(),
        },
    ]
}

fn resume_script() -> Vec<h4rn_harness::HarnessEvent> {
    use h4rn_harness::HarnessEvent;
    use h4rn_protocol::Usage;

    vec![
        HarnessEvent::TextDelta {
            text: "resumed".into(),
        },
        HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 2,
                ..Usage::default()
            },
        },
    ]
}

fn resume_settings() -> LifecycleSettings {
    LifecycleSettings::default()
        .with_working_exit_grace(chrono::TimeDelta::milliseconds(80))
        .with_stale_bypass(chrono::TimeDelta::seconds(60))
}

fn resume_options() -> ThreadManagerOptions {
    ThreadManagerOptions {
        settings: resume_settings(),
        tick_interval: Duration::from_millis(20),
        clock: None,
    }
}

async fn stable_policy(client: &mut Client, failover: bool) {
    client
        .request(ClientRequest::SetAutoResumePolicy {
            policy: AutoResumePolicy {
                failover,
                jitter_ms: 0,
                ..AutoResumePolicy::default()
            },
        })
        .await
        .expect("set auto-resume policy");
}

async fn thread_summary(client: &mut Client, thread_id: &ThreadId) -> h4rn_protocol::ThreadSummary {
    match client
        .request(ClientRequest::GetThread {
            thread_id: thread_id.clone(),
            since_seq: 0,
        })
        .await
        .expect("get thread")
    {
        ResponseBody::Thread { summary, .. } => summary,
        other => panic!("unexpected GetThread response: {other:?}"),
    }
}

async fn thread_usage(client: &mut Client, thread_id: &ThreadId) -> h4rn_protocol::UsageReport {
    match client
        .request(ClientRequest::GetUsage {
            scope: UsageScope::Thread,
            scope_id: Some(thread_id.as_str().to_owned()),
            include_children: false,
            range: UsageRange::default(),
            group_by: UsageGroupBy::None,
            bucket: None,
            series_group_by: None,
        })
        .await
        .expect("get usage")
    {
        ResponseBody::UsageReport { report } => report,
        other => panic!("unexpected GetUsage response: {other:?}"),
    }
}

async fn schedules(client: &mut Client) -> Vec<h4rn_protocol::ResumeSchedule> {
    match client
        .request(ClientRequest::ListResets)
        .await
        .expect("list resets")
    {
        ResponseBody::Resets { resets } => resets.schedules,
        other => panic!("unexpected ListResets response: {other:?}"),
    }
}

#[tokio::test]
async fn a_limit_hit_stays_working_and_auto_resumes_at_the_provider_reset() {
    let clock = ManualClock::new();
    let reset_at = clock.now() + chrono::TimeDelta::seconds(4);
    let daemon = boot_with(
        clocked_options(&clock, resume_settings()),
        FakeHarness::with_session_scripts(vec![limit_script(Some(reset_at)), resume_script()]),
    )
    .await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    stable_policy(&mut client, false).await;
    let summary = create_with_model(
        &mut client,
        host,
        "do the thing",
        HarnessKind::ClaudeCode,
        "claude-sonnet-5",
    )
    .await;
    let thread_id = summary.thread_id;

    let (_, event) = client
        .wait_event(|event| matches!(event, Event::UsageLimitHit { .. }))
        .await;
    match event {
        Event::UsageLimitHit {
            thread_id: limited,
            reset_at: Some(at),
            auto_resume,
        } => {
            assert_eq!(limited, thread_id);
            assert_eq!(at, reset_at);
            assert!(auto_resume);
        }
        other => panic!("unexpected limit event: {other:?}"),
    }

    let (_, status) = client.wait_for_turn_completed().await;
    assert_eq!(status, TurnStatus::Failed);
    let limited = thread_summary(&mut client, &thread_id).await;
    assert_eq!(limited.tab, ThreadTab::Working);
    assert_eq!(limited.agent_state, AgentState::Scheduled);
    let resume_at = limited
        .resume_at
        .expect("a scheduled thread names its resume time");
    assert!(
        resume_at >= reset_at,
        "the resume target is never earlier than the provider reset"
    );
    let limit = limited.limit.clone().expect("the limit is summarised");
    assert_eq!(limit.harness, Some(HarnessKind::ClaudeCode));
    assert_eq!(limit.reset_at, Some(reset_at));

    match client
        .request(ClientRequest::GetLimits)
        .await
        .expect("limits")
    {
        ResponseBody::Limits { report } => assert!(!report.limits.is_empty()),
        other => panic!("unexpected GetLimits response: {other:?}"),
    }
    let persisted = schedules(&mut client).await;
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].thread_id, thread_id);
    assert_eq!(persisted[0].state, ResumeState::Scheduled);

    // The provider resets: the schedule fires on the next tick and starts a
    // new turn. Time is the test's to move, so the schedule cannot come due
    // while the limit is being asserted.
    clock.advance(chrono::TimeDelta::seconds(5));
    client
        .wait_event(|event| {
            matches!(
                event,
                Event::ItemCompleted {
                    item: Item::Message { role: MessageRole::User, text, .. },
                    ..
                } if text.contains("usage limit reset")
            )
        })
        .await;
    client
        .wait_event(|event| matches!(event, Event::TurnStarted { .. }))
        .await;
    let (_, status) = client.wait_for_turn_completed().await;
    assert_eq!(status, TurnStatus::Completed);

    let usage = thread_usage(&mut client, &thread_id).await;
    assert_eq!(usage.totals.turns, 2);
    assert_eq!(usage.totals.input_tokens, 11);
    assert_eq!(usage.totals.output_tokens, 7);
    assert!(usage.totals.cost_micros > 0);

    let resets = schedules(&mut client).await;
    assert_eq!(resets[0].state, ResumeState::Fired);
    assert!(resets[0].last_error.is_none());

    // Cross the post-turn grace so the resumed turn is projected as done.
    clock.advance(chrono::TimeDelta::milliseconds(200));
    let (_, done) = client.wait_for_tab(ThreadTab::Done).await;
    assert_eq!(done.agent_state, AgentState::Done);
    assert!(
        done.resume_at.is_none(),
        "a fired schedule no longer names a resume time"
    );
    assert!(
        done.limit.is_none(),
        "a completed resume clears the limit summary"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn cancelling_auto_resume_returns_the_thread_to_rate_limited() {
    let reset_at = chrono::Utc::now() + chrono::Duration::hours(1);
    let daemon = boot_with(
        resume_options(),
        FakeHarness::with_session_scripts(vec![limit_script(Some(reset_at)), resume_script()]),
    )
    .await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    stable_policy(&mut client, false).await;
    let summary = create_with_model(
        &mut client,
        host,
        "do the thing",
        HarnessKind::ClaudeCode,
        "claude-sonnet-5",
    )
    .await;
    let thread_id = summary.thread_id;

    client
        .wait_event(|event| matches!(event, Event::UsageLimitHit { .. }))
        .await;
    client.wait_for_turn_completed().await;

    match client
        .request(ClientRequest::CancelResume {
            thread_id: thread_id.clone(),
        })
        .await
        .expect("cancel resume")
    {
        ResponseBody::ResumeChanged {
            schedule: Some(schedule),
        } => assert_eq!(schedule.state, ResumeState::Cancelled),
        other => panic!("unexpected CancelResume response: {other:?}"),
    }

    let cancelled = thread_summary(&mut client, &thread_id).await;
    assert_eq!(cancelled.tab, ThreadTab::Done);
    assert_eq!(cancelled.agent_state, AgentState::RateLimited);
    assert!(
        cancelled.resume_at.is_none(),
        "a cancelled schedule clears the resume target"
    );
    assert!(
        cancelled.limit.is_some(),
        "the observed limit stays visible after cancelling"
    );

    let quiet = tokio::time::timeout(
        Duration::from_millis(400),
        client.wait_event(|event| matches!(event, Event::TurnStarted { .. })),
    )
    .await;
    assert!(quiet.is_err(), "a cancelled schedule must not resume");

    assert_eq!(
        schedules(&mut client).await[0].state,
        ResumeState::Cancelled
    );

    let error = client
        .request(ClientRequest::ResumeNow {
            thread_id: thread_id.clone(),
        })
        .await
        .expect_err("a cancelled schedule cannot resume");
    assert_eq!(error.code, ProtoErrorCode::Conflict);

    daemon.stop().await;
}

#[tokio::test]
async fn resume_now_fires_a_scheduled_thread_without_waiting_for_the_reset() {
    let reset_at = chrono::Utc::now() + chrono::Duration::hours(1);
    let daemon = boot_with(
        resume_options(),
        FakeHarness::with_session_scripts(vec![limit_script(Some(reset_at)), resume_script()]),
    )
    .await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    stable_policy(&mut client, false).await;
    let summary = create_with_model(
        &mut client,
        host,
        "do the thing",
        HarnessKind::ClaudeCode,
        "claude-sonnet-5",
    )
    .await;
    let thread_id = summary.thread_id;

    client
        .wait_event(|event| matches!(event, Event::UsageLimitHit { .. }))
        .await;
    client.wait_for_turn_completed().await;

    match client
        .request(ClientRequest::ResumeNow {
            thread_id: thread_id.clone(),
        })
        .await
        .expect("resume now")
    {
        ResponseBody::ResumeChanged {
            schedule: Some(schedule),
        } => assert_eq!(schedule.state, ResumeState::Fired),
        other => panic!("unexpected ResumeNow response: {other:?}"),
    }

    client
        .wait_event(|event| matches!(event, Event::TurnStarted { .. }))
        .await;
    let (_, status) = client.wait_for_turn_completed().await;
    assert_eq!(status, TurnStatus::Completed);
    assert_eq!(thread_usage(&mut client, &thread_id).await.totals.turns, 2);
    assert_eq!(schedules(&mut client).await[0].state, ResumeState::Fired);
    let resumed = thread_summary(&mut client, &thread_id).await;
    assert!(resumed.resume_at.is_none());
    assert!(resumed.limit.is_none());

    daemon.stop().await;
}

#[tokio::test]
async fn resume_now_acknowledges_before_a_slow_first_send() {
    let reset_at = chrono::Utc::now() + chrono::Duration::hours(1);
    // The adapter holds the resumed turn's input until the test opens this
    // gate, so an answer that arrives first provably did not wait for the send.
    let (harness, gate) =
        FakeHarness::with_session_scripts(vec![limit_script(Some(reset_at)), resume_script()])
            .with_send_gate();
    let daemon = boot_with(resume_options(), harness).await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    stable_policy(&mut client, false).await;
    let summary = create_with_model(
        &mut client,
        host,
        "do the thing",
        HarnessKind::ClaudeCode,
        "claude-sonnet-5",
    )
    .await;
    let thread_id = summary.thread_id;
    client
        .wait_event(|event| matches!(event, Event::UsageLimitHit { .. }))
        .await;
    client.wait_for_turn_completed().await;

    match client
        .request(ClientRequest::ResumeNow {
            thread_id: thread_id.clone(),
        })
        .await
        .expect("resume now")
    {
        ResponseBody::ResumeChanged {
            schedule: Some(schedule),
        } => assert_eq!(schedule.state, ResumeState::Fired),
        other => panic!("unexpected ResumeNow response: {other:?}"),
    }
    assert!(
        !gate.is_released(),
        "ResumeNow answered while the harness was still holding the input"
    );

    // The resumed turn still runs and reports progress through events once the
    // adapter accepts the input.
    gate.release();
    client
        .wait_event(|event| matches!(event, Event::TurnStarted { .. }))
        .await;
    let (_, status) = client.wait_for_turn_completed().await;
    assert_eq!(status, TurnStatus::Completed);
    assert_eq!(thread_usage(&mut client, &thread_id).await.totals.turns, 2);

    daemon.stop().await;
}

#[tokio::test]
async fn send_message_acknowledges_before_a_slow_harness_write() {
    let (harness, gate) = FakeHarness::new().with_send_gate();
    let daemon = boot_with(resume_options(), harness).await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let summary = create_with_host(&mut client, host, "first", HarnessKind::ClaudeCode).await;
    let thread_id = summary.thread_id;
    client.wait_for_turn_completed().await;

    // The adapter now holds the text. The request must be acknowledged as soon
    // as the turn is recorded, not when the write lands.
    let accepted = client
        .request(ClientRequest::SendMessage {
            thread_id: thread_id.clone(),
            text: "second".into(),
            attachments: Vec::new(),
        })
        .await
        .expect("send");
    assert!(matches!(accepted, ResponseBody::MessageAccepted { .. }));
    assert!(
        !gate.is_released(),
        "SendMessage answered while the harness was still holding the write"
    );

    // The thread record stays available while that write is in flight, so a
    // snapshot or settings request queued behind it still answers.
    client
        .request(ClientRequest::GetThread {
            thread_id: thread_id.clone(),
            since_seq: 0,
        })
        .await
        .expect("get thread");
    assert!(!gate.is_released());

    gate.release();
    let (_, status) = client.wait_for_turn_completed().await;
    assert_eq!(status, TurnStatus::Completed);

    daemon.stop().await;
}

#[tokio::test]
async fn a_restart_rehydrates_the_summary_resume_target() {
    let reset_at = chrono::Utc::now() + chrono::Duration::hours(1);
    let mut daemon = boot_with(
        resume_options(),
        FakeHarness::with_session_scripts(vec![limit_script(Some(reset_at)), resume_script()]),
    )
    .await;
    let dir = daemon._dir.take().expect("temp dir").keep();
    let host = host_id_for(&daemon).await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let summary = create_with_model(
        &mut client,
        host,
        "do the thing",
        HarnessKind::ClaudeCode,
        "claude-sonnet-5",
    )
    .await;
    let thread_id = summary.thread_id;
    client
        .wait_event(|event| matches!(event, Event::UsageLimitHit { .. }))
        .await;
    client.wait_for_turn_completed().await;
    daemon.stop().await;

    let config = Config {
        data_dir: dir.clone(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Auto,
        network_access: None,
        network_port: 0,
    };
    let rebooted = DaemonBuilder::new(config)
        .with_harness(Arc::new(FakeHarness::new()))
        .with_lifecycle(resume_options())
        .build()
        .await
        .expect("second daemon");
    let token = TokenStore::new(&dir).load().unwrap().unwrap();
    let bound = rebooted.bind().await.expect("bind again");
    let addr = bound.local_addr();
    let (shutdown, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        bound
            .serve(async move {
                let _ = rx.await;
            })
            .await
    });
    let second = TestDaemon {
        addr,
        token,
        _dir: None,
        shutdown: Some(shutdown),
        task,
    };

    let mut client = Client::connect(&second).await;
    client.hello().await;
    let restored = thread_summary(&mut client, &thread_id).await;
    assert!(
        restored.resume_at.is_some(),
        "the persisted schedule is visible before the next tick"
    );
    assert!(
        restored.limit.is_some(),
        "the limit record survives the restart"
    );

    second.stop().await;
}

/// Matches a request that does *not* carry `name`.
struct WithoutHeader(&'static str);

impl wiremock::Match for WithoutHeader {
    fn matches(&self, request: &wiremock::Request) -> bool {
        !request.headers.contains_key(self.0)
    }
}

async fn mock_proxy_catalog(server: &wiremock::MockServer) {
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, ResponseTemplate};

    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(WithoutHeader("anthropic-version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "claude-sonnet-5", "owned_by": "anthropic"},
                {"id": "claude-opus-5", "owned_by": "anthropic"}
            ]
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "claude-sonnet-5", "type": "model"}]
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v0/management/auth-files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"files": []})))
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_scheduled_resume_records_the_proxy_failover_choice() {
    let server = wiremock::MockServer::start().await;
    mock_proxy_catalog(&server).await;

    let reset_at = chrono::Utc::now() + chrono::Duration::milliseconds(200);
    let daemon = boot_with(
        resume_options(),
        FakeHarness::with_session_scripts(vec![limit_script(Some(reset_at)), resume_script()]),
    )
    .await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let profile = match client
        .request(ClientRequest::AddProxyProfile {
            profile: ProxyProfileDraft {
                name: "mock-failover".into(),
                base_url: server.uri(),
                management_key: Some("mgmt-secret".into()),
                api_key: Some("sk-client".into()),
                default: true,
            },
        })
        .await
        .expect("add proxy profile")
    {
        ResponseBody::ProxyProfile { profile } => profile,
        other => panic!("unexpected AddProxyProfile response: {other:?}"),
    };
    client
        .request(ClientRequest::SetProxyHarnessOverride {
            profile_id: profile.id.clone(),
            harness: HarnessKind::ClaudeCode,
            enabled: true,
            default_model: None,
        })
        .await
        .expect("enable proxy for Claude Code");
    client
        .request(ClientRequest::SetAutoResumePolicy {
            policy: AutoResumePolicy {
                failover: true,
                max_wait_ms: 3_600_000,
                prompt: "continue after the limit".into(),
                jitter_ms: 0,
            },
        })
        .await
        .expect("set policy");

    let summary = create_with_model(
        &mut client,
        host,
        "do the thing",
        HarnessKind::ClaudeCode,
        "claude-sonnet-5",
    )
    .await;
    let thread_id = summary.thread_id;

    client
        .wait_event(|event| matches!(event, Event::UsageLimitHit { .. }))
        .await;
    client.wait_for_turn_completed().await;
    client
        .wait_event(|event| matches!(event, Event::TurnStarted { .. }))
        .await;
    client.wait_for_turn_completed().await;

    let schedule = schedules(&mut client).await.remove(0);
    assert_eq!(schedule.state, ResumeState::Fired);
    let failover = schedule.failover.expect("a failover choice was recorded");
    assert_eq!(failover.model, "claude-opus-5");
    assert!(failover.reason.contains("same family"));

    let resumed = thread_summary(&mut client, &thread_id).await;
    assert_eq!(resumed.model, "claude-opus-5");

    daemon.stop().await;
}

/// Same catalogue as [`mock_proxy_catalog`], but the plain `/v1/models` read
/// is delayed so a test can observe what waits on it.
async fn mock_slow_proxy_catalog(server: &wiremock::MockServer, delay: Duration) {
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, ResponseTemplate};

    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(WithoutHeader("anthropic-version"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(delay)
                .set_body_json(json!({
                    "data": [
                        {"id": "claude-sonnet-5", "owned_by": "anthropic"},
                        {"id": "claude-opus-5", "owned_by": "anthropic"}
                    ]
                })),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "claude-sonnet-5", "type": "model"}]
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v0/management/auth-files"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"files": []})))
        .mount(server)
        .await;
}

/// A resume that is choosing a failover talks to the proxy. That HTTP call
/// must not hold the thread record lock: GetThread from another connection has
/// to answer while the proxy is still thinking. Before the fix the resume held
/// the lock across `pick_failover`, so this request waited out the whole delay.
#[tokio::test]
async fn a_slow_proxy_does_not_freeze_the_thread_it_resumes() {
    let server = wiremock::MockServer::start().await;
    let delay = Duration::from_secs(5);
    mock_slow_proxy_catalog(&server, delay).await;

    let reset_at = chrono::Utc::now() + chrono::Duration::hours(1);
    let daemon = boot_with(
        resume_options(),
        FakeHarness::with_session_scripts(vec![limit_script(Some(reset_at)), resume_script()]),
    )
    .await;
    let host = host_id_for(&daemon).await;

    let mut resumer = Client::connect(&daemon).await;
    resumer.hello().await;
    let thread_id = limited_thread_on_a_proxy(&mut resumer, host, &server).await;
    stable_policy(&mut resumer, true).await;

    let mut observer = Client::connect(&daemon).await;
    observer.hello().await;

    let resumed_thread = thread_id.clone();
    let resume = tokio::spawn(async move {
        resumer
            .request(ClientRequest::ResumeNow {
                thread_id: resumed_thread,
            })
            .await
    });

    // Let the resume reach the proxy call.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let answer = tokio::time::timeout(
        Duration::from_secs(1),
        observer.request(ClientRequest::GetThread {
            thread_id: thread_id.clone(),
            since_seq: 0,
        }),
    )
    .await
    .expect("GetThread must not wait for the proxy")
    .expect("get thread");
    assert!(matches!(answer, ResponseBody::Thread { .. }));

    let fired = tokio::time::timeout(Duration::from_secs(20), resume)
        .await
        .expect("the resume finishes once the proxy answers")
        .expect("resume task")
        .expect("resume now");
    let ResponseBody::ResumeChanged {
        schedule: Some(schedule),
    } = fired
    else {
        panic!("unexpected ResumeNow response: {fired:?}");
    };
    assert_eq!(schedule.state, ResumeState::Fired);
    assert_eq!(
        thread_summary(&mut observer, &thread_id).await.model,
        "claude-opus-5"
    );

    daemon.stop().await;
}

/// A cancellation that lands while the resume is choosing a failover must win.
/// The resume only claims its attempt after the proxy answers, under the
/// thread lock and with a conditional ledger update, so a card the user
/// cancelled can never start a turn or switch the model.
#[tokio::test]
async fn cancelling_while_the_proxy_picks_a_failover_does_not_resume() {
    let server = wiremock::MockServer::start().await;
    let delay = Duration::from_secs(2);
    mock_slow_proxy_catalog(&server, delay).await;

    let reset_at = chrono::Utc::now() + chrono::Duration::hours(1);
    let daemon = boot_with(
        resume_options(),
        FakeHarness::with_session_scripts(vec![limit_script(Some(reset_at)), resume_script()]),
    )
    .await;
    let host = host_id_for(&daemon).await;

    let mut resumer = Client::connect(&daemon).await;
    resumer.hello().await;
    let thread_id = limited_thread_on_a_proxy(&mut resumer, host, &server).await;
    stable_policy(&mut resumer, true).await;

    let mut canceler = Client::connect(&daemon).await;
    canceler.hello().await;

    let resumed_thread = thread_id.clone();
    let resume = tokio::spawn(async move {
        resumer
            .request(ClientRequest::ResumeNow {
                thread_id: resumed_thread,
            })
            .await
    });

    // Let the resume reach the proxy call, then cancel it while it waits.
    tokio::time::sleep(Duration::from_millis(300)).await;
    match canceler
        .request(ClientRequest::CancelResume {
            thread_id: thread_id.clone(),
        })
        .await
        .expect("cancel resume")
    {
        ResponseBody::ResumeChanged {
            schedule: Some(schedule),
        } => assert_eq!(schedule.state, ResumeState::Cancelled),
        other => panic!("unexpected CancelResume response: {other:?}"),
    }

    let resumed = tokio::time::timeout(Duration::from_secs(20), resume)
        .await
        .expect("the paused resume finishes")
        .expect("resume task")
        .expect("resume now");
    match resumed {
        ResponseBody::ResumeChanged { schedule } => assert!(
            schedule.is_none(),
            "a cancelled resume reports no fired schedule: {schedule:?}"
        ),
        other => panic!("unexpected ResumeNow response: {other:?}"),
    }

    let quiet = tokio::time::timeout(
        Duration::from_millis(600),
        canceler.wait_event(|event| matches!(event, Event::TurnStarted { .. })),
    )
    .await;
    assert!(quiet.is_err(), "a cancelled resume must not start a turn");

    let cancelled = thread_summary(&mut canceler, &thread_id).await;
    assert_eq!(
        cancelled.model, "claude-sonnet-5",
        "a cancelled resume must not switch models"
    );
    assert_eq!(
        schedules(&mut canceler).await[0].state,
        ResumeState::Cancelled
    );

    daemon.stop().await;
}

/// The shared setup of the fail-over tests: a mock proxy serving two Claude
/// models, a Claude Code override through it and a policy that never fires on
/// its own (the reset is an hour away).
async fn limited_thread_on_a_proxy(
    client: &mut Client,
    host: HostId,
    server: &wiremock::MockServer,
) -> ThreadId {
    let profile = match client
        .request(ClientRequest::AddProxyProfile {
            profile: ProxyProfileDraft {
                name: "mock-failover".into(),
                base_url: server.uri(),
                management_key: Some("mgmt-secret".into()),
                api_key: Some("sk-client".into()),
                default: true,
            },
        })
        .await
        .expect("add proxy profile")
    {
        ResponseBody::ProxyProfile { profile } => profile,
        other => panic!("unexpected AddProxyProfile response: {other:?}"),
    };
    client
        .request(ClientRequest::SetProxyHarnessOverride {
            profile_id: profile.id.clone(),
            harness: HarnessKind::ClaudeCode,
            enabled: true,
            default_model: None,
        })
        .await
        .expect("enable proxy for Claude Code");
    stable_policy(client, false).await;
    let summary = create_with_model(
        client,
        host,
        "do the thing",
        HarnessKind::ClaudeCode,
        "claude-sonnet-5",
    )
    .await;
    client
        .wait_event(|event| matches!(event, Event::UsageLimitHit { .. }))
        .await;
    client.wait_for_turn_completed().await;
    summary.thread_id
}

#[tokio::test]
async fn a_limit_hit_thread_fails_over_now_to_the_default_or_a_chosen_model() {
    let server = wiremock::MockServer::start().await;
    mock_proxy_catalog(&server).await;

    let reset_at = chrono::Utc::now() + chrono::Duration::hours(1);
    let daemon = boot_with(
        resume_options(),
        FakeHarness::with_session_scripts(vec![limit_script(Some(reset_at)), resume_script()]),
    )
    .await;
    let host = host_id_for(&daemon).await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let thread_id = limited_thread_on_a_proxy(&mut client, host, &server).await;

    // The daemon names its default and every eligible alternative.
    let options = match client
        .request(ClientRequest::ListFailoverOptions {
            thread_id: thread_id.clone(),
        })
        .await
        .expect("list failover options")
    {
        ResponseBody::FailoverOptions { options } => options,
        other => panic!("unexpected ListFailoverOptions response: {other:?}"),
    };
    assert_eq!(options.current_model, "claude-sonnet-5");
    assert!(options.unavailable.is_none());
    assert_eq!(
        options.default.as_ref().map(|choice| choice.model.as_str()),
        Some("claude-opus-5")
    );
    assert_eq!(options.options.first(), options.default.as_ref());

    // A model the daemon did not offer is refused and nothing fires.
    let refused = client
        .request(ClientRequest::FailOver {
            thread_id: thread_id.clone(),
            model: Some("gpt-not-offered".into()),
            account: None,
        })
        .await
        .expect_err("an ineligible model is refused");
    assert_eq!(refused.code, h4rn_protocol::ProtoErrorCode::InvalidRequest);
    assert_eq!(
        schedules(&mut client).await.remove(0).state,
        ResumeState::Scheduled
    );

    // Choosing an eligible model fires the schedule now, an hour early.
    let changed = client
        .request(ClientRequest::FailOver {
            thread_id: thread_id.clone(),
            model: Some("claude-opus-5".into()),
            account: None,
        })
        .await
        .expect("fail over");
    let ResponseBody::ResumeChanged {
        schedule: Some(fired),
    } = changed
    else {
        panic!("unexpected FailOver response: {changed:?}");
    };
    assert_eq!(fired.state, ResumeState::Fired);
    assert_eq!(
        fired
            .failover
            .as_ref()
            .map(|failover| failover.model.as_str()),
        Some("claude-opus-5")
    );
    client
        .wait_event(|event| matches!(event, Event::TurnStarted { .. }))
        .await;
    client.wait_for_turn_completed().await;
    assert_eq!(
        thread_summary(&mut client, &thread_id).await.model,
        "claude-opus-5"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn fail_over_without_a_proxy_says_why_instead_of_guessing() {
    let reset_at = chrono::Utc::now() + chrono::Duration::hours(1);
    let daemon = boot_with(
        resume_options(),
        FakeHarness::with_session_scripts(vec![limit_script(Some(reset_at))]),
    )
    .await;
    let host = host_id_for(&daemon).await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    stable_policy(&mut client, false).await;
    let summary = create_with_model(
        &mut client,
        host,
        "do the thing",
        HarnessKind::ClaudeCode,
        "claude-sonnet-5",
    )
    .await;
    client
        .wait_event(|event| matches!(event, Event::UsageLimitHit { .. }))
        .await;
    client.wait_for_turn_completed().await;

    let options = match client
        .request(ClientRequest::ListFailoverOptions {
            thread_id: summary.thread_id.clone(),
        })
        .await
        .expect("list failover options")
    {
        ResponseBody::FailoverOptions { options } => options,
        other => panic!("unexpected ListFailoverOptions response: {other:?}"),
    };
    assert!(options.options.is_empty());
    assert!(options.default.is_none());
    assert!(
        options
            .unavailable
            .is_some_and(|reason| reason.contains("proxy"))
    );

    let refused = client
        .request(ClientRequest::FailOver {
            thread_id: summary.thread_id,
            model: None,
            account: None,
        })
        .await
        .expect_err("nothing to fail over to");
    assert_eq!(refused.code, h4rn_protocol::ProtoErrorCode::Conflict);
    daemon.stop().await;
}

#[tokio::test]
async fn an_edit_tool_call_reaches_the_client_with_its_diffstat() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let workspace = tempfile::tempdir().expect("temp workspace");
    // A file that already exists, so the stat is a real diff and not "all new".
    std::fs::write(workspace.path().join("notes.txt"), "keep\nold\n").unwrap();

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    client
        .request(ClientRequest::CreateThread {
            host,
            harness: HarnessKind::ClaudeCode,
            workspace_path: workspace.path().display().to_string(),
            model: "fake-model".into(),
            prompt: "edit it [[write:notes.txt]]keep\nnew\nmore".into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            worktree: None,
        })
        .await
        .expect("create thread");

    let event = client
        .wait_event(|event| {
            matches!(
                event,
                Event::ToolCall { tool_call, .. }
                    if tool_call.status == h4rn_protocol::ToolCallStatus::Completed
            )
        })
        .await;
    let (_, Event::ToolCall { tool_call, .. }) = event else {
        unreachable!("filtered above");
    };
    // `old` became `new` and `more` was added: two lines in, one out.
    assert_eq!(
        tool_call.stats,
        Some(h4rn_protocol::ToolStats::Diff {
            added: 2,
            removed: 1
        })
    );
    daemon.stop().await;
}

#[tokio::test]
async fn turn_completed_carries_the_turns_change_set() {
    // The harness announces the edit, then waits for the test: the daemon
    // journals the announced tool call only after it captured the pre-turn
    // snapshot, so releasing the gate here is what makes the diff base the
    // file's pre-turn state instead of a race against a fixed sleep.
    let (harness, write_gate) = FakeHarness::new().with_write_gate();
    let daemon = boot_with(
        ThreadManagerOptions {
            settings: LifecycleSettings::default()
                .with_working_exit_grace(chrono::TimeDelta::milliseconds(80))
                .with_stale_bypass(chrono::TimeDelta::seconds(60)),
            tick_interval: Duration::from_millis(20),
            clock: None,
        },
        harness,
    )
    .await;
    let host = host_id_for(&daemon).await;
    let workspace = tempfile::tempdir().expect("temp workspace");

    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let result = client
        .request(ClientRequest::CreateThread {
            host,
            harness: HarnessKind::ClaudeCode,
            workspace_path: workspace.path().display().to_string(),
            model: "fake-model".into(),
            prompt: "make a note [[write:notes.txt]]hello from the fake harness".into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            worktree: None,
        })
        .await
        .expect("create thread");
    assert!(matches!(result, ResponseBody::ThreadCreated { .. }));

    client
        .wait_event(|event| {
            matches!(
                event,
                Event::ToolCall { tool_call, .. }
                    if tool_call.status == ToolCallStatus::Running
            )
        })
        .await;
    write_gate.release();

    let (_, event) = client
        .wait_event(|event| matches!(event, Event::TurnCompleted { .. }))
        .await;
    let Event::TurnCompleted { changes, .. } = event else {
        unreachable!();
    };
    let changes = changes.expect("the daemon computed a change set");
    assert_eq!(changes.files.len(), 1, "one file changed: {changes:?}");
    assert_eq!(changes.files[0].path, "notes.txt");
    assert_eq!(changes.files[0].added, 1);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("notes.txt")).expect("written"),
        "hello from the fake harness"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn thread_summary_names_the_workspace_branch() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let workspace = tempfile::tempdir().expect("temp workspace");
    let status = std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .arg(workspace.path())
        .status()
        .expect("run git init");
    assert!(status.success(), "git init failed");

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let result = client
        .request(ClientRequest::CreateThread {
            host,
            harness: HarnessKind::ClaudeCode,
            workspace_path: workspace.path().display().to_string(),
            model: "fake-model".into(),
            prompt: "name the branch".into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            worktree: None,
        })
        .await
        .expect("create thread");
    let ResponseBody::ThreadCreated { summary, .. } = result else {
        panic!("expected ThreadCreated");
    };
    assert_eq!(summary.branch.as_deref(), Some("main"));

    daemon.stop().await;
}

#[tokio::test]
async fn applying_proxy_overrides_drops_the_sessions_of_routed_harnesses() {
    let server = wiremock::MockServer::start().await;
    mock_proxy_catalog(&server).await;
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let profile = match client
        .request(ClientRequest::AddProxyProfile {
            profile: ProxyProfileDraft {
                name: "mock-apply".into(),
                base_url: server.uri(),
                management_key: Some("mgmt-secret".into()),
                api_key: Some("sk-client".into()),
                default: true,
            },
        })
        .await
        .expect("add proxy profile")
    {
        ResponseBody::ProxyProfile { profile } => profile,
        other => panic!("unexpected AddProxyProfile response: {other:?}"),
    };
    // The client sees where the override lands, and never the key.
    let claude = profile
        .harnesses
        .iter()
        .find(|routed| routed.harness == HarnessKind::ClaudeCode)
        .expect("claude row");
    assert_eq!(claude.config_path, "~/.claude/settings.json");
    assert!(claude.preview.iter().any(|line| line.value.is_none()));
    assert!(!format!("{:?}", claude.preview).contains("sk-client"));

    let summary = create_with_model(
        &mut client,
        host,
        "hello",
        HarnessKind::ClaudeCode,
        "claude-sonnet-5",
    )
    .await;
    client.wait_for_turn_completed().await;

    // Nothing is routed yet, so nothing restarts.
    let applied = |response| match response {
        ResponseBody::ProxyApplied {
            restarted,
            interrupted,
        } => (restarted, interrupted),
        other => panic!("unexpected ApplyProxyOverrides response: {other:?}"),
    };
    let (restarted, _) = applied(
        client
            .request(ClientRequest::ApplyProxyOverrides {
                profile_id: profile.id.clone(),
            })
            .await
            .expect("apply with no routed harness"),
    );
    assert!(restarted.is_empty());

    client
        .request(ClientRequest::SetProxyHarnessOverride {
            profile_id: profile.id.clone(),
            harness: HarnessKind::ClaudeCode,
            enabled: true,
            default_model: None,
        })
        .await
        .expect("enable proxy for Claude Code");
    let (restarted, interrupted) = applied(
        client
            .request(ClientRequest::ApplyProxyOverrides {
                profile_id: profile.id.clone(),
            })
            .await
            .expect("apply"),
    );
    assert_eq!(restarted, vec![summary.thread_id]);
    assert!(interrupted.is_empty());

    daemon.stop().await;
}

/// A limit hit whose turn never completes: the thread stays active while its
/// resume schedule comes due.
fn limit_while_active_script(
    reset_at: chrono::DateTime<chrono::Utc>,
) -> Vec<h4rn_harness::HarnessEvent> {
    let mut script = limit_script(Some(reset_at));
    script.pop();
    script
}

#[tokio::test]
async fn a_due_schedule_on_an_active_thread_waits_silently_without_a_duplicate_prompt() {
    let reset_at = chrono::Utc::now() + chrono::Duration::milliseconds(100);
    let daemon = boot_with(
        resume_options(),
        FakeHarness::with_session_scripts(vec![limit_while_active_script(reset_at)]),
    )
    .await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    stable_policy(&mut client, false).await;
    create_with_model(
        &mut client,
        host,
        "do the thing",
        HarnessKind::ClaudeCode,
        "claude-sonnet-5",
    )
    .await;
    client
        .wait_event(|event| matches!(event, Event::UsageLimitHit { .. }))
        .await;

    // Observe for 3.5 s: the schedule is due the whole time, the turn is active.
    let mut errors = 0;
    let mut prompts = 0;
    let _ = tokio::time::timeout(Duration::from_millis(3500), async {
        loop {
            let (_, event) = client.next_event().await;
            match event {
                Event::Error { .. } => errors += 1,
                Event::ItemCompleted {
                    item:
                        Item::Message {
                            role: MessageRole::User,
                            text,
                            ..
                        },
                    ..
                } if text.contains("usage limit reset") => prompts += 1,
                _ => {}
            }
        }
    })
    .await;
    assert!(
        errors <= 1,
        "an active thread must not emit an Error every tick, got {errors}"
    );
    assert_eq!(prompts, 0, "no duplicate resume prompt while active");
    daemon.stop().await;
}

#[tokio::test]
async fn a_message_appended_to_an_active_turn_replaces_the_stale_schedule() {
    let reset_at = chrono::Utc::now() + chrono::Duration::hours(1);
    let daemon = boot_with(
        resume_options(),
        FakeHarness::with_session_scripts(vec![limit_while_active_script(reset_at)]),
    )
    .await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    stable_policy(&mut client, false).await;
    let summary = create_with_model(
        &mut client,
        host,
        "do the thing",
        HarnessKind::ClaudeCode,
        "claude-sonnet-5",
    )
    .await;
    client
        .wait_event(|event| matches!(event, Event::UsageLimitHit { .. }))
        .await;
    assert_eq!(
        schedules(&mut client).await[0].state,
        ResumeState::Scheduled
    );

    client
        .request(ClientRequest::SendMessage {
            thread_id: summary.thread_id.clone(),
            text: "new instructions".into(),
            attachments: Vec::new(),
        })
        .await
        .expect("send message");

    assert_eq!(
        schedules(&mut client).await[0].state,
        ResumeState::Cancelled,
        "a new message supersedes the stale resume"
    );
    daemon.stop().await;
}
