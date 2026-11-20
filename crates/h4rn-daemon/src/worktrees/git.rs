//! Blocking git helpers for worktree tracking.
//!
//! Every function here shells out to `git` and blocks. Callers on the async
//! runtime must run them inside `tokio::task::spawn_blocking`.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

/// How often a bounded git call checks whether its child has exited.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Keys whose string values are whole paths (they may contain spaces).
const PATH_KEYS: &[&str] = &[
    "cwd",
    "workdir",
    "path",
    "file_path",
    "notebook_path",
    "filePath",
    "directory",
    "dir",
];

/// Most candidates one tool call can contribute.
const MAX_CANDIDATES: usize = 64;

/// Longest string scanned for path tokens.
const MAX_SCAN_BYTES: usize = 16 * 1024;

/// One linked worktree reported by `git worktree list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitWorktree {
    /// Normalised absolute path.
    pub path: PathBuf,
    /// Checked-out branch; `None` for a detached HEAD.
    pub branch: Option<String>,
}

/// One worktree registration as `git worktree list --porcelain` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    /// Normalised absolute path.
    pub path: PathBuf,
    /// Checked-out branch by short name; `None` for a detached HEAD.
    pub branch: Option<String>,
    /// Git considers the registration stale because the directory is gone.
    pub prunable: bool,
}

fn command(dir: &Path) -> Command {
    let mut command = Command::new("git");
    // Read-only inspection must never wait on the index lock or a prompt.
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-C")
        .arg(dir);
    command
}

/// Runs git in `dir`, returning trimmed stdout or trimmed stderr on failure.
pub fn run<I, S>(dir: &Path, args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = command(dir)
        .args(args)
        .output()
        .map_err(|error| format!("could not run git: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        Err(if stderr.is_empty() {
            format!("git exited with {}", output.status)
        } else {
            stderr.to_owned()
        })
    }
}

/// Runs git in `dir`, killing it once `limit` has passed.
///
/// Clearing must not hang on a wedged checkout (an unresponsive filesystem, a
/// stale index lock): each command gets its own deadline and a timed-out child
/// is killed, so the rest of the clear can go on and report a skip. The error
/// for a deadline is `git timed out after Ns`; [`timed_out`] recognises it.
pub fn run_with_timeout<I, S>(dir: &Path, args: I, limit: Duration) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut child = command(dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run git: {error}"))?;
    // Drain both pipes while the child runs: a command whose output fills the
    // pipe buffer would otherwise never exit and look like a timeout. Each
    // reader sends its bytes back when the pipe closes.
    let stdout = child.stdout.take().map(spawn_reader);
    let stderr = child.stderr.take().map(spawn_reader);
    let deadline = Instant::now() + limit;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break None;
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not run git: {error}"));
            }
        }
    };
    // Collecting output is bounded too: a descendant (a background process a
    // git alias left running) can keep the pipes open long after git exited.
    // Wait for each reader with the deadline left plus a short grace, then
    // detach it and report the timeout instead of hanging.
    let output_deadline =
        Instant::now() + deadline.saturating_duration_since(Instant::now()) + OUTPUT_GRACE;
    let stdout = read_output(stdout, output_deadline);
    let stderr = read_output(stderr, output_deadline);
    if timed_out || stdout.is_err() || stderr.is_err() {
        return Err(timeout_message(limit));
    }
    let stdout = stdout.unwrap_or_default();
    let stderr = stderr.unwrap_or_default();
    let Some(status) = status else {
        return Err(timeout_message(limit));
    };
    if status.success() {
        Ok(String::from_utf8_lossy(&stdout).trim().to_owned())
    } else {
        let stderr = String::from_utf8_lossy(&stderr);
        let stderr = stderr.trim();
        Err(if stderr.is_empty() {
            format!("git exited with {status}")
        } else {
            stderr.to_owned()
        })
    }
}

/// How long output may still be arriving after the child is gone.
const OUTPUT_GRACE: Duration = Duration::from_secs(1);

/// Reads one pipe to the end on its own thread and sends the bytes back.
///
/// The sender drops when the pipe closes, so the receiver can tell "no more
/// output" from "a descendant still holds the pipe".
fn spawn_reader(
    mut pipe: impl std::io::Read + Send + 'static,
) -> (JoinHandle<()>, Receiver<Vec<u8>>) {
    let (sender, receiver) = channel();
    let handle = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = pipe.read_to_end(&mut buffer);
        let _ = sender.send(buffer);
    });
    (handle, receiver)
}

