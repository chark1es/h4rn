//! Moving a thread to another harness between turns.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use h4rn_daemon::journal::{Journal, SqliteJournal};
use h4rn_daemon::provider_switch::SwitchRequest;
use h4rn_daemon::threads::{NewThread, ThreadManager, ThreadManagerOptions};
use h4rn_harness::FakeHarness;
use h4rn_protocol::{
    DroppedSettingKind, Event, HarnessKind, HostId, MessageRole, RuntimeMode, TurnId, TurnStatus,
};

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

fn thread_request(host: &HostId, workspace: &Path, prompt: &str) -> NewThread {
    NewThread {
        thread_id: None,
        parent_thread_id: None,
        subagent_id: None,
        host: host.clone(),
        harness: HarnessKind::ClaudeCode,
        agent_id: None,
        workspace_path: workspace.display().to_string(),
        model: "claude-sonnet-5".into(),
        prompt: prompt.into(),
        attachments: Vec::new(),
        runtime_mode: RuntimeMode::ApprovalRequired,
        reasoning_effort: None,
        fast: false,
        worktree: None,
    }
}

async fn open_manager(
    journal: Arc<dyn Journal>,
    claude: FakeHarness,
    codex: FakeHarness,
    opencode: Option<FakeHarness>,
) -> Arc<ThreadManager> {
    let mut catalog =
        h4rn_daemon::harnesses::StaticHarnessCatalog::with(Arc::new(claude)).and(Arc::new(codex));
    if let Some(opencode) = opencode {
        catalog = catalog.and(Arc::new(opencode));
    }
    Arc::new(
        ThreadManager::open(
            journal,
            Arc::new(catalog),
            HostId::new("host_switch"),
            ThreadManagerOptions {
                tick_interval: Duration::from_secs(3600),
                ..ThreadManagerOptions::default()
            },
            None,
        )
        .await
        .expect("manager"),
    )
}

async fn wait_for_turn(
    events: &mut tokio::sync::broadcast::Receiver<h4rn_daemon::journal::WireEvent>,
    turn: &TurnId,
) -> (TurnStatus, String) {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        let mut text = String::new();
        loop {
            let event = events.recv().await.expect("thread event");
            match event.event {
                Event::ItemCompleted {
                    item:
                        h4rn_protocol::Item::Message {
                            role: MessageRole::Assistant,
                            text: body,
                            ..
                        },
                    ..
                } => text.push_str(&body),
                Event::TurnCompleted {
                    turn_id, status, ..
                } if &turn_id == turn => return (status, text),
                _ => {}
            }
        }
    })
    .await
    .expect("turn completion")
}

fn codex_switch() -> SwitchRequest {
    SwitchRequest {
        harness: HarnessKind::Codex,
        agent_id: None,
        model: "gpt-5.1-codex".into(),
    }
}

