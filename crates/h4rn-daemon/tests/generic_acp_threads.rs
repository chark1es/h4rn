//! End-to-end: a user-configured local ACP command through the thread manager.
//!
//! The agent is the `agy-fixture` peer from `h4rn-test-fixtures`, launched by
//! path as any other configured command would be. It echoes prompts, emits a
//! tool call, asks for permission and honours a cancel — the four protocol
//! paths the generic adapter must not re-implement.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use h4rn_daemon::acp_agents::AcpAgentService;
use h4rn_daemon::harnesses::StaticHarnessCatalog;
use h4rn_daemon::journal::SqliteJournal;
use h4rn_daemon::threads::{NewThread, ThreadManager, ThreadManagerOptions};
use h4rn_harness::GenericAcpHarness;
use h4rn_protocol::{
    AcpAgentSpec, AcpWorkingDir, ApprovalDecision, Event, HarnessKind, HostId, RuntimeMode,
    ThreadId, TurnStatus,
};
use tokio::sync::broadcast;

const TURN_TIMEOUT: Duration = Duration::from_secs(60);

fn fixture_binary() -> Option<PathBuf> {
    static BINARY: OnceLock<Option<PathBuf>> = OnceLock::new();
    BINARY.get_or_init(build_fixture).clone()
}

fn fixture_target_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    static OWNED: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| match std::env::var_os("CARGO_TARGET_TMPDIR") {
        Some(dir) => {
            use std::hash::{DefaultHasher, Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            env!("CARGO_MANIFEST_DIR").hash(&mut hasher);
            PathBuf::from(dir).join(format!("generic-acp-fixture-{:016x}", hasher.finish()))
        }
        None => OWNED
            .get_or_init(|| tempfile::tempdir().expect("a temporary build directory"))
            .path()
            .to_path_buf(),
    })
}

fn build_fixture() -> Option<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let output = std::process::Command::new(cargo)
        .args([
            "build",
            "--package",
            "h4rn-test-fixtures",
            "--bin",
            "agy-fixture",
            "--manifest-path",
        ])
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .args(["--target-dir"])
        .arg(fixture_target_dir())
        .args(["--message-format=json"])
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "skipping: cargo could not build the ACP fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-artifact") {
            continue;
        }
        if message
            .get("target")
            .and_then(|target| target.get("name"))
            .and_then(serde_json::Value::as_str)
            != Some("agy-fixture")
        {
            continue;
        }
        return message
            .get("executable")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from);
    }
    None
}