/// Waits for one reader's output until `deadline`; then detaches the thread.
///
/// `Err(())` means the bound passed with the pipe still open, which is a
/// timeout for the whole call rather than empty output.
fn read_output(
    reader: Option<(JoinHandle<()>, Receiver<Vec<u8>>)>,
    deadline: Instant,
) -> Result<Vec<u8>, ()> {
    let Some((handle, receiver)) = reader else {
        return Ok(Vec::new());
    };
    let wait = deadline.saturating_duration_since(Instant::now());
    let output = match receiver.recv_timeout(wait) {
        Ok(buffer) => Ok(buffer),
        Err(RecvTimeoutError::Timeout) => Err(()),
        // The reader thread panicked; treat it as no output.
        Err(RecvTimeoutError::Disconnected) => Ok(Vec::new()),
    };
    // Dropping the handle detaches a stuck reader instead of joining it.
    drop(handle);
    output
}

fn timeout_message(limit: Duration) -> String {
    format!("git timed out after {}s", limit.as_secs())
}

/// Whether an error from [`run_with_timeout`] means the child was killed.
pub fn timed_out(error: &str) -> bool {
    error.starts_with("git timed out after ")
}

/// Whether a git command exits successfully.
#[cfg(test)]
pub fn succeeds<I, S>(dir: &Path, args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    command(dir)
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Normalises a path for comparison.
///
/// The path is canonicalised exactly as written first, so a `..` after a
/// symlink means what the filesystem says it means: `link -> checkout/subdir`
/// makes `link/..` the checkout, not the symlink's parent directory. Only when
/// the path does not exist is the longest existing ancestor canonicalised (as
/// written, again without interpreting `..` first) and `.`/`..` folded lexically
/// in the unresolved suffix.
pub fn norm(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return strip_verbatim(canonical);
    }
    let components: Vec<Component<'_>> = path.components().collect();
    for split in (0..components.len()).rev() {
        let mut prefix = PathBuf::new();
        for component in &components[..split] {
            prefix.push(component.as_os_str());
        }
        let Ok(canonical) = std::fs::canonicalize(&prefix) else {
            continue;
        };
        let mut out = canonical;
        for component in &components[split..] {
            out.push(component.as_os_str());
        }
        return strip_verbatim(fold(out));
    }
    fold(path.to_path_buf())
}

