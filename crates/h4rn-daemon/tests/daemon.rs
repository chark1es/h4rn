//! End-to-end tests for the local daemon.
//!
//! Each test boots a real axum server on an ephemeral port with `FakeHarness`,
//! connects a real `tokio-tungstenite` client and drives the wire protocol.

use std::collections::VecDeque;
use std::path::Path;
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
    DeviceInfo, Event, ForgeCredentialSource, ForgeKind, HarnessKind, HostId, Item,
    LinearProjectTeam, MessageRole, ProtoError, ProtoErrorCode, ProxyProfileDraft, ResponseBody,
    ResumeState, ServerFrame, TextGenerationSettings, ThreadFilter, ThreadId, ThreadTab,
    TitleModelMode, TitleModelSetting, ToolCallStatus, Topic, TurnStatus, UsageGroupBy, UsageRange,
    UsageScope,
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
    boot_inner_with_text_generation(options, harness, isolated, None).await
}

async fn boot_inner_with_text_generation(
    options: ThreadManagerOptions,
    harness: FakeHarness,
    isolated: bool,
    text_generation: Option<Arc<h4rn_daemon::text_generation::TextGenerationService>>,
) -> TestDaemon {
    let dir = tempfile::tempdir().expect("tempdir");
    boot_in_dir(dir, options, harness, isolated, text_generation, None).await
}

/// Boots an isolated daemon whose Linear client talks to `endpoint`.
async fn boot_with_linear_endpoint(endpoint: impl Into<String>) -> TestDaemon {
    let dir = tempfile::tempdir().expect("tempdir");
    boot_in_dir(
        dir,
        ThreadManagerOptions::default(),
        FakeHarness::new(),
        true,
        None,
        Some(endpoint.into()),
    )
    .await
}

async fn boot_in_dir(
    dir: tempfile::TempDir,
    options: ThreadManagerOptions,
    harness: FakeHarness,
    isolated: bool,
    text_generation: Option<Arc<h4rn_daemon::text_generation::TextGenerationService>>,
    linear_endpoint: Option<String>,
) -> TestDaemon {
    boot_at(
        dir.path().to_path_buf(),
        Some(dir),
        options,
        harness,
        isolated,
        text_generation,
        linear_endpoint,
    )
    .await
}

