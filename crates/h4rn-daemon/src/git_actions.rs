//! Git actions for a thread: status, commit, push and the input a model needs
//! to write a commit message or pull request.
//!
//! Every command runs `git` on the host in the thread's workspace. Nothing is
//! inferred: branch, upstream, counts, paths and in-progress operations are
//! git's own answers, and a failure is reported with git's message (or the
//! tail of a hook's output) rather than guessed.
//!
//! Safety rules that hold for every call here:
//!
//! - user text is never a git option: commit messages travel as the value of
//!   `-m`, and paths are validated as workspace-relative, then passed after
//!   `--` with literal pathspec parsing, so a name like `--amend` or `:(top)`
//!   is only ever a path;
//! - the real commit and push run hooks (`--no-verify` is never passed to
//!   them), and a hook's failure is shown; the read-only check that runs
//!   before opening a pull request skips hooks and submodule recursion, so it
//!   has no side effects and cannot block or slow the open;
//! - a push is never forced.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use h4rn_protocol::{
    GitChangeSet, GitCommitResult, GitOperation, GitPushResult, GitThreadStatus, ProtoError,
    ProtoErrorCode, ThreadId,
};

use crate::threads::ThreadManager;
use crate::worktrees::git;

/// Deadline for one status read.
const STATUS_TIMEOUT: Duration = Duration::from_secs(30);
/// Deadline for staging and committing, including hooks.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(180);
/// Deadline for a push, including slow remotes.
const PUSH_TIMEOUT: Duration = Duration::from_secs(180);
/// The whole budget for opening a pull request: the remote lookup, the push
/// and the forge request. The dialog waits 240 s for the answer, so a request
/// that follows this budget always reports before the client gives up.
pub const OPEN_PR_BUDGET: Duration = Duration::from_secs(220);
/// The pre-pull-request lookup may take at most this much of that budget.
const PR_LOOKUP_TIMEOUT: Duration = Duration::from_secs(30);
/// Time reserved for the forge request inside the budget, so a slow push
/// cannot leave the forge call with nothing.
const PR_FORGE_RESERVE: Duration = Duration::from_secs(30);
/// Deadline for the reads that build model input.
const INPUT_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest commit message the daemon accepts.
const MESSAGE_MAX_CHARS: usize = 20_000;
/// How much of a git or hook failure is quoted back.
const ERROR_TAIL_CHARS: usize = 2_000;
/// Longest diff sent to a model.
const DIFF_MAX_CHARS: usize = 12_000;
/// Longest commit list sent to a model.
const COMMITS_MAX_CHARS: usize = 8_000;
/// Longest diff stat sent to a model.
const DIFF_STAT_MAX_CHARS: usize = 4_000;
/// How many recent subjects are offered for a commit message.
const RECENT_SUBJECTS: usize = 20;
/// Longest recent subject kept.
const SUBJECT_MAX_CHARS: usize = 120;
/// The empty tree's object id in a SHA-1 repository: the commit a diff starts
/// from in a repository that has no commits yet.
const SHA1_EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// The empty tree's object id in a SHA-256 repository.
const SHA256_EMPTY_TREE: &str = "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";
/// A path that always reads as an empty file, for `git hash-object`.
#[cfg(not(windows))]
const EMPTY_FILE: &str = "/dev/null";
#[cfg(windows)]
const EMPTY_FILE: &str = "NUL";

/// One deadline shared by the lookup, the push and the forge request that
/// open a pull request, so no step can push the whole request past the
/// budget the client waits for.
#[derive(Clone, Copy, Debug)]
pub struct Deadline {
    at: Instant,
}

impl Deadline {
    /// A deadline `budget` from now.
    pub fn after(budget: Duration) -> Self {
        Self {
            at: Instant::now() + budget,
        }
    }

    /// Time left, never negative.
    pub fn remaining(&self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    /// Time one step may use: its own cap, or what is left when that is less.
    pub fn step(&self, cap: Duration) -> Duration {
        cap.min(self.remaining())
    }
}

/// What a pre-pull-request push may use: the budget left after the lookup cap
/// and the forge reserve, and never what would eat the forge's own time.
fn pr_push_budget(deadline: &Deadline) -> Duration {
    let cap = OPEN_PR_BUDGET
        .saturating_sub(PR_LOOKUP_TIMEOUT)
        .saturating_sub(PR_FORGE_RESERVE);
    cap.min(deadline.remaining().saturating_sub(PR_FORGE_RESERVE))
}

/// A git action failure at the daemon boundary.
#[derive(Clone, Debug, thiserror::Error)]
pub enum GitActionError {
    /// The request cannot be carried out as asked.
    #[error("{0}")]
    InvalidRequest(String),
    /// The repository is in a state that conflicts with the request.
    #[error("{0}")]
    Conflict(String),
    /// The remote rejected the credential.
    #[error("{0}")]
    Unauthorized(String),
    /// A git command failed.
    #[error("{0}")]
    Git(String),
    /// A local read, write or thread lookup failed.
    #[error("{0}")]
    Local(String),
}

impl GitActionError {
    /// Maps the failure onto the protocol error class and a user hint.
    pub fn into_proto(self) -> ProtoError {
        let (code, hint) = match &self {
            Self::InvalidRequest(_) => (
                ProtoErrorCode::InvalidRequest,
                "Fix the field the message names and try again.",
            ),
            Self::Conflict(_) => (
                ProtoErrorCode::Conflict,
                "Finish or abort the operation in the workspace, then try again.",
            ),
            Self::Unauthorized(_) => (
                ProtoErrorCode::Unauthorized,
                "Sign in to the git remote in a terminal, then push again.",
            ),
            Self::Git(_) => (
                ProtoErrorCode::Internal,
                "Read the git message above, fix it in the workspace and try again.",
            ),
            Self::Local(_) => (
                ProtoErrorCode::Internal,
                "Check the workspace path and the daemon log.",
            ),
        };
        ProtoError::new(code, self.to_string(), hint)
    }
}

impl From<GitActionError> for ProtoError {
    fn from(error: GitActionError) -> Self {
        error.into_proto()
    }
}

/// What a commit-message request sends to a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInput {
    /// The diff that would be committed, already capped.
    pub diff: String,
    /// Recent commit subjects, newest first.
    pub recent_subjects: Vec<String>,
}

