//! Integration tests for the IDE-mode filesystem + git service.
//!
//! These exercise the security boundary (traversal, symlink escape), the
//! optimistic write contract, streamed search, git `HEAD` diffs, per-turn
//! snapshots and the watcher. Every test builds its own temporary workspace;
//! none touches the real data directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use h4rn_daemon::fs::{FsError, FsService};
use h4rn_protocol::{DiffAgainst, FsCreateKind, FsFileContent, ThreadId, TurnId};

/// A temporary workspace plus daemon data directory.
struct Fixture {
    dir: tempfile::TempDir,
    workspace: PathBuf,
    data: PathBuf,
    service: FsService,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        let data = dir.path().join("data");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&data).expect("data");
        let service = FsService::new(data.clone()).expect("service");
        service.allow_root(&workspace).expect("allow root");
        Self {
            dir,
            workspace,
            data,
            service,
        }
    }

    fn workspace(&self) -> &str {
        self.workspace.to_str().expect("utf-8 workspace")
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.workspace.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, content).expect("write");
    }
}

fn git(workspace: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_repo(fixture: &Fixture) {
    git(&fixture.workspace, &["init", "-q"]);
    git(
        &fixture.workspace,
        &["config", "user.email", "test@h4rn.dev"],
    );
    git(&fixture.workspace, &["config", "user.name", "h4rn test"]);
    git(&fixture.workspace, &["add", "."]);
    git(&fixture.workspace, &["commit", "-q", "-m", "initial"]);
}

#[test]
fn traversal_is_rejected_before_touching_disk() {
    let fixture = Fixture::new();
    fixture.write("inside.txt", "ok");

    for path in ["../outside.txt", "src/../../outside.txt", "/etc/passwd"] {
        assert!(
            matches!(
                fixture.service.read(fixture.workspace(), path),
                Err(FsError::Forbidden(_) | FsError::NotFound(_))
            ),
            "{path:?} should not resolve"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlink_escape_is_rejected() {
    let fixture = Fixture::new();
    let outside = fixture.dir.path().join("secret.txt");
    fs::write(&outside, "top secret").expect("outside file");
    std::os::unix::fs::symlink(&outside, fixture.workspace.join("escape.txt")).expect("symlink");

    let error = fixture
        .service
        .read(fixture.workspace(), "escape.txt")
        .expect_err("symlink escape must fail");
    assert!(matches!(error, FsError::Forbidden(_)), "got {error:?}");
}

#[cfg(unix)]
#[test]
fn diff_does_not_follow_a_symlink_outside_the_workspace() {
    // A diff may report a path it cannot safely read as "missing", but it must
    // never follow a link out of the workspace and expose the target's bytes.
    let fixture = Fixture::new();
    let outside = fixture.dir.path().join("secret.txt");
    fs::write(&outside, "top secret\n").expect("outside file");
    std::os::unix::fs::symlink(&outside, fixture.workspace.join("escape.txt")).expect("symlink");

    let diff = fixture
        .service
        .diff(fixture.workspace(), "escape.txt", &DiffAgainst::Head)
        .expect("an escaping symlink is reported as missing, not an error");
    assert_eq!(diff.path, "escape.txt");
    assert!(
        diff.after.is_none(),
        "the bytes outside the workspace must not be read: {diff:?}"
    );
}

#[cfg(unix)]
#[test]
fn turn_changes_does_not_follow_a_symlink_out_of_the_workspace() {
    let fixture = Fixture::new();
    let thread = ThreadId::new("th_escape");
    let turn = TurnId::new("turn_escape");
    let snapshots = fixture.service.snapshots();
    // The turn announces a file that does not exist yet, so the snapshot
    // records its absence and the diff would treat everything as added.
    snapshots
        .capture(
            &fixture.workspace,
            &thread,
            &turn,
            &["escape.txt".to_owned()],
        )
        .expect("capture");

    // The path becomes a link to a file outside the workspace after capture.
    let outside = fixture.dir.path().join("secret.txt");
    fs::write(&outside, "top secret\n").expect("outside file");
    std::os::unix::fs::symlink(&outside, fixture.workspace.join("escape.txt")).expect("symlink");

    let changes = fixture
        .service
        .turn_changes(fixture.workspace(), &thread, &turn)
        .expect("turn changes");
    assert!(
        changes.files.is_empty(),
        "content outside the workspace must never be reported: {:?}",
        changes.files
    );
}

#[test]
fn listing_respects_gitignore_and_reports_statuses() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join(".gitignore"), "ignored.txt\n").expect("gitignore");
    fixture.write("tracked.txt", "one\n");
    git_repo(&fixture);
    fixture.write("tracked.txt", "two\n");
    fixture.write("new.txt", "fresh\n");
    fixture.write("ignored.txt", "nope\n");

    let listing = fixture
        .service
        .list(fixture.workspace(), "")
        .expect("list root");
    let names: Vec<&str> = listing
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert!(names.contains(&"tracked.txt"), "{names:?}");
    assert!(names.contains(&"new.txt"), "{names:?}");
    assert!(!names.contains(&"ignored.txt"), "{names:?}");

    let tracked = listing
        .entries
        .iter()
        .find(|entry| entry.name == "tracked.txt")
        .expect("tracked entry");
    assert_eq!(
        tracked.git_status,
        Some(h4rn_protocol::GitFileStatus::Modified)
    );
    let new = listing
        .entries
        .iter()
        .find(|entry| entry.name == "new.txt")
        .expect("new entry");
    assert_eq!(
        new.git_status,
        Some(h4rn_protocol::GitFileStatus::Untracked)
    );
}

#[test]
fn binary_reads_refuse_with_a_hint_and_keep_an_etag() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("blob.bin"), [0u8, 159, 146, 150]).expect("binary");
    let file: FsFileContent = fixture
        .service
        .read(fixture.workspace(), "blob.bin")
        .expect("read");
    assert!(file.binary);
    assert!(file.content.is_none());
    assert!(file.etag.is_some());
    assert!(file.hint.expect("hint").contains("binary"));
}

