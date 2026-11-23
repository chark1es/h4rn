//! Webhook delivery over the daemon's real HTTP server.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use h4rn_daemon::auth::TokenStore;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::network_access::NetworkAccess;
use h4rn_daemon::schedules::ScheduleService;
use h4rn_harness::FakeHarness;
use h4rn_protocol::{
    ClientFrame, ClientRequest, DeviceId, DeviceInfo, Event, HarnessKind, IntervalUnit, Item,
    MAX_WEBHOOK_BODY_BYTES, MessageRole, ProtoError, ResponseBody, RuntimeMode,
    ScheduleDestination, ScheduleDraft, ScheduleRunOutcome, ScheduleSpec, ScheduleTaskId,
    ServerFrame,
};
use reqwest::StatusCode;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

struct HttpDaemon {
    addr: std::net::SocketAddr,
    extra: Option<std::net::SocketAddr>,
    token: String,
    network: std::sync::Arc<NetworkAccess>,
    schedules: std::sync::Arc<ScheduleService>,
    manager: std::sync::Arc<h4rn_daemon::threads::ThreadManager>,
    host: h4rn_protocol::HostId,
    _dir: tempfile::TempDir,
    workspace: tempfile::TempDir,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

async fn boot() -> HttpDaemon {
    boot_with(None, None).await
}

/// Boots a daemon with the listener configuration a test needs.
///
/// `network_access` resolves the persisted LAN-listener setting; `None` leaves
/// it off.
async fn boot_with(
    extra_bind: Option<std::net::SocketAddr>,
    network_access: Option<bool>,
) -> HttpDaemon {
    let dir = tempfile::tempdir().expect("dir");
    let workspace = tempfile::tempdir().expect("workspace");
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Off,
        network_access,
        network_port: 0,
    };
    let daemon = DaemonBuilder::new(config)
        .without_title_agents()
        .with_harness(std::sync::Arc::new(FakeHarness::new()))
        .build()
        .await
        .expect("build");
    let token = TokenStore::new(dir.path())
        .load()
        .expect("read token")
        .expect("token exists");
    let schedules = daemon.schedules();
    let manager = daemon.manager();
    let host = daemon.local_host();
    let bound = daemon.bind().await.expect("bind");
    let addr = bound.local_addr();
    let extra = bound.addrs().get(1).copied();
    let network = bound.network();
    let (shutdown, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        bound
            .serve(async move {
                let _ = rx.await;
            })
            .await
    });
    HttpDaemon {
        addr,
        extra,
        token,
        network,
        schedules,
        manager,
        host,
        _dir: dir,
        workspace,
        shutdown,
        task,
    }
}

impl HttpDaemon {
    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.task.await;
    }
}

fn webhook_draft(host: &h4rn_protocol::HostId, workspace: &std::path::Path) -> ScheduleDraft {
    ScheduleDraft {
        name: "PR review".into(),
        host: host.clone(),
        workspace_path: workspace.display().to_string(),
        harness: HarnessKind::ClaudeCode,
        agent_id: None,
        model: "fake-model".into(),
        runtime_mode: RuntimeMode::ApprovalRequired,
        prompt: "Review {{body.pull_request.html_url}} from {{headers.x-event}}".into(),
        schedule: ScheduleSpec::Webhook,
        timezone: "UTC".into(),
        destination: ScheduleDestination::NewThread,
        enabled: true,
    }
}

async fn wait_runs(
    service: &ScheduleService,
    task: &ScheduleTaskId,
    count: usize,
) -> Vec<h4rn_protocol::ScheduleRun> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let runs = service.runs(task).await.expect("runs");
            if runs.len() >= count {
                return runs;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("run history")
}

