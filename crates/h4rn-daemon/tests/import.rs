//! Importing Claude Code and Codex transcripts into real threads, then
//! continuing one through the fake harness.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use h4rn_daemon::handoff::DEFAULT_HANDOFF_BYTES;
use h4rn_daemon::import::ImportService;
use h4rn_daemon::journal::{Journal, SqliteJournal};
use h4rn_daemon::projects::ProjectService;
use h4rn_daemon::threads::{ThreadManager, ThreadManagerOptions};
use h4rn_harness::FakeHarness;
use h4rn_protocol::{
    AgentState, Event, HarnessKind, HostId, ImportEvent, MessageRole, ThreadFilter, ThreadTab,
};
use serde_json::json;

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

struct Fixture {
    _home: tempfile::TempDir,
    home: PathBuf,
    workspace: PathBuf,
    import: ImportService,
    manager: Arc<ThreadManager>,
    journal: Arc<dyn Journal>,
}

/// Opens a journal, thread manager and import service over one temp home.
async fn services(
    home_path: &Path,
    claude: FakeHarness,
    codex: FakeHarness,
) -> (Arc<dyn Journal>, Arc<ThreadManager>, ImportService) {
    let host = HostId::new("host_import");
    let journal: Arc<dyn Journal> = Arc::new(SqliteJournal::in_memory().await.expect("journal"));
    let catalog = Arc::new(
        h4rn_daemon::harnesses::StaticHarnessCatalog::with(Arc::new(claude)).and(Arc::new(codex)),
    );
    let manager = Arc::new(
        ThreadManager::open(
            Arc::clone(&journal),
            catalog,
            host.clone(),
            ThreadManagerOptions {
                tick_interval: Duration::from_secs(3600),
                ..ThreadManagerOptions::default()
            },
            None,
        )
        .await
        .expect("manager"),
    );
    let projects = Arc::new(
        ProjectService::open_with_home(
            &home_path.join("data"),
            host.clone(),
            home_path.to_path_buf(),
        )
        .expect("projects"),
    );
    let import = ImportService::with_homes(
        Arc::clone(&journal),
        projects,
        Arc::clone(&manager),
        host,
        home_path.to_path_buf(),
        home_path.join(".claude"),
        home_path.join(".codex"),
    );
    (journal, manager, import)
}

fn write_jsonl(path: &Path, lines: &[serde_json::Value]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("transcript dir");
    }
    let mut body = String::new();
    for line in lines {
        body.push_str(&line.to_string());
        body.push('\n');
    }
    std::fs::write(path, body).expect("transcript");
}

fn claude_record(kind: &str, text: &str, model: Option<&str>, at: &str) -> serde_json::Value {
    let mut value = json!({
        "type": kind,
        "sessionId": "claude-session-1",
        "cwd": "/placeholder",
        "timestamp": at,
        "message": {
            "role": kind,
            "content": [{"type": "text", "text": text}],
        },
    });
    if let Some(model) = model {
        value["message"]["model"] = json!(model);
    }
    value
}

/// A Claude transcript long enough that its handoff must be budgeted.
fn claude_transcript(workspace: &Path) -> Vec<serde_json::Value> {
    let cwd = workspace.display().to_string();
    let mut records = vec![claude_record(
        "user",
        "Implement the feature and keep it quiet",
        None,
        "2026-11-18T09:00:00Z",
    )];
    for index in 0..30 {
        let mut assistant = claude_record(
            "assistant",
            &format!("step {index} {}", "x".repeat(900)),
            Some("claude-sonnet-5"),
            "2026-11-18T09:01:00Z",
        );
        assistant["cwd"] = json!(cwd);
        records.push(assistant);
        let user = claude_record(
            "user",
            &format!("follow-up {index}"),
            None,
            "2026-11-18T09:02:00Z",
        );
        records.push(user);
    }
    for record in &mut records {
        record["cwd"] = json!(cwd);
    }
    records
}

/// A Codex rollout with one canonical user turn and its matching copy.
fn codex_transcript() -> Vec<serde_json::Value> {
    vec![
        json!({
            "timestamp": "2026-11-19T10:00:00Z",
            "type": "session_meta",
            "payload": {"id": "codex-session-1", "cwd": "/placeholder"},
        }),
        json!({
            "timestamp": "2026-11-19T10:00:00Z",
            "type": "turn_context",
            "payload": {"model": "gpt-5.1-codex"},
        }),
        json!({
            "timestamp": "2026-11-19T10:00:01Z",
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "Port the importer"},
        }),
        json!({
            "timestamp": "2026-11-19T10:00:01Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Port the importer"}],
                "internal_chat_message_metadata_passthrough": {"turn_id": "turn-1"},
            },
        }),
        json!({
            "timestamp": "2026-11-19T10:00:02Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Ported it"}],
            },
        }),
    ]
}

async fn fixture(claude: FakeHarness, codex: FakeHarness) -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let home_path = home.path().to_path_buf();
    let workspace = home_path.join("code").join("api");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let (journal, manager, import) = services(&home_path, claude, codex).await;

    let mut claude_lines = claude_transcript(&workspace);
    for line in &mut claude_lines {
        line["cwd"] = json!(workspace.display().to_string());
    }
    write_jsonl(
        &home_path
            .join(".claude/projects/-code-api")
            .join("claude-session-1.jsonl"),
        &claude_lines,
    );
    let mut codex_lines = codex_transcript();
    for line in &mut codex_lines {
        if let Some(payload) = line.get_mut("payload")
            && payload.get("cwd").is_some()
        {
            payload["cwd"] = json!(workspace.display().to_string());
        }
    }
    write_jsonl(
        &home_path
            .join(".codex/sessions/2026/11/19")
            .join("rollout-2026-11-19T10-00-00-codex-session-1.jsonl"),
        &codex_lines,
    );

    Fixture {
        _home: home,
        home: home_path,
        workspace,
        import,
        manager,
        journal,
    }
}