#[test]
fn write_conflict_returns_current_content_instead_of_clobbering() {
    let fixture = Fixture::new();
    fixture.write("doc.md", "original\n");
    let opened = fixture
        .service
        .read(fixture.workspace(), "doc.md")
        .expect("read");
    let first = fixture
        .service
        .write(
            fixture.workspace(),
            "doc.md",
            "mine\n",
            opened.etag.as_deref(),
            Some("test"),
        )
        .expect("write");
    assert!(first.conflict.is_none());

    // Someone else saves between the client's read and its write.
    fs::write(fixture.workspace.join("doc.md"), "theirs\n").expect("theirs");
    let conflict = fixture
        .service
        .write(
            fixture.workspace(),
            "doc.md",
            "mine again\n",
            Some(first.etag.as_str()),
            Some("test"),
        )
        .expect("write");
    let conflict = conflict.conflict.expect("conflict payload");
    assert_eq!(conflict.current_content.as_deref(), Some("theirs\n"));
    assert_ne!(conflict.current_etag.as_deref(), Some(first.etag.as_str()));
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("doc.md")).expect("file"),
        "theirs\n",
    );
}

#[test]
fn creates_deletes_and_restores_round_trip() {
    let fixture = Fixture::new();
    let entry = fixture
        .service
        .create(fixture.workspace(), "src/new.rs", FsCreateKind::File, None)
        .expect("create");
    assert_eq!(entry.path, "src/new.rs");
    let deleted = fixture
        .service
        .delete(fixture.workspace(), "src/new.rs", None)
        .expect("delete");
    assert!(!fixture.workspace.join("src/new.rs").exists());
    fixture
        .service
        .restore(fixture.workspace(), &deleted.trash_id, None)
        .expect("restore");
    assert!(fixture.workspace.join("src/new.rs").exists());

    let audit = fs::read_to_string(fixture.data.join("audit.jsonl")).expect("audit log");
    assert!(audit.contains("\"action\":\"delete\""), "{audit}");
    assert!(audit.contains("\"action\":\"restore\""), "{audit}");
}

#[test]
fn diff_against_head_shows_both_sides() {
    let fixture = Fixture::new();
    fixture.write("src/main.rs", "fn main() {}\n");
    git_repo(&fixture);
    fixture.write("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n");

    let diff = fixture
        .service
        .diff(fixture.workspace(), "src/main.rs", &DiffAgainst::Head)
        .expect("diff");
    assert_eq!(diff.label, "HEAD");
    assert_eq!(diff.before.as_deref(), Some("fn main() {}\n"));
    assert_eq!(
        diff.after.as_deref(),
        Some("fn main() {\n    println!(\"hi\");\n}\n")
    );
    assert!(diff.note.is_none());
}

#[test]
fn a_deleted_file_diffs_against_head_as_a_deletion() {
    // I4: the diff resolved the target with `Existing`, so a file the IDE was
    // asked to show after deletion failed instead of showing the deletion.
    let fixture = Fixture::new();
    fixture.write("src/gone.rs", "fn gone() {}\n");
    git_repo(&fixture);
    // Remove the parent too, so resolution must walk to an existing ancestor.
    fs::remove_dir_all(fixture.workspace.join("src")).expect("delete");

    let diff = fixture
        .service
        .diff(fixture.workspace(), "src/gone.rs", &DiffAgainst::Head)
        .expect("a deleted file must still diff");
    assert_eq!(diff.path, "src/gone.rs");
    assert_eq!(diff.before.as_deref(), Some("fn gone() {}\n"));
    assert!(
        diff.after.is_none(),
        "a deleted file has no current side: {diff:?}"
    );
    assert!(!diff.binary);
}

