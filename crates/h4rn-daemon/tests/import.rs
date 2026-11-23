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
    Event, HarnessKind, HostId, ImportEvent, MessageRole, ThreadFilter, ThreadTab,
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