/// Boots a daemon over a data directory the caller keeps alive, so a test can
/// stop it, change what is stored and boot again.
async fn boot_at(
    data_dir: std::path::PathBuf,
    owned: Option<tempfile::TempDir>,
    options: ThreadManagerOptions,
    harness: FakeHarness,
    isolated: bool,
    text_generation: Option<Arc<h4rn_daemon::text_generation::TextGenerationService>>,
    linear_endpoint: Option<String>,
) -> TestDaemon {
    let token_dir = data_dir.clone();
    let config = Config {
        data_dir,
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Auto,
        network_access: None,
        network_port: 0,
    };

    let mut builder = DaemonBuilder::new(config)
        // Titles never spawn the machine's own Claude Code.
        .without_title_agents()
        .with_harness_registry(Arc::new(OfflineRegistry))
        // No test may run a package manager against the machine's real CLIs.
        .with_update_runner(Arc::new(RefusingRunner))
        .with_lifecycle(options);
    if let Some(service) = text_generation {
        builder = builder.with_text_generation(service);
    }
    if let Some(endpoint) = linear_endpoint {
        builder = builder.with_linear_endpoint(endpoint);
    }
    builder = if isolated {
        builder.with_catalog(Arc::new(
            h4rn_daemon::harnesses::StaticHarnessCatalog::with(Arc::new(harness)),
        ))
    } else {
        builder.with_harness(Arc::new(harness))
    };
    let daemon = builder.build().await.expect("build daemon");
    let token = TokenStore::new(&token_dir)
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
        _dir: owned,
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

    /// This integration clock only drives wall-time lifecycle windows; no test
    /// here waits on a retry deadline, so a real monotonic reading is enough.
    fn monotonic(&self) -> std::time::Instant {
        std::time::Instant::now()
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
                | ServerFrame::ScheduleEvent { .. }
                | ServerFrame::ImportEvent { .. } => {}
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
    create_in_workspace(client, host, "/tmp/h4rn-workspace", prompt, harness, model).await
}

/// Creates a thread whose workspace is a real folder on this host.
async fn create_in_workspace(
    client: &mut Client,
    host: HostId,
    workspace: &str,
    prompt: &str,
    harness: HarnessKind,
    model: &str,
) -> h4rn_protocol::ThreadSummary {
    let result = client
        .request(ClientRequest::CreateThread {
            host,
            harness,
            agent_id: None,
            workspace_path: workspace.into(),
            model: model.into(),
            prompt: prompt.into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            fast: false,
            worktree: None,
        })
        .await
        .expect("create thread");
    match result {
        ResponseBody::ThreadCreated { summary, .. } => summary,
        other => panic!("unexpected CreateThread response: {other:?}"),
    }
}

/// Runs one git command and returns its trimmed stdout.
fn git_in(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A repository with one commit and two tracked files.
fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("repo");
    git_in(dir.path(), &["init", "-b", "main"]);
    git_in(dir.path(), &["config", "user.name", "h4rn test"]);
    git_in(
        dir.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git_in(dir.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("a.txt"), "one\n").expect("write");
    std::fs::write(dir.path().join("b.txt"), "two\n").expect("write");
    git_in(dir.path(), &["add", "a.txt", "b.txt"]);
    git_in(dir.path(), &["commit", "-m", "Initial commit"]);
    dir
}

#[tokio::test]
async fn healthz_reports_no_active_turns_when_idle() {
    let daemon = boot().await;
    let health: serde_json::Value = reqwest::get(format!("http://{}/healthz", daemon.addr))
        .await
        .expect("fetch healthz")
        .json()
        .await
        .expect("parse healthz");
    assert_eq!(health["status"], "ok");
    assert_eq!(health["version"], h4rn_daemon::version());
    assert_eq!(health["active_turns"], 0);
    daemon.stop().await;
}

#[tokio::test]
async fn an_active_turn_blocks_the_update_guard() {
    // An empty script leaves the first turn running, so `/healthz` reports it
    // for as long as this test takes.
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
    create_with_host(
        &mut client,
        host,
        "update me after this turn",
        HarnessKind::ClaudeCode,
    )
    .await;

    let health: serde_json::Value = reqwest::get(format!("http://{}/healthz", daemon.addr))
        .await
        .expect("fetch healthz")
        .json()
        .await
        .expect("parse healthz");
    assert!(
        health["active_turns"].as_u64().expect("active_turns") >= 1,
        "the turn must still be running: {health}"
    );

    let refused = h4rn_daemon::update::check_daemon_idle(daemon.addr, false, false)
        .await
        .expect_err("an active turn must block an update");
    assert!(refused.to_string().contains("active turn"), "{refused}");
    assert!(refused.to_string().contains("--force"), "{refused}");

    // `--force` overrides the busy guard; it only prints a warning.
    h4rn_daemon::update::check_daemon_idle(daemon.addr, false, true)
        .await
        .expect("--force overrides the busy guard");

    daemon.stop().await;
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
    // The daemon finished the turn; nobody placed it.
    assert!(!done.placed_by_user);

    // Another client's manual move to Done arrives as the same plain summary,
    // but names the placement so a notifier does not call it "Finished".
    let mut other = Client::connect(&daemon).await;
    other.hello().await;
    other
        .request(ClientRequest::SetThreadTab {
            thread_id: thread_id.clone(),
            tab: ThreadTab::Working,
            pinned: false,
        })
        .await
        .expect("reopen");
    let (_, reopened) = other.wait_for_tab(ThreadTab::Working).await;
    assert!(reopened.placed_by_user);
    other
        .request(ClientRequest::SetThreadTab {
            thread_id: thread_id.clone(),
            tab: ThreadTab::Done,
            pinned: false,
        })
        .await
        .expect("done by hand");
    let placed = client.wait_for_tab(ThreadTab::Done).await;
    assert!(placed.1.placed_by_user);

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
    assert!(settled.placed_by_user);

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
async fn a_large_plain_replay_arrives_in_one_response() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let summary = create_with_host(&mut client, host, "first", HarnessKind::ClaudeCode).await;
    let thread_id = summary.thread_id.clone();
    client.wait_for_turn_completed().await;
    client.wait_for_tab(ThreadTab::Done).await;

    let large = "L".repeat(400 * 1024);
    client
        .request(ClientRequest::SendMessage {
            thread_id: thread_id.clone(),
            text: large.clone(),
            attachments: Vec::new(),
        })
        .await
        .expect("send");
    client.wait_for_turn_completed().await;
    client.wait_for_tab(ThreadTab::Done).await;
    drop(client);

    let mut resumed = Client::connect(&daemon).await;
    resumed.hello().await;
    let response = resumed
        .request(ClientRequest::GetThread {
            thread_id: thread_id.clone(),
            since_seq: 0,
        })
        .await
        .expect("get thread");
    let ResponseBody::Thread {
        events,
        next_seq,
        more,
        ..
    } = response
    else {
        panic!("unexpected get response: {response:?}");
    };
    assert!(!more, "a plain loopback replay is never paged");
    assert_eq!(
        next_seq,
        events.last().map(|event| event.seq).unwrap_or(0),
        "the cursor covers the last event"
    );
    let seqs: Vec<u64> = events.iter().map(|event| event.seq).collect();
    assert_eq!(
        seqs,
        (1..=next_seq).collect::<Vec<_>>(),
        "one response carries every event of the only thread"
    );
    assert!(
        events.iter().any(|event| matches!(
            &event.event,
            Event::ItemCompleted {
                item: Item::Message { text, .. },
                ..
            } if *text == large
        )),
        "the 400 KiB event arrives whole"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn subscribe_answers_with_the_journal_head_it_served() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let first = client
        .request(ClientRequest::Subscribe {
            topics: vec![Topic::All],
            since_seq: None,
        })
        .await
        .expect("subscribe");
    let ResponseBody::Subscribed { replay_through } = first else {
        panic!("subscribe must name its replay boundary: {first:?}");
    };

    // A first connect starts at the live edge: everything the new thread emits
    // after the subscribe answer is above the boundary the daemon named.
    create_with_host(&mut client, host, "boundary", HarnessKind::ClaudeCode).await;
    let last_seq = loop {
        let (seq, event) = client.next_event().await;
        assert!(
            seq > replay_through,
            "live sequence {seq} must be above the served head {replay_through}"
        );
        if matches!(event, Event::TurnCompleted { .. }) {
            break seq;
        }
    };
    drop(client);

    let mut resumed = Client::connect(&daemon).await;
    resumed.hello().await;
    let replay = resumed
        .request(ClientRequest::Subscribe {
            topics: vec![Topic::All],
            since_seq: Some(last_seq),
        })
        .await
        .expect("resubscribe");
    let ResponseBody::Subscribed { replay_through } = replay else {
        panic!("resubscribe must name its replay boundary: {replay:?}");
    };
    assert!(
        replay_through >= last_seq,
        "the boundary {replay_through} must cover the cursor {last_seq}"
    );

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

/// The wire request copies a finished turn and answers with the new thread,
/// leaving the source row alone.
#[tokio::test]
async fn fork_thread_returns_the_new_thread_with_its_lineage() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let summary = create_with_host(&mut client, host, "first", HarnessKind::ClaudeCode).await;
    let thread_id = summary.thread_id.clone();
    client.wait_for_turn_completed().await;
    client.wait_for_tab(ThreadTab::Done).await;

    let ResponseBody::Thread { events, .. } = client
        .request(ClientRequest::GetThread {
            thread_id: thread_id.clone(),
            since_seq: 0,
        })
        .await
        .expect("read the source")
    else {
        panic!("expected a thread replay");
    };
    let turn_id = events
        .iter()
        .find_map(|seq_event| match &seq_event.event {
            Event::ItemCompleted {
                turn_id,
                item: h4rn_protocol::Item::Message { role, .. },
                ..
            } if *role == h4rn_protocol::MessageRole::User => Some(turn_id.clone()),
            _ => None,
        })
        .expect("the first turn");

    let fork = client
        .request(ClientRequest::ForkThread {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
        })
        .await
        .expect("fork");
    let ResponseBody::ThreadForked {
        thread_id: fork_id,
        summary: fork_summary,
    } = fork
    else {
        panic!("expected a fork response: {fork:?}");
    };
    assert_eq!(fork_summary.title, format!("{} (fork)", summary.title));
    assert_eq!(
        fork_summary.forked_from,
        Some(h4rn_protocol::ForkRef {
            thread_id: thread_id.clone(),
            turn_id,
        })
    );
    assert_eq!(fork_summary.workspace_path, summary.workspace_path);

    let source = client
        .request(ClientRequest::GetThread {
            thread_id: thread_id.clone(),
            since_seq: 0,
        })
        .await
        .expect("read the source again");
    let ResponseBody::Thread {
        summary: source_summary,
        ..
    } = source
    else {
        panic!("expected the source thread");
    };
    assert_eq!(source_summary.title, summary.title);
    assert!(source_summary.forked_from.is_none());
    assert_ne!(fork_id, thread_id);

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
        // Titles never spawn the machine's own Claude Code.
        .without_title_agents()
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
        // Titles never spawn the machine's own Claude Code.
        .without_title_agents()
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
async fn create_and_send_report_durable_ids_and_a_non_git_workspace_is_refused_for_a_worktree() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let plain = tempfile::tempdir().expect("plain folder");
    let error = client
        .request(ClientRequest::CreateThread {
            host: host.clone(),
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            workspace_path: plain.path().to_string_lossy().into_owned(),
            model: "fake-model".into(),
            prompt: "needs a worktree".into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::FullAccess,
            reasoning_effort: None,
            fast: false,
            worktree: Some(h4rn_protocol::WorktreeRequest::Generated(true)),
        })
        .await
        .expect_err("a folder that is not a repository has no worktree");
    assert_eq!(error.code, ProtoErrorCode::InvalidRequest);
    assert!(error.message.contains("not a git repository"), "{error:?}");

    let created = client
        .request(ClientRequest::CreateThread {
            host,
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            workspace_path: "/tmp/h4rn-workspace".into(),
            model: "fake-model".into(),
            prompt: "report ids".into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::FullAccess,
            reasoning_effort: None,
            fast: false,
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
async fn a_thread_runs_in_its_new_worktree_and_the_tracker_shows_the_thread() {
    // The test keeps the data directory so it can stop the daemon, remove the
    // thread the way a deletion would, and boot again over the same store.
    let data_dir = tempfile::tempdir().expect("data");
    let daemon = boot_at(
        data_dir.path().to_path_buf(),
        None,
        ThreadManagerOptions::default(),
        FakeHarness::new(),
        true,
        None,
        None,
    )
    .await;
    let host = host_id_for(&daemon).await;
    let repo = git_repo();
    let location = tempfile::tempdir().expect("worktree location");
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    // The location and prefix are this machine's, read by the daemon that
    // would create the checkout.
    match client
        .request(ClientRequest::SetWorktreeSettings {
            settings: h4rn_protocol::WorktreeSettings {
                location: Some(location.path().to_string_lossy().into_owned()),
                branch_prefix: "feat".into(),
            },
        })
        .await
        .expect("save worktree settings")
    {
        ResponseBody::WorktreeSettings { settings } => {
            assert_eq!(settings.branch_prefix, "feat");
            assert_eq!(
                settings.location.as_deref(),
                Some(location.path().to_string_lossy().as_ref())
            );
        }
        other => panic!("unexpected settings response: {other:?}"),
    }
    match client
        .request(ClientRequest::GetWorktreeSettings)
        .await
        .expect("read worktree settings")
    {
        ResponseBody::WorktreeSettings { settings } => {
            assert_eq!(settings.branch_prefix, "feat");
        }
        other => panic!("unexpected settings response: {other:?}"),
    }

    let created = match client
        .request(ClientRequest::CreateThread {
            host,
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            workspace_path: repo.path().to_string_lossy().into_owned(),
            model: "fake-model".into(),
            prompt: "Add the marker file [[write:marker.txt]]hello\n".into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::FullAccess,
            reasoning_effort: None,
            fast: false,
            worktree: Some(h4rn_protocol::WorktreeRequest::Generated(true)),
        })
        .await
        .expect("create a thread in a worktree")
    {
        ResponseBody::ThreadCreated {
            thread_id, summary, ..
        } => (thread_id, summary),
        other => panic!("unexpected create response: {other:?}"),
    };
    let (thread_id, summary) = created;
    let workspace = Path::new(&summary.workspace_path).to_path_buf();
    assert!(
        workspace.starts_with(location.path()),
        "{} is not under {}",
        workspace.display(),
        location.path().display()
    );
    let branch = summary.branch.clone().expect("the worktree's branch");
    assert!(branch.starts_with("feat/"), "{branch}");
    assert_eq!(
        git_in(&workspace, &["rev-parse", "--abbrev-ref", "HEAD"]),
        branch
    );
    assert!(
        !repo.path().join("marker.txt").exists(),
        "the main checkout is never written to"
    );

    client.wait_for_turn_completed().await;
    // The fake harness writes into the workspace it was started with, so this
    // proves the first session ran in the worktree.
    let marker = workspace.join("marker.txt");
    assert!(marker.is_file(), "the session wrote {}", marker.display());
    assert!(!repo.path().join("marker.txt").exists());

    // The thread list names the checkout, and Settings lists it as the
    // thread's own worktree.
    let tracked = match client
        .request(ClientRequest::ListWorktrees)
        .await
        .expect("list worktrees")
    {
        ResponseBody::Worktrees { worktrees } => worktrees,
        other => panic!("unexpected worktrees response: {other:?}"),
    };
    let entry = tracked
        .iter()
        .find(|worktree| worktree.path == summary.workspace_path)
        .expect("the thread's worktree is tracked");
    assert_eq!(entry.branch.as_deref(), Some(branch.as_str()));
    assert_eq!(entry.origin, h4rn_protocol::WorktreeOrigin::Thread);
    assert!(
        entry
            .accessed_by
            .iter()
            .any(|access| access.thread_id == thread_id),
        "the thread owns its worktree: {entry:?}"
    );

    // An untracked marker is uncommitted work: a plain clear keeps it.
    let outcome = match client
        .request(ClientRequest::ClearWorktrees {
            paths: Some(vec![summary.workspace_path.clone()]),
            force: false,
            bulk: false,
        })
        .await
        .expect("clear the worktree")
    {
        ResponseBody::WorktreesCleared {
            removed, skipped, ..
        } => (removed, skipped),
        other => panic!("unexpected clear response: {other:?}"),
    };
    assert!(outcome.0.is_empty());
    assert!(
        outcome
            .1
            .iter()
            .any(|skip| skip.path == summary.workspace_path
                && skip.reason.to_lowercase().contains("uncommitted")),
        "a dirty worktree is refused: {outcome:?}"
    );
    assert!(marker.is_file());

    std::fs::remove_file(&marker).expect("remove the marker");

    // Delete the thread, then boot again over the same worktree store. The row
    // must still be there — the create-time registration is what keeps it —
    // and it must still be clearable.
    daemon.stop().await;
    delete_thread_row(data_dir.path(), &thread_id).await;
    let daemon = boot_at(
        data_dir.path().to_path_buf(),
        None,
        ThreadManagerOptions::default(),
        FakeHarness::new(),
        true,
        None,
        None,
    )
    .await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    match client
        .request(ClientRequest::ListThreads {
            filter: ThreadFilter::default(),
        })
        .await
        .expect("list threads after the deletion")
    {
        ResponseBody::Threads { threads } => assert!(
            threads.iter().all(|thread| thread.thread_id != thread_id),
            "the thread was deleted: {threads:?}"
        ),
        other => panic!("unexpected list response: {other:?}"),
    }
    let tracked = match client
        .request(ClientRequest::ListWorktrees)
        .await
        .expect("list worktrees after the deletion")
    {
        ResponseBody::Worktrees { worktrees } => worktrees,
        other => panic!("unexpected worktrees response: {other:?}"),
    };
    let entry = tracked
        .iter()
        .find(|worktree| worktree.path == summary.workspace_path)
        .unwrap_or_else(|| panic!("the deleted thread's worktree is still listed: {tracked:?}"));
    assert_eq!(entry.origin, h4rn_protocol::WorktreeOrigin::Thread);
    assert_eq!(entry.branch.as_deref(), Some(branch.as_str()));
    assert!(entry.exists);

    let outcome = match client
        .request(ClientRequest::ClearWorktrees {
            paths: Some(vec![summary.workspace_path.clone()]),
            force: false,
            bulk: false,
        })
        .await
        .expect("clear the checkout left by the deleted thread")
    {
        ResponseBody::WorktreesCleared {
            removed, skipped, ..
        } => (removed, skipped),
        other => panic!("unexpected clear response: {other:?}"),
    };
    assert_eq!(outcome.0, vec![summary.workspace_path.clone()]);
    assert!(outcome.1.is_empty(), "{:?}", outcome.1);
    assert!(!workspace.exists());
    match client
        .request(ClientRequest::ListWorktrees)
        .await
        .expect("list after clearing")
    {
        ResponseBody::Worktrees { worktrees } => assert!(
            worktrees
                .iter()
                .all(|worktree| worktree.path != summary.workspace_path),
            "the cleared row is forgotten: {worktrees:?}"
        ),
        other => panic!("unexpected worktrees response: {other:?}"),
    }

    daemon.stop().await;
}

/// Removes a thread's durable row, which is what a deletion removes.
///
/// The daemon has no delete-thread request yet, so the integration test edits
/// the journal directly; every other way of making a thread disappear would
/// also leave its row, and with it the live-derived worktree row, in place.
async fn delete_thread_row(data_dir: &Path, thread_id: &ThreadId) {
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(data_dir.join("events.db"));
    let pool = sqlx::SqlitePool::connect_with(options)
        .await
        .expect("open the daemon's journal");
    let deleted = sqlx::query("DELETE FROM daemon_threads WHERE thread_id = ?")
        .bind(thread_id.as_str())
        .execute(&pool)
        .await
        .expect("delete the thread row")
        .rows_affected();
    assert_eq!(deleted, 1, "the thread row was there to delete");
    pool.close().await;
}

#[tokio::test]
async fn a_named_branch_checked_out_elsewhere_is_refused() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let repo = git_repo();
    let elsewhere = tempfile::tempdir().expect("elsewhere");
    let other = elsewhere.path().join("busy");
    git_in(repo.path(), &["branch", "busy"]);
    git_in(
        repo.path(),
        &["worktree", "add", other.to_str().unwrap(), "busy"],
    );
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let error = client
        .request(ClientRequest::CreateThread {
            host,
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            workspace_path: repo.path().to_string_lossy().into_owned(),
            model: "fake-model".into(),
            prompt: "use a busy branch".into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::FullAccess,
            reasoning_effort: None,
            fast: false,
            worktree: Some(h4rn_protocol::WorktreeRequest::Named("busy".into())),
        })
        .await
        .expect_err("a branch already checked out cannot be reused");
    assert_eq!(error.code, ProtoErrorCode::InvalidRequest);
    assert!(error.message.contains("already checked out"), "{error:?}");

    // Nothing was created for the refused request.
    match client
        .request(ClientRequest::ListThreads {
            filter: ThreadFilter::default(),
        })
        .await
        .expect("list threads")
    {
        ResponseBody::Threads { threads } => assert!(threads.is_empty()),
        other => panic!("unexpected list response: {other:?}"),
    }

    daemon.stop().await;
}

#[tokio::test]
async fn a_worktree_for_another_host_is_refused_before_anything_is_created() {
    let daemon = boot().await;
    let repo = git_repo();
    let location = tempfile::tempdir().expect("location");
    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    client
        .request(ClientRequest::SetWorktreeSettings {
            settings: h4rn_protocol::WorktreeSettings {
                location: Some(location.path().to_string_lossy().into_owned()),
                ..h4rn_protocol::WorktreeSettings::default()
            },
        })
        .await
        .expect("save worktree settings");

    // This daemon only creates threads for its own host; the other host's
    // daemon is the one that would resolve the workspace and make the
    // checkout there.
    let error = client
        .request(ClientRequest::CreateThread {
            host: HostId::new("host_elsewhere"),
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            workspace_path: repo.path().to_string_lossy().into_owned(),
            model: "fake-model".into(),
            prompt: "elsewhere".into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::FullAccess,
            reasoning_effort: None,
            fast: false,
            worktree: Some(h4rn_protocol::WorktreeRequest::Generated(true)),
        })
        .await
        .expect_err("another host's thread is not this daemon's");
    assert_eq!(error.code, ProtoErrorCode::Unsupported);
    assert_eq!(
        std::fs::read_dir(location.path()).expect("list").count(),
        0,
        "nothing was created on this machine"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn worktree_settings_refuse_a_relative_location() {
    let daemon = boot().await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let error = client
        .request(ClientRequest::SetWorktreeSettings {
            settings: h4rn_protocol::WorktreeSettings {
                location: Some("relative/path".into()),
                ..h4rn_protocol::WorktreeSettings::default()
            },
        })
        .await
        .expect_err("a relative location is not a path on the machine");
    assert_eq!(error.code, ProtoErrorCode::InvalidRequest);
    assert!(error.message.contains("absolute path"), "{error:?}");
    assert!(
        error
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("absolute path")),
        "the hint fits a relative path: {error:?}"
    );

    // A branch prefix that cannot start a name gets a hint about the prefix,
    // not about paths.
    let error = client
        .request(ClientRequest::SetWorktreeSettings {
            settings: h4rn_protocol::WorktreeSettings {
                branch_prefix: ".hidden".into(),
                ..h4rn_protocol::WorktreeSettings::default()
            },
        })
        .await
        .expect_err("a prefix that cannot start a branch name is refused");
    assert_eq!(error.code, ProtoErrorCode::InvalidRequest);
    assert!(error.message.contains("branch prefix"), "{error:?}");
    assert!(
        error
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("prefix") || hint.contains("branch")),
        "the hint fits the prefix: {error:?}"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn worktree_settings_refuse_a_location_inside_a_registered_project() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let repo = git_repo();
    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    client
        .request(ClientRequest::AddProject {
            host,
            source: h4rn_protocol::ProjectSource::Existing {
                path: repo.path().to_string_lossy().into_owned(),
            },
            name: None,
        })
        .await
        .expect("register the project");

    let inside = repo.path().join("inside");
    std::fs::create_dir(&inside).expect("inside");
    let error = client
        .request(ClientRequest::SetWorktreeSettings {
            settings: h4rn_protocol::WorktreeSettings {
                location: Some(inside.to_string_lossy().into_owned()),
                ..h4rn_protocol::WorktreeSettings::default()
            },
        })
        .await
        .expect_err("worktrees under the main checkout would dirty it");
    assert_eq!(error.code, ProtoErrorCode::InvalidRequest);
    assert!(
        error.message.contains("inside the project checkout"),
        "{error:?}"
    );
    assert!(
        error
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("outside the project checkout")),
        "{error:?}"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn worktree_settings_refuse_a_location_that_holds_the_project_itself() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let root = tempfile::tempdir().expect("root");
    let repo = root.path().join("api");
    std::fs::create_dir(&repo).expect("api");
    git_in(&repo, &["init", "-b", "main"]);
    git_in(&repo, &["config", "user.name", "h4rn test"]);
    git_in(&repo, &["config", "user.email", "test@example.invalid"]);
    git_in(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("a.txt"), "one\n").expect("write");
    git_in(&repo, &["add", "a.txt"]);
    git_in(&repo, &["commit", "-m", "Initial commit"]);

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    client
        .request(ClientRequest::AddProject {
            host,
            source: h4rn_protocol::ProjectSource::Existing {
                path: repo.to_string_lossy().into_owned(),
            },
            name: None,
        })
        .await
        .expect("register the project");

    // New checkouts go to `<location>/api/<branch>`, which is the main
    // checkout itself when the location is its parent folder.
    let error = client
        .request(ClientRequest::SetWorktreeSettings {
            settings: h4rn_protocol::WorktreeSettings {
                location: Some(root.path().to_string_lossy().into_owned()),
                ..h4rn_protocol::WorktreeSettings::default()
            },
        })
        .await
        .expect_err("the destination would sit in the main checkout");
    assert_eq!(error.code, ProtoErrorCode::InvalidRequest);
    assert!(
        error.message.contains("inside the project checkout"),
        "{error:?}"
    );
    assert!(
        error
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("outside the project checkout")),
        "{error:?}"
    );

    daemon.stop().await;
}

/// A project registered at a linked worktree is named after the main checkout,
/// so a location holding that name can put new checkouts in the registered
/// checkout itself even though the path is nowhere near it. Creation refuses
/// that; saving the setting must refuse it too.
#[tokio::test]
async fn worktree_settings_refuse_a_location_that_holds_a_linked_worktrees_checkout() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let root = tempfile::tempdir().expect("root");
    let main = root.path().join("api");
    std::fs::create_dir(&main).expect("api");
    git_in(&main, &["init", "-b", "main"]);
    git_in(&main, &["config", "user.name", "h4rn test"]);
    git_in(&main, &["config", "user.email", "test@example.invalid"]);
    git_in(&main, &["config", "commit.gpgsign", "false"]);
    std::fs::write(main.join("a.txt"), "one\n").expect("write");
    git_in(&main, &["add", "a.txt"]);
    git_in(&main, &["commit", "-m", "Initial commit"]);

    // A linked worktree that happens to carry the main checkout's folder name:
    // the project is named after `api`, so a location of `<root>/wt` holds the
    // checkout destination `<root>/wt/api`, which is this workspace itself.
    let container = root.path().join("wt");
    std::fs::create_dir(&container).expect("wt");
    let linked = container.join("api");
    git_in(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            linked.to_str().unwrap(),
        ],
    );

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    client
        .request(ClientRequest::AddProject {
            host,
            source: h4rn_protocol::ProjectSource::Existing {
                path: linked.to_string_lossy().into_owned(),
            },
            name: None,
        })
        .await
        .expect("register the linked worktree as the project");

    let error = client
        .request(ClientRequest::SetWorktreeSettings {
            settings: h4rn_protocol::WorktreeSettings {
                location: Some(container.to_string_lossy().into_owned()),
                ..h4rn_protocol::WorktreeSettings::default()
            },
        })
        .await
        .expect_err("the destination is the registered checkout itself");
    assert_eq!(error.code, ProtoErrorCode::InvalidRequest);
    assert!(
        error.message.contains("inside the project checkout"),
        "{error:?}"
    );
    match client
        .request(ClientRequest::GetWorktreeSettings)
        .await
        .expect("read worktree settings")
    {
        ResponseBody::WorktreeSettings { settings } => {
            assert_eq!(
                settings.location, None,
                "the refused location was not saved"
            );
        }
        other => panic!("unexpected settings response: {other:?}"),
    }

    daemon.stop().await;
}

#[tokio::test]
async fn a_bad_saved_worktree_setting_does_not_stop_the_daemon() {
    let data_dir = tempfile::tempdir().expect("data");
    std::fs::write(
        data_dir.path().join("worktree-settings.json"),
        r#"{"branch_prefix":".hidden"}"#,
    )
    .expect("write the bad setting");
    // The daemon must still start, with the defaults in place.
    let daemon = boot_at(
        data_dir.path().to_path_buf(),
        Some(data_dir),
        ThreadManagerOptions::default(),
        FakeHarness::new(),
        true,
        None,
        None,
    )
    .await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    match client
        .request(ClientRequest::GetWorktreeSettings)
        .await
        .expect("read worktree settings")
    {
        ResponseBody::WorktreeSettings { settings } => {
            assert_eq!(settings.branch_prefix, "h4rn/");
        }
        other => panic!("unexpected settings response: {other:?}"),
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
        // Titles never spawn the machine's own Claude Code.
        .without_title_agents()
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
            agent_id: None,
            workspace_path: workspace.path().display().to_string(),
            model: "fake-model".into(),
            prompt: "edit it [[write:notes.txt]]keep\nnew\nmore".into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            fast: false,
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
            agent_id: None,
            workspace_path: workspace.path().display().to_string(),
            model: "fake-model".into(),
            prompt: "make a note [[write:notes.txt]]hello from the fake harness".into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            fast: false,
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
            agent_id: None,
            workspace_path: workspace.path().display().to_string(),
            model: "fake-model".into(),
            prompt: "name the branch".into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            fast: false,
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

#[tokio::test]
async fn a_fixture_coding_agent_writes_the_generated_thread_title() {
    let support = tempfile::tempdir().expect("support dir");
    // The fixture records its argv and stdin beside itself, so run a copy in
    // the temp directory rather than the one in the repo.
    let fixture = support.path().join("fake-title-agent.sh");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-title-agent.sh"),
        &fixture,
    )
    .expect("copy fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let service = Arc::new(
        h4rn_daemon::text_generation::TextGenerationService::open(support.path())
            .expect("open text generation")
            .with_file_keys()
            .expect("file keys")
            .with_agent_install(HarnessKind::ClaudeCode, &fixture),
    );
    service
        .set_settings(TextGenerationSettings {
            title: TitleModelSetting {
                mode: TitleModelMode::CodingAgent,
                harness: Some(HarnessKind::ClaudeCode),
                generator_id: None,
                model: None,
            },
            source_control: Default::default(),
        })
        .await
        .expect("select the coding agent");
    let daemon = boot_inner_with_text_generation(
        ThreadManagerOptions::default(),
        FakeHarness::new(),
        true,
        Some(service),
    )
    .await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    client
        .request(ClientRequest::Subscribe {
            topics: vec![Topic::All],
            since_seq: None,
        })
        .await
        .expect("subscribe");
    let summary = create_with_host(
        &mut client,
        host,
        "Fix the flaky auth test",
        HarnessKind::ClaudeCode,
    )
    .await;
    assert_eq!(summary.title, "Fix the flaky auth test");
    let thread_id = summary.thread_id.clone();

    client
        .wait_event(|event| {
            matches!(
                event,
                Event::ThreadUpserted(summary)
                    if summary.thread_id == thread_id && summary.title == "Fixture title"
            )
        })
        .await;

    // Read the row back through the protocol as the authoritative check.
    let renamed = thread_summary(&mut client, &summary.thread_id).await;
    assert_eq!(renamed.title, "Fixture title");
    daemon.stop().await;
}

/// A test daemon may never start the host's own `claude` or `codex`.
///
/// A fake `claude` is put first on `PATH`; an ordinary thread must create,
/// keep its first-line title and never run it.
#[cfg(unix)]
#[tokio::test]
async fn an_ordinary_thread_never_starts_the_hosts_coding_agent() {
    use std::os::unix::fs::PermissionsExt;

    let fake_bin = tempfile::tempdir().expect("fake bin");
    let marker = fake_bin.path().join("invoked");
    let script = fake_bin.path().join("claude");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ntouch '{}'\nprintf 'a title from the host\\n'\n",
            marker.display()
        ),
    )
    .expect("write the fake claude");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    // PATH is process-global, so this test runs alone while it is changed and
    // a guard puts the old value back even if an assertion panics. An async
    // mutex because the test awaits the daemon while it holds the lock.
    static PATH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _path_lock = PATH_LOCK.lock().await;
    struct RestorePath(Option<std::ffi::OsString>);
    impl Drop for RestorePath {
        fn drop(&mut self) {
            if let Some(path) = self.0.take() {
                // SAFETY: the lock above serializes PATH changes in this test
                // binary, and the value is the one this test replaced.
                unsafe { std::env::set_var("PATH", path) };
            }
        }
    }
    let original = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fake_bin.path().to_path_buf()];
    paths.extend(std::env::split_paths(&original));
    let joined = std::env::join_paths(paths).expect("join PATH");
    let _restore = RestorePath(Some(original));
    // SAFETY: see the guard above.
    unsafe { std::env::set_var("PATH", &joined) };

    let daemon = boot_isolated().await;
    let host = host_id_for(&daemon).await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let summary = create_with_host(
        &mut client,
        host,
        "Keep this title",
        HarnessKind::ClaudeCode,
    )
    .await;
    // A title job that wrongly fell back to the host's claude would start it
    // immediately; this is ample time for the marker to appear.
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert!(!marker.exists(), "a test daemon started the host's claude");
    let read_back = thread_summary(&mut client, &summary.thread_id).await;
    assert_eq!(read_back.title, "Keep this title");
    daemon.stop().await;
}

/// A Codex thread in Coding-agent mode keeps its first-line title and never
/// starts Codex: h4rn does not use Codex as a title backend.
#[cfg(unix)]
#[tokio::test]
async fn a_codex_thread_keeps_its_first_line_and_never_starts_codex() {
    use std::os::unix::fs::PermissionsExt;

    let support = tempfile::tempdir().expect("support dir");
    let marker = support.path().join("codex-started");
    let script = support.path().join("codex");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ntouch '{}'\nprintf 'Codex title\\n'\n",
            marker.display()
        ),
    )
    .expect("write the fake codex");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let service = Arc::new(
        h4rn_daemon::text_generation::TextGenerationService::open(support.path())
            .expect("open text generation")
            .with_file_keys()
            .expect("file keys")
            // Even an explicit install cannot make Codex a title backend.
            .with_agent_install(HarnessKind::Codex, &script),
    );
    service
        .set_settings(TextGenerationSettings {
            title: TitleModelSetting::default(),
            source_control: Default::default(),
        })
        .await
        .expect("coding agent mode");
    let daemon = boot_inner_with_text_generation(
        ThreadManagerOptions::default(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
        true,
        Some(service),
    )
    .await;
    let host = host_id_for(&daemon).await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let summary = create_with_host(&mut client, host, "Keep this title", HarnessKind::Codex).await;
    assert_eq!(summary.title, "Keep this title");
    // A wrongly used Codex backend would start immediately; this is ample time.
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert!(
        !marker.exists(),
        "a Codex thread started Codex for its title"
    );
    let read_back = thread_summary(&mut client, &summary.thread_id).await;
    assert_eq!(read_back.title, "Keep this title");
    daemon.stop().await;
}