#[cfg(unix)]
#[test]
fn diff_refuses_a_dangling_symlink_leaf() {
    // A leaf that is itself a dangling symlink is not followed: the diff
    // reports the path as missing, so a deletion still renders.
    let fixture = Fixture::new();
    let outside = fixture.dir.path().join("not-yet.txt");
    std::os::unix::fs::symlink(&outside, fixture.workspace.join("trap.txt")).expect("symlink");

    let diff = fixture
        .service
        .diff(fixture.workspace(), "trap.txt", &DiffAgainst::Head)
        .expect("a dangling symlink is reported as missing, not an error");
    assert_eq!(diff.path, "trap.txt");
    assert!(
        diff.after.is_none(),
        "a dangling link has no readable current side: {diff:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_tracked_symlink_whose_target_was_deleted_diffs_as_a_deletion() {
    // A tracked link keeps its HEAD blob (the link target text) after the
    // target is deleted. Diffing the link must not error just because the
    // link is now dangling.
    let fixture = Fixture::new();
    fixture.write("target.txt", "content\n");
    std::os::unix::fs::symlink("target.txt", fixture.workspace.join("link.txt")).expect("symlink");
    git_repo(&fixture);
    fs::remove_file(fixture.workspace.join("target.txt")).expect("delete the target");

    let diff = fixture
        .service
        .diff(fixture.workspace(), "link.txt", &DiffAgainst::Head)
        .expect("a dangling tracked symlink must still diff");
    assert_eq!(diff.path, "link.txt");
    assert_eq!(diff.before.as_deref(), Some("target.txt"));
    assert!(
        diff.after.is_none(),
        "the link's target is gone, so there is no current side: {diff:?}"
    );
}

#[test]
fn diff_against_an_agent_turn_uses_the_pre_turn_snapshot() {
    let fixture = Fixture::new();
    fixture.write("notes.txt", "before the turn\n");
    let thread = ThreadId::new("th_test");
    let turn = TurnId::new("turn_test");
    let snapshots = fixture.service.snapshots();
    snapshots
        .capture(
            &fixture.workspace,
            &thread,
            &turn,
            &["notes.txt".to_owned()],
        )
        .expect("capture");

    fixture.write("notes.txt", "before the turn\nafter the turn\n");
    let against = DiffAgainst::AgentTurn {
        thread_id: thread.clone(),
        turn_id: turn.clone(),
    };
    let diff = fixture
        .service
        .diff(fixture.workspace(), "notes.txt", &against)
        .expect("diff");
    assert_eq!(diff.before.as_deref(), Some("before the turn\n"));
    assert_eq!(
        diff.after.as_deref(),
        Some("before the turn\nafter the turn\n")
    );

    let changes = fixture
        .service
        .turn_changes(fixture.workspace(), &thread, &turn)
        .expect("turn changes");
    assert_eq!(changes.files.len(), 1);
    assert_eq!(changes.files[0].path, "notes.txt");
    assert_eq!(changes.files[0].added, 1);
    assert_eq!(changes.files[0].removed, 0);
}

#[test]
fn a_file_the_turn_created_diffs_from_nothing() {
    let fixture = Fixture::new();
    let thread = ThreadId::new("th_new");
    let turn = TurnId::new("turn_new");
    let snapshots = fixture.service.snapshots();
    snapshots
        .capture(&fixture.workspace, &thread, &turn, &["new.txt".to_owned()])
        .expect("capture");
    fixture.write("new.txt", "written by the turn\n");

    let diff = fixture
        .service
        .diff(
            fixture.workspace(),
            "new.txt",
            &DiffAgainst::AgentTurn {
                thread_id: thread.clone(),
                turn_id: turn.clone(),
            },
        )
        .expect("diff");
    assert!(
        diff.before.is_none(),
        "a new file has no pre-turn text, so revert can delete it"
    );
    assert_eq!(diff.after.as_deref(), Some("written by the turn\n"));
    assert!(diff.note.is_some());
}

#[test]
fn a_file_deleted_after_the_pre_turn_snapshot_diffs_as_a_deletion() {
    let fixture = Fixture::new();
    fixture.write("notes.txt", "before the turn\n");
    let thread = ThreadId::new("th_deleted");
    let turn = TurnId::new("turn_deleted");
    let snapshots = fixture.service.snapshots();
    snapshots
        .capture(
            &fixture.workspace,
            &thread,
            &turn,
            &["notes.txt".to_owned()],
        )
        .expect("capture");
    fs::remove_file(fixture.workspace.join("notes.txt")).expect("delete");

    let diff = fixture
        .service
        .diff(
            fixture.workspace(),
            "notes.txt",
            &DiffAgainst::AgentTurn {
                thread_id: thread.clone(),
                turn_id: turn.clone(),
            },
        )
        .expect("a deletion must diff against the snapshot");
    assert_eq!(diff.before.as_deref(), Some("before the turn\n"));
    assert!(
        diff.after.is_none(),
        "a deleted file has no current side: {diff:?}"
    );

    let changes = fixture
        .service
        .turn_changes(fixture.workspace(), &thread, &turn)
        .expect("turn changes");
    assert_eq!(changes.files.len(), 1);
    assert_eq!(changes.files[0].path, "notes.txt");
}

#[tokio::test(flavor = "multi_thread")]
async fn search_streams_matches_and_finishes() {
    let fixture = Fixture::new();
    fixture.write("src/a.ts", "const needle = 1;\n");
    fixture.write("src/b.ts", "nothing here\n");
    fixture.write("README.md", "a needle in prose\n");

    let (search_id, mut rx) = fixture
        .service
        .search(
            fixture.workspace(),
            "needle",
            false,
            false,
            Some("*.ts"),
            None,
        )
        .expect("search");
    assert!(search_id.starts_with("search-"));

    let mut paths = Vec::new();
    let mut done = false;
    while let Some(batch) = rx.recv().await {
        for hit in batch.results {
            paths.push(hit.path);
        }
        if batch.done {
            done = true;
            break;
        }
    }
    assert!(done, "search never finished");
    assert_eq!(paths, vec!["src/a.ts".to_owned()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_reports_a_change_once_a_watch_exists() {
    let fixture = Fixture::new();
    fixture.write("watched.txt", "before\n");
    let mut changes = fixture.service.subscribe();
    fixture
        .service
        .watch(fixture.workspace())
        .expect("watch workspace");

    // Give inotify a moment to attach before the write that should be seen.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    fixture.write("watched.txt", "after\n");

    let change = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let change = changes.recv().await.expect("broadcast");
            if change.path == "watched.txt" {
                return change;
            }
        }
    })
    .await
    .expect("watcher event within timeout");
    assert_eq!(change.change, h4rn_protocol::FsChangeKind::Modified);
}

#[cfg(unix)]
#[test]
fn create_refuses_a_dangling_symlink_leaf_that_points_outside() {
    let fixture = Fixture::new();
    let outside = fixture.dir.path().join("not-yet.txt");
    std::os::unix::fs::symlink(&outside, fixture.workspace.join("trap.txt")).expect("symlink");

    let error = fixture
        .service
        .create(
            fixture.workspace(),
            "trap.txt",
            FsCreateKind::File,
            Some("test"),
        )
        .expect_err("dangling symlink must not be followed");
    assert!(matches!(error, FsError::Forbidden(_)), "got {error:?}");
    assert!(
        !outside.exists(),
        "nothing may be created outside the workspace"
    );

    let error = fixture
        .service
        .write(fixture.workspace(), "trap.txt", "pwned", None, Some("test"))
        .expect_err("write through a dangling link must fail");
    assert!(matches!(error, FsError::Forbidden(_)), "got {error:?}");
    assert!(!outside.exists());
}

#[test]
fn saving_a_file_deleted_mid_edit_reports_a_conflict_instead_of_recreating_it() {
    let fixture = Fixture::new();
    fixture.write("gone.md", "original\n");
    let opened = fixture
        .service
        .read(fixture.workspace(), "gone.md")
        .expect("read");
    fs::remove_file(fixture.workspace.join("gone.md")).expect("delete");

    let result = fixture
        .service
        .write(
            fixture.workspace(),
            "gone.md",
            "mine\n",
            opened.etag.as_deref(),
            Some("test"),
        )
        .expect("write");
    let conflict = result.conflict.expect("deleted file must conflict");
    assert_eq!(conflict.current_etag, None);
    assert_eq!(conflict.current_content, None);
    assert_eq!(conflict.expected_etag, opened.etag);
    assert!(
        !fixture.workspace.join("gone.md").exists(),
        "the file must stay deleted"
    );

    // Keep-mine retries as a create (no base etag) and succeeds.
    let retry = fixture
        .service
        .write(fixture.workspace(), "gone.md", "mine\n", None, Some("test"))
        .expect("retry");
    assert!(retry.conflict.is_none());
}
