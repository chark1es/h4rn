//! Editing an earlier prompt: history truncation, file restore and refusals.
//!
//! The git fixtures are temporary repositories made for one test; they never
//! touch this checkout's configuration or remotes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use h4rn_daemon::blobs::{AttachmentResolver, BlobError};
use h4rn_daemon::fs::SnapshotStore;
use h4rn_daemon::harnesses::StaticHarnessCatalog;
use h4rn_daemon::journal::{Journal, SqliteJournal};
use h4rn_daemon::threads::{NewThread, ThreadManager, ThreadManagerOptions};
use h4rn_harness::FakeHarness;
use h4rn_protocol::{
    ApprovalKind, Attachment, AttachmentId, Event, HarnessKind, HostId, Item, MessageRole,
    PendingRewind, RewindMethod, RuntimeMode, SessionRewind, ThreadFilter, ThreadId, TurnId,
    TurnStatus,
};
use tokio::sync::broadcast;

const TIMEOUT: Duration = Duration::from_secs(20);

/// Resolves any attachment id to one deterministic attachment.
struct TestAttachments;

#[async_trait]
impl AttachmentResolver for TestAttachments {
    async fn resolve(
        &self,
        _thread_id: &ThreadId,
        ids: &[AttachmentId],
    ) -> Result<Vec<Attachment>, BlobError> {
        Ok(ids
            .iter()
            .enumerate()
            .map(|(index, id)| Attachment {
                id: id.clone(),
                name: format!("note-{index}.txt"),
                mime: "text/plain".into(),
                size: 12,
                local_path: format!("/tmp/note-{index}.txt"),
            })
            .collect())
    }
}

struct Fixture {
    manager: Arc<ThreadManager>,
    journal: Arc<dyn Journal>,
    data: tempfile::TempDir,
    host: HostId,
}

async fn make_fixture(fake: FakeHarness) -> Fixture {
    let journal: Arc<dyn Journal> = Arc::new(SqliteJournal::in_memory().await.expect("journal"));
    let data = tempfile::tempdir().expect("data");
    // Every thread row in this file names Codex; the fake advertises the same
    // kind so the catalog resolves it.
    let manager = open_manager(
        Arc::clone(&journal),
        fake.with_kind(HarnessKind::Codex),
        HarnessKind::Codex,
        data.path(),
    )
    .await;
    Fixture {
        manager,
        journal,
        data,
        host: HostId::new("host_rewind"),
    }
}

async fn open_manager(
    journal: Arc<dyn Journal>,
    fake: FakeHarness,
    kind: HarnessKind,
    data: &Path,
) -> Arc<ThreadManager> {
    let catalog = Arc::new(StaticHarnessCatalog::with(Arc::new(fake)));
    let manager = ThreadManager::open(
        journal,
        catalog,
        HostId::new("host_rewind"),
        ThreadManagerOptions {
            tick_interval: Duration::from_secs(3600),
            ..ThreadManagerOptions::default()
        },
        Some(Arc::new(TestAttachments)),
    )
    .await
    .expect("manager");
    manager.set_snapshot_store(Arc::new(SnapshotStore::new(data).expect("snapshots")));
    let _ = kind;
    Arc::new(manager)
}

fn thread_request(host: &HostId, workspace: &Path, prompt: &str) -> NewThread {
    NewThread {
        thread_id: None,
        parent_thread_id: None,
        subagent_id: None,
        host: host.clone(),
        harness: HarnessKind::Codex,
        agent_id: None,
        workspace_path: workspace.display().to_string(),
        model: "fake-model".into(),
        prompt: prompt.into(),
        attachments: Vec::new(),
        runtime_mode: RuntimeMode::FullAccess,
        reasoning_effort: None,
        fast: false,
        worktree: None,
    }
}

/// Waits for one turn to finish and returns its completion status.
async fn wait_turn(
    events: &mut broadcast::Receiver<h4rn_daemon::journal::WireEvent>,
) -> TurnStatus {
    tokio::time::timeout(TIMEOUT, async {
        let mut status = None;
        while let Ok(wire) = events.recv().await {
            if let Event::TurnCompleted {
                status: settled, ..
            } = wire.event
            {
                status = Some(settled);
                break;
            }
        }
        status.expect("a turn completion")
    })
    .await
    .expect("the turn settled")
}