/// What a pull request request sends to a model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestInput {
    /// Commit subjects and bodies on the branch, already capped.
    pub commits: String,
    /// `git diff --stat` for the branch against its base, already capped.
    pub diff_stat: String,
    /// The branch the range was taken against.
    pub base: String,
}

/// The workspace of a thread, refusing a path git could not use.
pub async fn workspace(
    manager: &Arc<ThreadManager>,
    thread_id: &ThreadId,
) -> Result<PathBuf, GitActionError> {
    let snapshot = manager
        .get_thread(thread_id, 0)
        .await
        .map_err(|error| GitActionError::Local(error.to_string()))?;
    let path = PathBuf::from(&snapshot.summary.workspace_path);
    if !path.is_dir() {
        return Err(GitActionError::Local(format!(
            "the thread workspace {} is not a directory on this host",
            path.display()
        )));
    }
    Ok(path)
}

/// Reads a thread workspace's git state.
pub async fn status(
    manager: &Arc<ThreadManager>,
    thread_id: &ThreadId,
) -> Result<GitThreadStatus, GitActionError> {
    let workspace = workspace(manager, thread_id).await?;
    blocking(move || status_blocking(&workspace)).await
}

/// Stages what was asked and commits the index.
pub async fn commit(
    manager: &Arc<ThreadManager>,
    thread_id: &ThreadId,
    message: String,
    include_unstaged: bool,
    paths: Option<Vec<String>>,
) -> Result<GitCommitResult, GitActionError> {
    let workspace = workspace(manager, thread_id).await?;
    blocking(move || commit_blocking(&workspace, &message, include_unstaged, paths.as_deref()))
        .await
}

/// Pushes the current branch, optionally creating its upstream on `origin`.
pub async fn push(
    manager: &Arc<ThreadManager>,
    thread_id: &ThreadId,
    set_upstream: bool,
) -> Result<GitPushResult, GitActionError> {
    let workspace = workspace(manager, thread_id).await?;
    blocking(move || push_blocking(&workspace, set_upstream)).await
}

/// Builds the model input for a commit message.
pub async fn commit_input(
    manager: &Arc<ThreadManager>,
    thread_id: &ThreadId,
    include_unstaged: bool,
    paths: Option<Vec<String>>,
) -> Result<CommitInput, GitActionError> {
    let workspace = workspace(manager, thread_id).await?;
    blocking(move || commit_input_blocking(&workspace, include_unstaged, paths.as_deref())).await
}

/// Builds the model input for a pull request's title and body.
pub async fn pull_request_input(
    manager: &Arc<ThreadManager>,
    thread_id: &ThreadId,
    base: Option<String>,
) -> Result<PullRequestInput, GitActionError> {
    let workspace = workspace(manager, thread_id).await?;
    blocking(move || pull_request_input_blocking(&workspace, base.as_deref())).await
}

/// Pushes a branch before a pull request when the remote needs it.
///
/// Nothing is pushed when every destination's branch already points at
/// `HEAD`. The remote is asked directly, with a dry run of the exact push the
/// real push will run (same remote and refspec, hooks skipped), so a stale
/// remote-tracking ref cannot skip a needed push or hide one that moved. A
/// missing branch is created on `remote`.
pub async fn push_for_pull_request(
    workspace: &Path,
    remote: &str,
    branch: &str,
) -> Result<bool, GitActionError> {
    push_for_pull_request_until(workspace, remote, branch, Deadline::after(OPEN_PR_BUDGET)).await
}

/// [`push_for_pull_request`] inside a deadline shared with the forge request
/// that follows it.
pub async fn push_for_pull_request_until(
    workspace: &Path,
    remote: &str,
    branch: &str,
    deadline: Deadline,
) -> Result<bool, GitActionError> {
    let workspace = workspace.to_path_buf();
    let remote = remote.to_owned();
    let branch = branch.to_owned();
    blocking(move || push_for_pull_request_blocking(&workspace, &remote, &branch, deadline)).await
}

/// Runs one blocking git action on the blocking pool.
async fn blocking<T, F>(work: F) -> Result<T, GitActionError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, GitActionError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| GitActionError::Local(format!("git action task failed: {error}")))?
}

/// `git` with the worktree's environment and a deadline, stdout trimmed.
fn git_text(workspace: &Path, args: &[&str], limit: Duration) -> Result<String, GitActionError> {
    git::run_with_timeout(workspace, args, limit).map_err(GitActionError::Git)
}

/// `git` as above, but `None` when it fails; for questions git answers with an
/// exit status (`@{upstream}` on a branch that has none).
fn git_opt(workspace: &Path, args: &[&str], limit: Duration) -> Option<String> {
    git::run_with_timeout(workspace, args, limit).ok()
}

/// `git` keeping stdout byte-exact, for NUL-delimited paths.
fn git_bytes(workspace: &Path, args: &[&str], limit: Duration) -> Result<Vec<u8>, GitActionError> {
    git::run_bytes_with_timeout(workspace, args, limit).map_err(GitActionError::Git)
}

/// Whether `path` names an in-progress git operation, resolved by git itself.
fn git_path_exists(workspace: &Path, name: &str) -> bool {
    git_opt(
        workspace,
        &["rev-parse", "--git-path", name],
        STATUS_TIMEOUT,
    )
    .map(|value| workspace.join(value.trim()).exists())
    .unwrap_or(false)
}

