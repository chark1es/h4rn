//! Integration tests for branch and commit review.
//!
//! Every test builds a real temporary git repository and drives the daemon's
//! filesystem service. Nothing here touches the network: remote-tracking refs
//! are created locally, which is exactly what a fetch would leave behind.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use h4rn_daemon::fs::{FsError, FsService, MAX_READ_BYTES};
use h4rn_protocol::{DiffAgainst, GitFileStatus};

fn run_git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct Repo {
    dir: tempfile::TempDir,
    workspace: PathBuf,
    service: FsService,
}

impl Repo {
    fn new(branch: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        let data = dir.path().join("data");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&data).expect("data");
        let service = FsService::new(data).expect("service");
        service.allow_root(&workspace).expect("allow root");
        let repo = Self {
            dir,
            workspace,
            service,
        };
        repo.git(&["init", "-q", "-b", branch]);
        repo.git(&["config", "user.email", "test@h4rn.dev"]);
        repo.git(&["config", "user.name", "h4rn test"]);
        repo
    }

    fn workspace(&self) -> &str {
        self.workspace.to_str().expect("utf-8 workspace")
    }

    fn git(&self, args: &[&str]) -> String {
        run_git(&self.workspace, args)
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.workspace.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, content).expect("write");
    }

    fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }
}

fn base(base: Option<&str>) -> DiffAgainst {
    DiffAgainst::Base {
        base: base.map(str::to_owned),
    }
}

fn commit_against(sha: &str) -> DiffAgainst {
    DiffAgainst::Commit {
        sha: sha.to_owned(),
    }
}

#[test]
fn default_base_prefers_remote_head_then_main_then_master() {
    // A remote whose default branch is `release` wins over a local `main`.
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    let head = repo.commit("initial");
    fs::create_dir_all(repo.dir.path().join("remote.git")).expect("remote dir");
    repo.git(&[
        "remote",
        "add",
        "origin",
        repo.dir.path().join("remote.git").to_str().unwrap(),
    ]);
    repo.git(&["update-ref", "refs/remotes/origin/release", &head]);
    repo.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/release",
    ]);
    let files = repo
        .service
        .diff_files(repo.workspace(), &base(None))
        .expect("remote head resolves");
    assert_eq!(files.base.as_deref(), Some("origin/release"));

    // No remote: a local main.
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    let files = repo
        .service
        .diff_files(repo.workspace(), &base(None))
        .expect("main resolves");
    assert_eq!(files.base.as_deref(), Some("main"));

    // No main: master.
    let repo = Repo::new("master");
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    let files = repo
        .service
        .diff_files(repo.workspace(), &base(None))
        .expect("master resolves");
    assert_eq!(files.base.as_deref(), Some("master"));

    // Neither, and no remote: an honest error, not a guess.
    let repo = Repo::new("feature");
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    let error = repo
        .service
        .diff_files(repo.workspace(), &base(None))
        .expect_err("there is no base");
    assert!(matches!(error, FsError::Invalid(_)), "got {error:?}");
    assert!(error.to_string().contains("No base branch"));
}

#[test]
fn merge_base_excludes_commits_made_on_the_base_after_the_fork() {
    let repo = Repo::new("main");
    repo.write("shared.txt", "one\n");
    repo.commit("initial");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.write("feature.txt", "work\n");
    repo.commit("feature work");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("base-later.txt", "landed on main\n");
    repo.commit("main moved on");
    repo.git(&["checkout", "-q", "feature"]);

    let files = repo
        .service
        .diff_files(repo.workspace(), &base(Some("main")))
        .expect("branch comparison");
    let paths: Vec<&str> = files.files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(paths, vec!["feature.txt"]);
}

#[test]
fn branch_list_reports_renames_untracked_files_and_their_lines() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\ntwo\nthree\n");
    repo.write("keep.txt", "keep\n");
    repo.commit("initial");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.git(&["mv", "a.txt", "b.txt"]);
    repo.write("c.txt", "one\ntwo\n");
    repo.commit("rename and add");

    let files = repo
        .service
        .diff_files(repo.workspace(), &base(Some("main")))
        .expect("branch comparison");
    let renamed = files
        .files
        .iter()
        .find(|file| file.path == "b.txt")
        .expect("rename appears");
    assert_eq!(renamed.status, GitFileStatus::Renamed);
    assert_eq!(renamed.old_path.as_deref(), Some("a.txt"));

    repo.write("untracked.txt", "alpha\nbeta\n");
    let files = repo
        .service
        .diff_files(repo.workspace(), &base(Some("main")))
        .expect("branch comparison");
    let untracked = files
        .files
        .iter()
        .find(|file| file.path == "untracked.txt")
        .expect("untracked appears");
    assert_eq!(untracked.status, GitFileStatus::Untracked);
    assert_eq!(untracked.added, 2);
    assert_eq!(untracked.removed, 0);

    // The diff reads the rename's base content from the old path.
    let diff = repo
        .service
        .diff(repo.workspace(), "b.txt", &base(Some("main")))
        .expect("rename diff");
    assert_eq!(diff.before.as_deref(), Some("one\ntwo\nthree\n"));
    assert_eq!(diff.after.as_deref(), Some("one\ntwo\nthree\n"));
}

