//! Cross-harness subagent integration through the real MCP hub (ADR-038).
//!
//! The parent and child are both [`FakeHarness`] adapters, but every tool call
//! travels the shipping path: a streamable-HTTP POST to the daemon's `/mcp`
//! endpoint, authenticated with a bearer token minted for the parent thread.
//! That proves token scope, `h4rn__*` tool registration, supervision, approval
//! mirroring and the worktree lifecycle without spending model tokens.

use std::sync::Arc;
use std::time::Duration;

use h4rn_core::LifecycleSettings;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::harnesses::{HarnessCatalog, StaticHarnessCatalog};
use h4rn_daemon::hosts::HostInventory;
use h4rn_daemon::threads::{NewThread, ThreadManager, ThreadManagerOptions};
use h4rn_harness::{FakeHarness, HarnessEvent};
use h4rn_mcp::session::McpScope;
use h4rn_protocol::{
    AgentState, ApprovalDecision, ApprovalKind, ApprovalOption, AutoResumePolicy, Event,
    HarnessKind, Item, RuntimeMode, SubagentId, SubagentState, ThreadFilter, ThreadId, TurnStatus,
    Usage, UsageGroupBy, UsageScope,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

struct TestDaemon {
    addr: std::net::SocketAddr,
    manager: Arc<ThreadManager>,
    subagents: Arc<h4rn_daemon::subagents::SubagentManager>,
    mcp: Arc<h4rn_daemon::mcp::McpService>,
    data_dir: std::path::PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
    _dir: Option<tempfile::TempDir>,
}

impl TestDaemon {
    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "subagents@h4rn.invalid"]);
    run(&["config", "user.name", "h4rn subagents"]);
    std::fs::write(dir.path().join("README.md"), "# subagents\n").expect("write readme");
    run(&["add", "README.md"]);
    run(&["commit", "-q", "-m", "init"]);
    dir
}

fn catalog() -> Arc<StaticHarnessCatalog> {
    let mut catalog = StaticHarnessCatalog::new();
    catalog.register(Arc::new(FakeHarness::new()));
    catalog.register(Arc::new(FakeHarness::new().with_kind(HarnessKind::Codex)));
    catalog.register(Arc::new(
        FakeHarness::with_events(vec![HarnessEvent::ApprovalRequested {
            approval_id: "a1".into(),
            kind: ApprovalKind::Command,
            prompt: "run the child command".into(),
            options: Vec::<ApprovalOption>::new(),
        }])
        .with_kind(HarnessKind::Opencode),
    ));
    Arc::new(catalog)
}

async fn boot() -> TestDaemon {
    boot_with_catalog(catalog()).await
}

async fn boot_with_catalog(catalog: Arc<StaticHarnessCatalog>) -> TestDaemon {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut daemon = boot_in(dir.path().to_path_buf(), catalog).await;
    daemon._dir = Some(dir);
    daemon
}

async fn boot_in(data_dir: std::path::PathBuf, catalog: Arc<StaticHarnessCatalog>) -> TestDaemon {
    let config = Config {
        data_dir: data_dir.clone(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Auto,
        network_access: None,
        network_port: 0,
    };
    let options = ThreadManagerOptions {
        settings: LifecycleSettings::default()
            .with_working_exit_grace(chrono::TimeDelta::milliseconds(200)),
        tick_interval: Duration::from_millis(20),
        clock: None,
    };
    let daemon = DaemonBuilder::new(config)
        .with_catalog(catalog)
        .with_lifecycle(options)
        .build()
        .await
        .expect("build daemon");
    let manager = daemon.manager();
    let subagents = daemon.subagents();
    let mcp = daemon.mcp();
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
        manager,
        subagents,
        mcp,
        data_dir,
        shutdown: Some(shutdown),
        task,
        _dir: None,
    }
}

/// A raw streamable-HTTP MCP client authenticated as one harness session.
struct HubClient {
    url: String,
    token: String,
    http: reqwest::Client,
}

impl HubClient {
    fn new(daemon: &TestDaemon, parent: &ThreadId) -> Self {
        let catalog = Arc::new(StaticHarnessCatalog::new());
        let host = HostInventory::detect(catalog as Arc<dyn HarnessCatalog>).local_id();
        let token = daemon
            .mcp
            .hub()
            .issue_session(McpScope::harness(
                HarnessKind::ClaudeCode,
                host,
                Some(parent.clone()),
            ))
            .token;
        Self {
            url: format!("http://{}/mcp", daemon.addr),
            token,
            http: reqwest::Client::new(),
        }
    }