/// The branch a pull request should target by default.
pub(crate) fn default_branch(workspace: &Path) -> Option<String> {
    if let Some(remote_head) = git_opt(
        workspace,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
        STATUS_TIMEOUT,
    ) {
        let name = remote_head.trim();
        let name = name.strip_prefix("origin/").unwrap_or(name);
        if !name.is_empty() {
            return Some(name.to_owned());
        }
    }
    for candidate in ["main", "master"] {
        if git_opt(
            workspace,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{candidate}"),
            ],
            STATUS_TIMEOUT,
        )
        .is_some()
        {
            return Some(candidate.to_owned());
        }
    }
    None
}

/// The status of one workspace, blocking.
pub(crate) fn status_blocking(workspace: &Path) -> Result<GitThreadStatus, GitActionError> {
    if !workspace.is_dir() {
        return Err(GitActionError::Local(format!(
            "workspace {} is not a directory",
            workspace.display()
        )));
    }
    let inside = git_opt(
        workspace,
        &["rev-parse", "--is-inside-work-tree"],
        STATUS_TIMEOUT,
    );
    if inside.as_deref() != Some("true") {
        return Err(GitActionError::InvalidRequest(format!(
            "{} is not inside a git working tree",
            workspace.display()
        )));
    }
    let branch = git_opt(
        workspace,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        STATUS_TIMEOUT,
    )
    .map(|value| value.trim().to_owned())
    .filter(|value| !value.is_empty());
    let upstream = git_opt(
        workspace,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
        STATUS_TIMEOUT,
    )
    .map(|value| value.trim().to_owned())
    .filter(|value| !value.is_empty());
    let (ahead, behind) = match &upstream {
        Some(_) => git_opt(
            workspace,
            &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
            STATUS_TIMEOUT,
        )
        .and_then(|value| parse_ahead_behind(&value))
        .unwrap_or((0, 0)),
        None => (0, 0),
    };

    let staged = name_status_paths(git_bytes(
        workspace,
        &["diff", "--cached", "--relative", "--name-status", "-z"],
        STATUS_TIMEOUT,
    )?);
    let unstaged = name_status_paths(git_bytes(
        workspace,
        &["diff", "--relative", "--name-status", "-z"],
        STATUS_TIMEOUT,
    )?);
    let untracked = nul_paths(git_bytes(
        workspace,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        STATUS_TIMEOUT,
    )?);

    let operation = if git_path_exists(workspace, "rebase-merge")
        || git_path_exists(workspace, "rebase-apply")
    {
        Some(GitOperation::Rebase)
    } else if git_path_exists(workspace, "MERGE_HEAD") {
        Some(GitOperation::Merge)
    } else if git_path_exists(workspace, "CHERRY_PICK_HEAD") {
        Some(GitOperation::CherryPick)
    } else {
        None
    };

    Ok(GitThreadStatus {
        branch,
        upstream,
        ahead,
        behind,
        staged: GitChangeSet::new(staged),
        unstaged: GitChangeSet::new(unstaged),
        untracked: GitChangeSet::new(untracked),
        operation,
        default_branch: default_branch(workspace),
    })
}

/// Parses `git rev-list --left-right --count`'s `ahead\tbehind`.
fn parse_ahead_behind(value: &str) -> Option<(u32, u32)> {
    let mut parts = value.split_whitespace();
    let ahead = parts.next()?.parse().ok()?;
    let behind = parts.next()?.parse().ok()?;
    Some((ahead, behind))
}

/// Paths from `git diff --name-status -z`, taking the current path of a
/// rename or copy.
fn name_status_paths(output: Vec<u8>) -> Vec<String> {
    let mut records = output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    let mut paths = Vec::new();
    while let Some(status) = records.next() {
        let renamed = matches!(status.first(), Some(b'R' | b'C'));
        if renamed {
            let _previous = records.next();
            if let Some(path) = records.next().and_then(utf8_path) {
                paths.push(path);
            }
        } else if let Some(path) = records.next().and_then(utf8_path) {
            paths.push(path);
        }
    }
    paths
}

/// Every non-empty record of NUL-delimited output that is valid UTF-8.
fn nul_paths(output: Vec<u8>) -> Vec<String> {
    output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .filter_map(utf8_path)
        .collect()
}

fn utf8_path(bytes: &[u8]) -> Option<String> {
    String::from_utf8(bytes.to_vec()).ok()
}