async fn manager() -> Option<ThreadManager> {
    let binary = fixture_binary()?;
    let data = tempfile::tempdir().expect("data dir");
    let agents = AcpAgentService::open(data.path()).expect("agent service");
    agents
        .add(AcpAgentSpec {
            id: "fixture".into(),
            name: "ACP fixture".into(),
            command: binary.display().to_string(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            working_dir: AcpWorkingDir::ThreadWorkspace,
        })
        .expect("configure the fixture");
    let catalog = StaticHarnessCatalog::with(Arc::new(GenericAcpHarness::new(Arc::new(agents))));
    let manager = ThreadManager::open(
        Arc::new(SqliteJournal::in_memory().await.expect("journal")),
        Arc::new(catalog),
        HostId::new("host_test"),
        ThreadManagerOptions {
            tick_interval: Duration::from_secs(3600),
            ..ThreadManagerOptions::default()
        },
        None,
    )
    .await
    .expect("manager");
    Some(manager)
}

async fn create(manager: &ThreadManager, workspace: &std::path::Path) -> ThreadId {
    manager
        .create_thread(NewThread {
            thread_id: None,
            parent_thread_id: None,
            subagent_id: None,
            host: HostId::new("host_test"),
            harness: HarnessKind::Acp,
            agent_id: Some("fixture".into()),
            workspace_path: workspace.display().to_string(),
            model: "default".into(),
            prompt: "hello".into(),
            attachments: Vec::new(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            worktree: None,
        })
        .await
        .expect("create")
        .thread_id
}

fn skip_without_fixture() {
    eprintln!("skipping: the ACP fixture could not be built");
}

async fn next_turn(
    events: &mut broadcast::Receiver<h4rn_daemon::journal::WireEvent>,
    mut on_event: impl FnMut(&Event),
) -> (TurnStatus, String, Vec<String>) {
    tokio::time::timeout(TURN_TIMEOUT, async {
        let mut text = String::new();
        let mut tools = Vec::new();
        loop {
            let Ok(wire) = events.recv().await else {
                panic!("the event stream closed before the turn ended");
            };
            on_event(&wire.event);
            match wire.event {
                Event::MessageDelta { text: delta, .. } => text.push_str(&delta),
                Event::ToolCall { tool_call, .. } => {
                    tools.push(format!("{}:{:?}", tool_call.name, tool_call.status))
                }
                Event::TurnCompleted { status, .. } => return (status, text, tools),
                _ => {}
            }
        }
    })
    .await
    .expect("the turn ended in time")
}

#[cfg(unix)]
#[tokio::test]
async fn a_configured_agent_can_be_probed_for_its_version_and_models() {
    let Some(binary) = fixture_binary() else {
        return skip_without_fixture();
    };
    let data = tempfile::tempdir().expect("data dir");
    let agents = AcpAgentService::open(data.path()).expect("agent service");
    agents
        .add(AcpAgentSpec {
            id: "fixture".into(),
            name: "ACP fixture".into(),
            command: binary.display().to_string(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            working_dir: AcpWorkingDir::ThreadWorkspace,
        })
        .expect("configure");
    let probe = agents.probe("fixture").await.expect("probe");
    assert!(probe.installed);
    assert!(probe.error.is_none(), "probe failed: {probe:?}");
    assert!(probe.version.is_some(), "the fixture reports a version");
    assert_eq!(
        probe
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        vec!["fixture-model"]
    );
    // A successful probe remembers the reported version for Settings.
    assert_eq!(
        agents.list()[0].version.as_deref(),
        probe.version.as_deref()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_generic_agent_runs_a_prompt_and_a_tool_call() {
    let workspace = tempfile::tempdir().expect("workspace");
    let Some(manager) = manager().await else {
        return skip_without_fixture();
    };
    let mut events = manager.subscribe();
    let thread = create(&manager, workspace.path()).await;
    let (status, text, _) = next_turn(&mut events, |_| {}).await;
    assert_eq!(status, TurnStatus::Completed);
    assert!(text.contains("echo: hello"), "got {text:?}");

    manager
        .send_message(&thread, "run H4RN_FIXTURE_TOOL".into(), Vec::new())
        .await
        .expect("send");
    let (status, text, tools) = next_turn(&mut events, |_| {}).await;
    assert_eq!(status, TurnStatus::Completed);
    assert!(text.contains("echo: run H4RN_FIXTURE_TOOL"), "got {text:?}");
    assert!(
        tools
            .iter()
            .any(|tool| tool.contains("Run the fixture command:Running")),
        "the generic adapter maps tool calls: {tools:?}"
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.contains("Run the fixture command:Completed")),
        "the completion reaches the same row: {tools:?}"
    );

    manager.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_generic_agent_asks_for_and_receives_permission() {
    let workspace = tempfile::tempdir().expect("workspace");
    let Some(manager) = manager().await else {
        return skip_without_fixture();
    };
    let mut events = manager.subscribe();
    let thread = create(&manager, workspace.path()).await;
    let (status, _, _) = next_turn(&mut events, |_| {}).await;
    assert_eq!(status, TurnStatus::Completed);

    manager
        .send_message(&thread, "ask H4RN_FIXTURE_PERMISSION".into(), Vec::new())
        .await
        .expect("send");

    // Answer the hand from inside the pump, then keep draining the same turn.
    let (status, text, tools) = {
        let mut answered = false;
        let mut approvals = Vec::new();
        let outcome = tokio::time::timeout(TURN_TIMEOUT, async {
            let mut text = String::new();
            let mut tools = Vec::new();
            loop {
                let wire = events.recv().await.expect("event");
                match wire.event {
                    Event::MessageDelta { text: delta, .. } => text.push_str(&delta),
                    Event::ToolCall { tool_call, .. } => {
                        tools.push(format!("{}:{:?}", tool_call.name, tool_call.status));
                    }
                    Event::ApprovalRequested {
                        approval_id, kind, ..
                    } => {
                        approvals.push(format!("{kind:?}"));
                        if answered {
                            continue;
                        }
                        answered = true;
                        manager
                            .respond_approval(
                                &thread,
                                &approval_id,
                                ApprovalDecision::Approve,
                                Some("allow".into()),
                                None,
                            )
                            .await
                            .expect("answer the approval");
                    }
                    Event::TurnCompleted { status, .. } => {
                        return (status, text, tools);
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("the turn ended");
        assert_eq!(approvals, vec!["Command"], "one command approval");
        outcome
    };

    assert_eq!(status, TurnStatus::Completed);
    assert!(
        text.contains("permission answered"),
        "the agent continued after the answer: {text:?}"
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.contains("Run the fixture command")),
        "the approval names the tool call: {tools:?}"
    );

    manager.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_generic_agents_turn_can_be_cancelled() {
    let workspace = tempfile::tempdir().expect("workspace");
    let Some(manager) = manager().await else {
        return skip_without_fixture();
    };
    let mut events = manager.subscribe();
    let thread = create(&manager, workspace.path()).await;
    let (status, _, _) = next_turn(&mut events, |_| {}).await;
    assert_eq!(status, TurnStatus::Completed);

    manager
        .send_message(&thread, "hold H4RN_FIXTURE_CANCEL".into(), Vec::new())
        .await
        .expect("send");
    manager.interrupt(&thread).await.expect("interrupt");

    let (status, _, _) = next_turn(&mut events, |_| {}).await;
    assert_eq!(status, TurnStatus::Interrupted);

    // The same agent process keeps the session for the next prompt.
    manager
        .send_message(&thread, "after cancel".into(), Vec::new())
        .await
        .expect("send");
    let (status, text, _) = next_turn(&mut events, |_| {}).await;
    assert_eq!(status, TurnStatus::Completed);
    assert!(text.contains("echo: after cancel"), "got {text:?}");

    manager.shutdown().await;
}
