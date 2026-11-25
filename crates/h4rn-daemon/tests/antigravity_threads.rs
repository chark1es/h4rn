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
use h4rn_protocol::{
    AgentState, Event, HarnessKind, HostId, RuntimeMode, ThreadId, TurnId, TurnStatus,
};
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
        agent_id: None,
        workspace_path: workspace.display().to_string(),
        model: "fixture-model".into(),
        prompt: prompt.into(),
        attachments: Vec::new(),
        runtime_mode: RuntimeMode::ApprovalRequired,
        reasoning_effort: None,
        fast: false,
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

/// Waits, bounded, for the thread to show the schedule the limit armed.
///
/// The limit can land after the turn completion (the exit report races it), so
/// reading the summary once would be a guess instead of an assertion.
async fn wait_for_scheduled(
    manager: &ThreadManager,
    thread_id: &ThreadId,
) -> h4rn_protocol::ThreadSummary {
    tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            let summary = manager
                .thread_summary(thread_id)
                .await
                .expect("thread summary");
            if summary.agent_state == AgentState::Scheduled {
                return summary;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the limit turned into a scheduled resume")
}

/// Waits, bounded, for a fixture marker file to appear.
async fn wait_for_path(path: &std::path::Path, what: &str) {
    tokio::time::timeout(TURN_TIMEOUT, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
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
    let dying = manager
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

    // The follow-up queues behind the dying turn, not behind a fresh one: it
    // has a queue row, not a turn of its own, until that turn settles.
    let queued = manager
        .send_message(&thread_id, "second".into(), Vec::new())
        .await
        .expect("send");
    assert!(
        queued.queued_message_id.is_some(),
        "the follow-up waits in the queue"
    );
    assert_eq!(
        queued.turn_id, dying.turn_id,
        "a queued message is not a turn of its own"
    );
    std::fs::write(workspace.path().join(".agy-fixture-exit-gate"), b"")
        .expect("release the fixture's exit gate");

    let failed = next_turn(&mut events).await;
    assert_eq!(failed.turn_id, dying.turn_id, "the dying turn is A");
    assert_eq!(
        queued.turn_id, dying.turn_id,
        "the follow-up waits behind A and has no turn yet"
    );
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

/// The process closes stdout and then exits without writing any stderr. The
/// exit status is shorter than the generic stdout-close notice, but it is the
/// specific account of the death and must name the row.
#[cfg(unix)]
#[tokio::test]
async fn a_stderrless_exit_report_still_names_the_exit_status() {
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
            "please H4RN_FIXTURE_SILENT_EXIT".into(),
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
    let row = &failed.errors[0];
    assert!(
        row.starts_with("agy exited ("),
        "the exit status names the row: {row}"
    );
    assert!(
        !row.contains("stdout closed"),
        "the generic close notice did not fold the exit report away: {row}"
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

/// A limit that only the exit report carries after `end_turn` still pauses the
/// thread when the reply is an ordinary newline-terminated frame, as a real
/// peer writes it.
///
/// The peer answers and exits at once; its exit closes stdout, so the process
/// can be reaped before the reader has drained the reply already in the pipe.
/// The reply must still settle the turn as completed, and a message queued
/// behind it must not be failed for a death it never touched: the promoted turn
/// goes back to the pause instead of failing, and no fresh agent starts into
/// the provider that refused it.
///
/// The prompt gate is closed until the follow-up has been queued, so the
/// message really is queued behind the turn rather than opened as a new one.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_limit_only_the_exit_report_carries_pauses_after_end_turn() {
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

    // The marker turn is answered, then its stdout closes; the reset is only on
    // the exit report that follows. The follow-up queues behind the turn.
    let thread_id: ThreadId = created.thread_id.clone();
    manager
        .send_message(
            &thread_id,
            "please H4RN_FIXTURE_LIMIT_EXIT_AFTER_TURN".into(),
            Vec::new(),
        )
        .await
        .expect("send");
    let queued = manager
        .send_message(&thread_id, "queued".into(), Vec::new())
        .await
        .expect("queue");
    // Let the turn answer and exit at once. The prompt gate keeps the answer
    // held until the follow-up was queued, so the promotion is guaranteed to
    // happen while the reply and the death race; the exit then closes stdout
    // the way a real peer's exit does.
    std::fs::write(
        workspace
            .path()
            .join(".agy-fixture-limit-after-turn-prompt-gate"),
        b"",
    )
    .expect("let the fixture answer");

    let ended = next_turn(&mut events).await;
    assert_eq!(ended.status, TurnStatus::Completed, "{:?}", ended.errors);
    assert!(
        ended.text.contains("echo:"),
        "the reply that was in the pipe before the exit is the turn's result: {:?}",
        ended.text
    );

    // Whatever order the completion and the report took, the thread ends
    // scheduled on the exit report's limit.
    let scheduled = wait_for_scheduled(&manager, &thread_id).await;
    let meter = manager.meter().expect("the test manager has a meter");
    let schedules = meter.schedules().await.expect("read schedules");
    assert_eq!(schedules.len(), 1, "one persisted schedule: {schedules:?}");
    assert_eq!(schedules[0].thread_id, thread_id);
    assert_eq!(
        scheduled.agent_state,
        AgentState::Scheduled,
        "the thread waits for the limit's reset"
    );

    // The queued turn was not failed for a death it never touched: it waits for
    // the schedule and did not start a fresh agent into the paused provider.
    assert_eq!(
        session_starts(workspace.path()),
        1,
        "the queued turn did not start a new agent"
    );
    // The pump gives a closed completion one bounded window for the exit
    // report, so a failure would be published inside (500 ms) plus the
    // projection's own step. Watch past it: a completion of the queued turn,
    // or a thread that fell out of Scheduled, is exactly the bug.
    assert!(
        tokio::time::timeout(Duration::from_millis(800), next_turn(&mut events))
            .await
            .is_err(),
        "the queued turn ({}) was failed or promoted past a usage limit",
        queued.turn_id
    );
    let summary = manager
        .thread_summary(&thread_id)
        .await
        .expect("thread summary");
    assert_eq!(
        summary.agent_state,
        AgentState::Scheduled,
        "the queued turn is still the thread's parked work"
    );

    manager.shutdown().await;
}

/// The harder ordering, forced with a gate: the queued message is promoted and
/// written while the transport is still healthy, the agent acts on it (an edit
/// it never reports), and only then does it close stdout and exit with the
/// limit on stderr.
///
/// The agent accepted the prompt, so the turn settles as limited and keeps its
/// trace; it is never handed back to the pause. A reset must not send that
/// prompt again: it may already have made the edit, and the person can resend.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_limit_after_a_promoted_turn_the_agent_read_settles_and_is_not_re_sent() {
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
            "please H4RN_FIXTURE_LIMIT_EXIT_AFTER_QUEUE".into(),
            Vec::new(),
        )
        .await
        .expect("send");
    let _queued = manager
        .send_message(&thread_id, "queued".into(), Vec::new())
        .await
        .expect("queue");
    // The turn answers only now, so the follow-up above was queued behind it.
    std::fs::write(
        workspace
            .path()
            .join(".agy-fixture-limit-after-queue-prompt-gate"),
        b"",
    )
    .expect("let the fixture answer");
    // The fixture writes this marker once the promoted prompt reached it: the
    // queue was promoted and written while the transport was still healthy.
    wait_for_path(
        &workspace
            .path()
            .join(".agy-fixture-limit-after-queue-second"),
        "the promoted prompt reached the agent",
    )
    .await;
    // The agent acted on the prompt without reporting a tool call, exactly what
    // the daemon cannot see: acceptance is the only proof it may have worked.
    wait_for_path(
        &workspace.path().join(".agy-fixture-limit-after-queue-edit"),
        "the agent acted on the promoted prompt",
    )
    .await;
    // Only now does the agent close stdout and exit with the reset.
    std::fs::write(
        workspace.path().join(".agy-fixture-limit-after-queue-gate"),
        b"",
    )
    .expect("let the fixture exit");

    let ended = next_turn(&mut events).await;
    assert_eq!(ended.status, TurnStatus::Completed, "{:?}", ended.errors);

    // The promoted turn is the third one; it settles as limited.
    let limited = next_turn(&mut events).await;
    assert_ne!(limited.turn_id, first.turn_id);
    assert_ne!(limited.turn_id, ended.turn_id);
    assert_eq!(
        limited.status,
        TurnStatus::Failed,
        "an accepted prompt settles as limited; it is not re-sent: {:?}",
        limited.errors
    );
    assert!(
        !limited.limits.is_empty(),
        "the exit report's limit was never published: {:?}",
        limited.errors
    );

    let scheduled = wait_for_scheduled(&manager, &thread_id).await;
    let meter = manager.meter().expect("the test manager has a meter");
    let schedules = meter.schedules().await.expect("read schedules");
    assert_eq!(schedules.len(), 1, "one persisted schedule: {schedules:?}");
    assert_eq!(schedules[0].thread_id, thread_id);
    assert_eq!(scheduled.agent_state, AgentState::Scheduled);

    assert_eq!(
        session_starts(workspace.path()),
        1,
        "the accepted prompt started a second agent instead of settling"
    );
    let meta = manager.thread_meta(&thread_id).await.expect("thread meta");
    assert!(
        meta.parked.is_none(),
        "the accepted prompt was parked for a second run"
    );

    // The reset fires the thread's resume, and the agent is never sent the
    // queued prompt again. The fixture echoes every prompt it reads, so a
    // repeat would show up in the resumed turn's text.
    manager
        .resume_now(&thread_id)
        .await
        .expect("the surviving schedule fires");
    let resumed = next_turn(&mut events).await;
    assert!(
        !resumed.text.contains("queued"),
        "the accepted prompt was sent again at reset: {:?}",
        resumed.text
    );

    manager.shutdown().await;
}

/// An idle death whose exit report carries the reset keeps the thread
/// Scheduled: the diagnostic row is published without flipping the thread to
/// Error, so its armed auto-resume stays in place.
#[cfg(unix)]
#[tokio::test]
async fn an_idle_limit_exit_keeps_the_thread_scheduled() {
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

    // The marker turn is answered while the transport is healthy; the death
    // happens only after the test has seen that completion.
    let thread_id: ThreadId = created.thread_id.clone();
    manager
        .send_message(
            &thread_id,
            "please H4RN_FIXTURE_LIMIT_EXIT_BETWEEN_TURNS".into(),
            Vec::new(),
        )
        .await
        .expect("send");
    let answered = next_turn(&mut events).await;
    assert_eq!(
        answered.status,
        TurnStatus::Completed,
        "{:?}",
        answered.errors
    );
    std::fs::write(workspace.path().join(".agy-fixture-limit-idle-gate"), b"")
        .expect("let the idle agent close and exit");

    let row = tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            let Ok(wire) = events.recv().await else {
                panic!("the event stream closed before the idle death was reported");
            };
            if let Event::Error { message, .. } = wire.event {
                return message;
            }
        }
    })
    .await
    .expect("the idle death is published without another Send");
    assert!(
        row.starts_with("agy exited ("),
        "the row is the exit report, not the generic close: {row}"
    );
    assert!(
        row.contains("rate limit reached"),
        "the row keeps the reset the exit report carried: {row}"
    );

    let meter = manager.meter().expect("the test manager has a meter");
    let schedules = meter.schedules().await.expect("read schedules");
    assert_eq!(schedules.len(), 1, "one persisted schedule: {schedules:?}");
    assert_eq!(schedules[0].thread_id, thread_id);
    let summary = manager
        .thread_summary(&thread_id)
        .await
        .expect("thread summary");
    assert_eq!(
        summary.agent_state,
        AgentState::Scheduled,
        "the idle row did not turn the armed resume into an Error"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                let Ok(wire) = events.recv().await else {
                    return None;
                };
                if let Event::Error { message, .. } = wire.event {
                    return Some(message);
                }
            }
        })
        .await
        .is_err(),
        "one idle death is one diagnostic row"
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