#[tokio::test]
async fn a_webhook_post_answers_202_and_fills_the_prompt_as_untrusted_data() {
    let daemon = boot().await;
    let task = daemon
        .schedules
        .create(webhook_draft(&daemon.host, daemon.workspace.path()))
        .await
        .expect("create");
    let path = task.webhook_path.clone().expect("path");
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://{}{path}", daemon.addr))
        .header("x-event", "pull_request")
        .header("content-type", "application/json")
        .body(r#"{"pull_request":{"html_url":"https://example.test/pr/9"}}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let runs = wait_runs(&daemon.schedules, &task.task_id, 1).await;
    assert_eq!(runs[0].outcome, ScheduleRunOutcome::Started);
    let thread_id = runs[0].thread_id.clone().expect("thread");
    let snapshot = daemon
        .manager
        .get_thread(&thread_id, 0)
        .await
        .expect("thread");
    let prompt = snapshot
        .events
        .iter()
        .find_map(|wire| match &wire.event {
            Event::ItemCompleted {
                item: Item::Message { role, text, .. },
                ..
            } if *role == MessageRole::User => Some(text.clone()),
            _ => None,
        })
        .expect("user prompt");
    assert!(prompt.contains("untrusted HTTP request"));
    assert!(prompt.contains("<webhook-data name=\"body.pull_request.html_url\">"));
    assert!(prompt.contains("https://example.test/pr/9"));
    assert!(prompt.contains("<webhook-data name=\"headers.x-event\">pull_request</webhook-data>"));
    daemon.stop().await;
}

#[tokio::test]
async fn only_post_runs_and_a_rotated_path_404s() {
    let daemon = boot().await;
    let task = daemon
        .schedules
        .create(webhook_draft(&daemon.host, daemon.workspace.path()))
        .await
        .expect("create");
    let old_path = task.webhook_path.clone().expect("path");
    let client = reqwest::Client::new();

    for method in [
        reqwest::Method::GET,
        reqwest::Method::PUT,
        reqwest::Method::DELETE,
    ] {
        let response = client
            .request(method, format!("http://{}{old_path}", daemon.addr))
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    let (_, new_path) = daemon
        .schedules
        .rotate_webhook(&task.task_id)
        .await
        .expect("rotate");
    let new_path = new_path.expect("new path");
    assert_ne!(old_path, new_path);

    let old = client
        .post(format!("http://{}{old_path}", daemon.addr))
        .body("{}")
        .send()
        .await
        .expect("old");
    assert_eq!(old.status(), StatusCode::NOT_FOUND);
    let new = client
        .post(format!("http://{}{new_path}", daemon.addr))
        .body("{}")
        .send()
        .await
        .expect("new");
    assert_eq!(new.status(), StatusCode::ACCEPTED);
    daemon.stop().await;
}

#[tokio::test]
async fn oversized_bodies_are_refused_before_they_reach_the_task() {
    let daemon = boot().await;
    let task = daemon
        .schedules
        .create(webhook_draft(&daemon.host, daemon.workspace.path()))
        .await
        .expect("create");
    let path = task.webhook_path.expect("path");
    // Twice the cap: the daemon must answer 413 whether the reader stops at
    // the limit or the body arrives well past it.
    let body = "x".repeat(MAX_WEBHOOK_BODY_BYTES * 2);
    let response = reqwest::Client::new()
        .post(format!("http://{}{path}", daemon.addr))
        .body(body)
        .send()
        .await
        .expect("post");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        daemon
            .schedules
            .runs(&task.task_id)
            .await
            .expect("runs")
            .is_empty()
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_burst_of_deliveries_hits_the_rate_limit() {
    let daemon = boot().await;
    let task = daemon
        .schedules
        .create(webhook_draft(&daemon.host, daemon.workspace.path()))
        .await
        .expect("create");
    let path = task.webhook_path.expect("path");
    let client = reqwest::Client::new();
    let mut statuses = Vec::new();
    for _ in 0..(h4rn_protocol::WEBHOOK_RATE_LIMIT_PER_MINUTE + 1) {
        statuses.push(
            client
                .post(format!("http://{}{path}", daemon.addr))
                .body("{}")
                .send()
                .await
                .expect("post")
                .status(),
        );
    }
    assert!(
        statuses[..h4rn_protocol::WEBHOOK_RATE_LIMIT_PER_MINUTE as usize]
            .iter()
            .all(|status| *status == StatusCode::ACCEPTED)
    );
    assert_eq!(
        statuses.last(),
        Some(&StatusCode::TOO_MANY_REQUESTS),
        "the burst is refused, not queued"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn list_create_and_history_travel_over_the_protocol() {
    // The same service shape the WebSocket handlers use, exercised directly
    // so a protocol regression shows up without a socket in the way.
    let daemon = boot().await;
    let mut draft = webhook_draft(&daemon.host, daemon.workspace.path());
    draft.schedule = ScheduleSpec::Every {
        count: 2,
        unit: IntervalUnit::Minutes,
    };
    let task = daemon.schedules.create(draft).await.expect("create");
    assert!(task.next_run_at.is_some());
    let list = daemon.schedules.list().await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "PR review");
    assert!(
        daemon
            .schedules
            .runs(&task.task_id)
            .await
            .expect("runs")
            .is_empty()
    );
    daemon.stop().await;
}

/// A minimal protocol client for the socket-level tests.
struct WsClient {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: u64,
}

impl WsClient {
    async fn connect(daemon: &HttpDaemon) -> Self {
        let url = format!("ws://{}/ws?token={}", daemon.addr, daemon.token);
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("connect the socket");
        Self { ws, next_id: 1 }
    }

    async fn request(&mut self, body: ClientRequest) -> Result<ResponseBody, ProtoError> {
        let id = self.next_id;
        self.next_id += 1;
        let text = serde_json::to_string(&ClientFrame { id, body }).expect("serialise");
        self.ws
            .send(Message::Text(text.into()))
            .await
            .expect("send frame");
        loop {
            let message = tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("timed out waiting for a reply")
                .expect("socket closed")
                .expect("socket error");
            let Message::Text(text) = message else {
                continue;
            };
            let frame: ServerFrame = serde_json::from_str(&text).expect("parse frame");
            if let ServerFrame::Response {
                id: response_id,
                result,
            } = frame
                && response_id == id
            {
                return result;
            }
        }
    }

    async fn hello(&mut self) {
        self.request(ClientRequest::Hello {
            protocol_version: h4rn_protocol::PROTOCOL_VERSION,
            device: DeviceInfo {
                device_id: DeviceId::new("dev_schedules"),
                name: "schedules integration test".into(),
                platform: "linux".into(),
                app_version: "0.1.0".into(),
            },
        })
        .await
        .expect("hello");
    }
}

#[tokio::test]
async fn the_socket_lists_creates_and_reads_history_without_exposing_the_secret() {
    let daemon = boot().await;
    let mut client = WsClient::connect(&daemon).await;
    client.hello().await;

    let mut draft = webhook_draft(&daemon.host, daemon.workspace.path());
    draft.prompt = "Review {{body.ref}}".into();
    let created = client
        .request(ClientRequest::CreateSchedule { task: draft })
        .await
        .expect("create over the socket");
    let webhook_task = match created {
        ResponseBody::ScheduleTask { task } => task,
        other => panic!("unexpected create answer: {other:?}"),
    };
    assert!(webhook_task.webhook_path_set);
    let path = daemon
        .schedules
        .webhook_url(&webhook_task.task_id)
        .await
        .expect("url")
        .expect("path");
    let secret = path.strip_prefix("/webhooks/").expect("prefix").to_owned();

    // A view read of the list never carries the secret.
    let listed = client
        .request(ClientRequest::ListSchedules)
        .await
        .expect("list");
    let wire = serde_json::to_string(&listed).expect("serialise list");
    assert!(!wire.contains(&secret), "a list answer leaked the path");
    match listed {
        ResponseBody::Schedules { tasks } => {
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].name, "PR review");
        }
        other => panic!("unexpected list answer: {other:?}"),
    }

    // A clock task runs by hand, and its history comes back over the socket.
    let mut clock_draft = webhook_draft(&daemon.host, daemon.workspace.path());
    clock_draft.schedule = ScheduleSpec::Every {
        count: 2,
        unit: IntervalUnit::Minutes,
    };
    let created = client
        .request(ClientRequest::CreateSchedule { task: clock_draft })
        .await
        .expect("create a clock task");
    let clock_task = match created {
        ResponseBody::ScheduleTask { task } => task,
        other => panic!("unexpected create answer: {other:?}"),
    };
    client
        .request(ClientRequest::RunScheduleNow {
            task_id: clock_task.task_id.clone(),
        })
        .await
        .expect("run now");
    let runs = client
        .request(ClientRequest::ListScheduleRuns {
            task_id: clock_task.task_id.clone(),
        })
        .await
        .expect("history");
    match runs {
        ResponseBody::ScheduleRuns { runs } => {
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].outcome, ScheduleRunOutcome::Started);
            assert!(runs[0].thread_id.is_some());
        }
        other => panic!("unexpected history answer: {other:?}"),
    }

    // The secret is still revealable through the explicit admin request.
    let revealed = client
        .request(ClientRequest::GetScheduleWebhookUrl {
            task_id: webhook_task.task_id,
        })
        .await
        .expect("reveal");
    match revealed {
        ResponseBody::ScheduleWebhookUrl { path, .. } => {
            assert_eq!(path, Some(format!("/webhooks/{secret}")));
        }
        other => panic!("unexpected reveal answer: {other:?}"),
    }
    daemon.stop().await;
}