#[tokio::test]
async fn a_provider_test_does_not_block_the_connection() {
    use h4rn_protocol::{TextGeneratorDraft, TextGeneratorKind};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = wiremock::MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(1))
                .set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": "ok" } }]
                })),
        )
        .mount(&server)
        .await;

    let support = tempfile::tempdir().expect("support dir");
    let service = Arc::new(
        h4rn_daemon::text_generation::TextGenerationService::open(support.path())
            .expect("open text generation")
            .with_file_keys()
            .expect("file keys"),
    );
    let generator = service
        .add_generator(TextGeneratorDraft {
            name: "Slow local".into(),
            kind: TextGeneratorKind::OpenAiCompatible,
            base_url: format!("{}/v1", server.uri()),
            model: "tiny-model".into(),
            api_key: None,
        })
        .await
        .expect("add generator");
    let daemon = boot_inner_with_text_generation(
        ThreadManagerOptions::default(),
        FakeHarness::new(),
        true,
        Some(Arc::clone(&service)),
    )
    .await;

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let test_id = client
        .send(ClientRequest::TestTextGenerator {
            generator_id: generator.id.clone(),
            model: None,
        })
        .await;
    let list_id = client.send(ClientRequest::ListHosts).await;

    let mut order = Vec::new();
    let mut report_ok = None;
    while order.len() < 2 {
        if let ServerFrame::Response { id, result } = client.read_frame().await {
            match id {
                id if id == test_id => {
                    let body = result.expect("test result");
                    match body {
                        ResponseBody::TextGeneratorTested { report } => {
                            report_ok = Some(report.ok);
                        }
                        other => panic!("unexpected test response: {other:?}"),
                    }
                    order.push("test");
                }
                id if id == list_id => {
                    result.expect("hosts result");
                    order.push("hosts");
                }
                other => panic!("unexpected response id {other}"),
            }
        }
    }
    assert_eq!(
        order,
        vec!["hosts", "test"],
        "a pending Test must not hold the read loop"
    );
    assert_eq!(report_ok, Some(true));
    daemon.stop().await;
}

