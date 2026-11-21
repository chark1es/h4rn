//! End-to-end: the Antigravity ACP adapter and the thread manager together.
//!
//! The agent is the `agy-fixture` binary from the `h4rn-test-fixtures` crate: a
//! minimal ACP peer that echoes prompts and can vanish mid-turn. That is the one
//! thing the daemon-level fake cannot model, because only a real transport
//! reports its own death through the adapter.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use h4rn_daemon::harnesses::{HarnessCatalog, StaticHarnessCatalog};
use h4rn_daemon::journal::{SqliteJournal, WireEvent};
use h4rn_daemon::threads::{NewThread, ThreadManager, ThreadManagerOptions};
use h4rn_harness::{AntigravityHarness, HarnessInstall};
use h4rn_protocol::{Event, HarnessKind, HostId, RuntimeMode, ThreadId, TurnId, TurnStatus};
use tokio::sync::broadcast;

const TURN_TIMEOUT: Duration = Duration::from_secs(60);

/// The fixture peer, built on demand from its own test-only crate.
///
/// The fixture is deliberately not a binary of this package: that would make
/// `cargo run -p h4rn-daemon` ambiguous and ship a test peer with every build
/// and install. Cargo builds it into the test's own target directory and
/// prints the executable path in its JSON output, so nothing here guesses a
/// profile or a target layout.
fn fixture_binary() -> Option<PathBuf> {
    static BINARY: OnceLock<Option<PathBuf>> = OnceLock::new();
    BINARY.get_or_init(build_fixture).clone()
}

/// The message a test prints when the fixture cannot be built.
///
/// A checkout without Cargo on `PATH` (or with a broken build) skips the
/// end-to-end tests instead of failing them: the fixture is a test aid, not a
/// product binary, and a missing one says nothing about the daemon.
fn skip_without_fixture() {
    eprintln!(
        "skipping: the ACP fixture could not be built (cargo is unavailable on PATH or the build failed)"
    );
}

/// A build directory owned by this test process.
///
/// A shared directory would let another checkout on the same machine rebuild or
/// replace the fixture between this process's path lookup and its launch, so the
/// binary the test runs might not be the one it built. Cargo's per-target test
/// tmp directory is unique to this checkout's target dir; when it is not set
/// (a non-Cargo runner), a process-owned temporary directory keeps the same
/// guarantee and lives exactly as long as the test binary.
fn fixture_target_dir() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    static OWNED: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| match std::env::var_os("CARGO_TARGET_TMPDIR") {
        Some(dir) => {
            // Cargo's tmp directory belongs to one target dir; the checkout is
            // part of the name too, so two checkouts that share a target dir
            // still cannot rebuild each other's fixture.
            use std::hash::{DefaultHasher, Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            env!("CARGO_MANIFEST_DIR").hash(&mut hasher);
            let checkout = hasher.finish();
            PathBuf::from(dir).join(format!("agy-fixture-build-{checkout:016x}"))
        }
        None => OWNED
            .get_or_init(|| tempfile::tempdir().expect("a temporary build directory"))
            .path()
            .to_path_buf(),
    })
}

fn build_fixture() -> Option<PathBuf> {
    let target_dir = fixture_target_dir().clone();
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
        .arg(&target_dir)
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
    eprintln!(
        "skipping: cargo did not report the ACP fixture's executable: {}",
        stdout.lines().take(5).collect::<Vec<_>>().join("\n")
    );
    None
}