    async fn rpc(&self, method: &str, params: Value) -> Value {
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .await
            .expect("MCP request");
        assert_eq!(response.status(), 200, "HTTP status for {method}");
        let body: Value = response.json().await.expect("MCP body");
        assert_eq!(body["jsonrpc"], "2.0", "body for {method}: {body}");
        body
    }

    async fn call(&self, name: &str, arguments: Value) -> Value {
        let body = self
            .rpc("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        assert!(body.get("error").is_none(), "{name} failed: {body}");
        body["result"].clone()
    }

    async fn spawn(&self, arguments: Value) -> Value {
        let result = self.call("h4rn__spawn_subagent", arguments).await;
        assert_eq!(result["isError"], false, "spawn failed: {result}");
        result["structuredContent"]["subagent"].clone()
    }
}

fn subagent_id(view: &Value) -> SubagentId {
    SubagentId::new(
        view["subagent_id"]
            .as_str()
            .expect("subagent id in the spawn result"),
    )
}

fn real_catalog() -> Arc<StaticHarnessCatalog> {
    let mut catalog = StaticHarnessCatalog::new();
    for adapter in h4rn_harness::registry::builtin_adapters() {
        catalog.register(adapter);
    }
    Arc::new(catalog)
}

async fn create_parent_with(
    daemon: &TestDaemon,
    workspace: &std::path::Path,
    prompt: &str,
    runtime_mode: RuntimeMode,
) -> ThreadId {
    let catalog = Arc::new(StaticHarnessCatalog::new());
    let host = HostInventory::detect(catalog as Arc<dyn HarnessCatalog>).local_id();
    daemon
        .manager
        .create_thread(NewThread {
            thread_id: None,
            parent_thread_id: None,
            subagent_id: None,
            host,
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            workspace_path: workspace.display().to_string(),
            model: "default".into(),
            prompt: prompt.to_owned(),
            attachments: Vec::new(),
            runtime_mode,
            worktree: None,
        })
        .await
        .expect("create parent")
        .thread_id
}

async fn create_parent(daemon: &TestDaemon, workspace: &std::path::Path) -> ThreadId {
    create_parent_with(
        daemon,
        workspace,
        "parent thread",
        RuntimeMode::ApprovalRequired,
    )
    .await
}

async fn wait_for_parent_approval(
    manager: &ThreadManager,
    parent: &ThreadId,
    subagent: &SubagentId,
) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = manager.get_thread(parent, 0).await.expect("parent thread");
        for wire in snapshot.events {
            if let Event::ApprovalRequested {
                approval_id,
                subagent_id: Some(owner),
                prompt,
                ..
            } = wire.event
                && &owner == subagent
            {
                assert!(
                    prompt.starts_with("[Subagent] "),
                    "mirrored prompt should be attributed: {prompt:?}"
                );
                return approval_id;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the child approval never reached the parent thread"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Waits until `parent` has journaled `count` resolutions of one mirrored
/// approval. Cancelling a child resolves the mirror on a task of its own, so
/// the count is a state to wait for, not a fact to read once.
async fn wait_for_mirrored_resolutions(
    manager: &ThreadManager,
    parent: &ThreadId,
    approval_id: &str,
    count: usize,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = manager.get_thread(parent, 0).await.expect("parent thread");
        let resolved = snapshot
            .events
            .iter()
            .filter(|wire| {
                matches!(
                    &wire.event,
                    Event::ApprovalResolved { approval_id: id, .. } if id == approval_id
                )
            })
            .count();
        if resolved >= count {
            assert_eq!(
                resolved, count,
                "the mirror resolves once per answered or settled hand"
            );
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "expected {count} resolutions of {approval_id}, saw {resolved}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn subagents_round_trip_through_the_mcp_hub() {
    let repo = git_repo();
    let daemon = boot().await;
    let parent = create_parent(&daemon, repo.path()).await;
    let hub = HubClient::new(&daemon, &parent);

    let tools = hub.rpc("tools/list", json!({})).await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tool list")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for expected in [
        "h4rn__spawn_subagent",
        "h4rn__get_subagent_status",
        "h4rn__get_subagent_result",
        "h4rn__cancel_subagent",
        "h4rn__list_subagents",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }

    // 1. A cross-harness child in an isolated git worktree.
    let spawned = hub
        .spawn(json!({
            "task": "reply with pong",
            "harness": "codex",
            "isolation": "worktree",
            "budget": {"max_turns": 4, "max_wall_time_secs": 60}
        }))
        .await;
    let echo_child = subagent_id(&spawned);
    assert_eq!(spawned["harness"], "codex");
    assert_eq!(spawned["isolation"], "worktree");
    assert_eq!(spawned["depth"], 1);
    assert!(
        spawned["branch"]
            .as_str()
            .is_some_and(|branch| branch.starts_with("h4rn/sa-")),
        "{spawned}"
    );

    // The parent journal gets a durable Delegated row, which names the child
    // thread; child threads stay out of the top-level list unless asked for.
    let parent_view = daemon
        .manager
        .get_thread(&parent, 0)
        .await
        .expect("parent projection");
    let child_thread = parent_view
        .events
        .iter()
        .find_map(|wire| match &wire.event {
            Event::ItemCompleted {
                item:
                    Item::Delegated {
                        subagent_id: id,
                        thread_id,
                        ..
                    },
                ..
            } if id == &echo_child => thread_id.clone(),
            _ => None,
        });
    assert!(
        child_thread.is_some(),
        "the parent journal needs a Delegated row"
    );
    let child_thread = child_thread.expect("delegated child thread id");
    let top_level = daemon.manager.list_threads(&ThreadFilter::default()).await;
    assert!(
        top_level
            .iter()
            .all(|thread| thread.thread_id != child_thread),
        "top-level listing must hide child threads"
    );

    let finished = hub
        .call(
            "h4rn__get_subagent_result",
            json!({"subagent_id": echo_child, "wait_secs": 10}),
        )
        .await;
    assert_eq!(finished["isError"], false, "{finished}");
    let view = &finished["structuredContent"]["subagent"];
    assert_eq!(view["state"], "completed", "{view}");
    assert!(
        view["result"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("echo: reply with pong")),
        "child result text: {view}"
    );
    let worktree = daemon
        .data_dir
        .join("subagents")
        .join(echo_child.as_str())
        .join("worktree");
    assert!(
        !worktree.exists(),
        "the worktree must be removed when the child settles: {}",
        worktree.display()
    );
    let branches = std::process::Command::new("git")
        .args(["branch", "--list", "h4rn/sa-*"])
        .current_dir(repo.path())
        .output()
        .expect("list branches");
    assert!(
        !String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
        "a completed worktree child keeps its branch"
    );

    let status = hub
        .call(
            "h4rn__get_subagent_status",
            json!({"subagent_id": echo_child}),
        )
        .await;
    assert_eq!(
        status["structuredContent"]["subagent"]["state"],
        "completed"
    );

    let listed = hub
        .call("h4rn__list_subagents", json!({"include_finished": true}))
        .await;
    let list = listed["structuredContent"]["subagents"]
        .as_array()
        .expect("subagent list");
    assert!(
        list.iter()
            .any(|entry| entry["subagent_id"] == echo_child.as_str()),
        "{list:?}"
    );

    // 2. A child approval is mirrored onto the parent, and answering it on the
    //    parent reaches the child's harness.
    let approval_child = subagent_id(
        &hub.spawn(json!({"task": "needs approval", "harness": "opencode"}))
            .await,
    );
    let parent_approval = wait_for_parent_approval(&daemon.manager, &parent, &approval_child).await;
    assert_eq!(parent_approval, "sa:a1");
    daemon
        .manager
        .respond_approval(
            &parent,
            &parent_approval,
            ApprovalDecision::Approve,
            None,
            None,
        )
        .await
        .expect("answer the mirrored approval");

    let cancelled = hub
        .call(
            "h4rn__cancel_subagent",
            json!({"subagent_id": approval_child}),
        )
        .await;
    assert_eq!(cancelled["isError"], false, "{cancelled}");
    assert_eq!(
        cancelled["structuredContent"]["subagent"]["state"], "cancelled",
        "{cancelled}"
    );

    // 3. Cancelling a child that is still waiting for approval resolves its
    //    mirrored hand on the parent.
    let pending_child = subagent_id(
        &hub.spawn(json!({"task": "cancel while approving", "harness": "opencode"}))
            .await,
    );
    wait_for_parent_approval(&daemon.manager, &parent, &pending_child).await;
    let cancelled = hub
        .call(
            "h4rn__cancel_subagent",
            json!({"subagent_id": pending_child}),
        )
        .await;
    assert_eq!(
        cancelled["structuredContent"]["subagent"]["state"],
        "cancelled"
    );
    let pending_view = &cancelled["structuredContent"]["subagent"];
    let child_thread = pending_view["child_thread_id"]
        .as_str()
        .expect("child thread id")
        .to_owned();
    wait_for_mirrored_resolutions(&daemon.manager, &ThreadId::new(child_thread), "a1", 1).await;

    wait_for_mirrored_resolutions(&daemon.manager, &parent, "sa:a1", 2).await;

    // 4. A token scoped to another thread cannot touch this parent's child.
    let stranger = create_parent(&daemon, repo.path()).await;
    let intruder = HubClient::new(&daemon, &stranger);
    let denied = intruder
        .rpc(
            "tools/call",
            json!({"name": "h4rn__get_subagent_status", "arguments": {"subagent_id": echo_child}}),
        )
        .await;
    assert!(
        denied["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("belongs to another thread")),
        "{denied}"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn a_wall_time_budget_interrupts_the_child() {
    let daemon = boot().await;
    let parent = create_parent(&daemon, daemon.data_dir.as_path()).await;
    let hub = HubClient::new(&daemon, &parent);

    // The scripted approval child never finishes on its own, so only the
    // supervisor's wall-clock enforcement can end it.
    let spawned = hub
        .spawn(json!({
            "task": "run forever",
            "harness": "opencode",
            "budget": {"max_wall_time_secs": 1}
        }))
        .await;
    let id = subagent_id(&spawned);
    let finished = hub
        .call(
            "h4rn__get_subagent_result",
            json!({"subagent_id": id, "wait_secs": 10}),
        )
        .await;
    let view = &finished["structuredContent"]["subagent"];
    assert_eq!(view["state"], "budget_exceeded", "{view}");
    assert!(
        view["result"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("wall-time budget")),
        "{view}"
    );

    daemon.stop().await;
}

// ---------------------------------------------------------------------------
// Metering, auto-resume and durability integration (ADR-024, ADR-038).
// ---------------------------------------------------------------------------

fn catalog_with_child(child: FakeHarness) -> Arc<StaticHarnessCatalog> {
    let mut catalog = StaticHarnessCatalog::new();
    catalog.register(Arc::new(FakeHarness::new()));
    catalog.register(Arc::new(child));
    Arc::new(catalog)
}

fn limit_then_resume_child(reset_at: chrono::DateTime<chrono::Utc>) -> FakeHarness {
    let first = vec![
        HarnessEvent::Usage {
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Usage::default()
            },
        },
        HarnessEvent::LimitHit {
            reset_at: Some(reset_at),
            auto_resume: true,
        },
        HarnessEvent::TurnCompleted {
            status: TurnStatus::Failed,
            usage: Usage::default(),
        },
    ];
    let second = vec![
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
    ];
    FakeHarness::with_session_scripts(vec![first, second]).with_kind(HarnessKind::Codex)
}

async fn parent_usage(
    manager: &ThreadManager,
    parent: &ThreadId,
    include_children: bool,
) -> h4rn_protocol::UsageReport {
    manager
        .usage_report(h4rn_meter::ReportQuery {
            scope: UsageScope::Thread,
            scope_id: Some(parent.as_str().to_owned()),
            include_children,
            range: Default::default(),
            group_by: UsageGroupBy::None,
            bucket: None,
            series_group_by: None,
        })
        .await
        .expect("usage report")
}

#[tokio::test]
async fn subagent_usage_is_metered_and_rolls_up_into_the_parent() {
    let repo = git_repo();
    let daemon = boot_with_catalog(catalog_with_child(
        FakeHarness::new().with_kind(HarnessKind::Codex),
    ))
    .await;
    let parent = create_parent(&daemon, repo.path()).await;
    let hub = HubClient::new(&daemon, &parent);

    let id = subagent_id(
        &hub.spawn(json!({
            "task": "reply with pong",
            "harness": "codex",
            "budget": {"max_wall_time_secs": 30}
        }))
        .await,
    );
    let finished = hub
        .call(
            "h4rn__get_subagent_result",
            json!({"subagent_id": id, "wait_secs": 10}),
        )
        .await;
    assert_eq!(
        finished["structuredContent"]["subagent"]["state"], "completed",
        "{finished}"
    );

    let own = parent_usage(&daemon.manager, &parent, false).await;
    let rolled = parent_usage(&daemon.manager, &parent, true).await;
    assert_eq!(own.totals.turns, 1, "only the parent's own turn counts");
    assert_eq!(
        rolled.totals.turns,
        own.totals.turns + 1,
        "the child's turn rolls up into the parent"
    );

    // The child's row keeps its own thread id and names the parent, so it can
    // also be reported on its own.
    let meter = daemon.manager.meter().expect("metering is enabled");
    let rows = meter
        .store()
        .rows(&h4rn_meter::UsageFilter {
            thread_id: Some(parent.as_str().to_owned()),
            include_children: true,
            ..Default::default()
        })
        .await
        .expect("ledger rows");
    assert!(
        rows.iter()
            .any(|row| row.parent_thread_id.as_deref() == Some(parent.as_str())),
        "a child row must record its parent: {rows:?}"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn a_subagent_limit_follows_auto_resume_and_the_parent_sees_scheduled() {
    let reset_at = chrono::Utc::now() + chrono::Duration::milliseconds(300);
    let daemon = boot_with_catalog(catalog_with_child(limit_then_resume_child(reset_at))).await;
    daemon.manager.set_auto_resume_policy(AutoResumePolicy {
        failover: false,
        max_wait_ms: 3_600_000,
        prompt: "continue after the limit".into(),
        jitter_ms: 0,
    });
    let repo = git_repo();
    let parent = create_parent(&daemon, repo.path()).await;
    let hub = HubClient::new(&daemon, &parent);

    let id = subagent_id(
        &hub.spawn(json!({
            "task": "hit a usage limit",
            "harness": "codex",
            "budget": {"max_wall_time_secs": 120}
        }))
        .await,
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let scheduled = loop {
        let views = daemon.subagents.list(Some(&parent), true).await;
        if let Some(view) = views.iter().find(|view| view.subagent_id == id)
            && view.state == SubagentState::Scheduled
        {
            break view.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the parent never saw the child as Scheduled"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(scheduled.agent_state, AgentState::Scheduled);
    assert!(
        scheduled.result.is_none(),
        "a scheduled child has no result yet"
    );
    assert!(
        daemon
            .manager
            .limits_report()
            .await
            .expect("limits")
            .limits
            .iter()
            .any(|limit| limit.thread_id.as_ref() == scheduled.child_thread_id.as_ref()),
        "the child's limit is recorded in the ledger"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let views = daemon.subagents.list(Some(&parent), true).await;
        if let Some(view) = views.iter().find(|view| view.subagent_id == id)
            && view.state.is_terminal()
        {
            assert_eq!(view.state, SubagentState::Completed, "{view:?}");
            let result = view.result.as_ref().expect("a completed result");
            assert!(
                result.text.contains("resumed"),
                "the resumed turn's text is the result: {result:?}"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the child never resumed after its usage limit"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let own = parent_usage(&daemon.manager, &parent, false).await;
    let rolled = parent_usage(&daemon.manager, &parent, true).await;
    assert_eq!(
        rolled.totals.turns,
        own.totals.turns + 2,
        "the limited turn and the resumed turn both roll up"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn a_subagent_token_budget_is_enforced_from_the_metered_ledger() {
    let child = FakeHarness::with_session_scripts(vec![vec![
        HarnessEvent::Usage {
            usage: Usage {
                input_tokens: 50,
                output_tokens: 10,
                ..Usage::default()
            },
        },
        HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Usage::default(),
        },
    ]])
    .with_kind(HarnessKind::Codex);
    let daemon = boot_with_catalog(catalog_with_child(child)).await;
    let repo = git_repo();
    let parent = create_parent(&daemon, repo.path()).await;
    let hub = HubClient::new(&daemon, &parent);

    let id = subagent_id(
        &hub.spawn(json!({
            "task": "spend too many tokens",
            "harness": "codex",
            "budget": {"max_tokens": 5, "max_wall_time_secs": 30}
        }))
        .await,
    );
    let finished = hub
        .call(
            "h4rn__get_subagent_result",
            json!({"subagent_id": id, "wait_secs": 10}),
        )
        .await;
    let view = &finished["structuredContent"]["subagent"];
    assert_eq!(view["state"], "budget_exceeded", "{view}");
    assert!(
        view["result"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("token budget")),
        "{view}"
    );
    assert!(
        view["progress"]["tokens"].as_u64().unwrap_or(0) >= 60,
        "the meter's count is what the card shows: {view}"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn subagent_records_survive_a_daemon_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data_dir = dir.path().to_path_buf();

    let first = boot_in(data_dir.clone(), catalog()).await;
    let parent = create_parent(&first, dir.path()).await;
    let hub = HubClient::new(&first, &parent);
    let id = subagent_id(
        &hub.spawn(json!({
            "task": "reply with pong",
            "harness": "codex",
            "budget": {"max_wall_time_secs": 30}
        }))
        .await,
    );
    let finished = hub
        .call(
            "h4rn__get_subagent_result",
            json!({"subagent_id": id, "wait_secs": 10}),
        )
        .await;
    assert_eq!(
        finished["structuredContent"]["subagent"]["state"], "completed",
        "{finished}"
    );
    first.stop().await;

    // A fresh daemon on the same data directory rebuilds the record, status and
    // result, and the parent's journal still has the durable Delegated card.
    let second = boot_in(data_dir, catalog()).await;
    let views = second.subagents.list(Some(&parent), true).await;
    assert_eq!(views.len(), 1, "the subagent record survives the restart");
    assert_eq!(views[0].subagent_id, id);
    assert_eq!(views[0].state, SubagentState::Completed);
    let result = views[0].result.as_ref().expect("restored result");
    assert!(
        result.text.contains("echo: reply with pong"),
        "the result is rebuilt: {result:?}"
    );
    let fetched = second
        .subagents
        .get(&id)
        .await
        .expect("get by id after restart");
    assert_eq!(fetched.state, SubagentState::Completed);

    let snapshot = second
        .manager
        .get_thread(&parent, 0)
        .await
        .expect("parent projection");
    assert!(
        snapshot.events.iter().any(|wire| matches!(
            &wire.event,
            Event::ItemCompleted {
                item: Item::Delegated { subagent_id, .. },
                ..
            } if subagent_id == &id
        )),
        "the parent keeps its Delegated card across a restart"
    );

    second.stop().await;
}

/// Real cross-harness delegation: Claude Code drives the `h4rn__*` tools and a
/// Codex child answers. Ignored by default; it spawns both binaries and spends
/// tokens.
///
/// ```text
/// cargo test -p h4rn-daemon --test e2e_subagents -- --ignored --nocapture \
///     real_claude_delegates_to_codex
/// ```
#[tokio::test]
#[ignore = "spawns real Claude Code and Codex processes and spends tokens"]
async fn real_claude_delegates_to_codex() {
    let catalog = real_catalog();
    if catalog.detect(HarnessKind::ClaudeCode).is_none()
        || catalog.detect(HarnessKind::Codex).is_none()
    {
        eprintln!("skipping: Claude Code and Codex must both be installed");
        return;
    }

    let repo = git_repo();
    let daemon = boot_with_catalog(catalog).await;
    let prompt = "Use the MCP tool `h4rn__spawn_subagent` exactly once with arguments \
                  {\"task\":\"Reply with exactly: pong\",\"harness\":\"codex\"}. \
                  Then call `h4rn__get_subagent_result` with the returned subagent_id and \
                  wait_secs 120. Do not do anything else.";
    let parent = create_parent_with(&daemon, repo.path(), prompt, RuntimeMode::FullAccess).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    loop {
        let children = daemon.subagents.list(Some(&parent), true).await;
        if let Some(child) = children.first()
            && child.state.is_terminal()
        {
            assert_eq!(
                child.state,
                SubagentState::Completed,
                "the Codex child did not complete: {child:?}"
            );
            let result = child.result.as_ref().expect("a completed result");
            assert!(
                result.text.to_lowercase().contains("pong"),
                "the Codex child did not answer pong: {:?}",
                result.text
            );
            daemon.stop().await;
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the Claude parent did not finish delegating in time"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