async fn wait_finished(events: &mut tokio::sync::broadcast::Receiver<ImportEvent>) -> ImportEvent {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                // A long run publishes more frames than the channel keeps; the
                // newest snapshot is always still there.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(error) => panic!("import event: {error}"),
            };
            if !event.progress.running {
                return event;
            }
        }
    })
    .await
    .expect("import finished")
}

#[tokio::test]
async fn imports_both_formats_and_skips_duplicates_on_a_second_run() {
    let fixture = fixture(
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let scan = fixture.import.scan().await;
    assert_eq!(scan.projects.len(), 1, "one folder groups both sources");
    let project = &scan.projects[0];
    assert_eq!(project.conversations, 2);
    assert_eq!(project.importable, 2);
    assert!(!project.git);
    assert_eq!(project.path, fixture.workspace.display().to_string());

    let mut events = fixture.import.subscribe();
    let started = fixture
        .import
        .start(vec![project.key.clone()])
        .await
        .expect("start");
    assert!(started.running);
    let finished = wait_finished(&mut events).await.progress;
    assert!(!finished.running);
    assert_eq!(finished.imported, 2);
    assert_eq!(finished.failed, 0);

    let threads = fixture.manager.list_threads(&ThreadFilter::default()).await;
    assert_eq!(threads.len(), 2);
    let claude = threads
        .iter()
        .find(|thread| thread.harness == HarnessKind::ClaudeCode)
        .expect("claude thread");
    assert_eq!(claude.title, "Implement the feature and keep it quiet");
    assert_eq!(claude.model, "claude-sonnet-5");
    assert_eq!(
        claude.workspace_path,
        fixture.workspace.display().to_string()
    );
    let codex = threads
        .iter()
        .find(|thread| thread.harness == HarnessKind::Codex)
        .expect("codex thread");
    assert_eq!(codex.title, "Port the importer");
    assert_eq!(codex.model, "gpt-5.1-codex");

    // Every kept message is on the wire, with roles in order.
    let snapshot = fixture
        .manager
        .get_thread(&claude.thread_id, 0)
        .await
        .expect("snapshot");
    let messages: Vec<(MessageRole, String)> = snapshot
        .events
        .iter()
        .filter_map(|entry| match &entry.event {
            h4rn_protocol::Event::ItemCompleted {
                item: h4rn_protocol::Item::Message { role, text, .. },
                ..
            } => Some((*role, text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        messages.first().map(|entry| entry.1.as_str()),
        Some("Implement the feature and keep it quiet")
    );
    assert_eq!(
        messages.last().map(|entry| entry.0),
        Some(MessageRole::User)
    );

    // A second run imports nothing new.
    let mut events = fixture.import.subscribe();
    fixture
        .import
        .start(vec![project.key.clone()])
        .await
        .expect("start again");
    let again = wait_finished(&mut events).await.progress;
    assert_eq!(again.imported, 0);
    assert_eq!(again.skipped, 2);
    let threads = fixture.manager.list_threads(&ThreadFilter::default()).await;
    assert_eq!(threads.len(), 2);
    assert!(fixture.home.join(".claude").is_dir(), "source is untouched");
}

#[tokio::test]
async fn a_failed_native_resume_falls_back_to_a_bounded_handoff() {
    let fixture = fixture(
        FakeHarness::new().with_resume_failure("no such session"),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let scan = fixture.import.scan().await;
    let key = scan.projects[0].key.clone();
    fixture.import.start(vec![key]).await.expect("start");
    let threads = wait_for_threads(&fixture, 2).await;
    let claude = threads
        .iter()
        .find(|thread| thread.harness == HarnessKind::ClaudeCode)
        .expect("claude thread")
        .clone();

    let request = "Please also add structured logs";
    let mut events = fixture.manager.subscribe();
    fixture
        .manager
        .send_message(&claude.thread_id, request.into(), Vec::new())
        .await
        .expect("send");
    let echo = wait_for_echo(&mut events).await;
    assert!(
        echo.contains("Earlier conversation history"),
        "the fallback sends the saved history: {echo}"
    );
    assert!(echo.contains("Implement the feature and keep it quiet"));
    assert_eq!(
        echo.matches(request).count(),
        1,
        "the retry sends the request once, not as history and again as the request: {echo}"
    );
    assert!(echo.ends_with(request), "the new request is untouched");
    let limit =
        DEFAULT_HANDOFF_BYTES + "My next message:\n".len() + request.len() + "echo: ".len() + 2;
    assert!(
        echo.len() <= limit,
        "handoff stayed within the default budget: {} > {limit}",
        echo.len()
    );
    // The retry cleared the dead resume id, so a second message resumes the
    // session the retry started instead of trying the dead one again.
    let second = "And a health check";
    fixture
        .manager
        .send_message(&claude.thread_id, second.into(), Vec::new())
        .await
        .expect("second send");
    let echo = wait_for_echo(&mut events).await;
    assert!(!echo.contains("Earlier conversation history"));
    assert!(echo.ends_with(second));
}

#[tokio::test]
async fn a_large_request_still_hands_off_the_recent_history_once() {
    let fixture = fixture(
        FakeHarness::new().with_resume_failure("no such session"),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let scan = fixture.import.scan().await;
    fixture
        .import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    let threads = wait_for_threads(&fixture, 2).await;
    let claude = threads
        .iter()
        .find(|thread| thread.harness == HarnessKind::ClaudeCode)
        .expect("claude thread")
        .clone();

    // Most of the default 16 KB budget. The journaled copy of this request is
    // the newest message in the thread; it must not be read back as history
    // and push the recent messages out.
    let request = format!("{}please also add structured logs", "detail ".repeat(1_800));
    assert!(
        request.len() > 12_000 && request.len() < 15_000,
        "the request takes most of the budget: {} bytes",
        request.len()
    );
    let mut events = fixture.manager.subscribe();
    fixture
        .manager
        .send_message(&claude.thread_id, request.clone(), Vec::new())
        .await
        .expect("send");
    let echo = wait_for_echo(&mut events).await;
    assert!(
        echo.contains("Earlier conversation history"),
        "a large request still leaves room for the history: {echo}"
    );
    assert!(
        echo.contains("Implement the feature and keep it quiet"),
        "the original request survives a large new one"
    );
    assert!(
        echo.contains("follow-up 29") && echo.contains("step 29"),
        "the recent messages survive a large new one: {}",
        &echo[..echo.len().min(400)]
    );
    assert_eq!(
        echo.matches(&request).count(),
        1,
        "the request appears once, not as history and again as the request"
    );
    assert!(echo.ends_with(&request), "the new request is untouched");
    let limit =
        DEFAULT_HANDOFF_BYTES + "My next message:\n".len() + request.len() + "echo: ".len() + 2;
    assert!(
        echo.len() <= limit,
        "the handoff stayed within the default budget: {} > {limit}",
        echo.len()
    );
}

#[tokio::test]
async fn a_native_resume_is_preferred_when_the_harness_accepts_it() {
    let fixture = fixture(
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let scan = fixture.import.scan().await;
    fixture
        .import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    let threads = wait_for_threads(&fixture, 2).await;
    let claude = threads
        .iter()
        .find(|thread| thread.harness == HarnessKind::ClaudeCode)
        .expect("claude thread")
        .clone();

    let request = "carry on";
    let mut events = fixture.manager.subscribe();
    fixture
        .manager
        .send_message(&claude.thread_id, request.into(), Vec::new())
        .await
        .expect("send");
    let echo = wait_for_echo(&mut events).await;
    assert!(
        !echo.contains("Earlier conversation history"),
        "a resumed session already holds the history: {echo}"
    );
    assert!(echo.ends_with(request));
}

#[tokio::test]
async fn a_later_startup_death_keeps_the_resume_id_and_fails_normally() {
    // The imported thread's one native resume succeeds and a turn runs on it.
    let fixture = fixture(
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let scan = fixture.import.scan().await;
    fixture
        .import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    let threads = wait_for_threads(&fixture, 2).await;
    let claude = threads
        .iter()
        .find(|thread| thread.harness == HarnessKind::ClaudeCode)
        .expect("claude thread")
        .clone();

    let mut events = fixture.manager.subscribe();
    let first = fixture
        .manager
        .send_message(&claude.thread_id, "carry on".into(), Vec::new())
        .await
        .expect("send");
    let (status, reply) = wait_for_turn(&mut events, &first.turn_id).await;
    assert_eq!(status, h4rn_protocol::TurnStatus::Completed);
    assert!(
        !reply.contains("Earlier conversation history"),
        "the native resume already holds the history: {reply}"
    );

    let resume = stored_meta(&fixture, &claude.thread_id)
        .await
        .resume_id
        .expect("the resumed session id");
    let imported = stored_meta(&fixture, &claude.thread_id)
        .await
        .imported
        .expect("imported origin");
    assert!(
        !imported.native_resume,
        "the thread spent its one native resume when the session started"
    );

    // A restart whose startup dies before it reports a session must fail that
    // turn as any other thread would: the stale imported messages are not a
    // fallback that could lose the turns made here.
    let restarted = restarted_manager(
        &fixture,
        FakeHarness::new().with_resume_death("No conversation found with session ID"),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let mut events = restarted.subscribe();
    let second = restarted
        .send_message(&claude.thread_id, "and then the tests".into(), Vec::new())
        .await
        .expect("send");
    let (status, reply) = wait_for_turn(&mut events, &second.turn_id).await;
    assert_eq!(
        status,
        h4rn_protocol::TurnStatus::Failed,
        "the turn fails normally instead of replaying the imported history"
    );
    assert!(
        !reply.contains("Earlier conversation history"),
        "no handoff was built from the stale import: {reply}"
    );
    assert_eq!(
        stored_meta(&fixture, &claude.thread_id).await.resume_id,
        Some(resume),
        "the live session id is kept for the next attempt"
    );
    restarted.shutdown().await;
}

#[tokio::test]
async fn an_import_keeps_the_transcript_only_in_its_journaled_items() {
    let fixture = fixture(
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let scan = fixture.import.scan().await;
    fixture
        .import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    let threads = wait_for_threads(&fixture, 2).await;
    let claude = threads
        .iter()
        .find(|thread| thread.harness == HarnessKind::ClaudeCode)
        .expect("claude thread")
        .clone();

    let imported = stored_meta(&fixture, &claude.thread_id)
        .await
        .imported
        .expect("imported origin");
    let stored = serde_json::to_string(&imported).expect("serialised origin");
    assert!(
        !stored.contains("Implement the feature and keep it quiet"),
        "the imported origin is not a second copy of the transcript: {stored}"
    );
    assert!(
        stored.len() < 1_000,
        "the origin holds only what a handoff needs: {} bytes",
        stored.len()
    );

    // An unrelated upsert (a rename) does not rewrite a transcript either.
    fixture
        .manager
        .rename_thread(&claude.thread_id, "Renamed import")
        .await
        .expect("rename");
    let again = stored_meta(&fixture, &claude.thread_id)
        .await
        .imported
        .expect("imported origin");
    assert_eq!(
        serde_json::to_string(&again).expect("serialised origin"),
        stored
    );
}

#[tokio::test]
async fn a_restarted_imported_thread_still_hands_off_its_history() {
    let fixture = fixture(
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let scan = fixture.import.scan().await;
    fixture
        .import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    let threads = wait_for_threads(&fixture, 2).await;
    let claude = threads
        .iter()
        .find(|thread| thread.harness == HarnessKind::ClaudeCode)
        .expect("claude thread")
        .clone();

    // The history the handoff needs lives in the journaled messages, so a
    // restart loses nothing.
    let restarted = restarted_manager(
        &fixture,
        FakeHarness::new().with_resume_failure("no such session"),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let mut events = restarted.subscribe();
    let turn = restarted
        .send_message(&claude.thread_id, "please continue".into(), Vec::new())
        .await
        .expect("send");
    let (status, reply) = wait_for_turn(&mut events, &turn.turn_id).await;
    assert_eq!(status, h4rn_protocol::TurnStatus::Completed);
    assert!(
        reply.contains("Earlier conversation history"),
        "the restart's handoff carries the saved conversation: {reply}"
    );
    assert!(reply.contains("Implement the feature and keep it quiet"));
    assert!(reply.ends_with("please continue"), "{reply}");
    restarted.shutdown().await;
}

async fn wait_for_threads(fixture: &Fixture, count: usize) -> Vec<h4rn_protocol::ThreadSummary> {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            let threads = fixture.manager.list_threads(&ThreadFilter::default()).await;
            if threads.len() >= count {
                return threads;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("threads")
}

/// Opens a fresh manager over the fixture's journal, as a daemon restart does.
async fn restarted_manager(
    fixture: &Fixture,
    claude: FakeHarness,
    codex: FakeHarness,
) -> Arc<ThreadManager> {
    let catalog = Arc::new(
        h4rn_daemon::harnesses::StaticHarnessCatalog::with(Arc::new(claude)).and(Arc::new(codex)),
    );
    Arc::new(
        ThreadManager::open(
            Arc::clone(&fixture.journal),
            catalog,
            HostId::new("host_import"),
            ThreadManagerOptions {
                tick_interval: Duration::from_secs(3600),
                ..ThreadManagerOptions::default()
            },
            None,
        )
        .await
        .expect("restarted manager"),
    )
}

/// The stored metadata of one thread.
async fn stored_meta(
    fixture: &Fixture,
    thread: &h4rn_protocol::ThreadId,
) -> h4rn_daemon::journal::ThreadMeta {
    fixture
        .journal
        .list_threads()
        .await
        .expect("stored threads")
        .into_iter()
        .find(|meta| &meta.thread_id == thread)
        .expect("stored thread")
}

/// Waits for one turn to settle, returning its status and assistant text.
async fn wait_for_turn(
    events: &mut tokio::sync::broadcast::Receiver<h4rn_daemon::journal::WireEvent>,
    turn: &h4rn_protocol::TurnId,
) -> (h4rn_protocol::TurnStatus, String) {
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
    .expect("the turn settled")
}

/// The fake harness echoes the prompt it was sent in its reply.
async fn wait_for_echo(
    events: &mut tokio::sync::broadcast::Receiver<h4rn_daemon::journal::WireEvent>,
) -> String {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            let event = events.recv().await.expect("thread event");
            if let h4rn_protocol::Event::ItemCompleted {
                item: h4rn_protocol::Item::Message { role, text, .. },
                ..
            } = event.event
                && role == MessageRole::Assistant
                && text.contains("echo: ")
            {
                return text;
            }
        }
    })
    .await
    .expect("an echo reply")
}

/// One Claude conversation with a fixed session id and working directory.
fn claude_conversation(cwd: &Path, session: &str, prompt: &str) -> Vec<serde_json::Value> {
    let mut records = vec![
        claude_record("user", prompt, None, "2026-11-18T09:00:00Z"),
        claude_record(
            "assistant",
            "done",
            Some("claude-sonnet-5"),
            "2026-11-18T09:01:00Z",
        ),
    ];
    for record in &mut records {
        record["cwd"] = json!(cwd.display().to_string());
        record["sessionId"] = json!(session);
    }
    records
}

#[tokio::test]
async fn a_resume_that_dies_before_its_session_falls_back_to_the_handoff() {
    // The real Claude Code adapter returns `Ok` when the child starts; a bad
    // `--resume` then exits before it reports a session. The retry must reach
    // a fresh session with the saved history, once.
    let fixture = fixture(
        FakeHarness::new().with_resume_death("No conversation found with session ID"),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let scan = fixture.import.scan().await;
    fixture
        .import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    let threads = wait_for_threads(&fixture, 2).await;
    let claude = threads
        .iter()
        .find(|thread| thread.harness == HarnessKind::ClaudeCode)
        .expect("claude thread")
        .clone();

    let request = "Please also add structured logs";
    let mut events = fixture.manager.subscribe();
    fixture
        .manager
        .send_message(&claude.thread_id, request.into(), Vec::new())
        .await
        .expect("send");
    let echo = wait_for_echo(&mut events).await;
    assert!(
        echo.contains("Earlier conversation history"),
        "the retry sends the saved history: {echo}"
    );
    assert_eq!(
        echo.matches(request).count(),
        1,
        "the retry sends the request once, not as history and again as the request: {echo}"
    );
    assert!(echo.ends_with(request), "the new request is untouched");

    // The dead id was cleared, so the retry's session is the one the next
    // message resumes instead of trying the dead one again.
    let second = "And a health check";
    fixture
        .manager
        .send_message(&claude.thread_id, second.into(), Vec::new())
        .await
        .expect("second send");
    let echo = wait_for_echo(&mut events).await;
    assert!(!echo.contains("Earlier conversation history"), "{echo}");
    assert!(echo.ends_with(second));
}

#[tokio::test]
async fn two_copies_of_one_session_in_one_run_create_one_thread() {
    let home = tempfile::tempdir().expect("home");
    let home_path = home.path().to_path_buf();
    let workspace = home_path.join("code/api");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let (_, manager, import) = services(
        &home_path,
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    for project in ["one", "two"] {
        write_jsonl(
            &home_path
                .join(format!(".claude/projects/{project}"))
                .join("session-duplicate.jsonl"),
            &claude_conversation(&workspace, "session-duplicate", "one conversation"),
        );
    }

    let scan = import.scan().await;
    assert_eq!(scan.projects.len(), 1);
    assert_eq!(scan.projects[0].conversations, 2);
    let mut events = import.subscribe();
    import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    let finished = wait_finished(&mut events).await.progress;
    assert_eq!(finished.imported, 1, "the source is claimed once");
    assert_eq!(finished.skipped, 1);
    assert_eq!(
        manager.list_threads(&ThreadFilter::default()).await.len(),
        1
    );
}

#[tokio::test]
async fn importing_the_newest_hundred_still_offers_the_older_conversations() {
    let home = tempfile::tempdir().expect("home");
    let home_path = home.path().to_path_buf();
    let workspace = home_path.join("code/api");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let (_, manager, import) = services(
        &home_path,
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    for index in 0..102 {
        write_jsonl(
            &home_path
                .join(".claude/projects/api")
                .join(format!("session-{index:03}.jsonl")),
            &claude_conversation(&workspace, &format!("session-{index:03}"), "work"),
        );
    }

    let scan = import.scan().await;
    assert_eq!(
        scan.projects[0].conversations, 100,
        "one attempt offers at most 100 conversations"
    );
    assert!(scan.warnings.iter().any(|warning| warning.contains("100")));

    let mut events = import.subscribe();
    import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    let finished = wait_finished(&mut events).await.progress;
    assert_eq!(finished.imported, 100);
    assert_eq!(
        manager.list_threads(&ThreadFilter::default()).await.len(),
        100
    );

    // The imported conversations no longer use the quota, so the scan now
    // finds the two older ones and a second run imports them.
    let scan = import.scan().await;
    assert_eq!(scan.projects[0].importable, 2);
    let mut events = import.subscribe();
    import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("second start");
    let finished = wait_finished(&mut events).await.progress;
    assert_eq!(finished.imported, 2);
    assert_eq!(
        manager.list_threads(&ThreadFilter::default()).await.len(),
        102
    );
}

#[tokio::test]
async fn a_conversation_under_a_symlinked_downloads_folder_is_not_offered() {
    let home = tempfile::tempdir().expect("home");
    let home_path = home.path().to_path_buf();
    let real = home_path.join("real-downloads");
    let thing = real.join("thing");
    std::fs::create_dir_all(&thing).expect("downloads folder");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, home_path.join("Downloads")).expect("symlink");
    #[cfg(not(unix))]
    let _ = &real;
    let (_, _, import) = services(
        &home_path,
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    write_jsonl(
        &home_path.join(".claude/projects/downloads").join("d.jsonl"),
        &claude_conversation(&thing, "download-session", "downloaded work"),
    );
    let scan = import.scan().await;
    assert!(
        scan.projects.is_empty(),
        "a conversation under ~/Downloads is skipped: {:?}",
        scan.projects
    );
}

#[tokio::test]
async fn a_worktree_conversation_belongs_to_the_main_checkout() {
    let home = tempfile::tempdir().expect("home");
    let home_path = home.path().to_path_buf();
    let main = home_path.join("code/api");
    let worktree = home_path.join("code/api-wt");
    let git_dir = main.join(".git/worktrees/wt");
    std::fs::create_dir_all(&git_dir).expect("git dir");
    std::fs::write(
        main.join(".git/config"),
        "[remote \"origin\"]\n\turl = https://github.com/acme/api.git\n",
    )
    .expect("config");
    std::fs::write(git_dir.join("commondir"), "../..").expect("commondir");
    std::fs::create_dir_all(&worktree).expect("worktree");
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", git_dir.display()),
    )
    .expect("worktree pointer");
    let (_, manager, import) = services(
        &home_path,
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    write_jsonl(
        &home_path.join(".claude/projects/api").join("main.jsonl"),
        &claude_conversation(&main, "main-session", "work on main"),
    );
    write_jsonl(
        &home_path.join(".claude/projects/api-wt").join("wt.jsonl"),
        &claude_conversation(&worktree, "worktree-session", "work in the worktree"),
    );

    let scan = import.scan().await;
    assert_eq!(
        scan.projects.len(),
        1,
        "both conversations form one project"
    );
    assert_eq!(scan.projects[0].key, "github:acme/api");
    assert_eq!(scan.projects[0].path, main.display().to_string());
    assert_eq!(scan.projects[0].conversations, 2);

    let mut events = import.subscribe();
    import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    wait_finished(&mut events).await;
    let threads = manager.list_threads(&ThreadFilter::default()).await;
    let worktree_thread = threads
        .iter()
        .find(|thread| thread.workspace_path == worktree.display().to_string())
        .expect("the worktree conversation keeps its own working folder");
    assert_eq!(
        worktree_thread.project_path.as_deref(),
        Some(main.display().to_string().as_str()),
        "the sidebar groups it under the main checkout"
    );
    let main_thread = threads
        .iter()
        .find(|thread| thread.workspace_path == main.display().to_string())
        .expect("main thread");
    assert_eq!(main_thread.project_path, None);
}

#[tokio::test]
async fn an_imported_thread_is_still_done_with_its_timestamps_after_a_restart() {
    let fixture = fixture(
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let scan = fixture.import.scan().await;
    fixture
        .import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    wait_for_threads(&fixture, 2).await;

    let catalog = Arc::new(
        h4rn_daemon::harnesses::StaticHarnessCatalog::with(Arc::new(FakeHarness::new()))
            .and(Arc::new(FakeHarness::new().with_kind(HarnessKind::Codex))),
    );
    let restarted = Arc::new(
        ThreadManager::open(
            Arc::clone(&fixture.journal),
            catalog,
            HostId::new("host_import"),
            ThreadManagerOptions {
                tick_interval: Duration::from_secs(3600),
                ..ThreadManagerOptions::default()
            },
            None,
        )
        .await
        .expect("restarted manager"),
    );
    let threads = restarted.list_threads(&ThreadFilter::default()).await;
    let claude = threads
        .iter()
        .find(|thread| thread.harness == HarnessKind::ClaudeCode)
        .expect("imported thread");
    assert_eq!(claude.tab, ThreadTab::Done, "Done survives a restart");
    assert!(!claude.needs_attention, "an import raises no hand");
    assert!(!claude.unread);
    assert_eq!(
        claude.agent_state,
        AgentState::Done,
        "an imported thread stays finished after a restart"
    );
    assert_eq!(
        claude.last_activity.to_rfc3339(),
        "2026-11-18T09:02:00+00:00",
        "the sidebar sorts by the newest imported message, not the write time"
    );

    let snapshot = restarted
        .get_thread(&claude.thread_id, 0)
        .await
        .expect("snapshot");
    let first = snapshot
        .events
        .iter()
        .find_map(|entry| match &entry.event {
            Event::ItemCompleted {
                item: h4rn_protocol::Item::Message { text, .. },
                at: Some(at),
                ..
            } if text == "Implement the feature and keep it quiet" => Some(*at),
            _ => None,
        })
        .expect("the first message with its source time");
    assert_eq!(
        first.to_rfc3339(),
        "2026-11-18T09:00:00+00:00",
        "the message keeps the time it was written"
    );
}

#[tokio::test]
async fn a_prompt_beside_a_large_attachment_is_discovered_and_imported() {
    // Discovery reads records with the same streaming reader the import uses,
    // so a prompt sharing a record with a 5 MiB attachment is not hidden by a
    // fixed line cap before the parser ever sees it.
    let home = tempfile::tempdir().expect("home");
    let home_path = home.path().to_path_buf();
    let workspace = home_path.join("code/api");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let (_, manager, import) = services(
        &home_path,
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let cwd = workspace.display().to_string();
    let padding = "A".repeat(5 * 1024 * 1024);
    write_jsonl(
        &home_path
            .join(".claude/projects/-code-api")
            .join("session-large.jsonl"),
        &[json!({
            "type": "user",
            "sessionId": "large-session",
            "cwd": cwd,
            "timestamp": "2026-11-18T09:00:00Z",
            "message": {
                "role": "user",
                "content": [
                    {"type": "text", "text": "Summarise the screenshot"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": padding}},
                ],
            },
        })],
    );

    let scan = import.scan().await;
    assert_eq!(scan.projects.len(), 1, "{:?}", scan.warnings);
    assert_eq!(
        scan.projects[0].conversations, 1,
        "the conversation beside the attachment is offered"
    );
    let mut events = import.subscribe();
    import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    wait_finished(&mut events).await;

    let thread = manager
        .list_threads(&ThreadFilter::default())
        .await
        .into_iter()
        .find(|thread| thread.harness == HarnessKind::ClaudeCode)
        .expect("imported thread");
    let snapshot = manager
        .get_thread(&thread.thread_id, 0)
        .await
        .expect("snapshot");
    assert!(
        snapshot.events.iter().any(|entry| matches!(
            &entry.event,
            Event::ItemCompleted {
                item: h4rn_protocol::Item::Message { text, .. },
                ..
            } if text == "Summarise the screenshot"
        )),
        "the short prompt beside the attachment reaches the import"
    );
}

#[tokio::test]
async fn two_differently_named_copies_with_a_late_session_id_import_once() {
    // The session id sits in a record past the scan's head, so each copy's
    // provisional identity is its own file name. The full parse finds the id,
    // and the claim must use that identity: the conversation imports once.
    let home = tempfile::tempdir().expect("home");
    let home_path = home.path().to_path_buf();
    let workspace = home_path.join("code/api");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let (_, manager, import) = services(
        &home_path,
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let cwd = workspace.display().to_string();
    let transcript = || {
        let mut records = Vec::new();
        for index in 0..250 {
            let kind = if index % 2 == 0 { "user" } else { "assistant" };
            let mut record = json!({
                "type": kind,
                "cwd": cwd,
                "timestamp": "2026-11-18T09:00:00Z",
                "message": {
                    "role": kind,
                    "content": [{"type": "text", "text": format!("message {index}")}],
                },
            });
            if kind == "assistant" {
                record["message"]["model"] = json!("claude-sonnet-5");
            }
            if index == 249 {
                record["sessionId"] = json!("late-session");
            }
            records.push(record);
        }
        records
    };
    let directory = home_path.join(".claude/projects/-code-api");
    write_jsonl(&directory.join("copy-one.jsonl"), &transcript());
    write_jsonl(&directory.join("copy-two.jsonl"), &transcript());

    let scan = import.scan().await;
    assert_eq!(
        scan.projects[0].conversations, 2,
        "the head names no session, so both copies are offered"
    );
    let mut events = import.subscribe();
    import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    wait_finished(&mut events).await;

    let threads = manager
        .list_threads(&ThreadFilter::default())
        .await
        .into_iter()
        .filter(|thread| thread.harness == HarnessKind::ClaudeCode)
        .count();
    assert_eq!(threads, 1, "the two copies are one conversation");
}

#[tokio::test]
async fn an_imported_session_with_a_late_id_is_not_offered_again() {
    // The scan's head read stops before the record that names the session, so
    // its provisional identity is the file name. The import claims the
    // conversation under the full-parse identity. A later scan has to recognise
    // the transcript anyway, or the session is offered a second time and only
    // skipped after the person selects it again.
    let home = tempfile::tempdir().expect("home");
    let home_path = home.path().to_path_buf();
    let workspace = home_path.join("code/api");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let (_, manager, import) = services(
        &home_path,
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let cwd = workspace.display().to_string();
    let mut records = Vec::new();
    for index in 0..250 {
        let kind = if index % 2 == 0 { "user" } else { "assistant" };
        let mut record = json!({
            "type": kind,
            "cwd": cwd,
            "timestamp": "2026-11-18T09:00:00Z",
            "message": {
                "role": kind,
                "content": [{"type": "text", "text": format!("message {index}")}],
            },
        });
        if kind == "assistant" {
            record["message"]["model"] = json!("claude-sonnet-5");
        }
        if index == 249 {
            record["sessionId"] = json!("late-session");
        }
        records.push(record);
    }
    write_jsonl(
        &home_path
            .join(".claude/projects/-code-api")
            .join("late-id.jsonl"),
        &records,
    );

    let scan = import.scan().await;
    assert_eq!(scan.projects.len(), 1, "{:?}", scan.warnings);
    assert_eq!(
        scan.projects[0].importable, 1,
        "the conversation is offered once"
    );
    let mut events = import.subscribe();
    import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    wait_finished(&mut events).await;
    assert_eq!(
        manager
            .list_threads(&ThreadFilter::default())
            .await
            .into_iter()
            .filter(|thread| thread.harness == HarnessKind::ClaudeCode)
            .count(),
        1,
        "the conversation became one thread"
    );

    let scan = import.scan().await;
    assert_eq!(
        scan.projects[0].importable, 0,
        "an imported session is not offered again"
    );
    assert_eq!(
        scan.projects[0].conversations, 1,
        "the transcript still counts for its project"
    );
}

#[tokio::test]
async fn a_transcript_replaced_by_another_session_is_offered() {
    // A stored claim is about the conversation, not the file. When the same
    // path now holds a transcript that names a different session id, that is
    // another conversation and has to be offered, not hidden by the old claim.
    let home = tempfile::tempdir().expect("home");
    let home_path = home.path().to_path_buf();
    let workspace = home_path.join("code/api");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let (_, manager, import) = services(
        &home_path,
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let transcript = |session: &str, prompt: &str| {
        vec![
            json!({
                "type": "user",
                "sessionId": session,
                "cwd": workspace.display().to_string(),
                "timestamp": "2026-11-18T09:00:00Z",
                "message": {"role": "user", "content": [{"type": "text", "text": prompt}]},
            }),
            json!({
                "type": "assistant",
                "sessionId": session,
                "cwd": workspace.display().to_string(),
                "timestamp": "2026-11-18T09:00:01Z",
                "message": {
                    "role": "assistant",
                    "model": "claude-sonnet-5",
                    "content": [{"type": "text", "text": "done"}],
                },
            }),
        ]
    };
    let path = home_path
        .join(".claude/projects/-code-api")
        .join("conversation.jsonl");
    write_jsonl(&path, &transcript("session-first", "first question"));

    let scan = import.scan().await;
    assert_eq!(
        scan.projects[0].importable, 1,
        "the conversation is offered"
    );
    let mut events = import.subscribe();
    import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    wait_finished(&mut events).await;

    let scan = import.scan().await;
    assert_eq!(
        scan.projects[0].importable, 0,
        "an unchanged transcript is not offered again"
    );

    // The same path is now another conversation.
    write_jsonl(&path, &transcript("session-second", "replacement question"));
    let scan = import.scan().await;
    assert_eq!(
        scan.projects[0].importable, 1,
        "a different explicit session id at the same path is a new conversation"
    );
    assert_eq!(
        scan.projects[0].conversations, 1,
        "the folder still holds one transcript"
    );
    assert!(
        manager
            .list_threads(&ThreadFilter::default())
            .await
            .iter()
            .any(|thread| thread.title.contains("first question")),
        "the first conversation stayed"
    );

    let mut events = import.subscribe();
    import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    wait_finished(&mut events).await;
    let threads: Vec<_> = manager
        .list_threads(&ThreadFilter::default())
        .await
        .into_iter()
        .filter(|thread| thread.harness == HarnessKind::ClaudeCode)
        .collect();
    assert_eq!(threads.len(), 2, "the replacement became its own thread");
    assert!(
        threads
            .iter()
            .any(|thread| thread.title.contains("replacement question")),
        "the replacement's prompt titles it"
    );
}

#[tokio::test]
async fn a_path_keyed_rollout_that_grows_is_offered_again() {
    // A Codex rollout with no session id is keyed by its path, size and content
    // fingerprint. Once imported, a later scan must not hide the file by path
    // forever: a rollout that keeps growing is a new source, exactly as the
    // pre-path scan treated it.
    let home = tempfile::tempdir().expect("home");
    let home_path = home.path().to_path_buf();
    let workspace = home_path.join("code/api");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let (_, manager, import) = services(
        &home_path,
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let cwd = workspace.display().to_string();
    let rollout = home_path
        .join(".codex/sessions/2026/11/21")
        .join("rollout-2026-11-21T09-00-00.jsonl");
    let mut records = codex_transcript();
    records[0]["payload"]
        .as_object_mut()
        .expect("session meta")
        .remove("id");
    for record in &mut records {
        if let Some(payload) = record.get_mut("payload")
            && payload.get("cwd").is_some()
        {
            payload["cwd"] = json!(cwd);
        }
    }
    write_jsonl(&rollout, &records);

    let scan = import.scan().await;
    assert_eq!(scan.projects.len(), 1, "{:?}", scan.warnings);
    assert_eq!(
        scan.projects[0].importable, 1,
        "the id-less rollout is offered"
    );
    let mut events = import.subscribe();
    import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    wait_finished(&mut events).await;
    assert_eq!(
        manager.list_threads(&ThreadFilter::default()).await.len(),
        1,
        "the rollout became a thread"
    );

    let scan = import.scan().await;
    assert_eq!(
        scan.projects[0].importable, 0,
        "an unchanged rollout is not offered again"
    );

    // The same rollout is still being written to.
    records.push(json!({
        "timestamp": "2026-11-21T10:00:00Z",
        "type": "event_msg",
        "payload": {"type": "user_message", "message": "one more thing"},
    }));
    write_jsonl(&rollout, &records);
    let scan = import.scan().await;
    assert_eq!(
        scan.projects[0].importable, 1,
        "a grown rollout is offered again"
    );
    assert_eq!(
        scan.projects[0].conversations, 1,
        "the grown file is still one conversation"
    );
}

#[tokio::test]
async fn a_transcript_over_four_gigabytes_is_skipped_with_a_warning() {
    let fixture = fixture(
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    let huge = fixture
        .home
        .join(".codex/sessions/2026/11/20/rollout-huge.jsonl");
    std::fs::create_dir_all(huge.parent().expect("dir")).expect("dir");
    let file = std::fs::File::create(&huge).expect("file");
    file.set_len(h4rn_daemon::import::MAX_FILE_BYTES + 1)
        .expect("sparse size");
    drop(file);

    let scan = fixture.import.scan().await;
    assert!(
        scan.warnings
            .iter()
            .any(|warning| warning.contains("larger than 4 GiB")),
        "{:?}",
        scan.warnings
    );
    assert_eq!(
        scan.projects.len(),
        1,
        "the valid conversations are still offered"
    );
}

#[tokio::test]
async fn a_token_in_a_prompt_is_redacted_before_it_reaches_the_journal() {
    let home = tempfile::tempdir().expect("home");
    let home_path = home.path().to_path_buf();
    let workspace = home_path.join("code/api");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let (_, manager, import) = services(
        &home_path,
        FakeHarness::new(),
        FakeHarness::new().with_kind(HarnessKind::Codex),
    )
    .await;
    // Built at runtime so the repository never contains a literal token.
    let token = format!("{}{}{}", "gh", "p", "_0123456789abcdefghij");
    write_jsonl(
        &home_path.join(".claude/projects/api").join("secret.jsonl"),
        &claude_conversation(
            &workspace,
            "secret-session",
            &format!("my token is {token}"),
        ),
    );
    let scan = import.scan().await;
    let mut events = import.subscribe();
    import
        .start(vec![scan.projects[0].key.clone()])
        .await
        .expect("start");
    wait_finished(&mut events).await;
    let thread = manager
        .list_threads(&ThreadFilter::default())
        .await
        .into_iter()
        .find(|thread| thread.harness == HarnessKind::ClaudeCode)
        .expect("thread");
    let snapshot = manager
        .get_thread(&thread.thread_id, 0)
        .await
        .expect("snapshot");
    let text: String = snapshot
        .events
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::ItemCompleted {
                item: h4rn_protocol::Item::Message { text, .. },
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        !text.contains(&token),
        "the token never reaches the journal"
    );
    assert!(text.contains("[redacted]"));
}