/// An agent that answers a turn and then exits is a death between turns. The
/// adapter holds the session, and with it the event sender, so the daemon's
/// stream never ends while it owns the session: the failure row must be
/// published from the exit report itself, with no further Send, and the next
/// message must start a fresh agent instead of writing into the dead one.
#[cfg(unix)]
#[tokio::test]
async fn an_agent_that_exits_between_turns_reports_without_another_send() {
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

    // This turn is answered, then the fixture waits for the test before
    // exiting, so its death really lands between turns.
    let thread_id: ThreadId = created.thread_id.clone();
    manager
        .send_message(
            &thread_id,
            "please H4RN_FIXTURE_DIE_BETWEEN_TURNS".into(),
            Vec::new(),
        )
        .await
        .expect("send");
    let answered = next_turn(&mut events).await;
    assert_eq!(
        answered.status,
        TurnStatus::Completed,
        "{:?}",
        answered.errors
    );

    // The turn ended: let the agent exit. No Send follows, so the row can only
    // come from the idle death itself.
    std::fs::write(
        workspace.path().join(".agy-fixture-between-turns-gate"),
        b"",
    )
    .expect("release the fixture's exit gate");

    let row = tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            let Ok(wire) = events.recv().await else {
                panic!("the event stream closed before the idle death was reported");
            };
            if let Event::Error { message, .. } = wire.event {
                return message;
            }
        }
    })
    .await
    .expect("the idle death is published without another Send");
    assert!(
        row.contains("agy-fixture: the session ended between turns"),
        "the row keeps the agent's stderr: {row}"
    );

    // The dead session was detached: the next message runs on a new agent.
    manager
        .send_message(&thread_id, "next".into(), Vec::new())
        .await
        .expect("send");
    let next = next_turn(&mut events).await;
    assert_eq!(next.status, TurnStatus::Completed, "{:?}", next.errors);
    assert!(
        next.text.contains("echo: next"),
        "the next turn ran on a fresh agent: {:?}",
        next.text
    );
    assert_eq!(
        session_starts(workspace.path()),
        2,
        "the next message started a fresh agent process"
    );

    manager.shutdown().await;
}