#[tokio::test]
async fn an_issue_write_checks_ownership_against_the_checkouts_not_a_cache() {
    use h4rn_protocol::issues::{ForgeIssueFilter, ForgeIssueUpdate};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "login": "alice",
            "name": "Alice"
        })))
        .mount(&server)
        .await;
    // A read populates the ownership cache. A write after this must not
    // reuse it.
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    // The fork's parent reads fine but no checkout owns it, so it is
    // read-only.
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/parent/widget/issues"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/parent/widget/issues/4"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "number": 4,
            "title": "Upstream report",
            "state": "open",
            "user": {"login": "carol"},
            "comments": 0,
            "created_at": "2026-04-01T00:00:00Z",
            "updated_at": "2026-04-01T00:00:00Z",
            "html_url": "https://example.test/parent/widget/issues/4"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/parent/widget/issues/4/timeline"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let repo_dir = tempfile::tempdir().expect("repo");
    git_in(repo_dir.path(), &["init", "-b", "main"]);
    git_in(
        repo_dir.path(),
        &[
            "remote",
            "add",
            "upstream",
            &format!("{}/parent/widget.git", server.uri()),
        ],
    );
    git_in(
        repo_dir.path(),
        &[
            "remote",
            "add",
            "origin",
            &format!("{}/acme/widget.git", server.uri()),
        ],
    );

    // The mock server stands in for a forge host the daemon knows, so its
    // URL is what the checkout's remotes name and what ownership compares.
    let data_dir = tempfile::tempdir().expect("data dir");
    std::fs::write(
        data_dir.path().join(h4rn_daemon::forge::HOSTS_FILE),
        serde_json::to_vec(&serde_json::json!({
            "hosts": [{
                "host": "127.0.0.1",
                "forge": "github",
                "base_url": server.uri()
            }]
        }))
        .expect("host config"),
    )
    .expect("write host config");
    let daemon = boot_in_dir(
        data_dir,
        ThreadManagerOptions::default(),
        FakeHarness::new(),
        false,
        None,
        None,
    )
    .await;
    let host = host_id_for(&daemon).await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let account = match client
        .request(ClientRequest::ForgeAddAccount {
            forge: ForgeKind::GitHub,
            base_url: server.uri(),
            token: "fixture-token".into(),
            username: None,
            source: ForgeCredentialSource::Pat,
            repository: None,
        })
        .await
        .expect("add the forge account")
    {
        ResponseBody::ForgeAccount { account } => account,
        other => panic!("unexpected account response: {other:?}"),
    };
    let project = match client
        .request(ClientRequest::AddProject {
            host: host.clone(),
            source: h4rn_protocol::ProjectSource::Existing {
                path: repo_dir.path().to_string_lossy().into_owned(),
            },
            name: None,
        })
        .await
        .expect("register the checkout")
    {
        ResponseBody::Project { project } => project,
        other => panic!("unexpected project response: {other:?}"),
    };

    let repo = |owner: &str| h4rn_protocol::ForgeRepoRef {
        forge: ForgeKind::GitHub,
        base_url: server.uri(),
        owner: owner.into(),
        repo: "widget".into(),
    };
    client
        .request(ClientRequest::ForgeListIssues {
            account_id: Some(account.account_id.clone()),
            repo: repo("acme"),
            filter: ForgeIssueFilter::Open,
            page: 1,
            query: None,
        })
        .await
        .expect("the designated repository lists");

    // A repository no project checks out still reads, and says it is
    // read-only so the page can refuse writes before the request.
    let unowned_page = client
        .request(ClientRequest::ForgeListIssues {
            account_id: Some(account.account_id.clone()),
            repo: repo("parent"),
            filter: ForgeIssueFilter::Open,
            page: 1,
            query: None,
        })
        .await
        .expect("an unowned repository still reads");
    match unowned_page {
        ResponseBody::ForgeIssues { read_only, .. } => {
            assert!(read_only, "the parent repository is not ours to write");
        }
        other => panic!("unexpected list response: {other:?}"),
    }
    let unowned_issue = client
        .request(ClientRequest::ForgeGetIssue {
            account_id: Some(account.account_id.clone()),
            repo: repo("parent"),
            number: 4,
        })
        .await
        .expect("an unowned issue still reads");
    match unowned_issue {
        ResponseBody::ForgeIssue { read_only, .. } => {
            assert!(read_only, "the parent repository is not ours to write");
        }
        other => panic!("unexpected issue response: {other:?}"),
    }

    // A fork checkout designates its `origin`, never its `upstream`.
    let comment = client
        .request(ClientRequest::ForgeCommentIssue {
            account_id: Some(account.account_id.clone()),
            repo: repo("parent"),
            number: 4,
            body: "Should be refused".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(comment.code, ProtoErrorCode::Forbidden);
    let update = client
        .request(ClientRequest::ForgeUpdateIssue {
            account_id: Some(account.account_id.clone()),
            repo: repo("parent"),
            number: 4,
            update: ForgeIssueUpdate {
                title: Some("Should be refused".into()),
                ..ForgeIssueUpdate::default()
            },
        })
        .await
        .unwrap_err();
    assert_eq!(update.code, ProtoErrorCode::Forbidden);

    // The read above cached a positive answer for acme. Moving origin to a
    // fork must refuse the very next write, not 30 seconds later.
    git_in(
        repo_dir.path(),
        &[
            "remote",
            "set-url",
            "origin",
            &format!("{}/moved/widget.git", server.uri()),
        ],
    );
    let moved = client
        .request(ClientRequest::ForgeCommentIssue {
            account_id: Some(account.account_id.clone()),
            repo: repo("acme"),
            number: 4,
            body: "Should be refused".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(moved.code, ProtoErrorCode::Forbidden);

    // Removing the project must also take effect at once.
    client
        .request(ClientRequest::RemoveProject {
            host,
            project_id: project.id,
        })
        .await
        .expect("remove the project");
    let removed = client
        .request(ClientRequest::ForgeUpdateIssue {
            account_id: Some(account.account_id.clone()),
            repo: repo("acme"),
            number: 4,
            update: ForgeIssueUpdate {
                title: Some("Should be refused".into()),
                ..ForgeIssueUpdate::default()
            },
        })
        .await
        .unwrap_err();
    assert_eq!(removed.code, ProtoErrorCode::Forbidden);

    let writes = server
        .received_requests()
        .await
        .expect("recorded requests")
        .into_iter()
        .filter(|request| request.method.as_str() != "GET")
        .collect::<Vec<_>>();
    assert!(writes.is_empty(), "no tracker write was sent: {writes:?}");

    daemon.stop().await;
}

#[tokio::test]
async fn opening_a_linear_issue_reads_it_once() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    // One document serves the viewer connect and the issue detail alike;
    // GraphQL clients read only the keys they asked for.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "viewer": {
                    "name": "Acme",
                    "email": "alice@example.test",
                    "organization": {"name": "Acme"}
                },
                "issue": {
                    "id": "iss_1",
                    "identifier": "ENG-1",
                    "title": "Rate limit the import",
                    "description": "Imports hammer the API.",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-02T00:00:00Z",
                    "url": "https://linear.app/acme/issue/ENG-1",
                    "state": {"name": "In Progress", "type": "started"},
                    "team": {"id": "team_1", "key": "ENG"},
                    "assignee": null,
                    "labels": {"nodes": []},
                    "comments": {
                        "nodes": [],
                        "pageInfo": {"hasNextPage": false, "endCursor": null}
                    },
                    "attachments": {"nodes": []}
                }
            }
        })))
        .mount(&server)
        .await;

    let daemon = boot_with_linear_endpoint(format!("{}/graphql", server.uri())).await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let account = match client
        .request(ClientRequest::LinearConnect {
            api_key: "fixture-linear-key".into(),
        })
        .await
        .expect("connect Linear")
    {
        ResponseBody::LinearAccounts { accounts } => {
            accounts.into_iter().next().expect("one connected account")
        }
        other => panic!("unexpected connect response: {other:?}"),
    };

    // Linear's capabilities travel separately so the forge list keeps its old
    // shape for clients built before the field existed.
    match client
        .request(ClientRequest::ForgeListAccounts)
        .await
        .expect("list accounts")
    {
        ResponseBody::ForgeAccounts {
            capabilities,
            linear,
            ..
        } => {
            assert_eq!(capabilities.len(), 6, "only the six forges are listed");
            let linear = linear.expect("Linear's row is in its own field");
            assert!(linear.issues.read);
            assert!(!linear.issues.write);
        }
        other => panic!("unexpected accounts response: {other:?}"),
    }
    let before = server.received_requests().await.expect("recorded").len();
    let opened = client
        .request(ClientRequest::LinearGetIssue {
            account_id: account.account_id.clone(),
            issue_id: "iss_1".into(),
            comments_after: None,
        })
        .await
        .expect("open the issue");
    match opened {
        ResponseBody::LinearIssue { issue, read_only } => {
            assert_eq!(issue.summary.identifier, "ENG-1");
            // No project owns this team, so the page says read-only.
            assert!(read_only);
        }
        other => panic!("unexpected issue response: {other:?}"),
    }
    let requests = server.received_requests().await.expect("recorded");
    assert_eq!(
        requests.len() - before,
        1,
        "opening an issue must read the tracker once: {:?}",
        requests
            .iter()
            .map(|request| request.url.as_str())
            .collect::<Vec<_>>()
    );

    daemon.stop().await;
}