#[test]
fn commit_diff_reads_the_root_commit_and_the_first_parent() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    let root = repo.commit("root");

    let diff = repo
        .service
        .diff(repo.workspace(), "a.txt", &commit_against(&root))
        .expect("root diff");
    assert_eq!(diff.before, None);
    assert_eq!(diff.after.as_deref(), Some("one\n"));
    assert!(
        diff.note
            .as_deref()
            .unwrap_or_default()
            .contains("root commit")
    );

    repo.write("a.txt", "one\ntwo\n");
    let second = repo.commit("second");
    let diff = repo
        .service
        .diff(repo.workspace(), "a.txt", &commit_against(&second))
        .expect("second diff");
    assert_eq!(diff.before.as_deref(), Some("one\n"));
    assert_eq!(diff.after.as_deref(), Some("one\ntwo\n"));
}

#[test]
fn commit_diff_follows_a_rename_to_the_parents_old_path() {
    let repo = Repo::new("main");
    repo.write("old.txt", "one\ntwo\n");
    repo.commit("initial");
    repo.git(&["mv", "old.txt", "new.txt"]);
    repo.write("new.txt", "one\ntwo\nthree\n");
    let renamed = repo.commit("rename and add a line");

    let diff = repo
        .service
        .diff(repo.workspace(), "new.txt", &commit_against(&renamed))
        .expect("rename diff");
    assert_eq!(diff.before.as_deref(), Some("one\ntwo\n"));
    assert_eq!(diff.after.as_deref(), Some("one\ntwo\nthree\n"));

    let files = repo
        .service
        .diff_files(repo.workspace(), &commit_against(&renamed))
        .expect("commit files");
    let file = files
        .files
        .iter()
        .find(|file| file.path == "new.txt")
        .expect("rename appears");
    assert_eq!(file.status, GitFileStatus::Renamed);
    assert_eq!(file.old_path.as_deref(), Some("old.txt"));
    assert_eq!(file.added, 1);
    assert_eq!(file.removed, 0);
}

#[test]
fn commit_mode_reads_the_committed_blobs_not_a_large_working_copy() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    repo.write("a.txt", "one\ntwo\n");
    let second = repo.commit("second");

    // The working copy is far past the read cap. A commit review answers from
    // the commit's own blobs, so it must not be dragged down by that.
    repo.write("a.txt", &"x".repeat(MAX_READ_BYTES as usize + 10));

    let diff = repo
        .service
        .diff(repo.workspace(), "a.txt", &commit_against(&second))
        .expect("commit diff");
    assert!(!diff.too_large);
    assert_eq!(diff.before.as_deref(), Some("one\n"));
    assert_eq!(diff.after.as_deref(), Some("one\ntwo\n"));
}

#[test]
fn a_merge_commit_is_compared_with_its_first_parent_only() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    repo.git(&["checkout", "-q", "-b", "side"]);
    repo.write("side.txt", "from side\n");
    repo.commit("side work");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("main-only.txt", "from main\n");
    repo.commit("main work");
    repo.git(&["merge", "--no-ff", "-q", "-m", "merge side", "side"]);
    let merge = repo.git(&["rev-parse", "HEAD"]);

    let files = repo
        .service
        .diff_files(repo.workspace(), &commit_against(&merge))
        .expect("merge commit files");
    let paths: Vec<&str> = files.files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(paths, vec!["side.txt"]);

    let side = repo
        .service
        .diff(repo.workspace(), "side.txt", &commit_against(&merge))
        .expect("side diff");
    assert_eq!(side.before, None);
    assert_eq!(side.after.as_deref(), Some("from side\n"));

    // The main-side file was added before the merge, so it is unchanged
    // against the first parent; the merge commit does not claim it.
    let main_only = repo
        .service
        .diff(repo.workspace(), "main-only.txt", &commit_against(&merge))
        .expect("unchanged diff");
    assert_eq!(main_only.before.as_deref(), Some("from main\n"));
    assert_eq!(main_only.after.as_deref(), Some("from main\n"));
}