async fn manager() -> Option<ThreadManager> {
    let harness = AntigravityHarness::with_install(HarnessInstall {
        path: fixture_binary()?,
        version: "fixture".into(),
    });
    let catalog = StaticHarnessCatalog::new().and(Arc::new(harness));
    assert!(catalog.get(HarnessKind::Antigravity).is_some());
    let manager = ThreadManager::open(
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
    .expect("manager");
    // Attach a meter so a limit hit really arms and persists an auto-resume
    // schedule, as it does in the daemon.
    manager.set_meter(Arc::new(
        h4rn_meter::MeterHub::in_memory().await.expect("meter"),
    ));
    Some(manager)
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
    /// Usage limits reported during the turn: (has a reset time, auto-resume).
    limits: Vec<(bool, bool)>,
}

async fn next_turn(events: &mut broadcast::Receiver<WireEvent>) -> TurnOutcome {
    tokio::time::timeout(TURN_TIMEOUT, async {
        let mut errors = Vec::new();
        let mut text = String::new();
        let mut limits = Vec::new();
        loop {
            let Ok(wire) = events.recv().await else {
                panic!("the event stream closed before the turn ended");
            };
            match wire.event {
                Event::Error { message, .. } => errors.push(message),
                Event::MessageDelta { text: delta, .. } => text.push_str(&delta),
                Event::UsageLimitHit {
                    reset_at,
                    auto_resume,
                    ..
                } => limits.push((reset_at.is_some(), auto_resume)),
                Event::TurnCompleted {
                    turn_id, status, ..
                } => {
                    return TurnOutcome {
                        turn_id,
                        status,
                        errors,
                        text,
                        limits,
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
#[cfg(unix)]
#[tokio::test]
async fn a_closed_acp_transport_is_replaced_before_the_next_turn() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let Some(manager) = manager().await else {
        return skip_without_fixture();
    };
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
    // closed transport, the daemon fails the turn once, and it detaches. The
    // fixture holds its death until the gate file appears, so the follow-up
    // below can be queued while the transport is still alive.
    let thread_id: ThreadId = created.thread_id.clone();
    manager
        .send_message(&thread_id, "please H4RN_FIXTURE_EXIT".into(), Vec::new())
        .await
        .expect("send");
    let waiting = workspace.path().join(".agy-fixture-exit-waiting");
    tokio::time::timeout(TURN_TIMEOUT, async {
        while !waiting.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the fixture reached its exit gate before the turn ended");

    // The follow-up queues behind the dying turn, not behind a fresh one.
    let queued = manager
        .send_message(&thread_id, "second".into(), Vec::new())
        .await
        .expect("send");
    std::fs::write(workspace.path().join(".agy-fixture-exit-gate"), b"")
        .expect("release the fixture's exit gate");

    let failed = next_turn(&mut events).await;
    assert_eq!(failed.turn_id, queued.turn_id, "the dying turn is A");
    assert_eq!(failed.status, TurnStatus::Failed);
    assert_eq!(
        failed.errors.len(),
        1,
        "one error row for one dead transport: {:?}",
        failed.errors
    );
    assert!(
        failed.errors[0].contains("agy-fixture: the session died mid-turn"),
        "the failure row keeps the agent's stderr detail: {:?}",
        failed.errors
    );

    // The queued message must not be written into the closed client: it runs
    // on a fresh process instead.
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
        "the queued turn started a fresh agent process"
    );

    manager.shutdown().await;
}

/// An agent that leaves a diagnostic on stderr and closes its stdin before the
/// next prompt is flushed leaves one failure row, and that row names both the
/// writer's I/O error and the agent's own stderr line.
///
/// The fixture prints the line at the moment it closes stdin, not at the
/// handshake: the row must wait for the stderr drain rather than rely on an
/// early-buffered diagnostic.
#[cfg(unix)]
#[tokio::test]
async fn a_closed_stdin_keeps_both_the_io_error_and_the_stderr_detail() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let Some(manager) = manager().await else {
        return skip_without_fixture();
    };
    let mut events = manager.subscribe();
    let host = HostId::new("host_test");

    let created = manager
        .create_thread(new_thread(&host, workspace.path(), "first"))
        .await
        .expect("create");
    let first = next_turn(&mut events).await;
    assert_eq!(first.status, TurnStatus::Completed, "{:?}", first.errors);
    assert_eq!(session_starts(workspace.path()), 1);

    // This turn is answered, then the fixture closes its own stdin and keeps
    // running: the next prompt is never read.
    let thread_id: ThreadId = created.thread_id.clone();
    manager
        .send_message(
            &thread_id,
            "please H4RN_FIXTURE_CLOSE_STDIN".into(),
            Vec::new(),
        )
        .await
        .expect("send");
    let closing = next_turn(&mut events).await;
    assert_eq!(
        closing.status,
        TurnStatus::Completed,
        "{:?}",
        closing.errors
    );
    let closed = workspace.path().join(".agy-fixture-stdin-closed");
    tokio::time::timeout(TURN_TIMEOUT, async {
        while !closed.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the fixture closed its stdin");

    let failed_write = manager
        .send_message(&thread_id, "third".into(), Vec::new())
        .await
        .expect("send");
    let failed = next_turn(&mut events).await;
    assert_eq!(failed.turn_id, failed_write.turn_id);
    assert_eq!(failed.status, TurnStatus::Failed);
    assert_eq!(
        failed.errors.len(),
        1,
        "one row for the failed write: {:?}",
        failed.errors
    );
    let row = &failed.errors[0];
    assert!(
        row.contains("Broken pipe") || row.contains("os error 32"),
        "the row keeps the writer's I/O error: {row}"
    );
    assert!(
        row.contains("agy-fixture: closing stdin before the next prompt"),
        "the row keeps the agent's stderr line: {row}"
    );

    manager.shutdown().await;
}

/// The process closes stdout first, then writes its only diagnostic to stderr
/// and exits. The daemon's reader wakes on the EOF before the exit report
/// exists, so the failure row can name the line only if the prompt path waits
/// for the report (and its drained stderr).
#[cfg(unix)]
#[tokio::test]
async fn an_exit_report_after_the_stdout_eof_still_names_the_stderr() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let Some(manager) = manager().await else {
        return skip_without_fixture();
    };
    let mut events = manager.subscribe();
    let host = HostId::new("host_test");

    let created = manager
        .create_thread(new_thread(&host, workspace.path(), "first"))
        .await
        .expect("create");
    let first = next_turn(&mut events).await;
    assert_eq!(first.status, TurnStatus::Completed, "{:?}", first.errors);

    let thread_id: ThreadId = created.thread_id.clone();
    manager
        .send_message(
            &thread_id,
            "please H4RN_FIXTURE_CLOSE_STDOUT".into(),
            Vec::new(),
        )
        .await
        .expect("send");
    let failed = next_turn(&mut events).await;
    assert_eq!(failed.status, TurnStatus::Failed);
    assert_eq!(
        failed.errors.len(),
        1,
        "one row for one dead transport: {:?}",
        failed.errors
    );
    assert!(
        failed.errors[0].contains("agy-fixture: the session died after stdout closed"),
        "the row waits for the exit report and keeps its stderr: {:?}",
        failed.errors
    );

    manager.shutdown().await;
}

/// A rate-limit reset that only the exit report carries still reaches the
/// daemon's limit handling: the turn reports a limit with a reset time and
/// auto-resume, exactly as it does when the report wins the race on its own.
#[cfg(unix)]
#[tokio::test]
async fn an_exit_report_after_the_stdout_eof_still_schedules_the_resume() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let Some(manager) = manager().await else {
        return skip_without_fixture();
    };
    let mut events = manager.subscribe();
    let host = HostId::new("host_test");

    let created = manager
        .create_thread(new_thread(&host, workspace.path(), "first"))
        .await
        .expect("create");
    let first = next_turn(&mut events).await;
    assert_eq!(first.status, TurnStatus::Completed, "{:?}", first.errors);

    let thread_id: ThreadId = created.thread_id.clone();
    manager
        .send_message(
            &thread_id,
            "please H4RN_FIXTURE_RATE_LIMITED".into(),
            Vec::new(),
        )
        .await
        .expect("send");
    let failed = next_turn(&mut events).await;
    assert!(
        failed.status == TurnStatus::Failed,
        "the turn failed at its end: {:?}",
        failed.status
    );
    assert!(
        failed.limits.iter().any(|(reset, auto)| *reset && *auto),
        "the exit report's rate limit reached the auto-resume path: {:?}",
        failed.limits
    );
    let meter = manager.meter().expect("the test manager has a meter");
    let schedules = meter.schedules().await.expect("read schedules");
    assert_eq!(schedules.len(), 1, "one persisted schedule: {schedules:?}");
    assert_eq!(schedules[0].thread_id, thread_id);
    assert_eq!(
        schedules[0].state,
        h4rn_protocol::ResumeState::Waiting,
        "a reset beyond the policy's max wait is retried at the cap: {schedules:?}"
    );

    manager.shutdown().await;
}

/// A prompt whose write fails before it reaches the agent's stdin still
/// carries the rate-limit reset the agent printed on stderr: the turn reports a
/// limit with a reset time and auto-resume, the schedule is persisted, and the
/// message queued behind the failed write is not promoted past the pause.
#[cfg(unix)]
#[tokio::test]
async fn a_pre_flush_write_failure_still_schedules_the_resume() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let Some(manager) = manager().await else {
        return skip_without_fixture();
    };
    let mut events = manager.subscribe();
    let host = HostId::new("host_test");

    let created = manager
        .create_thread(new_thread(&host, workspace.path(), "first"))
        .await
        .expect("create");
    let first = next_turn(&mut events).await;
    assert_eq!(first.status, TurnStatus::Completed, "{:?}", first.errors);

    // This turn is answered, then the fixture prints its reset and closes its
    // own stdin while staying alive: the next prompt never reaches it.
    let thread_id: ThreadId = created.thread_id.clone();
    manager
        .send_message(
            &thread_id,
            "please H4RN_FIXTURE_STDIN_RATE_LIMITED".into(),
            Vec::new(),
        )
        .await
        .expect("send");
    let closing = next_turn(&mut events).await;
    assert_eq!(
        closing.status,
        TurnStatus::Completed,
        "{:?}",
        closing.errors
    );
    let closed = workspace.path().join(".agy-fixture-stdin-closed");
    tokio::time::timeout(TURN_TIMEOUT, async {
        while !closed.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the fixture closed its stdin");

    let failed_write = manager
        .send_message(&thread_id, "second".into(), Vec::new())
        .await
        .expect("send");
    let queued = manager
        .send_message(&thread_id, "third".into(), Vec::new())
        .await
        .expect("send");
    let failed = next_turn(&mut events).await;
    assert_eq!(failed.turn_id, failed_write.turn_id);
    assert_eq!(failed.status, TurnStatus::Failed);
    assert!(
        failed.limits.iter().any(|(reset, auto)| *reset && *auto),
        "the failed write's stderr carried the rate limit: {:?}",
        failed.limits
    );

    let meter = manager.meter().expect("the test manager has a meter");
    let schedules = meter.schedules().await.expect("read schedules");
    assert_eq!(schedules.len(), 1, "one persisted schedule: {schedules:?}");
    assert_eq!(schedules[0].thread_id, thread_id);

    // The thread is paused by the limit, so the queued message must stay
    // queued: promoting it would start a fresh agent at once.
    assert_eq!(
        session_starts(workspace.path()),
        1,
        "the queued turn did not start a new agent"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(250), next_turn(&mut events))
            .await
            .is_err(),
        "the queued turn ({}) started past a usage limit",
        queued.turn_id
    );

    manager.shutdown().await;
}