#[tokio::test]
async fn a_linear_team_another_project_uses_stays_writable_after_one_is_removed() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    // One superset response serves the connect, team list, ownership and
    // comment documents; GraphQL clients read only the keys they asked for.
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {
                "viewer": {
                    "name": "Acme",
                    "email": "alice@example.test",
                    "organization": {"name": "Acme"}
                },
                "teams": {"nodes": [{"id": "team_1", "key": "ENG", "name": "Engineering"}]},
                "issue": {"id": "iss_1", "team": {"id": "team_1"}},
                "commentCreate": {
                    "success": true,
                    "comment": {
                        "id": "c_1",
                        "body": "On it.",
                        "createdAt": "2026-01-03T00:00:00Z",
                        "updatedAt": null,
                        "user": {"displayName": "Alice"}
                    }
                }
            }
        })))
        .mount(&server)
        .await;

    let daemon = boot_with_linear_endpoint(format!("{}/graphql", server.uri())).await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let host = host_id_for(&daemon).await;

    let account = match client
        .request(ClientRequest::LinearConnect {
            api_key: "fixture-linear-key".into(),
        })
        .await
        .expect("connect Linear")
    {
        ResponseBody::LinearAccounts { accounts } => {
            accounts.into_iter().next().expect("one connected account")
        }
        other => panic!("unexpected connect response: {other:?}"),
    };

    let first_dir = tempfile::tempdir().expect("first workspace");
    let second_dir = tempfile::tempdir().expect("second workspace");
    let mut project_ids = Vec::new();
    for dir in [&first_dir, &second_dir] {
        let project = match client
            .request(ClientRequest::AddProject {
                host: host.clone(),
                source: h4rn_protocol::ProjectSource::Existing {
                    path: dir.path().to_string_lossy().into_owned(),
                },
                name: None,
            })
            .await
            .expect("register the workspace")
        {
            ResponseBody::Project { project } => project,
            other => panic!("unexpected project response: {other:?}"),
        };
        project_ids.push(project.id);
    }

    // Both projects use the same team; the first mapping is the one that will
    // be removed, so a check that stops at the first mapping goes stale.
    for project_id in &project_ids {
        client
            .request(ClientRequest::LinearSetProjectTeam {
                project_id: project_id.clone(),
                team: Some(LinearProjectTeam {
                    project_id: project_id.clone(),
                    account_id: account.account_id.clone(),
                    team_id: "team_1".into(),
                    team_key: "ENG".into(),
                    team_name: "Engineering".into(),
                }),
            })
            .await
            .expect("attach the team");
    }
    client
        .request(ClientRequest::RemoveProject {
            host,
            project_id: project_ids[0].clone(),
        })
        .await
        .expect("remove the first project");

    match client
        .request(ClientRequest::LinearCommentIssue {
            account_id: account.account_id.clone(),
            issue_id: "iss_1".into(),
            body: "On it.".into(),
        })
        .await
    {
        Ok(ResponseBody::LinearIssueCommentPosted { comment }) => {
            assert_eq!(comment.body, "On it.");
        }
        other => panic!("the second project still makes the team writable: {other:?}"),
    }

    daemon.stop().await;
}