#[test]
fn commit_list_returns_the_newest_hundred_newest_first() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    for index in 0..105 {
        repo.git(&[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &format!("commit {index}"),
        ]);
    }
    let head = repo.git(&["rev-parse", "HEAD"]);

    let list = repo
        .service
        .diff_commits(repo.workspace(), Some("main"))
        .expect("commit list");
    assert_eq!(list.base, "main");
    assert_eq!(list.commits.len(), 100);
    assert!(list.truncated);
    assert_eq!(list.commits[0].sha, head);
    assert_eq!(list.commits[0].subject, "commit 104");
    assert_eq!(list.commits[99].subject, "commit 5");
}

#[test]
fn commit_ids_are_validated_before_git_sees_them() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    let root = repo.commit("root");
    let tree = repo.git(&["rev-parse", "HEAD^{tree}"]);

    for sha in ["--output=/tmp/x", "-1", "zzzzzz", "abc", &tree] {
        let error = repo
            .service
            .diff_files(repo.workspace(), &commit_against(sha))
            .expect_err("not a commit");
        assert!(matches!(error, FsError::Invalid(_)), "{sha}: {error:?}");
    }

    // An abbreviated id that resolves to a commit is fine.
    let abbreviated = &root[..7];
    let files = repo
        .service
        .diff_files(repo.workspace(), &commit_against(abbreviated))
        .expect("abbreviated sha");
    assert_eq!(files.files.len(), 1);
    assert_eq!(files.files[0].path, "a.txt");
}

#[test]
fn base_names_are_validated_before_git_sees_them() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    for name in ["--output=/tmp/x", "-D", "a..b"] {
        let error = repo
            .service
            .diff_files(repo.workspace(), &base(Some(name)))
            .expect_err("not a branch");
        assert!(matches!(error, FsError::Invalid(_)), "{name}: {error:?}");
    }
}

#[test]
fn unrelated_histories_have_no_common_base() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    repo.git(&["checkout", "-q", "--orphan", "other"]);
    repo.git(&["rm", "-q", "-rf", "."]);
    repo.write("other.txt", "other\n");
    repo.commit("other root");

    let error = repo
        .service
        .diff_files(repo.workspace(), &base(Some("main")))
        .expect_err("no common history");
    assert!(error.to_string().contains("no common history"), "{error}");
}

#[test]
fn the_file_list_is_capped_and_says_so() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    for index in 0..2_005 {
        repo.write(&format!("many/file-{index:04}.txt"), "");
    }

    let files = repo
        .service
        .diff_files(repo.workspace(), &base(Some("main")))
        .expect("branch comparison");
    assert_eq!(files.files.len(), 2_000);
    assert!(files.truncated);
}

#[test]
fn branches_lists_local_and_remote_names_and_the_default() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    let head = repo.commit("initial");
    repo.git(&["branch", "feature"]);
    repo.git(&["update-ref", "refs/remotes/origin/main", &head]);
    repo.git(&["update-ref", "refs/remotes/origin/HEAD", &head]);

    let branches = repo
        .service
        .branches(repo.workspace())
        .expect("branch list");
    assert!(branches.branches.contains(&"main".to_owned()));
    assert!(branches.branches.contains(&"feature".to_owned()));
    assert!(branches.branches.contains(&"origin/main".to_owned()));
    assert!(!branches.branches.iter().any(|name| name.ends_with("/HEAD")));
    assert_eq!(branches.default_base.as_deref(), Some("main"));
}

#[test]
fn an_oversized_side_is_reported_instead_of_loaded() {
    let repo = Repo::new("main");
    repo.write("big.txt", &"a".repeat(2 * 1024 * 1024 + 1));
    repo.commit("initial");
    repo.git(&["checkout", "-q", "-b", "feature"]);
    repo.write("big.txt", "small now\n");
    repo.commit("shrink");

    let diff = repo
        .service
        .diff(repo.workspace(), "big.txt", &base(Some("main")))
        .expect("oversized diff");
    assert!(diff.too_large);
    assert_eq!(diff.before, None);
    assert!(
        diff.note
            .as_deref()
            .unwrap_or_default()
            .contains("too large")
    );
}

#[test]
fn an_oversized_head_side_is_capped_before_it_is_read() {
    let repo = Repo::new("main");
    repo.write("big.txt", &"a".repeat(MAX_READ_BYTES as usize + 1));
    repo.commit("initial");
    // The working copy is small; only the HEAD blob is oversized.
    repo.write("big.txt", "small now\n");

    let diff = repo
        .service
        .diff(repo.workspace(), "big.txt", &DiffAgainst::Head)
        .expect("head diff");
    assert!(diff.too_large);
    assert_eq!(diff.before, None);
    assert_eq!(diff.after.as_deref(), Some("small now\n"));
    assert!(
        diff.note
            .as_deref()
            .unwrap_or_default()
            .contains("too large")
    );
}