/// An idle death that only the process exit can report (a keeper holds stdout
/// open, so there is no EOF) still detaches the dead session and publishes its
/// one row, so the next message starts a fresh agent instead of failing on the
/// dead one.
#[cfg(unix)]
#[tokio::test]
async fn an_idle_death_only_the_exit_reports_detaches_before_the_next_send() {
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

    // Answer this turn while the transport is healthy; the death follows the
    // test's gate, between turns.
    let thread_id: ThreadId = created.thread_id.clone();
    manager
        .send_message(
            &thread_id,
            "please H4RN_FIXTURE_IDLE_EXIT_ONLY".into(),
            Vec::new(),
        )
        .await
        .expect("send");
    let answered = next_turn(&mut events).await;
    assert_eq!(
        answered.status,
        TurnStatus::Completed,
        "{:?}",
        answered.errors
    );
    std::fs::write(workspace.path().join(".agy-fixture-idle-exit-gate"), b"")
        .expect("let the idle agent exit");

    let row = tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            let Ok(wire) = events.recv().await else {
                panic!("the event stream closed before the idle death was reported");
            };
            if let Event::Error { message, .. } = wire.event {
                return message;
            }
        }
    })
    .await
    .expect("the exit report alone publishes the idle death");
    assert!(
        row.starts_with("agy exited ("),
        "the row is the exit report: {row}"
    );
    assert!(
        row.contains("rate limit reached"),
        "the row keeps the reset the exit report carried: {row}"
    );

    // The dead session was detached before the row was published, so this Send
    // must start a fresh agent instead of writing into the dead one.
    manager
        .send_message(&thread_id, "next".into(), Vec::new())
        .await
        .expect("send");
    let next = next_turn(&mut events).await;
    assert_eq!(next.status, TurnStatus::Completed, "{:?}", next.errors);
    assert!(
        next.text.contains("echo: next"),
        "the next turn ran on a fresh agent: {:?}",
        next.text
    );
    assert_eq!(
        session_starts(workspace.path()),
        2,
        "the next message started a fresh agent process"
    );

    manager.shutdown().await;
}