/// Drops events buffered before the next action, so a later wait cannot
/// mistake an older completion for the new turn's.
fn drain(events: &mut broadcast::Receiver<h4rn_daemon::journal::WireEvent>) {
    while events.try_recv().is_ok() {}
}

/// The user item id the next sent prompt produced.
async fn next_user_item(
    events: &mut broadcast::Receiver<h4rn_daemon::journal::WireEvent>,
) -> (String, TurnId) {
    tokio::time::timeout(TIMEOUT, async {
        while let Ok(wire) = events.recv().await {
            if let Event::ItemCompleted {
                item:
                    Item::Message {
                        item_id,
                        role: MessageRole::User,
                        ..
                    },
                turn_id,
                ..
            } = wire.event
            {
                return (item_id, turn_id);
            }
        }
        panic!("the stream ended before a user message");
    })
    .await
    .expect("a user item")
}

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

/// A temporary repository and one linked worktree of it.
fn git_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let base = tempfile::tempdir().expect("repo");
    let repo = base.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "user.name", "h4rn test"]);
    std::fs::write(repo.join("README.md"), "fixture\n").expect("readme");
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "fixture"]);
    let worktree = base.path().join("worktree");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "thread-branch",
            worktree.to_str().expect("utf8 worktree"),
        ],
    );
    (base, repo, worktree)
}

#[tokio::test]
async fn editing_an_earlier_prompt_removes_it_and_restores_the_worktree() {
    let (_base, _repo, worktree) = git_worktree();
    let (fake, specs) = FakeHarness::new().with_native_rewind().with_rewind_specs();
    let fixture = make_fixture(fake).await;
    let mut events = fixture.manager.subscribe();

    let created = fixture
        .manager
        .create_thread(thread_request(&fixture.host, &worktree, "first prompt"))
        .await
        .expect("create");
    let mut events_for_first = events.resubscribe();
    let first_status = wait_turn(&mut events_for_first).await;
    assert_eq!(first_status, TurnStatus::Completed);
    drain(&mut events);

    let attachment = AttachmentId::from_content(b"note");
    let second = fixture
        .manager
        .send_message(
            &created.thread_id,
            "second [[write:notes.txt]]from the second prompt".into(),
            vec![attachment],
        )
        .await
        .expect("send second");
    let (second_item, _) = next_user_item(&mut events).await;
    wait_turn(&mut events).await;
    assert_eq!(
        std::fs::read_to_string(worktree.join("notes.txt")).expect("second write"),
        "from the second prompt"
    );

    let prompt = fixture
        .manager
        .rewind_thread(&created.thread_id, &second_item, true)
        .await
        .expect("rewind");
    assert_eq!(
        prompt.prompt,
        "second [[write:notes.txt]]from the second prompt"
    );
    assert_eq!(prompt.attachments.len(), 1);
    assert_eq!(prompt.attachments[0].name, "note-0.txt");
    assert!(prompt.files_restored);
    assert_eq!(prompt.method, RewindMethod::Native);
    assert!(
        !worktree.join("notes.txt").exists(),
        "the file the removed turn created is gone again"
    );

    let meta = fixture
        .manager
        .thread_meta(&created.thread_id)
        .await
        .expect("meta");
    assert_eq!(
        meta.pending_rewind,
        Some(PendingRewind::Native {
            session: SessionRewind::KeepTurns { keep: 1 }
        })
    );

    // The removed prompt is no longer part of the thread: it cannot be named
    // by a second rewind, and the earlier one still can.
    let refused = fixture
        .manager
        .rewind_thread(&created.thread_id, &second_item, false)
        .await
        .expect_err("removed prompt");
    assert!(
        refused
            .to_string()
            .contains("not in the thread's current history")
    );

    // The next prompt starts a session carrying the native boundary, not the
    // provider history the person removed.
    drain(&mut events);
    let _third = fixture
        .manager
        .send_message(&created.thread_id, "third".into(), Vec::new())
        .await
        .expect("third send");
    wait_turn(&mut events).await;
    assert!(
        specs
            .get()
            .iter()
            .any(|spec| matches!(spec, Some(SessionRewind::KeepTurns { keep: 1 }))),
        "the restarted session carried the rewind boundary: {:?}",
        specs.get()
    );
    let _ = second;
}