#[tokio::test]
async fn a_switch_ends_the_old_session_and_the_first_new_message_hands_off() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().join("work");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let journal: Arc<dyn Journal> = Arc::new(SqliteJournal::in_memory().await.expect("journal"));
    let (claude, shutdowns) = FakeHarness::new().with_shutdown_count();
    let codex = FakeHarness::new().with_kind(HarnessKind::Codex);
    let manager = open_manager(journal, claude, codex, None).await;
    let mut events = manager.subscribe();

    let created = manager
        .create_thread(thread_request(
            &HostId::new("host_switch"),
            &workspace,
            "Implement the parser and keep it quiet",
        ))
        .await
        .expect("create");
    assert_eq!(
        wait_for_turn(&mut events, &created.turn_id).await.0,
        TurnStatus::Completed
    );

    let outcome = manager
        .switch_thread_harness(&created.thread_id, codex_switch())
        .await
        .expect("switch");
    assert_eq!(outcome.summary.harness, HarnessKind::Codex);
    assert_eq!(outcome.summary.model, "gpt-5.1-codex");
    assert!(outcome.dropped.is_empty(), "{:?}", outcome.dropped);
    assert_eq!(
        shutdowns.get(),
        1,
        "the old harness session was ended cleanly"
    );

    let request = "now add structured logs";
    let turn = manager
        .send_message(&created.thread_id, request.into(), Vec::new())
        .await
        .expect("send");
    let (status, reply) = wait_for_turn(&mut events, &turn.turn_id).await;
    assert_eq!(status, TurnStatus::Completed);
    assert!(
        reply.contains("Earlier conversation history"),
        "the new harness receives the journaled history: {reply}"
    );
    assert!(reply.contains("Implement the parser and keep it quiet"));
    assert_eq!(
        reply.matches(request).count(),
        1,
        "the request appears once, after the history: {reply}"
    );
    assert!(
        reply.ends_with(request),
        "the request is untouched: {reply}"
    );

    // Only the first turn after the switch carries the history.
    let second = manager
        .send_message(&created.thread_id, "and a health check".into(), Vec::new())
        .await
        .expect("second send");
    let (_, reply) = wait_for_turn(&mut events, &second.turn_id).await;
    assert!(
        !reply.contains("Earlier conversation history"),
        "the new session is resumed normally: {reply}"
    );
    assert!(reply.ends_with("and a health check"));
    manager.shutdown().await;
}

#[tokio::test]
async fn the_switch_divider_and_cleared_resume_id_survive_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().join("work");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let database = dir.path().join("journal.db");
    let journal: Arc<dyn Journal> =
        Arc::new(SqliteJournal::connect(&database).await.expect("journal"));
    let manager = open_manager(
        Arc::clone(&journal),
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
        None,
    )
    .await;
    let mut events = manager.subscribe();
    let created = manager
        .create_thread(thread_request(
            &HostId::new("host_switch"),
            &workspace,
            "the original request",
        ))
        .await
        .expect("create");
    assert_eq!(
        wait_for_turn(&mut events, &created.turn_id).await.0,
        TurnStatus::Completed
    );
    let before = manager.thread_meta(&created.thread_id).await.expect("meta");
    assert!(
        before.resume_id.is_some(),
        "the Claude session is resumable"
    );

    manager
        .switch_thread_harness(&created.thread_id, codex_switch())
        .await
        .expect("switch");
    let after = manager.thread_meta(&created.thread_id).await.expect("meta");
    assert_eq!(after.harness, HarnessKind::Codex);
    assert_eq!(after.resume_id, None, "the old resume id is cleared");
    assert!(after.handoff_pending);
    manager.shutdown().await;

    // A new daemon over the same journal sees the divider and the same cleared
    // row; its first message still hands off the history.
    let wire = journal
        .read_wire_since(&created.thread_id, 0)
        .await
        .expect("wire");
    assert!(
        wire.iter().any(|entry| matches!(
            entry.event,
            Event::HarnessSwitched {
                from: HarnessKind::ClaudeCode,
                to: HarnessKind::Codex,
                ..
            }
        )),
        "the divider is journaled"
    );
    let restarted = open_manager(
        journal,
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
        None,
    )
    .await;
    let meta = restarted
        .thread_meta(&created.thread_id)
        .await
        .expect("restored meta");
    assert_eq!(meta.resume_id, None);
    assert!(meta.handoff_pending);
    let mut events = restarted.subscribe();
    let request = "continue after the restart";
    let turn = restarted
        .send_message(&created.thread_id, request.into(), Vec::new())
        .await
        .expect("send");
    let (status, reply) = wait_for_turn(&mut events, &turn.turn_id).await;
    assert_eq!(status, TurnStatus::Completed);
    assert!(
        reply.contains("Earlier conversation history")
            && reply.contains("the original request")
            && reply.ends_with(request),
        "the restarted thread hands off its history: {reply}"
    );
    restarted.shutdown().await;
}

