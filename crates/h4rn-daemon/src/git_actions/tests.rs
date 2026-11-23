//! Git action tests against real temporary repositories.

use std::path::Path;
use std::process::Command;

use super::*;

/// A temporary repository with one initial commit and two tracked files.
struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Self { dir };
        repo.git(&["init", "-b", "main"]);
        repo.git(&["config", "user.name", "h4rn test"]);
        repo.git(&["config", "user.email", "test@example.invalid"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.write("a.txt", "one\n");
        repo.write("b.txt", "two\n");
        repo.git(&["add", "a.txt", "b.txt"]);
        repo.git(&["commit", "-m", "Initial commit"]);
        repo
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, name: &str, content: &str) {
        std::fs::write(self.path().join(name), content).expect("write file");
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path().join(name)).expect("read file")
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self.try_git(args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn try_git(&self, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run git")
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }
}

/// A bare repository to push to.
struct Bare {
    dir: tempfile::TempDir,
}

impl Bare {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(dir.path())
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git init --bare");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
}

fn repo_with_remote() -> (Repo, Bare) {
    let repo = Repo::new();
    let bare = Bare::new();
    repo.git(&["remote", "add", "origin", bare.path().to_str().unwrap()]);
    (repo, bare)
}

/// The commit a branch points at in a bare repository.
fn bare_head(path: &Path, name: &str) -> String {
    let output = Command::new("git")
        .args(["--git-dir", path.to_str().unwrap(), "rev-parse", name])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("rev-parse");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn status_counts_staged_unstaged_and_untracked_paths() {
    let repo = Repo::new();
    repo.write("a.txt", "staged\n");
    repo.git(&["add", "a.txt"]);
    repo.write("b.txt", "unstaged\n");
    repo.write("new.txt", "untracked\n");

    let status = status_blocking(repo.path()).expect("status");
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert_eq!(status.upstream, None);
    assert_eq!(status.staged.count, 1);
    assert_eq!(status.staged.paths, vec!["a.txt".to_owned()]);
    assert_eq!(status.unstaged.count, 1);
    assert_eq!(status.unstaged.paths, vec!["b.txt".to_owned()]);
    assert_eq!(status.untracked.count, 1);
    assert_eq!(status.untracked.paths, vec!["new.txt".to_owned()]);
    assert!(status.is_dirty());
    assert_eq!(status.operation, None);
    assert_eq!(status.default_branch.as_deref(), Some("main"));
}

#[test]
fn status_reports_each_in_progress_operation() {
    let repo = Repo::new();
    std::fs::create_dir(repo.path().join(".git/rebase-merge")).expect("rebase dir");
    assert_eq!(
        status_blocking(repo.path()).expect("status").operation,
        Some(GitOperation::Rebase)
    );
    std::fs::remove_dir(repo.path().join(".git/rebase-merge")).expect("remove");
    std::fs::write(repo.path().join(".git/MERGE_HEAD"), "0".repeat(40)).expect("merge head");
    assert_eq!(
        status_blocking(repo.path()).expect("status").operation,
        Some(GitOperation::Merge)
    );
    std::fs::remove_file(repo.path().join(".git/MERGE_HEAD")).expect("remove");
    std::fs::write(repo.path().join(".git/CHERRY_PICK_HEAD"), "0".repeat(40)).expect("pick head");
    assert_eq!(
        status_blocking(repo.path()).expect("status").operation,
        Some(GitOperation::CherryPick)
    );
}

#[test]
fn staged_only_commit_leaves_unstaged_changes_alone() {
    let repo = Repo::new();
    repo.write("a.txt", "staged\n");
    repo.git(&["add", "a.txt"]);
    repo.write("b.txt", "left alone\n");

    let result = commit_blocking(repo.path(), "Only a", false, None).expect("commit");
    assert_eq!(result.subject, "Only a");
    assert_eq!(result.status.staged.count, 0);
    assert_eq!(result.status.unstaged.paths, vec!["b.txt".to_owned()]);
    assert_eq!(repo.head(), result.commit);

    repo.write("b.txt", "included\n");
    repo.write("c.txt", "new file\n");
    let result = commit_blocking(repo.path(), "Include everything", true, None).expect("commit");
    assert_eq!(result.status.staged.count, 0);
    assert_eq!(result.status.unstaged.count, 0);
    assert_eq!(result.status.untracked.count, 0);
    assert_eq!(repo.read("b.txt"), "included\n");
}

#[test]
fn paths_stage_only_what_was_named() {
    let repo = Repo::new();
    repo.write("a.txt", "a changed\n");
    repo.write("b.txt", "b changed\n");
    let paths = vec!["a.txt".to_owned()];
    let result = commit_blocking(repo.path(), "Only a.txt", true, Some(&paths)).expect("commit");
    assert_eq!(result.status.staged.count, 0);
    assert_eq!(result.status.unstaged.paths, vec!["b.txt".to_owned()]);
}

#[test]
fn a_path_limited_commit_leaves_other_staged_paths_alone() {
    let repo = Repo::new();
    repo.write("a.txt", "a staged\n");
    repo.git(&["add", "a.txt"]);
    repo.write("b.txt", "b staged\n");
    repo.git(&["add", "b.txt"]);

    let paths = vec!["a.txt".to_owned()];
    let result = commit_blocking(repo.path(), "Only a", false, Some(&paths)).expect("commit");
    let committed = repo.git(&["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(committed, "a.txt");
    assert_eq!(
        result.status.staged.paths,
        vec!["b.txt".to_owned()],
        "b.txt is still staged"
    );
}

#[test]
fn a_path_limited_commit_ignores_a_change_staged_elsewhere() {
    let repo = Repo::new();
    repo.write("b.txt", "b staged\n");
    repo.git(&["add", "b.txt"]);
    let head = repo.head();

    let paths = vec!["a.txt".to_owned()];
    let error = commit_blocking(repo.path(), "Nothing here", true, Some(&paths))
        .expect_err("no change under a.txt");
    assert!(
        error.to_string().contains("nothing under those paths"),
        "{error}"
    );
    assert_eq!(repo.head(), head, "nothing was committed");
    assert_eq!(
        status_blocking(repo.path()).expect("status").staged.paths,
        vec!["b.txt".to_owned()]
    );
}

#[test]
fn commit_input_covers_exactly_the_requested_paths() {
    let repo = Repo::new();
    repo.write("a.txt", "a changed\n");
    repo.write("b.txt", "b changed\n");
    let input =
        commit_input_blocking(repo.path(), true, Some(&["a.txt".to_owned()])).expect("input");
    assert!(input.diff.contains("+a changed"), "{}", input.diff);
    assert!(
        !input.diff.contains("b changed"),
        "a path outside the request is not sent: {}",
        input.diff
    );
}

#[test]
fn paths_outside_the_workspace_and_option_shaped_names_are_refused() {
    let repo = Repo::new();
    repo.write("a.txt", "changed\n");
    for path in [
        "../elsewhere",
        "/etc/passwd",
        "--amend",
        "-p",
        ":(top)a.txt",
        "a\0b",
        " a.txt",
    ] {
        let error = commit_blocking(repo.path(), "message", true, Some(&[path.to_owned()]))
            .expect_err(path);
        assert!(
            matches!(error, GitActionError::InvalidRequest(_)),
            "{path:?}: {error:?}"
        );
    }
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), repo.head());
}

#[test]
fn an_option_like_message_is_a_message_not_an_option() {
    let repo = Repo::new();
    repo.write("a.txt", "changed\n");
    repo.git(&["add", "a.txt"]);
    let before = repo.git(&["rev-list", "--count", "HEAD"]);
    let result =
        commit_blocking(repo.path(), "--amend\n\nnot a flag", false, None).expect("commit");
    assert_eq!(result.subject, "--amend");
    let after = repo.git(&["rev-list", "--count", "HEAD"]);
    assert_ne!(before, after);
    assert_eq!(result.commit, repo.head());
}

#[test]
fn a_failing_hook_is_reported_with_its_output_tail() {
    let repo = Repo::new();
    let hooks = repo.path().join(".git/hooks");
    std::fs::create_dir_all(&hooks).expect("hooks dir");
    let hook = hooks.join("pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'the hook says no' >&2\necho 'and names the file' >&2\nexit 1\n",
    )
    .expect("hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    repo.write("a.txt", "changed\n");
    repo.git(&["add", "a.txt"]);
    let head = repo.head();
    let error = commit_blocking(repo.path(), "Blocked", false, None).expect_err("hook fails");
    let message = error.to_string();
    assert!(message.contains("hook says no"), "{message}");
    assert!(message.contains("names the file"), "{message}");
    assert_eq!(repo.head(), head, "nothing was committed");
}

#[test]
fn a_detached_head_and_a_rebase_both_refuse_a_commit() {
    let repo = Repo::new();
    repo.write("a.txt", "changed\n");
    repo.git(&["add", "a.txt"]);
    repo.git(&["checkout", "--detach"]);
    let error = commit_blocking(repo.path(), "No branch", false, None).expect_err("detached");
    assert!(matches!(error, GitActionError::Conflict(_)));

    repo.git(&["checkout", "main"]);
    std::fs::create_dir(repo.path().join(".git/rebase-merge")).expect("rebase");
    let error = commit_blocking(repo.path(), "No rebase", false, None).expect_err("rebase");
    assert!(matches!(error, GitActionError::Conflict(_)));
}

#[test]
fn a_real_paused_rebase_is_named_instead_of_the_detached_head() {
    let repo = Repo::new();
    repo.git(&["checkout", "-b", "feature"]);
    repo.write("a.txt", "feature\n");
    repo.git(&["add", "a.txt"]);
    repo.git(&["commit", "-m", "Feature change"]);
    repo.git(&["checkout", "main"]);
    repo.write("a.txt", "main\n");
    repo.git(&["add", "a.txt"]);
    repo.git(&["commit", "-m", "Main change"]);
    repo.git(&["checkout", "feature"]);

    let paused = repo.try_git(&["rebase", "main"]);
    assert!(!paused.status.success(), "the rebase stops on a conflict");
    let status = status_blocking(repo.path()).expect("status");
    assert_eq!(status.operation, Some(GitOperation::Rebase));
    assert_eq!(status.branch, None, "a paused rebase has a detached HEAD");

    let error = commit_blocking(repo.path(), "Resolve it", false, None).expect_err("rebase");
    let message = error.to_string();
    assert!(message.contains("rebase is in progress"), "{message}");
    assert!(!message.contains("detached HEAD"), "{message}");

    let error = push_blocking(repo.path(), true).expect_err("rebase");
    let message = error.to_string();
    assert!(message.contains("rebase is in progress"), "{message}");
}

#[test]
fn nothing_staged_is_refused_with_a_clear_message() {
    let repo = Repo::new();
    let error = commit_blocking(repo.path(), "Nothing", false, None).expect_err("empty");
    assert!(matches!(error, GitActionError::InvalidRequest(_)));
    assert!(error.to_string().contains("nothing is staged"), "{error}");
}

#[tokio::test]
async fn push_needs_an_upstream_unless_asked_to_set_one() {
    let (repo, bare) = repo_with_remote();
    let error = push_blocking(repo.path(), false).expect_err("no upstream");
    assert!(matches!(error, GitActionError::InvalidRequest(_)));
    assert!(error.to_string().contains("no upstream"), "{error}");

    let pushed = push_blocking(repo.path(), true).expect("push");
    assert_eq!(pushed.branch, "main");
    assert_eq!(pushed.upstream, "origin/main");
    assert_eq!(pushed.status.ahead, 0);
    let remote_head = Command::new("git")
        .args(["rev-parse", "main"])
        .current_dir(bare.path())
        .output()
        .expect("rev-parse");
    assert_eq!(
        String::from_utf8_lossy(&remote_head.stdout).trim(),
        repo.head()
    );
}

/// `push.default=matching` would move every matching branch. h4rn names one
/// refspec, so only the checked-out branch moves.
#[tokio::test]
async fn push_default_matching_never_pushes_another_branch() {
    let (repo, bare) = repo_with_remote();
    repo.git(&["branch", "side"]);
    push_blocking(repo.path(), true).expect("push main");
    repo.git(&["checkout", "side"]);
    push_blocking(repo.path(), true).expect("push side");
    repo.git(&["checkout", "main"]);

    // Move both branches: only `main` is pushed and `side` stays where it was.
    repo.write("b.txt", "main moves\n");
    repo.git(&["add", "b.txt"]);
    repo.git(&["commit", "-m", "Main moves"]);
    let main_head = repo.head();
    repo.git(&["checkout", "side"]);
    repo.write("c.txt", "side moves\n");
    repo.git(&["add", "c.txt"]);
    repo.git(&["commit", "-m", "Side moves"]);
    repo.git(&["checkout", "main"]);

    repo.git(&["config", "push.default", "matching"]);
    push_blocking(repo.path(), false).expect("push");

    let bare_head = |name: &str| {
        let output = Command::new("git")
            .args(["rev-parse", name])
            .current_dir(bare.path())
            .output()
            .expect("rev-parse");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    assert_eq!(bare_head("main"), main_head);
    assert_ne!(
        bare_head("side"),
        repo.git(&["rev-parse", "side"]),
        "the other branch must not move"
    );
}

/// A configured `remote.origin.push=+…` must never turn the push into a force.
#[tokio::test]
async fn a_forced_push_refspec_in_config_is_not_followed() {
    let (repo, bare) = repo_with_remote();
    push_blocking(repo.path(), true).expect("first push");
    let remote_head = bare_head(bare.path(), "main");

    // Another clone advances the remote, so a forced push would lose its work.
    let clone = tempfile::tempdir().expect("clone dir");
    let clone_path = clone.path().join("clone");
    let output = Command::new("git")
        .args(["clone"])
        .arg(bare.path())
        .arg(&clone_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("clone");
    assert!(output.status.success());
    let clone_git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&clone_path)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git");
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    clone_git(&["config", "user.name", "Someone Else"]);
    clone_git(&["config", "user.email", "else@example.invalid"]);
    clone_git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(clone_path.join("remote.txt"), "from the other clone\n").expect("write");
    clone_git(&["add", "remote.txt"]);
    clone_git(&["commit", "-m", "Remote change"]);
    clone_git(&["push", "origin", "main"]);
    let advanced = clone_git(&["rev-parse", "HEAD"]);
    assert_ne!(advanced, remote_head);

    repo.write("local.txt", "local\n");
    repo.git(&["add", "local.txt"]);
    repo.git(&["commit", "-m", "Local change"]);
    repo.git(&["config", "remote.origin.push", "+refs/heads/*:refs/heads/*"]);

    let error = push_blocking(repo.path(), false).expect_err("rejected");
    assert!(matches!(error, GitActionError::Conflict(_)), "{error:?}");
    assert_eq!(
        bare_head(bare.path(), "main"),
        advanced,
        "the remote was not force-updated"
    );
}

/// A branch that tracks `origin/main` is still published as `origin/<branch>`.
#[tokio::test]
async fn a_branch_tracking_origin_main_pushes_under_its_own_name() {
    let (repo, bare) = repo_with_remote();
    push_blocking(repo.path(), true).expect("publish main");
    let main_head = bare_head(bare.path(), "main");

    repo.git(&["checkout", "-b", "feature/widget"]);
    repo.git(&["branch", "--set-upstream-to=origin/main"]);
    repo.write("feature.txt", "feature\n");
    repo.git(&["add", "feature.txt"]);
    repo.git(&["commit", "-m", "Feature work"]);
    let feature_head = repo.head();

    let pushed = push_blocking(repo.path(), true).expect("push");
    assert_eq!(pushed.branch, "feature/widget");
    assert_eq!(pushed.upstream, "origin/feature/widget");
    assert_eq!(bare_head(bare.path(), "feature/widget"), feature_head);
    assert_eq!(
        bare_head(bare.path(), "main"),
        main_head,
        "main is untouched"
    );
}

/// Two remotes: the branch tracks `upstream/main`, but the fork's `origin` is
/// where h4rn pushes.
#[tokio::test]
async fn a_fork_layout_pushes_to_origin_not_the_upstream_remote() {
    let repo = Repo::new();
    let fork = Bare::new();
    let upstream = Bare::new();
    repo.git(&["remote", "add", "origin", fork.path().to_str().unwrap()]);
    repo.git(&[
        "remote",
        "add",
        "upstream",
        upstream.path().to_str().unwrap(),
    ]);
    // Seed the upstream remote, and make the branch track it.
    repo.git(&["push", "upstream", "main"]);
    repo.git(&["fetch", "upstream"]);
    repo.git(&["branch", "--set-upstream-to=upstream/main"]);
    let upstream_head = bare_head(upstream.path(), "main");

    repo.write("local.txt", "fork work\n");
    repo.git(&["add", "local.txt"]);
    repo.git(&["commit", "-m", "Fork work"]);
    let head = repo.head();

    let pushed = push_blocking(repo.path(), true).expect("push");
    assert_eq!(pushed.upstream, "origin/main");
    assert_eq!(bare_head(fork.path(), "main"), head);
    assert_eq!(
        bare_head(upstream.path(), "main"),
        upstream_head,
        "the upstream remote is not written"
    );
}

#[tokio::test]
async fn a_pull_request_push_uses_the_named_remote_not_the_upstream() {
    let repo = Repo::new();
    let fork = Bare::new();
    let upstream = Bare::new();
    repo.git(&["remote", "add", "origin", fork.path().to_str().unwrap()]);
    repo.git(&[
        "remote",
        "add",
        "upstream",
        upstream.path().to_str().unwrap(),
    ]);
    repo.git(&["push", "upstream", "main"]);
    repo.git(&["fetch", "upstream"]);
    repo.git(&["branch", "--set-upstream-to=upstream/main"]);
    let upstream_head = bare_head(upstream.path(), "main");

    repo.git(&["checkout", "-b", "feature/widget"]);
    repo.git(&["branch", "--set-upstream-to=upstream/main"]);
    repo.write("feature.txt", "feature\n");
    repo.git(&["add", "feature.txt"]);
    repo.git(&["commit", "-m", "Feature work"]);
    let head = repo.head();

    let pushed = push_for_pull_request(repo.path(), "origin", "feature/widget")
        .await
        .expect("push");
    assert!(pushed);
    assert_eq!(bare_head(fork.path(), "feature/widget"), head);
    assert_eq!(
        bare_head(upstream.path(), "main"),
        upstream_head,
        "the upstream remote is not written"
    );

    // A second call finds the branch published and does nothing.
    let pushed = push_for_pull_request(repo.path(), "origin", "feature/widget")
        .await
        .expect("no-op");
    assert!(!pushed);
}

#[tokio::test]
async fn a_rejected_push_is_reported_and_nothing_is_forced() {
    let (repo, bare) = repo_with_remote();
    push_blocking(repo.path(), true).expect("first push");

    // Another clone advances the remote, so the local branch is behind.
    let clone = tempfile::tempdir().expect("clone dir");
    let clone_path = clone.path().join("clone");
    let output = Command::new("git")
        .args(["clone"])
        .arg(bare.path())
        .arg(&clone_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("clone");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let clone_git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&clone_path)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    clone_git(&["config", "user.name", "Someone Else"]);
    clone_git(&["config", "user.email", "else@example.invalid"]);
    clone_git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(clone_path.join("remote.txt"), "from the other clone\n").expect("write");
    clone_git(&["add", "remote.txt"]);
    clone_git(&["commit", "-m", "Remote change"]);
    clone_git(&["push", "origin", "main"]);
    let remote_head = clone_git(&["rev-parse", "HEAD"]);

    repo.write("local.txt", "local\n");
    repo.git(&["add", "local.txt"]);
    repo.git(&["commit", "-m", "Local change"]);
    let local_head = repo.head();

    let error = push_blocking(repo.path(), false).expect_err("rejected");
    assert!(matches!(error, GitActionError::Conflict(_)), "{error:?}");
    assert!(error.to_string().contains("never force-pushes"), "{error}");
    assert_eq!(repo.head(), local_head, "local history is untouched");
    let still_remote = Command::new("git")
        .args(["rev-parse", "main"])
        .current_dir(bare.path())
        .output()
        .expect("rev-parse");
    assert_eq!(
        String::from_utf8_lossy(&still_remote.stdout).trim(),
        remote_head,
        "the remote was not force-updated"
    );
}

#[test]
fn an_auth_failure_is_reported_as_unauthorized() {
    let error = classify_push_error(
        "fatal: Authentication failed for 'https://example.invalid/acme/widget.git/'",
    );
    assert!(matches!(error, GitActionError::Unauthorized(_)));
    let error = classify_push_error(
        "! [rejected]        main -> main (non-fast-forward)\nerror: failed to push some refs",
    );
    assert!(matches!(error, GitActionError::Conflict(_)));
}

#[test]
fn a_credential_in_push_output_is_redacted() {
    // Built at runtime so no credential-shaped literal lives in the repository.
    let prefix = format!("{}{}{}", "g", "hp", "_");
    let token = format!("{prefix}{}", "x".repeat(32));
    let url = format!("https://{token}@example.invalid/acme/widget.git");
    let message = format!("fatal: Authentication failed for '{url}'");
    let error = classify_push_error(&message);
    assert!(
        matches!(error, GitActionError::Unauthorized(_)),
        "{error:?}"
    );
    let shown = error.to_string();
    assert!(
        !shown.contains(&token),
        "the token is not repeated: {shown}"
    );
    assert!(shown.contains("[redacted]"), "{shown}");
}

#[tokio::test]
async fn a_pull_request_push_only_pushes_when_it_must() {
    let (repo, bare) = repo_with_remote();
    let pushed = push_for_pull_request(repo.path(), "origin", "main")
        .await
        .expect("push");
    assert!(pushed, "a branch without an upstream is pushed");

    let pushed = push_for_pull_request(repo.path(), "origin", "main")
        .await
        .expect("no-op");
    assert!(!pushed, "an up-to-date branch is not pushed");

    repo.write("a.txt", "ahead\n");
    repo.git(&["add", "a.txt"]);
    repo.git(&["commit", "-m", "Ahead"]);
    let pushed = push_for_pull_request(repo.path(), "origin", "main")
        .await
        .expect("ahead");
    assert!(pushed, "a branch ahead of its upstream is pushed");
    let remote_head = Command::new("git")
        .args(["rev-parse", "main"])
        .current_dir(bare.path())
        .output()
        .expect("rev-parse");
    assert_eq!(
        String::from_utf8_lossy(&remote_head.stdout).trim(),
        repo.head()
    );
}

#[test]
fn commit_input_sends_the_diff_and_recent_subjects() {
    let repo = Repo::new();
    repo.write("a.txt", "staged\n");
    repo.git(&["add", "a.txt"]);
    let input = commit_input_blocking(repo.path(), false, None).expect("input");
    assert!(input.diff.contains("+staged"), "{}", input.diff);
    assert!(input.recent_subjects.contains(&"Initial commit".to_owned()));
}

#[test]
fn commit_input_with_unstaged_names_untracked_files_without_reading_them() {
    let repo = Repo::new();
    repo.write("b.txt", "unstaged\n");
    repo.write("secret.txt", "never sent\n");
    let input = commit_input_blocking(repo.path(), true, None).expect("input");
    assert!(input.diff.contains("+unstaged"), "{}", input.diff);
    assert!(input.diff.contains("- secret.txt"), "{}", input.diff);
    assert!(
        !input.diff.contains("never sent"),
        "untracked contents are not sent: {}",
        input.diff
    );
}

#[test]
fn commit_input_is_capped() {
    let repo = Repo::new();
    let huge = "line\n".repeat(DIFF_MAX_CHARS);
    repo.write("a.txt", &huge);
    repo.git(&["add", "a.txt"]);
    let input = commit_input_blocking(repo.path(), false, None).expect("input");
    assert!(input.diff.chars().count() <= DIFF_MAX_CHARS + "\n[truncated]".chars().count());
    assert!(input.diff.ends_with("[truncated]"), "{}", input.diff);
}

#[test]
fn pull_request_input_uses_commits_and_a_diff_stat() {
    let repo = Repo::new();
    repo.git(&["checkout", "-b", "feature"]);
    repo.write("a.txt", "feature\n");
    repo.git(&["add", "a.txt"]);
    repo.git(&["commit", "-m", "Feature change"]);
    let input = pull_request_input_blocking(repo.path(), Some("main")).expect("input");
    assert_eq!(input.base, "main");
    assert!(
        input.commits.contains("Feature change"),
        "{}",
        input.commits
    );
    assert!(input.diff_stat.contains("a.txt"), "{}", input.diff_stat);
}

#[test]
fn a_branch_name_cannot_be_option_shaped() {
    for branch in ["-x", "--upload-pack=evil", ".hidden", "a..b", ""] {
        assert!(validate_branch(branch).is_err(), "{branch:?}");
    }
    assert!(validate_branch("feature/one").is_ok());
}

#[test]
fn status_of_a_missing_workspace_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing");
    assert!(status_blocking(&missing).is_err());
}

#[test]
fn a_directory_that_is_not_a_repository_is_refused_plainly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let error = status_blocking(dir.path()).expect_err("not a repository");
    assert!(matches!(error, GitActionError::InvalidRequest(_)));
    assert!(error.to_string().contains("not inside a git working tree"));
}

#[test]
fn a_workspace_below_the_repository_root_reports_only_its_own_paths() {
    let repo = Repo::new();
    std::fs::create_dir(repo.path().join("sub")).expect("subdir");
    repo.write("sub/c.txt", "one\n");
    repo.git(&["add", "sub/c.txt"]);
    repo.git(&["commit", "-m", "Add sub"]);
    repo.write("sub/c.txt", "changed\n");
    repo.write("b.txt", "outside the workspace\n");
    let status = status_blocking(&repo.path().join("sub")).expect("status");
    assert_eq!(status.unstaged.count, 1);
    assert_eq!(status.unstaged.paths, vec!["c.txt".to_owned()]);
}

#[test]
fn a_commit_refuses_staged_changes_from_another_folder() {
    let repo = Repo::new();
    std::fs::create_dir(repo.path().join("sub")).expect("subdir");
    repo.write("sub/c.txt", "one\n");
    repo.git(&["add", "sub/c.txt"]);
    repo.git(&["commit", "-m", "Add sub"]);
    repo.write("b.txt", "staged at the root\n");
    repo.git(&["add", "b.txt"]);
    let error = commit_blocking(&repo.path().join("sub"), "Message", false, None)
        .expect_err("outside staged changes are refused");
    assert!(
        matches!(error, GitActionError::InvalidRequest(_)),
        "{error:?}"
    );
    assert!(
        error.to_string().contains("outside this workspace"),
        "{error}"
    );
}

#[test]
fn a_path_is_kept_verbatim() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().canonicalize().expect("canonical");
    assert_eq!(
        validate_path(&workspace, "src/with space.rs").expect("path"),
        "src/with space.rs"
    );
    assert!(validate_path(&workspace, "src/../with space.rs").is_err());
}