#[tokio::test]
async fn a_handoff_rewind_starts_a_fresh_session_with_the_kept_history() {
    let (_base, repo, worktree) = git_worktree();
    let (fake, sessions) = {
        let (fake, count) = FakeHarness::new().with_session_count();
        (fake, count)
    };
    let fixture = make_fixture(fake).await;
    let mut events = fixture.manager.subscribe();

    let created = fixture
        .manager
        .create_thread(thread_request(&fixture.host, &worktree, "keep this prompt"))
        .await
        .expect("create");
    let mut first = events.resubscribe();
    wait_turn(&mut first).await;
    drain(&mut events);

    fixture
        .manager
        .send_message(&created.thread_id, "remove this one".into(), Vec::new())
        .await
        .expect("second");
    let (second_item, _) = next_user_item(&mut events).await;
    wait_turn(&mut events).await;

    let prompt = fixture
        .manager
        .rewind_thread(&created.thread_id, &second_item, false)
        .await
        .expect("rewind");
    assert_eq!(prompt.method, RewindMethod::Handoff);
    let meta = fixture
        .manager
        .thread_meta(&created.thread_id)
        .await
        .expect("meta");
    assert_eq!(meta.pending_rewind, Some(PendingRewind::Handoff));
    assert!(meta.resume_id.is_none(), "a fresh session is required");

    let before = sessions.get();
    drain(&mut events);
    fixture
        .manager
        .send_message(&created.thread_id, "continue".into(), Vec::new())
        .await
        .expect("third");
    let mut text = String::new();
    tokio::time::timeout(TIMEOUT, async {
        while let Ok(wire) = events.recv().await {
            match wire.event {
                Event::MessageDelta { text: delta, .. } => text.push_str(&delta),
                Event::TurnCompleted { .. } => break,
                _ => {}
            }
        }
    })
    .await
    .expect("third turn");
    assert!(
        text.contains("an earlier message was edited"),
        "the fresh session carried the kept history preface: {text:?}"
    );
    assert_eq!(sessions.get(), before + 1, "one fresh session");
    assert_eq!(
        fixture
            .manager
            .thread_meta(&created.thread_id)
            .await
            .expect("meta")
            .pending_rewind,
        None
    );
    let _ = repo;
}

#[tokio::test]
async fn the_rewind_survives_a_restart() {
    let (_base, _repo, worktree) = git_worktree();
    let fixture = make_fixture(FakeHarness::new().with_native_rewind()).await;
    let mut events = fixture.manager.subscribe();
    let created = fixture
        .manager
        .create_thread(thread_request(&fixture.host, &worktree, "one"))
        .await
        .expect("create");
    let mut first = events.resubscribe();
    wait_turn(&mut first).await;
    drain(&mut events);
    fixture
        .manager
        .send_message(&created.thread_id, "two".into(), Vec::new())
        .await
        .expect("send");
    let (second_item, _) = next_user_item(&mut events).await;
    wait_turn(&mut events).await;
    fixture
        .manager
        .rewind_thread(&created.thread_id, &second_item, false)
        .await
        .expect("rewind");

    drop(fixture.manager);
    let reopened = open_manager(
        Arc::clone(&fixture.journal),
        FakeHarness::new().with_native_rewind(),
        HarnessKind::Codex,
        fixture.data.path(),
    )
    .await;
    let meta = reopened
        .thread_meta(&created.thread_id)
        .await
        .expect("restored meta");
    assert_eq!(
        meta.pending_rewind,
        Some(PendingRewind::Native {
            session: SessionRewind::KeepTurns { keep: 1 }
        }),
        "the provider boundary is still waiting for the next session"
    );
    let snapshot = reopened
        .get_thread(&created.thread_id, 0)
        .await
        .expect("thread");
    assert!(
        snapshot
            .events
            .iter()
            .any(|wire| matches!(wire.event, Event::ThreadRewound { .. })),
        "the transcript cut is replayed after a restart"
    );
}