#[tokio::test]
async fn a_large_editor_read_is_delivered_whole_on_the_plain_path() {
    // The plain loopback WebSocket has no transport frame bound. The session
    // limit is chosen by the request handler from the connection's origin, so
    // a read whose escaped JSON is over one secure frame must still arrive
    // whole here; only a regression that always applies the secure bound would
    // omit it.
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let workspace = tempfile::tempdir().expect("workspace");
    let content = "\"".repeat(400 * 1024);
    std::fs::write(workspace.path().join("big.txt"), &content).expect("write");
    let workspace_path = workspace
        .path()
        .to_str()
        .expect("utf-8 workspace")
        .to_owned();

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    client
        .request(ClientRequest::AddProject {
            host: host.clone(),
            source: h4rn_protocol::ProjectSource::Existing {
                path: workspace_path.clone(),
            },
            name: None,
        })
        .await
        .expect("authorise the workspace");

    let response = client
        .request(ClientRequest::FsRead {
            host,
            workspace: workspace_path,
            path: "big.txt".into(),
            bytes_base64: false,
            max_bytes: None,
        })
        .await
        .expect("read");
    match response {
        ResponseBody::FsFile { file } => {
            assert!(file.content_omitted.is_none());
            assert_eq!(file.content.as_deref(), Some(content.as_str()));
        }
        other => panic!("unexpected read response: {other:?}"),
    }

    daemon.stop().await;
}

