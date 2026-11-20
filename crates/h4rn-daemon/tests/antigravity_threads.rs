//! End-to-end: the Antigravity ACP adapter and the thread manager together.
//!
//! The agent is the `agy-fixture` binary another package builds: a minimal ACP
//! peer that echoes prompts and can vanish mid-turn. That is the one thing the
//! daemon-level fake cannot model, because only a real transport reports its
//! own death through the adapter.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use h4rn_daemon::harnesses::{HarnessCatalog, StaticHarnessCatalog};
use h4rn_daemon::journal::{SqliteJournal, WireEvent};
use h4rn_daemon::threads::{NewThread, ThreadManager, ThreadManagerOptions};
use h4rn_harness::{AntigravityHarness, HarnessInstall};
use h4rn_protocol::{Event, HarnessKind, HostId, RuntimeMode, ThreadId, TurnId, TurnStatus};
use tokio::sync::broadcast;

const TURN_TIMEOUT: Duration = Duration::from_secs(60);

/// The fixture is another package's binary, so build it on demand next to the
/// test binaries instead of relying on `CARGO_BIN_EXE_*`.
fn fixture_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("test exe");
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("target profile dir");
    let path = profile_dir.join(if cfg!(windows) {
        "agy-fixture.exe"
    } else {
        "agy-fixture"
    });
    if !path.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "-p", "h4rn-harness", "--bin", "agy-fixture"])
            .status()
            .expect("run cargo build for the agy fixture");
        assert!(status.success(), "building the agy fixture failed");
    }
    assert!(
        path.exists(),
        "missing fixture binary at {}",
        path.display()
    );
    path
}

async fn manager() -> ThreadManager {
    let harness = AntigravityHarness::with_install(HarnessInstall {
        path: fixture_binary(),
        version: "fixture".into(),
    });
    let catalog = StaticHarnessCatalog::new().and(Arc::new(harness));
    assert!(catalog.get(HarnessKind::Antigravity).is_some());
    ThreadManager::open(
        Arc::new(SqliteJournal::in_memory().await.expect("journal")),
        Arc::new(catalog),
        HostId::new("host_test"),
        ThreadManagerOptions {
            // The manager only reacts to this test's own calls.
            tick_interval: Duration::from_secs(3600),
            ..ThreadManagerOptions::default()
        },
        None,
    )
    .await
    .expect("manager")
}

fn new_thread(host: &HostId, workspace: &std::path::Path, prompt: &str) -> NewThread {
    NewThread {
        thread_id: None,
        parent_thread_id: None,
        subagent_id: None,
        host: host.clone(),
        harness: HarnessKind::Antigravity,
        workspace_path: workspace.display().to_string(),
        model: "fixture-model".into(),
        prompt: prompt.into(),
        attachments: Vec::new(),
        runtime_mode: RuntimeMode::ApprovalRequired,
        worktree: None,
    }
}

/// One turn's observable outcome.
struct TurnOutcome {
    turn_id: TurnId,
    status: TurnStatus,
    errors: Vec<String>,
    text: String,
}

async fn next_turn(events: &mut broadcast::Receiver<WireEvent>) -> TurnOutcome {
    tokio::time::timeout(TURN_TIMEOUT, async {
        let mut errors = Vec::new();
        let mut text = String::new();
        loop {
            let Ok(wire) = events.recv().await else {
                panic!("the event stream closed before the turn ended");
            };
            match wire.event {
                Event::Error { message, .. } => errors.push(message),
                Event::MessageDelta { text: delta, .. } => text.push_str(&delta),
                Event::TurnCompleted {
                    turn_id, status, ..
                } => {
                    return TurnOutcome {
                        turn_id,
                        status,
                        errors,
                        text,
                    };
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the turn ended in time")
}

fn session_starts(workspace: &std::path::Path) -> usize {
    std::fs::read_to_string(workspace.join(".agy-fixture-sessions"))
        .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
        .unwrap_or(0)
}

/// A closed ACP transport is detached before the next turn is promoted, so the
/// queued message starts a fresh agent instead of being written into a closed
/// client.
#[tokio::test]
async fn a_closed_acp_transport_is_replaced_before_the_next_turn() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let manager = manager().await;
    let mut events = manager.subscribe();
    let host = HostId::new("host_test");

    let created = manager
        .create_thread(new_thread(&host, workspace.path(), "first"))
        .await
        .expect("create");
    let first = next_turn(&mut events).await;
    assert_eq!(first.turn_id, created.turn_id);
    assert_eq!(first.status, TurnStatus::Completed, "{:?}", first.errors);
    assert!(first.text.contains("echo: first"), "got {:?}", first.text);
    assert_eq!(session_starts(workspace.path()), 1, "one agent process");

    // This prompt makes the fixture exit without answering: the adapter sees a
    // closed transport, the daemon fails the turn once, and it detaches.
    let thread_id: ThreadId = created.thread_id.clone();
    manager
        .send_message(&thread_id, "please H4RN_FIXTURE_EXIT".into(), Vec::new())
        .await
        .expect("send");
    let failed = next_turn(&mut events).await;
    assert_eq!(failed.status, TurnStatus::Failed);
    assert_eq!(
        failed.errors.len(),
        1,
        "one error row for one dead transport: {:?}",
        failed.errors
    );

    // The next message must not be written into the closed client: it starts a
    // fresh process rather than reporting another transport failure.
    manager
        .send_message(&thread_id, "second".into(), Vec::new())
        .await
        .expect("send");
    let second = next_turn(&mut events).await;
    assert_ne!(second.turn_id, failed.turn_id);
    assert_eq!(second.status, TurnStatus::Completed, "{:?}", second.errors);
    assert!(
        second.text.contains("echo: second"),
        "got {:?}",
        second.text
    );
    assert_eq!(
        session_starts(workspace.path()),
        2,
        "the next turn started a fresh agent process"
    );

    manager.shutdown().await;
}
