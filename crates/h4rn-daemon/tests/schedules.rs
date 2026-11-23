//! Webhook delivery over the daemon's real HTTP server.

use std::time::Duration;

use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::schedules::ScheduleService;
use h4rn_harness::FakeHarness;
use h4rn_protocol::{
    Event, HarnessKind, IntervalUnit, Item, MAX_WEBHOOK_BODY_BYTES, MessageRole, RuntimeMode,
    ScheduleDestination, ScheduleDraft, ScheduleRunOutcome, ScheduleSpec, ScheduleTaskId,
};
use reqwest::StatusCode;
use tokio::sync::oneshot;

struct HttpDaemon {
    addr: std::net::SocketAddr,
    schedules: std::sync::Arc<ScheduleService>,
    manager: std::sync::Arc<h4rn_daemon::threads::ThreadManager>,
    host: h4rn_protocol::HostId,
    _dir: tempfile::TempDir,
    workspace: tempfile::TempDir,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

async fn boot() -> HttpDaemon {
    let dir = tempfile::tempdir().expect("dir");
    let workspace = tempfile::tempdir().expect("workspace");
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
        .with_harness(std::sync::Arc::new(FakeHarness::new()))
        .build()
        .await
        .expect("build");
    let schedules = daemon.schedules();
    let manager = daemon.manager();
    let host = daemon.local_host();
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
    HttpDaemon {
        addr,
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

    let rotated = daemon
        .schedules
        .rotate_webhook(&task.task_id)
        .await
        .expect("rotate");
    let new_path = rotated.webhook_path.expect("new path");
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
    let body = "x".repeat(MAX_WEBHOOK_BODY_BYTES + 1);
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