#[tokio::test]
async fn a_switch_drops_the_settings_the_new_harness_does_not_offer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().join("work");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let host = HostId::new("host_switch");
    let journal: Arc<dyn Journal> = Arc::new(SqliteJournal::in_memory().await.expect("journal"));
    let manager = open_manager(
        journal,
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
        Some(FakeHarness::new().with_kind(HarnessKind::Opencode)),
    )
    .await;
    let mut events = manager.subscribe();

    // Plan is Claude Code's; Codex has no such mode, so it falls back and is
    // reported.
    let mut request = thread_request(&host, &workspace, "plan the work");
    request.runtime_mode = RuntimeMode::Plan;
    let created = manager.create_thread(request).await.expect("create");
    assert_eq!(
        wait_for_turn(&mut events, &created.turn_id).await.0,
        TurnStatus::Completed
    );
    let outcome = manager
        .switch_thread_harness(&created.thread_id, codex_switch())
        .await
        .expect("switch to codex");
    assert_eq!(outcome.summary.runtime_mode, RuntimeMode::ApprovalRequired);
    assert!(outcome.dropped.iter().any(|dropped| {
        dropped.setting == DroppedSettingKind::RuntimeMode && dropped.value == "plan"
    }));

    // Effort and fast belong to Codex; OpenCode offers neither.
    let mut request = thread_request(&host, &workspace, "code the work");
    request.harness = HarnessKind::Codex;
    request.model = "gpt-5.1-codex".into();
    request.runtime_mode = RuntimeMode::FullAccess;
    request.reasoning_effort = Some("ultra".into());
    request.fast = true;
    let second = manager.create_thread(request).await.expect("codex create");
    assert_eq!(
        wait_for_turn(&mut events, &second.turn_id).await.0,
        TurnStatus::Completed
    );
    let outcome = manager
        .switch_thread_harness(
            &second.thread_id,
            SwitchRequest {
                harness: HarnessKind::Opencode,
                agent_id: None,
                model: "claude-sonnet-5-5".into(),
            },
        )
        .await
        .expect("switch to opencode");
    assert_eq!(outcome.summary.runtime_mode, RuntimeMode::FullAccess);
    assert_eq!(outcome.summary.reasoning_effort, None);
    assert!(!outcome.summary.fast);
    let kinds = outcome
        .dropped
        .iter()
        .map(|dropped| dropped.setting)
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec![
            DroppedSettingKind::ReasoningEffort,
            DroppedSettingKind::Fast
        ],
        "{:?}",
        outcome.dropped
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn usage_is_metered_against_the_harness_that_ran_each_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().join("work");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let host = HostId::new("host_switch");
    let journal: Arc<dyn Journal> = Arc::new(SqliteJournal::in_memory().await.expect("journal"));
    let manager = open_manager(
        journal,
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
        None,
    )
    .await;
    let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
    manager.set_meter(Arc::clone(&meter));
    let mut events = manager.subscribe();
    let created = manager
        .create_thread(thread_request(&host, &workspace, "first"))
        .await
        .expect("create");
    assert_eq!(
        wait_for_turn(&mut events, &created.turn_id).await.0,
        TurnStatus::Completed
    );
    manager
        .switch_thread_harness(&created.thread_id, codex_switch())
        .await
        .expect("switch");
    let turn = manager
        .send_message(&created.thread_id, "second".into(), Vec::new())
        .await
        .expect("send");
    assert_eq!(
        wait_for_turn(&mut events, &turn.turn_id).await.0,
        TurnStatus::Completed
    );

    let rows = meter.store().all_rows().await.expect("usage rows");
    let rows = rows
        .into_iter()
        .filter(|row| row.thread_id.as_deref() == Some(created.thread_id.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2, "one ledger row per turn");
    let claude = rows
        .iter()
        .find(|row| row.harness.as_deref() == Some("claude-code"))
        .expect("the Claude turn was metered");
    assert_eq!(claude.model.as_deref(), Some("claude-sonnet-5"));
    let codex = rows
        .iter()
        .find(|row| row.harness.as_deref() == Some("codex"))
        .expect("the Codex turn was metered");
    assert_eq!(codex.model.as_deref(), Some("gpt-5.1-codex"));
    manager.shutdown().await;
}
