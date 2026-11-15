//! End-to-end test against a real Claude Code installation.
//!
//! Ignored by default because it needs credentials and spends tokens. Run it
//! with:
//!
//! ```text
//! cargo test -p h4rn-daemon --test e2e_claude -- --ignored --nocapture
//! ```
//!
//! The test boots the real daemon stack (built-in harness registry, SQLite
//! journal, thread manager), creates a thread in a temporary git repository,
//! drives one prompt to completion, and then restarts the daemon to prove the
//! harness session id is persisted and reused with `--resume`.

use std::sync::Arc;
use std::time::Duration;

use h4rn_core::LifecycleSettings;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::harnesses::{HarnessCatalog, StaticHarnessCatalog};
use h4rn_daemon::hosts::HostInventory;
use h4rn_daemon::journal::{Journal, SqliteJournal};
use h4rn_daemon::threads::{NewThread, ThreadManager, ThreadManagerOptions};
use h4rn_protocol::{Event, HarnessKind, RuntimeMode, ThreadId, ThreadTab};
use tokio::sync::broadcast::{Receiver, error::RecvError};

const PROMPT: &str = "Reply with exactly: pong";
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

fn real_catalog() -> Arc<StaticHarnessCatalog> {
    let mut catalog = StaticHarnessCatalog::new();
    for adapter in h4rn_harness::registry::builtin_adapters() {
        catalog.register(adapter);
    }
    Arc::new(catalog)
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
    run(&["config", "user.email", "e2e@h4rn.invalid"]);
    run(&["config", "user.name", "h4rn e2e"]);
    std::fs::write(dir.path().join("README.md"), "# e2e\n").expect("write readme");
    run(&["add", "README.md"]);
    run(&["commit", "-q", "-m", "init"]);
    dir
}

fn options() -> ThreadManagerOptions {
    ThreadManagerOptions {
        settings: LifecycleSettings::default()
            .with_working_exit_grace(chrono::TimeDelta::milliseconds(200)),
        tick_interval: Duration::from_millis(20),
        clock: None,
    }
}

async fn boot(config: &Config, catalog: Arc<StaticHarnessCatalog>) -> Arc<ThreadManager> {
    DaemonBuilder::new(config.clone())
        .with_catalog(catalog)
        .with_lifecycle(options())
        .build()
        .await
        .expect("build daemon")
        .manager()
}

async fn wait_for_turn(
    mut events: Receiver<h4rn_daemon::journal::WireEvent>,
    thread_id: &ThreadId,
) -> Vec<Event> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + TURN_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(RecvError::Lagged(missed))) => panic!("event stream lagged by {missed}"),
            Ok(Err(RecvError::Closed)) => panic!("event stream closed"),
            Err(_) => panic!("timed out waiting for the turn to complete"),
        };
        if &event.thread_id != thread_id {
            continue;
        }
        seen.push(event.event.clone());
        if matches!(&event.event, Event::TurnCompleted { .. }) {
            return seen;
        }
    }
}

async fn wait_for_done(manager: &ThreadManager, thread_id: &ThreadId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let threads = manager.list_threads(&Default::default()).await;
        if let Some(thread) = threads.iter().find(|thread| &thread.thread_id == thread_id)
            && thread.tab == ThreadTab::Done
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "thread never reached Done"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn stored_resume_id(config: &Config, thread_id: &ThreadId) -> Option<String> {
    SqliteJournal::connect(config.event_db_path())
        .await
        .expect("open journal")
        .list_threads()
        .await
        .expect("list threads")
        .into_iter()
        .find(|meta| &meta.thread_id == thread_id)
        .and_then(|meta| meta.resume_id)
}

#[tokio::test]
#[ignore = "spawns a real claude process and spends tokens"]
async fn real_claude_daemon_round_trip_and_resume() {
    let dir = tempfile::tempdir().expect("data dir");
    let repo = git_repo();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Auto,
    };

    let catalog = real_catalog();
    let claude = catalog
        .get(HarnessKind::ClaudeCode)
        .expect("claude adapter");
    if claude.detect().is_none() {
        eprintln!("skipping: claude is not installed on this host");
        return;
    }
    let host = HostInventory::detect(Arc::clone(&catalog) as Arc<dyn HarnessCatalog>).local_id();

    // --- first daemon: create the thread and run one turn -----------------
    let manager = boot(&config, Arc::clone(&catalog)).await;
    let events = manager.subscribe();

    let created = manager
        .create_thread(NewThread {
            thread_id: None,
            parent_thread_id: None,
            subagent_id: None,
            host,
            harness: HarnessKind::ClaudeCode,
            workspace_path: repo.path().display().to_string(),
            model: "default".into(),
            prompt: PROMPT.into(),
            attachments: Vec::new(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            worktree: None,
        })
        .await
        .expect("create thread");

    let events = wait_for_turn(events, &created.thread_id).await;
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            Event::MessageDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.to_lowercase().contains("pong"),
        "assistant text was {text:?}"
    );

    let usage = events
        .iter()
        .find_map(|event| match event {
            Event::TurnCompleted { usage, .. } => Some(usage),
            _ => None,
        })
        .expect("turn completed");
    assert!(
        usage.duration_ms > 0 || usage.cost_micros > 0 || usage.total_tokens() > 0,
        "TurnCompleted carried no usage: {usage:?}"
    );

    wait_for_done(&manager, &created.thread_id).await;
    let first_resume_id = stored_resume_id(&config, &created.thread_id)
        .await
        .expect("the harness session id must be durable");
    manager.shutdown().await;

    // --- second daemon: restore the thread and resume the same session ----
    let manager = boot(&config, Arc::clone(&catalog)).await;
    let restored = manager.list_threads(&Default::default()).await;
    assert!(
        restored
            .iter()
            .any(|thread| thread.thread_id == created.thread_id),
        "the thread must survive a daemon restart"
    );

    let events = manager.subscribe();
    manager
        .send_message(
            &created.thread_id,
            "Reply with exactly: pong again".into(),
            Vec::new(),
        )
        .await
        .expect("send second turn");
    let events = wait_for_turn(events, &created.thread_id).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::TurnCompleted { .. })),
        "the second turn must complete"
    );

    let resume_id = stored_resume_id(&config, &created.thread_id)
        .await
        .expect("the session id stays durable");
    assert_eq!(
        resume_id, first_resume_id,
        "the second turn must resume the same Claude session"
    );

    manager.shutdown().await;
    drop(repo);
}