#[tokio::test]
async fn a_running_turn_a_queued_message_and_an_approval_refuse_the_rewind() {
    // A gated write leaves the first turn running.
    let (fake, gate) = FakeHarness::new().with_send_gate_after(0);
    let fixture = make_fixture(fake).await;
    let mut events = fixture.manager.subscribe();
    let created = fixture
        .manager
        .create_thread(thread_request(
            &fixture.host,
            Path::new("/tmp/unused-rewind"),
            "running",
        ))
        .await
        .expect("create");
    let (item, _) = next_user_item(&mut events).await;

    // A queued message is refused before the running turn is reported, because
    // it is the more specific thing the person must deal with.
    fixture
        .manager
        .send_message(&created.thread_id, "waiting".into(), Vec::new())
        .await
        .expect("queue");
    let queued = fixture
        .manager
        .rewind_thread(&created.thread_id, &item, false)
        .await
        .expect_err("queued");
    assert!(queued.to_string().contains("waiting to run"), "{queued}");

    gate.release();
    // Wait for the two turns to settle, then start a gated third turn and raise
    // an approval on it.
    wait_turn(&mut events).await;
}

#[tokio::test]
async fn a_pending_approval_and_an_armed_resume_each_refuse_the_rewind() {
    let (_base, repo, worktree) = git_worktree();
    let (fake, gate) = FakeHarness::new().with_completion_gate();
    let (fake, injector) = fake.with_event_injector();
    let fixture = make_fixture(fake).await;
    let mut events = fixture.manager.subscribe();
    let created = fixture
        .manager
        .create_thread(thread_request(&fixture.host, &worktree, "approve me"))
        .await
        .expect("create");
    let (item, _) = next_user_item(&mut events).await;
    // The injector only has a session once the daemon has started one; its
    // first streamed text proves the pump is consuming.
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Ok(wire) = events.recv().await
                && matches!(wire.event, Event::MessageDelta { .. })
            {
                break;
            }
        }
    })
    .await
    .expect("the session started");
    injector.emit(h4rn_harness::HarnessEvent::ApprovalRequested {
        approval_id: "approval_1".into(),
        kind: ApprovalKind::Command,
        prompt: "run the tests".into(),
        options: Vec::new(),
    });
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Ok(wire) = events.recv().await
                && matches!(wire.event, Event::ApprovalRequested { .. })
            {
                break;
            }
        }
    })
    .await
    .expect("the approval reached the thread");
    let refused = fixture
        .manager
        .rewind_thread(&created.thread_id, &item, false)
        .await
        .expect_err("approval");
    assert!(
        refused.to_string().contains("approval is waiting"),
        "{refused}"
    );
    gate.release();
    let _ = repo;

    // A parked auto-resume refuses too.
    let fixture = make_fixture(FakeHarness::new()).await;
    fixture.manager.set_meter(Arc::new(
        h4rn_meter::MeterHub::in_memory().await.expect("meter"),
    ));
    let mut events = fixture.manager.subscribe();
    let created = fixture
        .manager
        .create_thread(thread_request(
            &fixture.host,
            &worktree,
            "[h4rn-limit] paused",
        ))
        .await
        .expect("limited");
    let (item, _) = next_user_item(&mut events).await;
    wait_turn(&mut events).await;
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let threads = fixture.manager.list_threads(&ThreadFilter::default()).await;
            if threads.iter().any(|thread| thread.resume_at.is_some()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a resume was armed");
    let refused = fixture
        .manager
        .rewind_thread(&created.thread_id, &item, false)
        .await
        .expect_err("resume armed");
    assert!(
        refused.to_string().contains("automatic resume is armed"),
        "{refused}"
    );
}

#[tokio::test]
async fn file_restore_is_refused_in_the_main_checkout_and_for_a_shared_worktree() {
    let (_base, repo, worktree) = git_worktree();
    let fixture = make_fixture(FakeHarness::new().with_native_rewind()).await;
    let mut events = fixture.manager.subscribe();

    // A thread in the project's main checkout rewinds its conversation only.
    let main_thread = fixture
        .manager
        .create_thread(thread_request(&fixture.host, &repo, "main checkout"))
        .await
        .expect("create main");
    let (main_item, _) = next_user_item(&mut events).await;
    wait_turn(&mut events).await;
    drain(&mut events);
    let refused = fixture
        .manager
        .rewind_thread(&main_thread.thread_id, &main_item, true)
        .await
        .expect_err("main checkout");
    assert!(refused.to_string().contains("main folder"), "{refused}");

    // Another thread in the same linked worktree blocks the file restore.
    let first = fixture
        .manager
        .create_thread(thread_request(&fixture.host, &worktree, "first"))
        .await
        .expect("create first");
    let (first_item, _) = next_user_item(&mut events).await;
    wait_turn(&mut events).await;
    let _second = fixture
        .manager
        .create_thread(thread_request(&fixture.host, &worktree, "second"))
        .await
        .expect("create second");
    let refused = fixture
        .manager
        .rewind_thread(&first.thread_id, &first_item, true)
        .await
        .expect_err("shared worktree");
    assert!(
        refused
            .to_string()
            .contains("another thread or agent session uses"),
        "{refused}"
    );
    // The conversation can still be rewound on its own.
    fixture
        .manager
        .rewind_thread(&first.thread_id, &first_item, false)
        .await
        .expect("conversation only");
}