/// Stages what was asked and commits the index, blocking.
fn commit_blocking(
    workspace: &Path,
    message: &str,
    include_unstaged: bool,
    paths: Option<&[String]>,
) -> Result<GitCommitResult, GitActionError> {
    let message = validate_message(message)?;
    let status = status_blocking(workspace)?;
    // A rebase leaves a detached HEAD too; naming the operation is the useful
    // message, so it is checked first.
    refuse_in_progress(&status, "committing")?;
    if status.branch.is_none() {
        return Err(GitActionError::Conflict(
            "the workspace has a detached HEAD; check out a branch before committing".into(),
        ));
    }
    let paths = paths
        .map(|paths| validate_paths(workspace, paths))
        .transpose()?;

    if include_unstaged || paths.is_some() {
        let mut args = vec!["--literal-pathspecs", "add"];
        if include_unstaged {
            args.push("-A");
        } else {
            // Only tracked changes; untracked files need the checkbox.
            args.push("-u");
        }
        args.push("--");
        match &paths {
            Some(paths) => args.extend(paths.iter().map(String::as_str)),
            None => args.push("."),
        }
        git_text(workspace, &args, COMMIT_TIMEOUT)?;
    }

    // Without paths, `git commit` records the whole index. Refuse when another
    // folder in the repository has staged changes this workspace does not
    // show. A path-limited commit uses `--only`, which leaves them alone.
    if paths.is_none() {
        let prefix =
            git_opt(workspace, &["rev-parse", "--show-prefix"], STATUS_TIMEOUT).unwrap_or_default();
        if !prefix.is_empty() {
            for path in nul_paths(git_bytes(
                workspace,
                &["diff", "--cached", "--name-only", "-z"],
                STATUS_TIMEOUT,
            )?) {
                if !path.starts_with(&prefix) {
                    return Err(GitActionError::InvalidRequest(format!(
                        "the index has staged changes outside this workspace ({path}); commit or unstage them there first"
                    )));
                }
            }
        }
    }

    // The "nothing to commit" answer covers exactly the paths this request
    // names, so a staged change elsewhere neither enables nor blocks it.
    let staged = match &paths {
        Some(paths) => {
            let mut args = vec![
                "--literal-pathspecs",
                "diff",
                "--cached",
                "--name-only",
                "-z",
                "--relative",
                "--",
            ];
            args.extend(paths.iter().map(String::as_str));
            git_bytes(workspace, &args, STATUS_TIMEOUT)?
        }
        None => git_bytes(
            workspace,
            &["diff", "--cached", "--name-only", "-z", "--relative"],
            STATUS_TIMEOUT,
        )?,
    };
    if staged.iter().all(|byte| *byte == 0) {
        return Err(GitActionError::InvalidRequest(if paths.is_some() {
            "nothing under those paths is ready to commit; stage or include changes first".into()
        } else if include_unstaged {
            "there is nothing to commit in this workspace".into()
        } else {
            "nothing is staged; stage changes or include unstaged changes first".into()
        }));
    }

    // The message is always the value of `-m`, and `--` ends the option list.
    // `--only` records these paths and leaves other staged work staged.
    let mut args = vec!["--literal-pathspecs", "commit"];
    if paths.is_some() {
        args.push("--only");
    }
    args.extend(["-m", message, "--"]);
    if let Some(paths) = &paths {
        args.extend(paths.iter().map(String::as_str));
    }
    if let Err(error) = git_text(workspace, &args, COMMIT_TIMEOUT) {
        let GitActionError::Git(message) = error else {
            return Err(error);
        };
        return Err(GitActionError::Git(format!(
            "git commit failed: {}",
            tail(&message)
        )));
    }

    let commit = git_text(workspace, &["rev-parse", "HEAD"], STATUS_TIMEOUT)?;
    let short_commit = git_text(workspace, &["rev-parse", "--short", "HEAD"], STATUS_TIMEOUT)?;
    let subject = git_text(workspace, &["log", "-1", "--format=%s"], STATUS_TIMEOUT)?;
    Ok(GitCommitResult {
        commit: commit.trim().to_owned(),
        short_commit: short_commit.trim().to_owned(),
        subject: subject.trim().to_owned(),
        status: status_blocking(workspace)?,
    })
}

/// Names an in-progress operation instead of a detached `HEAD` when both are
/// true, because the rebase tells the person what to do.
fn refuse_in_progress(status: &GitThreadStatus, action: &str) -> Result<(), GitActionError> {
    if let Some(operation) = status.operation {
        return Err(GitActionError::Conflict(format!(
            "a {} is in progress; finish or abort it before {action}",
            operation.label()
        )));
    }
    Ok(())
}

/// The remote a branch is pushed to.
///
/// `origin` is the forge remote in every layout h4rn creates. When it is
/// absent, the branch's configured remote is used, and a repository with one
/// remote has no choice to make. `push.default` and other push configuration
/// are never consulted: h4rn names both the remote and the ref.
fn push_remote(workspace: &Path, branch: &str) -> Result<String, GitActionError> {
    let remotes: Vec<String> = git_opt(workspace, &["remote"], STATUS_TIMEOUT)
        .map(|value| {
            value
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if remotes.iter().any(|remote| remote == "origin") {
        return Ok("origin".to_owned());
    }
    if let Some(remote) = git_opt(
        workspace,
        &["config", "--get", &format!("branch.{branch}.remote")],
        STATUS_TIMEOUT,
    )
    .map(|value| value.trim().to_owned())
    .filter(|remote| !remote.is_empty() && remote != ".")
        && remotes.contains(&remote)
    {
        return Ok(remote);
    }
    match remotes.len() {
        1 => Ok(remotes[0].clone()),
        0 => Err(GitActionError::InvalidRequest(
            "the workspace has no git remote to push to".into(),
        )),
        _ => Err(GitActionError::InvalidRequest(format!(
            "the workspace has several remotes and none is origin; h4rn will not guess which to push to ({})",
            remotes.join(", ")
        ))),
    }
}

/// The commit `<remote>/<branch>` points at, when the remote branch exists.
fn remote_branch_head(workspace: &Path, remote: &str, branch: &str) -> Option<String> {
    git_opt(
        workspace,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/{remote}/{branch}"),
        ],
        STATUS_TIMEOUT,
    )
    .map(|value| value.trim().to_owned())
    .filter(|value| !value.is_empty())
}

/// Whether every push destination already has this branch at `HEAD`.
///
/// `refs/remotes/<remote>/<branch>` is only what the last fetch saw. A branch
/// deleted on the forge keeps that ref until `fetch --prune`, so a decision
/// made from it can skip the push a pull request needs; a ref that a fetch has
/// not advanced can hide a remote that moved. A dry-run push asks the exact
/// targets the real push will use — the named remote, every `pushurl` and any
/// `insteadOf` rewrite resolved once, the same way `git push` resolves them —
/// so it cannot query a different repository than the ones about to be
/// written. Every destination must report the branch as up to date; one
/// `[new branch]` among several is still a push the pull request needs.
/// It also answers through [`classify_push_failure`], so a rejected credential
/// or branch is reported as one instead of as raw git output.
fn remote_is_up_to_date(
    workspace: &Path,
    remote: &str,
    branch: &str,
    limit: Duration,
) -> Result<bool, GitActionError> {
    let args = dry_run_push_args(branch, remote);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let (success, stdout, stderr) = match git::run_combined_with_timeout(workspace, &args, limit) {
        Ok(output) => output,
        Err(error) => return Err(classify_push_error(&error)),
    };
    let flags = push_record_flags(&stdout, &refspec);
    if !success {
        return Err(classify_push_failure(&stdout, &stderr));
    }
    Ok(!flags.is_empty() && flags.iter().all(|flag| *flag == "="))
}

/// The porcelain flag of every record for one refspec in a push's stdout.
///
/// `git push --porcelain` prints one `<flag>\t<from>:<to>\t<summary>` record
/// per ref and destination, so two `pushurl`s report the branch twice. An
/// empty answer means git said nothing about the branch.
fn push_record_flags<'a>(stdout: &'a str, refspec: &str) -> Vec<&'a str> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let flag = fields.next()?;
            (fields.next()? == refspec).then_some(flag)
        })
        .collect()
}