/// An agent that stops reading stdin after a turn turns the next, larger prompt
/// into a write-deadline failure. The reset it printed on stderr is the typed
/// limit the daemon persists, and the message queued behind that write waits
/// instead of starting a fresh agent and running into the limit again.
#[cfg(unix)]
#[tokio::test]
async fn a_stalled_agent_turns_a_large_prompt_into_a_persisted_limit() {
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

    // This turn is answered, then the fixture prints a reset on stderr and
    // stops reading stdin while staying alive.
    let thread_id: ThreadId = created.thread_id.clone();
    manager
        .send_message(
            &thread_id,
            "please H4RN_FIXTURE_STALL_STDIN".into(),
            Vec::new(),
        )
        .await
        .expect("send");
    let stalled = next_turn(&mut events).await;
    assert_eq!(
        stalled.status,
        TurnStatus::Completed,
        "{:?}",
        stalled.errors
    );

    // Larger than any pipe buffer, so the agent that stopped reading leaves it
    // unflushed until the adapter's write deadline tears the transport down.
    let large = "x".repeat(512 * 1024);
    let large_turn = manager
        .send_message(&thread_id, format!("large {large}"), Vec::new())
        .await
        .expect("send");
    let queued = manager
        .send_message(&thread_id, "queued".into(), Vec::new())
        .await
        .expect("send");

    let failed = next_turn(&mut events).await;
    assert_eq!(failed.turn_id, large_turn.turn_id);
    assert_eq!(failed.status, TurnStatus::Failed, "{:?}", failed.errors);
    assert!(
        failed.limits.iter().any(|(reset, auto)| *reset && *auto),
        "the stalled write's stderr carried the rate limit: {:?}",
        failed.limits
    );

    let meter = manager.meter().expect("the test manager has a meter");
    let schedules = meter.schedules().await.expect("read schedules");
    assert_eq!(schedules.len(), 1, "one persisted schedule: {schedules:?}");
    assert_eq!(schedules[0].thread_id, thread_id);
    assert_eq!(
        schedules[0].state,
        h4rn_protocol::ResumeState::Waiting,
        "the reset beyond the policy's max wait is retried at the cap: {schedules:?}"
    );

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