/// Folds `.` and `..` lexically, without consulting the filesystem.
fn fold(path: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Drops the Windows `\\?\` verbatim prefix canonicalisation adds.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => path,
    }
}

/// The main worktree of the repository containing `workspace`.
///
/// `None` for a non-repository, a bare repository or a submodule.
pub(crate) fn repo_of(workspace: &Path) -> Option<PathBuf> {
    let common = run(workspace, ["rev-parse", "--git-common-dir"]).ok()?;
    let common = norm(&workspace.join(common));
    if common.file_name()? == ".git" {
        common.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

/// The top level of the checkout containing `workspace`, normalised.
pub fn toplevel(workspace: &Path) -> Option<PathBuf> {
    let top = run(workspace, ["rev-parse", "--show-toplevel"]).ok()?;
    (!top.is_empty()).then(|| norm(Path::new(&top)))
}

/// Whether `workspace` is a linked worktree rather than a main checkout.
pub fn is_linked_worktree(workspace: &Path) -> bool {
    let Ok(out) = run(workspace, ["rev-parse", "--git-dir", "--git-common-dir"]) else {
        return false;
    };
    let mut lines = out.lines();
    let (Some(dir), Some(common)) = (lines.next(), lines.next()) else {
        return false;
    };
    norm(&workspace.join(dir)) != norm(&workspace.join(common))
}

/// Linked worktrees of `repo`, excluding its main worktree.
pub fn list_worktrees(repo: &Path) -> Result<Vec<GitWorktree>, String> {
    let main = norm(repo);
    Ok(registrations(repo)?
        .into_iter()
        .filter(|registration| registration.path != main)
        .map(|registration| GitWorktree {
            path: registration.path,
            branch: registration.branch,
        })
        .collect())
}

/// Every worktree registration of `repo`, main checkout included.
pub fn registrations(repo: &Path) -> Result<Vec<Registration>, String> {
    let out = run(repo, ["worktree", "list", "--porcelain"])?;
    Ok(parse_registrations(&out))
}

/// Parses `git worktree list --porcelain` output.
pub fn parse_registrations(out: &str) -> Vec<Registration> {
    let mut registrations = Vec::new();
    for block in out.split("\n\n") {
        let mut path = None;
        let mut branch = None;
        let mut prunable = false;
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(norm(Path::new(value)));
            } else if let Some(value) = line.strip_prefix("branch ") {
                branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_owned(),
                );
            } else if line.starts_with("prunable") {
                prunable = true;
            }
        }
        if let Some(path) = path {
            registrations.push(Registration {
                path,
                branch,
                prunable,
            });
        }
    }
    registrations
}

/// Worktrees present in `after` but not in `before`.
pub fn new_worktrees(before: &[GitWorktree], after: &[GitWorktree]) -> Vec<GitWorktree> {
    after
        .iter()
        .filter(|worktree| before.iter().all(|old| old.path != worktree.path))
        .cloned()
        .collect()
}

/// Whether `refs/heads/<branch>` exists in `repo`.
#[cfg(test)]
pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    succeeds(
        repo,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
}

/// Every branch under `refs/heads/h4rn/` in `repo`, by short name.
///
/// One `for-each-ref` answers for all of a repository's owned branches, which
/// beats a `rev-parse --verify` per subagent. Full ref names are requested so a
/// same-named tag cannot hide a branch; the `refs/heads/` prefix is then
/// stripped.
pub fn h4rn_branches(repo: &Path) -> HashSet<String> {
    match run(
        repo,
        ["for-each-ref", "--format=%(refname)", "refs/heads/h4rn/"],
    ) {
        Ok(out) => out
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .filter_map(|line| line.strip_prefix("refs/heads/"))
            .map(str::to_owned)
            .collect(),
        Err(_) => HashSet::new(),
    }
}

/// Whether the worktree has uncommitted changes; `None` when git failed.
///
/// Untracked files always count, whatever the user's `status.showUntrackedFiles`
/// config says, and submodules are inspected rather than skipped: a file the
/// user asked git to hide from `status` is still work that removing the
/// checkout would lose.
pub fn is_dirty(worktree: &Path) -> Option<bool> {
    run(
        worktree,
        [
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )
    .ok()
    .map(|out| !out.is_empty())
}

/// Whether `token` starts like an absolute path on Unix or Windows.
fn looks_absolute(token: &str) -> bool {
    if token.starts_with('/') {
        return token.len() > 1;
    }
    let bytes = token.as_bytes();
    bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn clean_token(token: &str) -> &str {
    let token = token.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ';' | ',' | '(' | ')'));
    // `--flag=/path` and `VAR=/path`.
    match token.split_once('=') {
        Some((_, value)) if looks_absolute(value) => value,
        _ => token,
    }
}

fn collect(value: &Value, key: Option<&str>, out: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            if out.len() >= MAX_CANDIDATES {
                return;
            }
            if key.is_some_and(|key| PATH_KEYS.contains(&key)) {
                let whole = text.trim();
                if looks_absolute(whole) {
                    out.push(whole.to_owned());
                    return;
                }
            }
            let mut end = text.len().min(MAX_SCAN_BYTES);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            for token in text[..end].split_whitespace() {
                let token = clean_token(token);
                if looks_absolute(token) && out.len() < MAX_CANDIDATES {
                    out.push(token.to_owned());
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect(item, key, out)),
        Value::Object(map) => map
            .iter()
            .for_each(|(key, item)| collect(item, Some(key), out)),
        _ => {}
    }
}

/// Absolute-path candidates found in a tool call's input JSON.
///
/// String values of well-known path keys are taken whole; every other string
/// (a shell command, say) is split on whitespace and quotes. Order is kept and
/// duplicates dropped.
pub fn path_candidates(input: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect(input, None, &mut out);
    let mut seen = std::collections::HashSet::new();
    out.retain(|candidate| seen.insert(candidate.clone()));
    out
}

/// Whether any string in the input mentions `worktree` (a `git worktree` call).
pub fn mentions_worktree(input: &Value) -> bool {
    match input {
        Value::String(text) => text.contains("worktree"),
        Value::Array(items) => items.iter().any(mentions_worktree),
        Value::Object(map) => map.values().any(mentions_worktree),
        _ => false,
    }
}