/// The exact push h4rn runs: one branch, named remote, never forced.
///
/// The refspec has no `+`, and it is on the command line, so neither
/// `push.default` nor `remote.<name>.push` can widen or force it. Porcelain
/// output is requested so a rejection's record is read from the run itself,
/// even when git's advice is turned off.
fn push_args(branch: &str, remote: &str, set_upstream: bool) -> Vec<String> {
    let mut args = push_command_args(branch, remote);
    if set_upstream {
        args.insert(3, "-u".to_owned());
    }
    args
}

/// The same push as a dry run, in porcelain form, for the pre-PR decision.
///
/// `--no-verify` belongs to this probe only: a dry run has no business running
/// the pre-push hook, which can fail, be slow, or change something. The real
/// push still runs it. `--recurse-submodules=no` is here for the same reason:
/// with `push.recurseSubmodules=on-demand` a dry run otherwise pushes
/// unpublished submodule commits for real, and the submodule's own pre-push
/// hook runs even though this command asks not to verify. The real push keeps
/// whatever recursion the repository configured.
fn dry_run_push_args(branch: &str, remote: &str) -> Vec<String> {
    let mut args = push_command_args(branch, remote);
    args.insert(3, "--dry-run".to_owned());
    args.insert(4, "--no-verify".to_owned());
    args.insert(5, "--recurse-submodules=no".to_owned());
    args
}

/// The push every caller shares: config hardening, porcelain, the remote and
/// the refspec.
fn push_command_args(branch: &str, remote: &str) -> Vec<String> {
    vec![
        "-c".to_owned(),
        "push.default=nothing".to_owned(),
        "push".to_owned(),
        "--porcelain".to_owned(),
        "--".to_owned(),
        remote.to_owned(),
        format!("refs/heads/{branch}:refs/heads/{branch}"),
    ]
}

/// Pushes the current branch, blocking.
fn push_blocking(workspace: &Path, set_upstream: bool) -> Result<GitPushResult, GitActionError> {
    let status = status_blocking(workspace)?;
    refuse_in_progress(&status, "pushing")?;
    let branch = status.branch.clone().ok_or_else(|| {
        GitActionError::Conflict(
            "the workspace has a detached HEAD; check out a branch before pushing".into(),
        )
    })?;
    validate_branch(&branch)?;
    let remote = push_remote(workspace, &branch)?;
    validate_remote(&remote)?;
    if remote_branch_head(workspace, &remote, &branch).is_none() && !set_upstream {
        return Err(GitActionError::InvalidRequest(format!(
            "branch {branch} has no upstream branch on {remote}; push with set_upstream to create {remote}/{branch}"
        )));
    }
    run_push(workspace, &push_args(&branch, &remote, set_upstream))?;
    let status = status_blocking(workspace)?;
    Ok(GitPushResult {
        branch: branch.clone(),
        remote_branch: format!("{remote}/{branch}"),
        status,
    })
}

/// Runs one push and turns an auth or rejected answer into an honest error.
fn run_push(workspace: &Path, args: &[String]) -> Result<(), GitActionError> {
    run_push_with(workspace, args, PUSH_TIMEOUT)
}

/// [`run_push`] with the time that step is allowed inside a shared budget.
fn run_push_with(workspace: &Path, args: &[String], limit: Duration) -> Result<(), GitActionError> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match git::run_combined_with_timeout(workspace, &args, limit) {
        Ok((true, _stdout, _stderr)) => Ok(()),
        Ok((false, stdout, stderr)) => Err(classify_push_failure(&stdout, &stderr)),
        Err(error) => Err(classify_push_error(&error)),
    }
}

/// Classifies a push or dry-run push from both of its streams.
///
/// A porcelain push prints each ref's fate on stdout, and exits non-zero when
/// one is rejected. With `advice.pushUpdateRejected=false` stderr then carries
/// only "failed to push some refs", so a rejection is invisible without the
/// stdout record. The two streams are joined for the decision and for the
/// detail, with stderr last because that is where the human-facing error is.
fn classify_push_failure(stdout: &str, stderr: &str) -> GitActionError {
    let mut combined = String::with_capacity(stdout.len() + stderr.len() + 1);
    combined.push_str(stdout.trim());
    if !stderr.trim().is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(stderr.trim());
    }
    classify_push_error(&combined)
}