/// A probe starts a third-party process and can take seconds. Another request
/// on the same connection must be answered while it is in flight.
#[cfg(unix)]
#[tokio::test]
async fn a_slow_acp_probe_does_not_block_the_connection() {
    let daemon = boot_isolated().await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let dir = tempfile::tempdir().expect("probe dir");
    let marker = dir.path().join("started");
    let gate = dir.path().join("gate");
    let mut env = std::collections::BTreeMap::new();
    env.insert("H4RN_TEST_MARKER".to_owned(), marker.display().to_string());
    env.insert("H4RN_TEST_GATE".to_owned(), gate.display().to_string());
    let added = client
        .request(ClientRequest::AddAcpAgent {
            agent: h4rn_protocol::AcpAgentSpec {
                id: "slow".into(),
                name: "Slow agent".into(),
                command: "sh".into(),
                // Parks inside the command until the test releases the gate, so
                // the probe is provably still pending.
                args: vec![
                    "-c".into(),
                    "touch \"$H4RN_TEST_MARKER\"; while [ ! -f \"$H4RN_TEST_GATE\" ]; do sleep 0.02; done"
                        .into(),
                ],
                env,
                working_dir: h4rn_protocol::AcpWorkingDir::ThreadWorkspace,
            },
        })
        .await;
    assert!(
        matches!(added, Ok(ResponseBody::AcpAgent { .. })),
        "configure the agent: {added:?}"
    );

    let probe_id = client
        .send(ClientRequest::ProbeAcpAgent { id: "slow".into() })
        .await;
    tokio::time::timeout(EVENT_TIMEOUT, async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the probe starts the command");

    // A plain read on the same socket must answer without waiting for the
    // probe's handshake, which is still parked in the command.
    let listed = tokio::time::timeout(
        Duration::from_secs(5),
        client.request(ClientRequest::ListAcpAgents),
    )
    .await
    .expect("another request answers while the probe is pending");
    assert!(
        matches!(listed, Ok(ResponseBody::AcpAgents { .. })),
        "{listed:?}"
    );
    assert!(!gate.exists(), "the probe was still pending");

    std::fs::write(&gate, b"go").expect("release the probe");
    let probe = tokio::time::timeout(EVENT_TIMEOUT, client.response(probe_id))
        .await
        .expect("the probe answers once it is released");
    assert!(
        matches!(probe, Ok(ResponseBody::AcpAgentProbe { .. })),
        "{probe:?}"
    );

    daemon.stop().await;
}

/// A workspace on `main` whose `origin` looks like GitHub and pushes to a
/// local bare repository, so opening a PR never touches the network.
fn forge_workspace() -> (tempfile::TempDir, tempfile::TempDir) {
    let workspace = tempfile::tempdir().expect("workspace");
    let remote = tempfile::tempdir().expect("remote");
    let status = std::process::Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg(remote.path())
        .output()
        .expect("init the bare remote");
    assert!(status.status.success(), "the bare remote must exist");
    git_in(workspace.path(), &["init", "-b", "main"]);
    git_in(
        workspace.path(),
        &["config", "user.email", "test@example.com"],
    );
    git_in(workspace.path(), &["config", "user.name", "Test"]);
    std::fs::write(workspace.path().join("README.md"), "test\n").expect("write a file");
    git_in(workspace.path(), &["add", "README.md"]);
    git_in(workspace.path(), &["commit", "-m", "initial"]);
    git_in(
        workspace.path(),
        &["remote", "add", "origin", "https://github.com/acme/widget"],
    );
    git_in(
        workspace.path(),
        &[
            "config",
            "remote.origin.pushurl",
            &remote.path().display().to_string(),
        ],
    );
    (workspace, remote)
}

/// Boots a daemon whose data directory already holds one GitHub account that
/// points at `forge_base`.
async fn boot_with_forge_account(forge_base: &str) -> (TestDaemon, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let accounts = serde_json::json!([{
        "account_id": "acct_1",
        "forge": "github",
        "base_url": forge_base,
        "username": "alice",
        "display_name": null,
        "scopes": [],
        "source": "pat",
        "created_at": "2026-01-01T00:00:00Z",
    }]);
    std::fs::write(
        dir.path().join("forge-accounts.json"),
        serde_json::to_vec(&accounts).expect("accounts"),
    )
    .expect("write accounts");
    std::fs::write(
        dir.path().join("forge-hosts.json"),
        serde_json::to_vec(&serde_json::json!({
            "hosts": [{"host": "github.com", "forge": "github", "base_url": forge_base}]
        }))
        .expect("hosts"),
    )
    .expect("write hosts");
    h4rn_transport::keys::write_secret_file(&dir.path().join("forge-token-acct_1"), b"test-token")
        .expect("store the token");

    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Auto,
        network_access: None,
        network_port: 0,
    };
    let daemon = DaemonBuilder::new(config)
        .without_title_agents()
        .with_harness_registry(Arc::new(OfflineRegistry))
        .with_update_runner(Arc::new(RefusingRunner))
        .with_lifecycle(ThreadManagerOptions {
            tick_interval: Duration::from_millis(20),
            ..ThreadManagerOptions::default()
        })
        .with_catalog(Arc::new(
            h4rn_daemon::harnesses::StaticHarnessCatalog::with(Arc::new(FakeHarness::new())),
        ))
        .build()
        .await
        .expect("build daemon");
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
    (
        TestDaemon {
            addr,
            token,
            _dir: None,
            shutdown: Some(shutdown),
            task,
        },
        dir,
    )
}

