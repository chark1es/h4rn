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
use h4rn_protocol::{HarnessKind, HostId, ImportEvent, MessageRole, ThreadFilter};
use serde_json::json;

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

struct Fixture {
    _home: tempfile::TempDir,
    home: PathBuf,
    workspace: PathBuf,
    import: ImportService,
    manager: Arc<ThreadManager>,
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
        ProjectService::open_with_home(&home_path.join("data"), host.clone(), home_path.clone())
            .expect("projects"),
    );
    let import = ImportService::with_homes(
        Arc::clone(&journal),
        projects,
        Arc::clone(&manager),
        host.clone(),
        home_path.clone(),
        home_path.join(".claude"),
        home_path.join(".codex"),
    );

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
    }
}

async fn wait_finished(events: &mut tokio::sync::broadcast::Receiver<ImportEvent>) -> ImportEvent {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            let event = events.recv().await.expect("import event");
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