/// Turns git's push output into the class the person needs to act on.
///
/// The text is redacted first: git can echo a credential-shaped URL back, both
/// a token in the URL and the userinfo or query that carried it, and an error
/// is not a place for a credential.
fn classify_push_error(error: &str) -> GitActionError {
    let error = h4rn_forge::redact(&redact_url_credentials(error));
    let lowered = error.to_ascii_lowercase();
    let detail = tail(&error);
    if [
        "authentication failed",
        "authentication required",
        "could not read username",
        "could not read password",
        "permission denied",
        "access denied",
        "invalid username or password",
        "terminal prompts disabled",
        "support for password authentication was removed",
        "returned error: 401",
        "returned error: 403",
        "403 forbidden",
        "401 unauthorized",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
    {
        return GitActionError::Unauthorized(format!(
            "the remote rejected the credential: {detail}"
        ));
    }
    if lowered.contains("[rejected]")
        || lowered.contains("non-fast-forward")
        || lowered.contains("fetch first")
        || lowered.contains("updates were rejected")
    {
        return GitActionError::Conflict(format!(
            "the remote rejected the push because it has commits this branch does not; pull or rebase, then push again (h4rn never force-pushes): {detail}"
        ));
    }
    GitActionError::Git(format!("git push failed: {detail}"))
}

/// Removes the credential-carrying parts of every URL in `text`.
///
/// Git echoes a failing URL whole, and a push URL can carry a credential in
/// its userinfo (`user:token@host`) or in its query (`?access_token=…`). The
/// token shapes [`h4rn_forge::redact`] knows are covered there; this covers
/// credentials with no recognisable shape by dropping the userinfo, the query
/// and the fragment, which never carry anything the message needs.
///
/// The host is kept, because it is what tells the person which remote failed,
/// and it may be an IPv4 address, an IPv6 literal in brackets, a hostname, or
/// any of those with a port. A URL runs from its scheme to the next
/// whitespace, or to the quote that closes git's `'…'` field. A quote inside
/// the query or fragment is not where the URL ends: git prints it raw, so
/// stopping there would leave the rest of the URL (and its token) in the
/// message. Nor does whitespace inside the quote end the URL: git prints the
/// field whole, and a URL with raw whitespace in it cannot be parsed, so the
/// whole field becomes a marker instead of being split at the whitespace and
/// leaving what follows it (often the rest of a token) in the message.
fn redact_url_credentials(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("://") {
        // The scheme starts at the run of scheme characters before `://`;
        // everything earlier in the string is prose.
        let scheme_start = rest[..index]
            .rfind(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.')))
            .map_or(0, |start| start + 1);
        output.push_str(&rest[..scheme_start]);
        let after = &rest[scheme_start..];
        // A quote directly before the scheme opens the field git prints; its
        // matching close is the URL's end, not the first quote inside it.
        let open_quote = rest[..scheme_start]
            .chars()
            .next_back()
            .filter(|ch| matches!(ch, '\'' | '"'));
        let (field, trailing) = split_url_field(after, open_quote);
        match field {
            UrlField::Url(candidate) => output.push_str(&redact_one_url(candidate)),
            UrlField::Whitespace => output.push_str("[redacted URL]"),
        }
        rest = trailing;
    }
    output.push_str(rest);
    output
}

/// One URL field as [`split_url_field`] found it.
enum UrlField<'a> {
    /// A URL with no raw whitespace; its parts can be redacted.
    Url(&'a str),
    /// A quoted field that contains raw whitespace. No real URL does, so the
    /// field cannot be split into a URL and prose, and is replaced whole.
    Whitespace,
}

/// Splits one URL field off the start of `text`, returning it and the rest.
///
/// An unquoted field ends at the first whitespace (ASCII or Unicode), with
/// sentence punctuation after the URL left in the trailing text. A field git
/// opened with a quote runs to its closing quote — the last matching quote on
/// that line that a message follows with `:`, `)`, `.`, `,` or a line end —
/// and whitespace before that quote does not end it: git prints the field
/// whole, so cutting at a whitespace inside it would leave the rest of the URL
/// (and its token) in the message. Such a field is reported as
/// [`UrlField::Whitespace`] because a URL never contains raw whitespace.
fn split_url_field(text: &str, open_quote: Option<char>) -> (UrlField<'_>, &str) {
    let Some(quote) = open_quote else {
        let end = text.find(char::is_whitespace).unwrap_or(text.len());
        let trimmed = trim_prose_end(&text[..end]);
        return (UrlField::Url(&text[..trimmed]), &text[trimmed..]);
    };

    let line_end = text.find('\n').unwrap_or(text.len());
    let line = &text[..line_end];
    let closing = line
        .char_indices()
        .filter(|(_, ch)| *ch == quote)
        .map(|(index, _)| index)
        .rfind(|index| {
            let after = &line[index + quote.len_utf8()..];
            after.is_empty()
                || after.starts_with([':', ')', '.', ','])
                || after.trim_start_matches([' ', '\t', '\r']).is_empty()
        });
    let Some(closing) = closing else {
        // No closing quote on this line: keep the run to the next whitespace
        // as one URL, as before, so an internal quote cannot cut it short.
        let end = text.find(char::is_whitespace).unwrap_or(text.len());
        let trimmed = trim_prose_end(&text[..end]);
        return (UrlField::Url(&text[..trimmed]), &text[trimmed..]);
    };
    let field = &text[..closing];
    if field.chars().any(char::is_whitespace) {
        return (UrlField::Whitespace, &text[closing..]);
    }
    let trimmed = trim_prose_end(field);
    (UrlField::Url(&text[..trimmed]), &text[trimmed..])
}

/// Trims sentence punctuation that a message puts after a URL.
///
/// A URL echoed in prose can be followed by `)`, `,` or a full stop. A
/// closing `]` is kept when the URL opens an IPv6 literal with `[`, because
/// it is part of the host there.
fn trim_prose_end(text: &str) -> usize {
    let mut trimmed = text.len();
    while let Some((index, ch)) = text[..trimmed].char_indices().next_back() {
        let prose = match ch {
            ')' | ',' | ';' | ':' | '.' | '}' | '!' | '?' => true,
            ']' => !text[..trimmed].contains('['),
            _ => false,
        };
        if !prose {
            break;
        }
        trimmed = index;
    }
    trimmed
}

/// One URL without its userinfo, query and fragment.
///
/// A URL the parser cannot read is replaced whole rather than trimmed by
/// guesswork: guessing its end is exactly how a token used to survive.
fn redact_one_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => redact_parsed_url(&parsed),
        Err(_) => "[redacted URL]".to_owned(),
    }
}

/// A parsed URL rebuilt from the parts that can be shown: scheme, host, port
/// and path; the userinfo, query and fragment become markers.
fn redact_parsed_url(parsed: &url::Url) -> String {
    let mut output = String::with_capacity(parsed.as_str().len());
    output.push_str(parsed.scheme());
    output.push_str("://");
    if !parsed.username().is_empty() || parsed.password().is_some() {
        output.push_str("[redacted]@");
    }
    match parsed.host() {
        Some(url::Host::Ipv6(address)) => {
            output.push('[');
            output.push_str(&address.to_string());
            output.push(']');
        }
        Some(url::Host::Ipv4(address)) => output.push_str(&address.to_string()),
        Some(url::Host::Domain(domain)) => output.push_str(domain),
        None => {}
    }
    if let Some(port) = parsed.port() {
        output.push(':');
        output.push_str(&port.to_string());
    }
    output.push_str(parsed.path());
    if parsed.query().is_some() {
        output.push_str("?[redacted]");
    }
    if parsed.fragment().is_some() {
        output.push_str("#[redacted]");
    }
    output
}