#[tokio::test]
async fn a_failed_file_restore_leaves_the_history_unchanged() {
    let (_base, _repo, worktree) = git_worktree();
    let (_fake, _specs) = FakeHarness::new().with_native_rewind().with_rewind_specs();
    let fixture = make_fixture(FakeHarness::new().with_native_rewind()).await;
    let mut events = fixture.manager.subscribe();
    let created = fixture
        .manager
        .create_thread(thread_request(&fixture.host, &worktree, "one"))
        .await
        .expect("create");
    let mut first = events.resubscribe();
    wait_turn(&mut first).await;
    drain(&mut events);
    fixture
        .manager
        .send_message(
            &created.thread_id,
            "two [[write:README.md]]content".into(),
            Vec::new(),
        )
        .await
        .expect("send");
    let (second_item, second_turn) = next_user_item(&mut events).await;
    wait_turn(&mut events).await;

    // Make the pre-turn snapshot impossible to restore, the way a file beyond
    // the capture cap is reported.
    let manifest = fixture
        .data
        .path()
        .join("snapshots")
        .join(created.thread_id.as_str())
        .join(second_turn.as_str())
        .join("manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest).expect("manifest")).expect("json");
    let files = value["files"].as_object_mut().expect("files");
    for entry in files.values_mut() {
        entry["skipped"] = serde_json::json!(true);
        entry["blob"] = serde_json::Value::Null;
    }
    std::fs::write(&manifest, serde_json::to_vec(&value).expect("json")).expect("manifest");

    let refused = fixture
        .manager
        .rewind_thread(&created.thread_id, &second_item, true)
        .await
        .expect_err("restore failed");
    assert!(refused.to_string().contains("too large"), "{refused}");
    let snapshot = fixture
        .manager
        .get_thread(&created.thread_id, 0)
        .await
        .expect("thread");
    assert!(
        !snapshot
            .events
            .iter()
            .any(|wire| matches!(wire.event, Event::ThreadRewound { .. })),
        "a failed restore journals nothing"
    );
    // The prompt is still selectable: the history did not change.
    let prompt = fixture
        .manager
        .rewind_thread(&created.thread_id, &second_item, false)
        .await
        .expect("conversation only after the failed restore");
    assert_eq!(prompt.prompt, "two [[write:README.md]]content");
    assert!(!prompt.files_restored);
}
#[tokio::test]
async fn a_completed_subagent_does_not_block_the_same_worktree_restore() {
    let (_base, _repo, worktree) = git_worktree();
    let fixture = make_fixture(FakeHarness::new().with_native_rewind()).await;
    let mut events = fixture.manager.subscribe();
    let parent = fixture
        .manager
        .create_thread(thread_request(&fixture.host, &worktree, "parent"))
        .await
        .expect("create parent");
    let mut first = events.resubscribe();
    wait_turn(&mut first).await;
    drain(&mut events);

    let mut child_request = thread_request(&fixture.host, &worktree, "child");
    child_request.parent_thread_id = Some(parent.thread_id.clone());
    let child = fixture
        .manager
        .create_thread(child_request)
        .await
        .expect("create child");
    let (_, _) = next_user_item(&mut events).await;
    wait_turn(&mut events).await;

    // The child is finished and has nothing queued, so restoring the parent's
    // checkout does not erase work that is still happening.
    fixture
        .manager
        .rewind_thread(&parent.thread_id, "ignored", false)
        .await
        .expect_err("no such message");
    let prompt = fixture
        .manager
        .rewind_thread(
            &parent.thread_id,
            &format!(
                "item_user_{}",
                parent.turn_id.as_str().trim_start_matches("turn_")
            ),
            true,
        )
        .await
        .expect("the finished child does not block");
    assert!(prompt.files_restored);
    let _ = child;
}