#[tokio::test]
async fn webhook_paths_are_served_only_on_the_loopback_and_explicit_binds() {
    let extra = "0.0.0.0:0".parse().expect("extra bind");
    let daemon = boot_with(Some(extra), Some(true)).await;
    let extra = daemon.extra.expect("the extra bind has a listener");
    let task = daemon
        .schedules
        .create(webhook_draft(&daemon.host, daemon.workspace.path()))
        .await
        .expect("create");
    let path = task.webhook_path.clone().expect("path");
    let client = reqwest::Client::new();

    let loopback = client
        .post(format!("http://{}{path}", daemon.addr))
        .body("{}")
        .send()
        .await
        .expect("loopback");
    assert_eq!(loopback.status(), StatusCode::ACCEPTED);

    let extra_addr = std::net::SocketAddr::new("127.0.0.1".parse().expect("ip"), extra.port());
    let explicit = client
        .post(format!("http://{extra_addr}{path}"))
        .body("{}")
        .send()
        .await
        .expect("explicit bind");
    assert_eq!(
        explicit.status(),
        StatusCode::ACCEPTED,
        "an explicitly configured bind serves webhooks"
    );

    let snapshot = daemon.network.snapshot();
    assert!(snapshot.enabled && snapshot.listening);
    let lan = client
        .post(format!("http://127.0.0.1:{}{path}", snapshot.port))
        .body("{}")
        .send()
        .await
        .expect("network listener");
    assert_eq!(
        lan.status(),
        StatusCode::NOT_FOUND,
        "the LAN listener must not serve webhook paths"
    );
    daemon.stop().await;
}