#[tokio::test]
async fn opening_a_pr_from_a_thread_links_it() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let pr = serde_json::json!({
        "number": 7,
        "title": "Add PR watch",
        "state": "open",
        "merged": false,
        "merged_at": null,
        "draft": false,
        "user": {"login": "alice"},
        "head": {"ref": "main", "sha": "sha-1", "repo": {"full_name": "acme/widget"}},
        "base": {"ref": "main", "sha": "base"},
        "updated_at": "2026-01-01T00:00:01Z",
        "html_url": "https://github.com/acme/widget/pull/7",
        "body": "",
        "mergeable": true,
        "additions": 1,
        "deletions": 0,
        "changed_files": 1,
        "commits": 1,
        "created_at": "2026-01-01T00:00:00Z"
    });
    Mock::given(method("POST"))
        .and(path("/api/v3/repos/acme/widget/pulls"))
        .respond_with(ResponseTemplate::new(201).set_body_json(pr.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr))
        .mount(&server)
        .await;

    let (daemon, _data_dir) = boot_with_forge_account(&server.uri()).await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let host = host_id_for(&daemon).await;
    let (workspace, _remote) = forge_workspace();

    let created = client
        .request(ClientRequest::CreateThread {
            host,
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            workspace_path: workspace.path().display().to_string(),
            model: "fake-model".into(),
            prompt: "first".into(),
            attachments: Vec::new(),
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            fast: false,
            worktree: None,
        })
        .await
        .expect("create thread");
    let summary = match created {
        ResponseBody::ThreadCreated { summary, .. } => summary,
        other => panic!("unexpected CreateThread response: {other:?}"),
    };

    let repo = h4rn_protocol::ForgeRepoRef {
        forge: h4rn_protocol::ForgeKind::GitHub,
        base_url: server.uri(),
        owner: "acme".into(),
        repo: "widget".into(),
    };
    let opened = client
        .request(ClientRequest::ForgeOpenPr {
            thread_id: summary.thread_id.clone(),
            repo: Some(repo.clone()),
            title: "Add PR watch".into(),
            body: String::new(),
            base: "main".into(),
            draft: false,
        })
        .await
        .expect("open the PR");
    match opened {
        ResponseBody::ForgePrOpened { pr } => assert_eq!(pr.summary.number, 7),
        other => panic!("unexpected ForgeOpenPr response: {other:?}"),
    }

    let listed = client
        .request(ClientRequest::ListThreadPullRequests {
            thread_id: summary.thread_id.clone(),
        })
        .await
        .expect("list the thread's pull requests");
    match listed {
        ResponseBody::ThreadPullRequests { pulls, .. } => {
            assert_eq!(pulls.len(), 1, "opening a PR must link it: {pulls:?}");
            assert_eq!(pulls[0].number, 7);
            assert_eq!(pulls[0].repo, repo);
            assert!(pulls[0].summary.is_some());
        }
        other => panic!("unexpected ListThreadPullRequests response: {other:?}"),
    }

    daemon.stop().await;
}

/// `git_status` and `git_commit` over a real connection, with a slow hook
/// proving the socket keeps answering other requests while git works.
#[cfg(unix)]
#[tokio::test]
async fn git_actions_answer_on_the_wire_and_never_block_the_socket() {
    use std::os::unix::fs::PermissionsExt;

    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let repo = git_repo();
    std::fs::write(repo.path().join("a.txt"), "changed\n").expect("write");
    git_in(repo.path(), &["add", "a.txt"]);

    // The hook parks the commit long enough for another frame to be answered.
    let hooks = repo.path().join(".git/hooks");
    std::fs::create_dir_all(&hooks).expect("hooks dir");
    let hook = hooks.join("pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nsleep 5\n").expect("hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let summary = create_in_workspace(
        &mut client,
        host,
        repo.path().to_str().unwrap(),
        "Commit the change",
        HarnessKind::ClaudeCode,
        "fake-model",
    )
    .await;
    client.wait_for_turn_completed().await;

    let status = client
        .request(ClientRequest::GitStatus {
            thread_id: summary.thread_id.clone(),
        })
        .await
        .expect("git status");
    match status {
        ResponseBody::GitStatus { status } => {
            assert_eq!(status.branch.as_deref(), Some("main"));
            assert_eq!(status.staged.count, 1);
            assert_eq!(status.staged.paths, vec!["a.txt".to_owned()]);
            assert_eq!(status.operation, None);
        }
        other => panic!("unexpected git status response: {other:?}"),
    }

    let commit_id = client
        .send(ClientRequest::GitCommit {
            thread_id: summary.thread_id.clone(),
            message: "Change a".into(),
            include_unstaged: false,
            paths: None,
        })
        .await;
    let hosts_id = client.send(ClientRequest::ListHosts).await;

    // The first reply must be the later request: the commit is still in its
    // hook, and the connection kept serving frames meanwhile.
    let first_reply = loop {
        if let ServerFrame::Response { id, .. } = client.read_frame().await {
            break id;
        }
    };
    assert_eq!(
        first_reply, hosts_id,
        "the socket must answer another request while the git action runs"
    );

    let committed = client.response(commit_id).await.expect("git commit");
    match committed {
        ResponseBody::GitCommitted { result } => {
            assert_eq!(result.subject, "Change a");
            assert_eq!(result.status.staged.count, 0);
        }
        other => panic!("unexpected git commit response: {other:?}"),
    }

    daemon.stop().await;
}

/// A PR opened the way the Git actions dialog opens one is linked to its
/// thread, so the header badge and the PR page find it afterwards.
#[tokio::test]
async fn a_pull_request_opened_from_the_git_actions_dialog_is_linked_to_its_thread() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "login": "alice",
            "name": "Alice Example",
        })))
        .mount(&server)
        .await;
    let pr = serde_json::json!({
        "number": 42,
        "title": "Fix the widget",
        "state": "open",
        "draft": false,
        "user": {"login": "alice"},
        "head": {"ref": "main", "sha": "abc123"},
        "base": {"ref": "main", "sha": "def456"},
        "updated_at": "2024-05-01T12:00:00Z",
        "created_at": "2024-05-01T09:00:00Z",
        "html_url": "https://forge.test/acme/widget/pull/42",
        "body": "Fixes the widget.",
    });
    Mock::given(method("POST"))
        .and(path("/api/v3/repos/acme/widget/pulls"))
        .respond_with(ResponseTemplate::new(201).set_body_json(pr.clone()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/pulls/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr))
        .mount(&server)
        .await;

    // The workspace's remote names a host that maps to the local test forge.
    let data = tempfile::tempdir().expect("data dir");
    std::fs::write(
        data.path().join("forge-hosts.json"),
        serde_json::json!({
            "hosts": [{"host": "forge.test", "forge": "github", "base_url": server.uri()}]
        })
        .to_string(),
    )
    .expect("forge hosts");
    let daemon = boot_in_dir(
        data,
        ThreadManagerOptions {
            settings: LifecycleSettings::default()
                .with_working_exit_grace(chrono::TimeDelta::milliseconds(80))
                .with_stale_bypass(chrono::TimeDelta::seconds(60)),
            tick_interval: Duration::from_millis(20),
            clock: None,
        },
        FakeHarness::new(),
        false,
        None,
        None,
    )
    .await;

    let host = host_id_for(&daemon).await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let repo = git_repo();
    let bare = tempfile::tempdir().expect("bare");
    let output = std::process::Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(bare.path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("init bare");
    assert!(output.status.success());
    git_in(
        repo.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://forge.test/acme/widget.git",
        ],
    );
    let push_url = format!("file://{}", bare.path().to_str().unwrap());
    // The fetch URL names the forge the daemon detects; the push URL is the
    // local bare repository, so a push never leaves the machine.
    git_in(
        repo.path(),
        &["remote", "set-url", "--push", "origin", &push_url],
    );

    let added = client
        .request(ClientRequest::ForgeAddAccount {
            forge: ForgeKind::GitHub,
            base_url: server.uri(),
            token: "test-token".into(),
            username: Some("alice".into()),
            source: ForgeCredentialSource::Pat,
            repository: None,
        })
        .await
        .expect("add forge account");
    assert!(
        matches!(added, ResponseBody::ForgeAccount { .. }),
        "{added:?}"
    );

    let summary = create_in_workspace(
        &mut client,
        host,
        repo.path().to_str().unwrap(),
        "Open the pull request",
        HarnessKind::ClaudeCode,
        "fake-model",
    )
    .await;
    client.wait_for_turn_completed().await;

    let opened = client
        .request(ClientRequest::ForgeOpenPr {
            thread_id: summary.thread_id.clone(),
            repo: None,
            title: "Fix the widget".into(),
            body: "Fixes the widget.".into(),
            base: "main".into(),
            draft: false,
        })
        .await
        .expect("open the pull request");
    match opened {
        ResponseBody::ForgePrOpened { pr } => assert_eq!(pr.summary.number, 42),
        other => panic!("unexpected open PR response: {other:?}"),
    }

    let listed = client
        .request(ClientRequest::ListThreadPullRequests {
            thread_id: summary.thread_id.clone(),
        })
        .await
        .expect("list links");
    match listed {
        ResponseBody::ThreadPullRequests { pulls, .. } => {
            assert_eq!(pulls.len(), 1, "the opened PR is linked to its thread");
            assert_eq!(pulls[0].number, 42);
            assert_eq!(pulls[0].repo.owner, "acme");
            assert_eq!(pulls[0].repo.repo, "widget");
            assert_eq!(
                pulls[0]
                    .summary
                    .as_ref()
                    .map(|summary| summary.title.as_str()),
                Some("Fix the widget")
            );
        }
        other => panic!("unexpected thread pull requests response: {other:?}"),
    }

    daemon.stop().await;
}