/// Pushes a branch before a PR, blocking; returns whether it pushed.
///
/// The decision is a dry-run push against the named remote, not the branch's
/// configured upstream, so a branch that tracks `origin/main` is still
/// published as its own name on the same remote — and the target is resolved
/// exactly as the real push resolves it.
fn push_for_pull_request_blocking(
    workspace: &Path,
    remote: &str,
    branch: &str,
    deadline: Deadline,
) -> Result<bool, GitActionError> {
    validate_branch(branch)?;
    validate_remote(remote)?;
    let status = status_blocking(workspace)?;
    refuse_in_progress(&status, "pushing")?;
    if status.branch.as_deref() != Some(branch) {
        return Err(GitActionError::Conflict(format!(
            "the workspace is on {}, not {branch}; check out {branch} before opening a pull request",
            status.branch.as_deref().unwrap_or("a detached HEAD")
        )));
    }
    // Compare the remote's own answer with `HEAD`, not the local tracking ref:
    // a stale ref can name a branch that was deleted, or miss one that moved.
    if remote_is_up_to_date(workspace, remote, branch, deadline.step(PR_LOOKUP_TIMEOUT))? {
        return Ok(false);
    }
    let target = format!("{remote}/{branch}");
    let set_upstream = status.upstream.as_deref() != Some(target.as_str());
    run_push_with(
        workspace,
        &push_args(branch, remote, set_upstream),
        pr_push_budget(&deadline),
    )?;
    Ok(true)
}

/// Validates one commit message.
fn validate_message(message: &str) -> Result<&str, GitActionError> {
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return Err(GitActionError::InvalidRequest(
            "write a commit message before committing".into(),
        ));
    }
    if trimmed.chars().count() > MESSAGE_MAX_CHARS {
        return Err(GitActionError::InvalidRequest(format!(
            "a commit message is at most {MESSAGE_MAX_CHARS} characters"
        )));
    }
    if trimmed.contains('\0') {
        return Err(GitActionError::InvalidRequest(
            "a commit message cannot contain a NUL byte".into(),
        ));
    }
    Ok(trimmed)
}

/// Validates one branch name before it is passed to git as an argument.
fn validate_branch(branch: &str) -> Result<(), GitActionError> {
    if branch.is_empty()
        || branch.starts_with('-')
        || branch.starts_with('.')
        || branch.contains("..")
        || branch.chars().any(char::is_control)
    {
        return Err(GitActionError::InvalidRequest(format!(
            "{branch:?} is not a branch h4rn will push"
        )));
    }
    Ok(())
}

/// Validates one remote name before it is passed to git as an argument.
fn validate_remote(remote: &str) -> Result<(), GitActionError> {
    if remote.is_empty() || remote.starts_with('-') || remote.chars().any(char::is_control) {
        return Err(GitActionError::InvalidRequest(format!(
            "{remote:?} is not a git remote h4rn will push to"
        )));
    }
    Ok(())
}

/// Validates requested paths as literal, workspace-relative names.
fn validate_paths(workspace: &Path, paths: &[String]) -> Result<Vec<String>, GitActionError> {
    if paths.is_empty() {
        return Err(GitActionError::InvalidRequest(
            "name at least one path, or leave paths empty for everything".into(),
        ));
    }
    let mut validated = Vec::with_capacity(paths.len());
    for path in paths {
        validated.push(validate_path(workspace, path)?);
    }
    Ok(validated)
}

/// One path: relative, inside the workspace, never option- or magic-shaped.
fn validate_path(workspace: &Path, path: &str) -> Result<String, GitActionError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(GitActionError::InvalidRequest(
            "a path cannot be empty".into(),
        ));
    }
    if path.trim() != path {
        return Err(GitActionError::InvalidRequest(format!(
            "path {path:?} has leading or trailing whitespace"
        )));
    }
    if path.starts_with('-') || path.starts_with(':') {
        return Err(GitActionError::InvalidRequest(format!(
            "path {path:?} cannot start with {start:?}",
            start = &path[..1]
        )));
    }
    if path.chars().any(char::is_control) {
        return Err(GitActionError::InvalidRequest(format!(
            "path {path:?} contains a control character"
        )));
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return Err(GitActionError::InvalidRequest(format!(
            "path {path:?} must be relative to the workspace"
        )));
    }
    for component in candidate.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(GitActionError::InvalidRequest(format!(
                    "path {path:?} escapes the workspace"
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(GitActionError::InvalidRequest(format!(
                    "path {path:?} must be relative to the workspace"
                )));
            }
        }
    }
    // The lexical check above already proves containment for a relative path
    // without `..`; this also refuses a workspace whose path is not itself
    // absolute, which no thread can have.
    if !workspace.is_absolute() {
        return Err(GitActionError::Local(format!(
            "workspace {} is not an absolute path",
            workspace.display()
        )));
    }
    Ok(path.to_owned())
}

/// The revision a working-tree diff starts from.
///
/// A repository with no commits has no `HEAD`; git diffs a first commit
/// against the empty tree. Its object id depends on the repository's object
/// format, so a SHA-256 repository gets its own empty tree rather than the
/// SHA-1 one (which git there cannot resolve).
fn diff_base(workspace: &Path) -> Result<String, GitActionError> {
    if git_opt(
        workspace,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        STATUS_TIMEOUT,
    )
    .is_some()
    {
        return Ok("HEAD".to_owned());
    }
    empty_tree(workspace)
}