#[cfg(unix)]
#[test]
fn an_untracked_symlink_is_listed_without_reading_its_target() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    let outside = repo.dir.path().join("secret.txt");
    fs::write(&outside, "top secret\n").expect("outside file");
    std::os::unix::fs::symlink(&outside, repo.workspace.join("link.txt")).expect("symlink");

    let files = repo
        .service
        .diff_files(repo.workspace(), &base(Some("main")))
        .expect("branch comparison");
    let link = files
        .files
        .iter()
        .find(|file| file.path == "link.txt")
        .expect("the link is listed");
    assert_eq!(link.status, GitFileStatus::Untracked);
    assert!(link.binary, "a link's target is never read as text");
    assert_eq!(link.added, 0);
}

#[test]
fn untracked_files_stop_at_the_file_cap_and_say_so() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    for index in 0..2_005 {
        repo.write(&format!("loose/file-{index:04}.txt"), "");
    }

    let files = repo
        .service
        .diff_files(repo.workspace(), &base(Some("main")))
        .expect("branch comparison");
    assert_eq!(files.files.len(), 2_000);
    assert!(files.truncated);
}

#[test]
fn untracked_line_counts_stop_at_the_read_budget() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    // Each file is under the per-file cap but the two together exceed the
    // untracked read budget.
    repo.write("budget-a.txt", &"a\n".repeat(700_000));
    repo.write("budget-b.txt", &"b\n".repeat(700_000));

    let files = repo
        .service
        .diff_files(repo.workspace(), &base(Some("main")))
        .expect("branch comparison");
    let first = files
        .files
        .iter()
        .find(|file| file.path == "budget-a.txt")
        .expect("first file");
    let second = files
        .files
        .iter()
        .find(|file| file.path == "budget-b.txt")
        .expect("second file");
    assert!(!first.counts_omitted);
    assert_eq!(first.added, 700_000);
    assert!(second.counts_omitted);
    assert_eq!(second.added, 0);
    assert!(!second.binary, "a text file past the budget is not binary");
}

#[cfg(unix)]
#[test]
fn an_unreadable_untracked_file_is_marked_not_listed_as_empty() {
    use std::os::unix::fs::PermissionsExt;

    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    repo.commit("initial");
    let path = repo.workspace.join("secret.txt");
    fs::write(&path, "hidden\n").expect("write");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");

    let files = repo
        .service
        .diff_files(repo.workspace(), &base(Some("main")))
        .expect("branch comparison");
    let secret = files
        .files
        .iter()
        .find(|file| file.path == "secret.txt")
        .expect("the unreadable file is listed");
    assert!(secret.binary);
    assert_eq!(secret.added, 0);

    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
}

#[test]
fn a_stale_remote_head_falls_through_to_the_remotes_main() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    let head = repo.commit("initial");
    repo.git(&[
        "remote",
        "add",
        "origin",
        repo.dir.path().join("remote.git").to_str().unwrap(),
    ]);
    repo.git(&["update-ref", "refs/remotes/origin/main", &head]);
    // The default-branch symref outlives a branch that was deleted or never
    // fetched. It must not become the base the review tries to use.
    repo.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/gone",
    ]);

    let files = repo
        .service
        .diff_files(repo.workspace(), &base(None))
        .expect("the remote's main resolves");
    assert_eq!(files.base.as_deref(), Some("origin/main"));
}

#[test]
fn a_shallow_boundary_commit_says_its_parent_is_missing() {
    let repo = Repo::new("main");
    repo.write("a.txt", "one\n");
    repo.commit("first");
    repo.write("a.txt", "one\ntwo\n");
    repo.commit("second");

    let shallow = repo.dir.path().join("shallow");
    let url = format!("file://{}", repo.workspace.display());
    let output = Command::new("git")
        .args([
            "clone",
            "-q",
            "--depth",
            "1",
            &url,
            shallow.to_str().unwrap(),
        ])
        .output()
        .expect("run git clone");
    assert!(
        output.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let service = FsService::new(repo.dir.path().join("shallow-data")).expect("service");
    service.allow_root(&shallow).expect("allow root");
    let boundary = run_git(&shallow, &["rev-parse", "HEAD"]);

    let error = service
        .diff(
            shallow.to_str().unwrap(),
            "a.txt",
            &commit_against(&boundary),
        )
        .expect_err("the boundary commit has no parent here");
    assert!(matches!(error, FsError::Invalid(_)), "got {error:?}");
    assert!(
        error.to_string().contains("shallow clone"),
        "unexpected message: {error}"
    );

    let error = service
        .diff_files(shallow.to_str().unwrap(), &commit_against(&boundary))
        .expect_err("the file list cannot diff a missing parent");
    assert!(error.to_string().contains("shallow clone"), "{error}");
}