/// The empty tree's object id for this repository's object format.
///
/// The documented formats have a known id; anything else (or a git too old to
/// name the format) asks git for it instead of guessing.
fn empty_tree(workspace: &Path) -> Result<String, GitActionError> {
    let format = git_text(
        workspace,
        &["rev-parse", "--show-object-format"],
        STATUS_TIMEOUT,
    )
    .unwrap_or_default();
    match format.trim() {
        "sha1" => Ok(SHA1_EMPTY_TREE.to_owned()),
        "sha256" => Ok(SHA256_EMPTY_TREE.to_owned()),
        _ => {
            let id = git_text(
                workspace,
                &["hash-object", "-t", "tree", EMPTY_FILE],
                STATUS_TIMEOUT,
            )?;
            let id = id.trim();
            if id.is_empty() {
                return Err(GitActionError::Git(
                    "git could not name the empty tree".into(),
                ));
            }
            Ok(id.to_owned())
        }
    }
}

/// Builds the commit-message input, blocking.
fn commit_input_blocking(
    workspace: &Path,
    include_unstaged: bool,
    paths: Option<&[String]>,
) -> Result<CommitInput, GitActionError> {
    let paths = paths
        .map(|paths| validate_paths(workspace, paths))
        .transpose()?;
    let mut args = vec!["--literal-pathspecs", "diff"];
    // A named path is committed with `--only`, which records its working-tree
    // content whether or not it was staged. Describe that, not just the index.
    // A repository with no commits has no `HEAD` to compare with; the empty
    // tree is the same baseline git uses for a first commit.
    let base;
    if include_unstaged || paths.is_some() {
        base = diff_base(workspace)?;
        args.push(&base);
    } else {
        args.push("--cached");
    }
    args.extend([
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--relative",
    ]);
    if paths.is_some() {
        args.push("--");
        if let Some(paths) = &paths {
            args.extend(paths.iter().map(String::as_str));
        }
    }
    let diff = git_text(workspace, &args, INPUT_TIMEOUT).map_err(|error| match error {
        GitActionError::Git(message) => {
            GitActionError::Git(format!("could not read the diff: {message}"))
        }
        other => other,
    })?;
    let mut diff = cap(&diff, DIFF_MAX_CHARS);

    if include_unstaged {
        // Untracked files are named, never read: their contents are not part
        // of the diff, and sending them would leak unrelated files.
        let mut untracked = nul_paths(git_bytes(
            workspace,
            &["ls-files", "--others", "--exclude-standard", "-z"],
            INPUT_TIMEOUT,
        )?);
        if let Some(paths) = &paths {
            untracked.retain(|path| {
                paths
                    .iter()
                    .any(|wanted| path == wanted || path.starts_with(&format!("{wanted}/")))
            });
        }
        if !untracked.is_empty() {
            diff.push_str("\n\nUntracked files (names only):\n");
            for path in untracked.iter().take(50) {
                diff.push_str("- ");
                diff.push_str(path);
                diff.push('\n');
            }
            if untracked.len() > 50 {
                diff.push_str(&format!("- and {} more\n", untracked.len() - 50));
            }
            diff = cap(&diff, DIFF_MAX_CHARS);
        }
    }
    if diff.trim().is_empty() {
        return Err(GitActionError::InvalidRequest(if include_unstaged {
            "there are no changes to describe".into()
        } else {
            "there are no staged changes to describe; stage something first".into()
        }));
    }

    let subjects = git_opt(
        workspace,
        &["log", &format!("-n{RECENT_SUBJECTS}"), "--format=%s"],
        INPUT_TIMEOUT,
    )
    .map(|value| {
        value
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| line.chars().take(SUBJECT_MAX_CHARS).collect())
            .collect()
    })
    .unwrap_or_default();
    Ok(CommitInput {
        diff,
        recent_subjects: subjects,
    })
}

/// Builds the pull request input, blocking.
fn pull_request_input_blocking(
    workspace: &Path,
    base: Option<&str>,
) -> Result<PullRequestInput, GitActionError> {
    let base = match base.map(str::trim).filter(|base| !base.is_empty()) {
        Some(base) => base.to_owned(),
        None => default_branch(workspace).ok_or_else(|| {
            GitActionError::InvalidRequest(
                "no default branch was detected; name the branch this pull request targets".into(),
            )
        })?,
    };
    if base.starts_with('-') || base.chars().any(char::is_control) {
        return Err(GitActionError::InvalidRequest(format!(
            "{base:?} is not a branch name"
        )));
    }
    let reference = ["refs/remotes/origin/", "refs/heads/", ""]
        .iter()
        .map(|prefix| format!("{prefix}{base}"))
        .find(|candidate| {
            git_opt(
                workspace,
                &["rev-parse", "--verify", "--quiet", candidate],
                STATUS_TIMEOUT,
            )
            .is_some()
        })
        .ok_or_else(|| {
            GitActionError::InvalidRequest(format!(
                "the workspace has no branch {base} to compare against"
            ))
        })?;
    let range = format!("{reference}..HEAD");
    let commits = git_text(
        workspace,
        &["log", "--format=%s%n%b%n---", &range],
        INPUT_TIMEOUT,
    )?;
    let commits = commits.trim();
    if commits.is_empty() {
        return Err(GitActionError::InvalidRequest(format!(
            "the branch has no commits against {base} to describe"
        )));
    }
    let stat = git_text(
        workspace,
        &[
            "diff",
            "--stat",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            &range,
        ],
        INPUT_TIMEOUT,
    )?;
    Ok(PullRequestInput {
        commits: cap(commits, COMMITS_MAX_CHARS),
        diff_stat: cap(stat.trim(), DIFF_STAT_MAX_CHARS),
        base,
    })
}

/// Truncates at a character boundary and says so.
fn cap(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut capped: String = value.chars().take(limit).collect();
    capped.push_str("\n[truncated]");
    capped
}

/// The end of a long failure message.
fn tail(value: &str) -> String {
    let value = value.trim();
    let count = value.chars().count();
    if count <= ERROR_TAIL_CHARS {
        return value.to_owned();
    }
    let tail: String = value.chars().skip(count - ERROR_TAIL_CHARS).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests;
