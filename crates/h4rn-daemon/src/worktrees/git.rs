//! Blocking git helpers for worktree tracking.
//!
//! Every function here shells out to `git` and blocks. Callers on the async
//! runtime must run them inside `tokio::task::spawn_blocking`.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

/// How often a bounded git call checks whether its child has exited.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Deadline for every git call the observation watcher makes.
///
/// Observation runs on the same loop as every thread's events, so one wedged
/// repository must not stall it: a call that outlives this is killed and that
/// event's work dropped.
pub const OBSERVATION_GIT_TIMEOUT: Duration = Duration::from_secs(10);

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

/// What a registration's `HEAD` names.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Head {
    /// A branch, by short name (`refs/heads/` stripped).
    Branch(String),
    /// A detached commit.
    Detached(String),
    /// `HEAD` could not be read or parsed (or the registration is a bare
    /// repository's). Never read as "nothing to protect".
    Unknown,
}

/// One worktree registration as `git worktree list --porcelain -z` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    /// Normalised absolute path.
    pub path: PathBuf,
    /// What the registration's `HEAD` names.
    pub head: Head,
    /// Git considers the registration stale because the directory is gone.
    pub prunable: bool,
    /// The registration is locked (`git worktree lock`).
    pub locked: bool,
    /// The registration is a bare repository's main one (porcelain `bare`).
    /// It has no working tree and never holds a branch checked out.
    pub bare: bool,
}

/// One read of every worktree registration of a repository.
///
/// `complete` is false when some registration could not be read: an admin
/// entry under `<common>/worktrees` whose `gitdir` is not a readable regular
/// file, or one that the answer does not represent (git's `-z` silently omits
/// an admin entry whose `gitdir` it cannot read).
/// A snapshot that is not complete never proves that something is absent: a
/// missing registration may be exactly the unreadable one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistrationSnapshot {
    /// Every registration that could be read, main worktree included.
    pub registrations: Vec<Registration>,
    /// Whether every registration could be read.
    pub complete: bool,
}

impl std::ops::Deref for RegistrationSnapshot {
    type Target = [Registration];

    fn deref(&self) -> &[Registration] {
        &self.registrations
    }
}

impl Registration {
    /// The checked-out branch by short name; `None` when detached or unknown.
    pub fn branch(&self) -> Option<&str> {
        match &self.head {
            Head::Branch(branch) => Some(branch),
            Head::Detached(_) | Head::Unknown => None,
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only: the git program this thread runs instead of `git`.
    pub static GIT_PROGRAM: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
    /// Test-only: read registrations from git's metadata on this thread, as
    /// for a git that rejects `worktree list -z`.
    pub static FORCE_METADATA_FALLBACK: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn program() -> std::ffi::OsString {
    GIT_PROGRAM
        .with(|program| program.borrow().clone())
        .map_or_else(|| "git".into(), PathBuf::into_os_string)
}

#[cfg(not(test))]
fn program() -> &'static str {
    "git"
}

#[cfg(test)]
fn forced_metadata_fallback() -> bool {
    FORCE_METADATA_FALLBACK.with(std::cell::Cell::get)
}

#[cfg(not(test))]
const fn forced_metadata_fallback() -> bool {
    false
}

/// Variables that make git write traces to stderr (or elsewhere).
///
/// Several lookups read any stderr output on success as "inconclusive" (a
/// broken-ref warning), so trace noise inherited from the daemon's environment
/// must never reach them.
const TRACE_VARS: &[&str] = &[
    "GIT_TRACE",
    "GIT_TRACE2",
    "GIT_TRACE2_EVENT",
    "GIT_TRACE2_PERF",
    "GIT_TRACE_PACKET",
    "GIT_TRACE_PERFORMANCE",
    "GIT_TRACE_SETUP",
    "GIT_TRACE_REFS",
    "GIT_TRACE_FSMONITOR",
    "GIT_TRACE_SHALLOW",
    "GIT_CURL_VERBOSE",
];

/// Config overrides for every git call that decides whether work would be
/// lost (`status`, `ls-files -v`, `worktree remove`).
///
/// A filesystem monitor (`core.fsmonitor`) or the untracked cache lets git
/// skip looking at files it was told have not changed. A hook that reports
/// nothing, or a stale cache, would then hide an edit or a new file, so these
/// calls always look at the disk themselves. `-c` reaches the `git status` that
/// `git worktree remove` runs in a child process too.
pub const SAFETY_CONFIG: [&str; 4] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
];

fn command(dir: &Path) -> Command {
    let mut command = Command::new(program());
    // Read-only inspection must never wait on the index lock or a prompt.
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    for name in TRACE_VARS {
        command.env_remove(name);
    }
    command.arg("-C").arg(dir);
    command
}

/// `ETXTBSY`: the program file is still open for writing somewhere.
///
/// Another thread that forks while a freshly written executable is open (a
/// test's fake git, a git being upgraded in place) briefly holds its write
/// descriptor until it execs, and `exec` of that file fails in that window.
const ETXTBSY: i32 = 26;

/// How many times a spawn that hit [`ETXTBSY`] is tried in all.
const SPAWN_ATTEMPTS: u32 = 5;

/// Spawns `command`, retrying a few times with a short sleep on [`ETXTBSY`].
fn spawn(command: &mut Command) -> std::io::Result<std::process::Child> {
    let mut attempt = 1;
    loop {
        match command.spawn() {
            Err(error)
                if cfg!(unix)
                    && error.raw_os_error() == Some(ETXTBSY)
                    && attempt < SPAWN_ATTEMPTS =>
            {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(10 * u64::from(attempt)));
            }
            result => return result,
        }
    }
}

/// Runs git in `dir`, returning trimmed stdout or trimmed stderr on failure.
#[cfg(test)]
pub fn run<I, S>(dir: &Path, args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = spawn(
        command(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .and_then(std::process::Child::wait_with_output)
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
    let stdout = run_bytes_with_timeout(dir, args, limit)?;
    Ok(String::from_utf8_lossy(&stdout).trim().to_owned())
}

/// [`run_with_timeout`], keeping stdout exactly as git wrote it.
///
/// Porcelain output with `-z` is NUL-delimited and may hold path bytes that are
/// not valid UTF-8; trimming or lossily decoding it would merge or damage
/// records, so those callers use this and parse the bytes.
pub fn run_bytes_with_timeout<I, S>(dir: &Path, args: I, limit: Duration) -> Result<Vec<u8>, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (status, stdout, stderr) = run_raw(dir, args, limit)?;
    if status.success() {
        Ok(stdout)
    } else {
        Err(failure_message(status, &stderr))
    }
}

/// Git's trimmed stderr, or its exit status when stderr is empty.
fn failure_message(status: ExitStatus, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("git exited with {status}")
    } else {
        stderr.to_owned()
    }
}

/// Runs git with a deadline and returns its exit status, stdout and stderr.
///
/// `Err` only when git could not be run or the deadline passed.
fn run_raw<I, S>(
    dir: &Path,
    args: I,
    limit: Duration,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut child = spawn(
        command(dir)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
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
    Ok((status, stdout, stderr))
}

/// Runs git for one path-valued answer (`rev-parse --show-toplevel` and the
/// like), keeping the path byte-exact.
///
/// Only the single trailing newline git adds is stripped, never other
/// whitespace, so a directory named `checkout ` stays distinct from
/// `checkout`. Output that is not valid UTF-8 is an error rather than a lossy
/// path: the protocol carries paths as strings, so such a path is skipped.
pub fn run_path_with_timeout<I, S>(dir: &Path, args: I, limit: Duration) -> Result<PathBuf, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    path_output(run_bytes_with_timeout(dir, args, limit)?)
}

/// A path from git's stdout: one trailing newline stripped, UTF-8 required.
fn path_output(mut bytes: Vec<u8>) -> Result<PathBuf, String> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    String::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|_| "git printed a path that is not valid UTF-8".to_owned())
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
    spawn(
        command(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
    .and_then(|mut child| child.wait())
    .is_ok_and(|status| status.success())
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

/// [`norm`], except that a final component that is a symlink is not followed.
///
/// Registration paths are compared exactly with the path a clear acts on, so a
/// registered path that has since been replaced by a symlink must keep its own
/// name instead of turning into the symlink's target (which may well be another
/// registered worktree). The parent is still normalised.
pub fn norm_listed(path: &Path) -> PathBuf {
    let is_symlink = path
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink());
    match (is_symlink, path.parent(), path.file_name()) {
        (true, Some(parent), Some(name)) => norm(parent).join(name),
        _ => norm(path),
    }
}

/// Folds `.` and `..` lexically, without consulting the filesystem.
pub(crate) fn fold(path: PathBuf) -> PathBuf {
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

/// The repository root of the repository containing `workspace`: its main
/// worktree, or for a bare repository the repository directory itself (the
/// common dir), which is where git commands about its linked worktrees run.
///
/// Bareness is git's answer (see [`is_bare`]), never the directory's name. A
/// non-bare repository whose common dir is not named `.git` (a
/// `--separate-git-dir` repository, a submodule) and a non-repository are
/// `None`. Bounded by [`OBSERVATION_GIT_TIMEOUT`] per git call because the
/// observation watcher calls it.
pub(crate) fn repo_of(workspace: &Path) -> Option<PathBuf> {
    let common = observed_path(workspace, "--git-common-dir")?;
    let common = norm(&workspace.join(common));
    if is_bare(&common, OBSERVATION_GIT_TIMEOUT).ok()? {
        return Some(common);
    }
    if common.file_name()? == ".git" {
        common.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

/// Whether the repository whose common dir is `common` is bare, as git
/// itself answers (`git --git-dir=<common> rev-parse --is-bare-repository`).
///
/// `Err` when git fails, times out, or answers anything but `true`/`false`.
pub(crate) fn is_bare(common: &Path, limit: Duration) -> Result<bool, String> {
    let answer = run_with_timeout(
        common,
        ["--git-dir=.", "rev-parse", "--is-bare-repository"],
        limit,
    )?;
    match answer.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!(
            "git could not tell whether the repository is bare: {other}"
        )),
    }
}

/// The `core.worktree` of the repository whose common dir is `common`,
/// resolved against it when relative and normalised; `None` when unset.
fn core_worktree(common: &Path, limit: Duration) -> Result<Option<PathBuf>, String> {
    let (status, stdout, stderr) = run_raw(
        common,
        ["--git-dir=.", "config", "--get", "core.worktree"],
        limit,
    )?;
    if status.success() {
        let value = path_output(stdout)?;
        return Ok((!value.as_os_str().is_empty()).then(|| norm(&common.join(value))));
    }
    // `git config --get` exits 1 for a key that is not set.
    if status.code() == Some(1) && stderr.is_empty() {
        return Ok(None);
    }
    Err(failure_message(status, &stderr))
}

/// One `rev-parse` path for the observation watcher, byte-exact and bounded by
/// [`OBSERVATION_GIT_TIMEOUT`]; `None` when git fails or the path is not UTF-8.
fn observed_path(workspace: &Path, flag: &str) -> Option<PathBuf> {
    match run_path_with_timeout(workspace, ["rev-parse", flag], OBSERVATION_GIT_TIMEOUT) {
        Ok(path) => (!path.as_os_str().is_empty()).then_some(path),
        Err(error) => {
            if timed_out(&error) {
                tracing::debug!(
                    %error,
                    workspace = %workspace.display(),
                    flag,
                    "git path lookup timed out"
                );
            }
            None
        }
    }
}

/// The common git directory of `repo`, normalised.
///
/// Bounded by the caller's deadline because clearing uses it to inspect the
/// worktree admin directories for an in-progress rebase or bisect.
pub fn common_dir(repo: &Path, limit: Duration) -> Result<PathBuf, String> {
    let common = run_path_with_timeout(repo, ["rev-parse", "--git-common-dir"], limit)?;
    Ok(norm(&repo.join(common)))
}

/// Common dirs resolved once per repository, for one clear or one listing.
///
/// Every registration snapshot cross-checks git's answer against the admin
/// entries under the common dir, and clearing inspects those admin entries
/// too; resolving it with `rev-parse --git-common-dir` for every snapshot
/// would cost one git spawn each. Only successful lookups are kept, so a
/// timeout is retried by the next caller.
#[derive(Debug, Default)]
pub struct CommonDirs(std::sync::Mutex<HashMap<PathBuf, PathBuf>>);

impl CommonDirs {
    /// The common dir of `repo`, asking git (bounded by `limit`) only the
    /// first time.
    pub fn get(&self, repo: &Path, limit: Duration) -> Result<PathBuf, String> {
        if let Some(common) = self.lock().get(repo) {
            return Ok(common.clone());
        }
        let common = common_dir(repo, limit)?;
        self.lock().insert(repo.to_path_buf(), common.clone());
        Ok(common)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, PathBuf>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The top level of the checkout containing `workspace`, normalised.
///
/// Bounded by [`OBSERVATION_GIT_TIMEOUT`] because the observation watcher
/// calls it.
pub fn toplevel(workspace: &Path) -> Option<PathBuf> {
    observed_path(workspace, "--show-toplevel").map(|top| norm(&top))
}

/// Whether `workspace` is a linked worktree rather than a main checkout.
///
/// The two directories are asked for separately so each path is read
/// byte-exact; a combined answer would have to be split on newlines.
pub fn is_linked_worktree(workspace: &Path) -> bool {
    let (Some(dir), Some(common)) = (
        observed_path(workspace, "--git-dir"),
        observed_path(workspace, "--git-common-dir"),
    ) else {
        return false;
    };
    norm(&workspace.join(dir)) != norm(&workspace.join(common))
}

/// Linked worktrees of `repo`, excluding its main worktree.
///
/// Registrations whose path is not valid UTF-8 are skipped: the protocol
/// carries paths as strings, so offering one would mean offering a lossy path
/// that can never name the directory it came from.
pub fn list_worktrees(repo: &Path) -> Result<Vec<GitWorktree>, String> {
    list_worktrees_with(repo, &CommonDirs::default())
}

/// [`list_worktrees`], resolving the common dir through `commons`.
pub fn list_worktrees_with(repo: &Path, commons: &CommonDirs) -> Result<Vec<GitWorktree>, String> {
    let main = norm(repo);
    let common = commons.get(repo, OBSERVATION_GIT_TIMEOUT)?;
    Ok(registrations_in(repo, &common, OBSERVATION_GIT_TIMEOUT)?
        .registrations
        .into_iter()
        .filter(|registration| registration.path != main)
        .filter(|registration| registration.path.to_str().is_some())
        .map(|registration| GitWorktree {
            branch: registration.branch().map(str::to_owned),
            path: registration.path,
        })
        .collect())
}

/// Every worktree registration of `repo`, main checkout included, resolving
/// its common dir first (see [`registrations_in`]).
#[cfg(test)]
pub fn registrations(repo: &Path) -> Result<RegistrationSnapshot, String> {
    let common = common_dir(repo, OBSERVATION_GIT_TIMEOUT)?;
    registrations_in(repo, &common, OBSERVATION_GIT_TIMEOUT)
}

/// Whether `git worktree list --porcelain -z` works in this process.
///
/// The answer cannot change while the daemon runs, so the first call records
/// it: after that, a repository with an old git never pays for the failed `-z`
/// probe again.
const PORCELAIN_Z_UNKNOWN: u8 = 0;
const PORCELAIN_Z_WORKS: u8 = 1;
const PORCELAIN_Z_UNSUPPORTED: u8 = 2;
static PORCELAIN_Z: AtomicU8 = AtomicU8::new(PORCELAIN_Z_UNKNOWN);

/// Every worktree registration of `repo`, whose common dir is `common`.
///
/// Callers that take several snapshots of one repository resolve the common
/// dir once (see [`CommonDirs`]).
///
/// `-z` (git 2.36+) is used when available. An older git rejects the switch,
/// and then git's own registration metadata is read instead (see
/// [`metadata_registrations`]); which form works is cached for the rest
/// of the process. Bounded by `limit` overall.
pub fn registrations_in(
    repo: &Path,
    common: &Path,
    limit: Duration,
) -> Result<RegistrationSnapshot, String> {
    let deadline = Instant::now() + limit;
    if forced_metadata_fallback() || PORCELAIN_Z.load(Ordering::Relaxed) == PORCELAIN_Z_UNSUPPORTED
    {
        return metadata_registrations(common, deadline);
    }
    match porcelain_z(repo, limit)? {
        // Git exits 0 even when it skipped an admin entry it could not read,
        // so the answer is complete only if it represents every admin entry.
        Some(registrations) => {
            PORCELAIN_Z.store(PORCELAIN_Z_WORKS, Ordering::Relaxed);
            let complete = represents_every_admin_entry(common, &registrations, deadline);
            Ok(RegistrationSnapshot {
                registrations,
                complete,
            })
        }
        None => {
            PORCELAIN_Z.store(PORCELAIN_Z_UNSUPPORTED, Ordering::Relaxed);
            metadata_registrations(common, deadline)
        }
    }
}

/// Whether a successful `-z` answer names a registration for every admin
/// entry under `<common>/worktrees`.
///
/// Git skips an admin entry whose `gitdir` it cannot read (empty, say) and
/// still exits 0, so its answer alone never proves absence. Each admin entry
/// is matched by the checkout path its `gitdir` names; an entry whose `gitdir`
/// cannot be read as a regular file, an entry that is not a plain directory
/// (checked without following symlinks), an entry the answer does not name,
/// an admin directory that cannot be listed, and running out of time (checked
/// while enumerating and again for every entry while matching) all make the
/// snapshot incomplete. Stray plain files and admin directories without any
/// `gitdir` hold no registration, as for git (see [`admin_entries`]). The
/// answer's paths are indexed once, so matching is linear in the entries.
fn represents_every_admin_entry(
    common: &Path,
    registrations: &[Registration],
    deadline: Instant,
) -> bool {
    let Ok((entries, complete)) = admin_entries(common, deadline) else {
        return false;
    };
    if !complete {
        return false;
    }
    let listed: HashSet<&Path> = registrations
        .iter()
        .map(|registration| registration.path.as_path())
        .collect();
    for entry in &entries {
        if Instant::now() >= deadline {
            return false;
        }
        if !listed.contains(norm_listed(&entry.checkout).as_path()) {
            return false;
        }
    }
    true
}

/// One `<common>/worktrees/<id>` admin directory whose `gitdir` was read.
struct AdminEntry {
    /// The admin directory itself.
    admin: PathBuf,
    /// The checkout the `gitdir` names, not yet normalised (see
    /// [`checkout_of_gitdir`]).
    checkout: PathBuf,
}

/// The checkout a registration's `gitdir` target names, as git derives it:
/// the target with a trailing `.git` component stripped when it has one, the
/// target itself otherwise.
fn checkout_of_gitdir(gitdir: PathBuf) -> PathBuf {
    if gitdir.file_name().is_some_and(|name| name == ".git")
        && let Some(parent) = gitdir.parent()
    {
        return parent.to_path_buf();
    }
    gitdir
}

/// Every admin directory under `<common>/worktrees` whose `gitdir` names
/// `checkout` (compared as the registration snapshot compares paths).
///
/// `Err` when the admin directory cannot be listed, or when the enumeration
/// was cut short or skipped an unreadable entry and nothing matched: the
/// unreadable entry may be the one. Bounded by `limit`.
pub fn admin_dirs_for(
    common: &Path,
    checkout: &Path,
    limit: Duration,
) -> Result<Vec<PathBuf>, String> {
    let deadline = Instant::now() + limit;
    let (entries, complete) = admin_entries(common, deadline)?;
    let found: Vec<PathBuf> = entries
        .into_iter()
        .filter(|entry| norm_listed(&entry.checkout) == checkout)
        .map(|entry| entry.admin)
        .collect();
    if found.is_empty() && !complete {
        return Err("could not read every worktree registration".into());
    }
    Ok(found)
}

/// Every admin directory under `<common>/worktrees` with a readable `gitdir`,
/// and whether every one could be read.
///
/// Bounded by `deadline`, checked per entry; running out of time stops the
/// enumeration and reports it incomplete. A stray plain file, or an admin
/// directory without any `gitdir`, holds no registration and is skipped as git
/// skips it. An entry that is neither a directory nor a plain file (checked
/// without following symlinks), or whose `gitdir` exists but cannot be read
/// as a regular file of at most [`MAX_METADATA_BYTES`] (a symlink to one is
/// followed, as git follows it) or is empty, makes the answer incomplete: its
/// registration is unknown. The checkout is derived like git does (see
/// [`checkout_of_gitdir`]). `Err` only when the directory itself cannot be
/// listed.
fn admin_entries(common: &Path, deadline: Instant) -> Result<(Vec<AdminEntry>, bool), String> {
    let admin_root = common.join("worktrees");
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(&admin_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((found, true)),
        Err(error) => return Err(format!("could not read {}: {error}", admin_root.display())),
    };
    let mut complete = true;
    for entry in entries {
        if Instant::now() >= deadline {
            return Ok((found, false));
        }
        let admin = entry
            .map_err(|error| format!("could not read {}: {error}", admin_root.display()))?
            .path();
        match admin.symlink_metadata() {
            Ok(meta) if meta.is_dir() => {}
            // A stray file holds no registration; git skips it too.
            Ok(meta) if meta.is_file() => continue,
            // A symlink, a FIFO, or an entry that vanished mid-read: unknown.
            _ => {
                complete = false;
                continue;
            }
        }
        let gitdir_file = admin.join("gitdir");
        match gitdir_file.symlink_metadata() {
            Ok(_) => {}
            // No `gitdir` at all: git cannot list it either.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                complete = false;
                continue;
            }
        }
        // A real `gitdir` file is one short line. One that cannot be read as a
        // regular file of at most 4 KiB is never followed, and its
        // registration is unknown.
        let Some(bytes) = read_bounded_following(&gitdir_file) else {
            complete = false;
            continue;
        };
        let gitdir = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        if gitdir.is_empty() {
            complete = false;
            continue;
        }
        // Relative `gitdir` files (newer git's `worktree.useRelativePaths`)
        // are relative to the admin directory; an absolute one replaces it.
        let checkout = checkout_of_gitdir(admin.join(path_from_bytes(gitdir)));
        found.push(AdminEntry { admin, checkout });
    }
    Ok((found, complete))
}

/// `git worktree list --porcelain -z`, parsed; `Ok(None)` when this git
/// rejects `-z`.
pub(crate) fn porcelain_z(
    repo: &Path,
    limit: Duration,
) -> Result<Option<Vec<Registration>>, String> {
    let (status, stdout, stderr) = run_raw(repo, ["worktree", "list", "--porcelain", "-z"], limit)?;
    if status.success() {
        return Ok(Some(parse_registrations(&stdout)));
    }
    let message = failure_message(status, &stderr);
    if unsupported_porcelain_z(status.code(), &message) {
        Ok(None)
    } else {
        Err(message)
    }
}

/// Whether a failed `-z` call means the installed git predates 2.36.
///
/// Only an `unknown switch`/`unknown option` error naming `z`, or exit status
/// 129 (git's usage exit) with `worktree` in the usage text, counts. A bare
/// `usage:` from anything else, a timeout or a repository error is not a
/// fallback signal.
fn unsupported_porcelain_z(code: Option<i32>, stderr: &str) -> bool {
    let names_z = stderr.lines().any(|line| {
        (line.contains("unknown switch") || line.contains("unknown option"))
            && (line.contains("`z'") || line.contains("'z'"))
    });
    names_z || (code == Some(129) && stderr.contains("worktree"))
}

/// [`metadata_registrations`] for `repo`, resolving its common dir first.
#[cfg(test)]
pub(crate) fn registrations_from_metadata(
    repo: &Path,
    limit: Duration,
) -> Result<RegistrationSnapshot, String> {
    let deadline = Instant::now() + limit;
    let common = common_dir(repo, limit)?;
    metadata_registrations(&common, deadline)
}

/// Every registration, read from git's metadata rather than parsed output.
///
/// The fallback for a git older than 2.36, and byte-exact on every version:
/// `<common>/worktrees/<id>/gitdir` holds the absolute path of the linked
/// worktree's `.git` file (only the final newline is stripped; the worktree is
/// that path with a trailing `.git` component stripped, as git derives it),
/// `<id>/HEAD` is `ref: refs/heads/<b>` or a detached object id, a
/// `<id>/locked` entry of any kind (checked without following symlinks)
/// marks a lock, and a checkout path that is not a real directory (checked
/// without following symlinks: a symlink there is not a live checkout) makes
/// the registration prunable. Whether the repository is bare is git's answer
/// (see [`is_bare`]), never the common dir's name. For a bare one the main
/// registration is `<common>` itself with an unknown `HEAD` and `bare` set, as
/// `-z` reports it (`bare`, no branch). For a non-bare one it is the parent of
/// a common dir named `.git`; otherwise (`--separate-git-dir`) it is
/// `core.worktree` when set, and when not, no main registration is made and the
/// snapshot is incomplete. A non-bare main registration's `HEAD` is
/// `<common>/HEAD`. Only regular files are read (see [`read_bounded`]); `gitdir` and
/// `HEAD` may be symlinks to a regular file, as git itself reads them. A
/// `HEAD` that cannot be read or parsed, or that names the all-zero object id,
/// is [`Head::Unknown`]. Admin entries are enumerated by [`admin_entries`],
/// bounded by `deadline`. An admin entry
/// whose `gitdir` exists but cannot be read that way (not a regular file,
/// oversized, unreadable, empty), or an admin entry that is neither a
/// directory nor a plain file, makes the snapshot incomplete: its
/// registration may be the one a caller is looking for. An admin entry
/// without any `gitdir`, or a plain file there, is skipped as git skips it.
/// Paths are kept byte-exact, valid UTF-8 or not, exactly as the `-z` parser
/// keeps them: containment, the exact-registration lookup and branch
/// occupancy must see every registration. Only what goes over the protocol is
/// filtered to UTF-8.
fn metadata_registrations(
    common: &Path,
    deadline: Instant,
) -> Result<RegistrationSnapshot, String> {
    let left = || deadline.saturating_duration_since(Instant::now());
    let mut registrations = Vec::new();
    let mut main_known = true;
    if is_bare(common, left())? {
        // A bare repository: `-z` lists the repository directory itself.
        registrations.push(Registration {
            path: common.to_path_buf(),
            head: Head::Unknown,
            prunable: false,
            locked: false,
            bare: true,
        });
    } else {
        let main = match common.parent() {
            Some(parent) if common.file_name().is_some_and(|name| name == ".git") => {
                Some(norm(parent))
            }
            // `--separate-git-dir`: the common dir says nothing about where
            // the main worktree is, unless `core.worktree` does.
            _ => core_worktree(common, left())?,
        };
        match main {
            Some(path) => registrations.push(Registration {
                path,
                head: read_head(&common.join("HEAD")),
                prunable: false,
                locked: false,
                bare: false,
            }),
            None => main_known = false,
        }
    }
    let (entries, complete) = admin_entries(common, deadline)?;
    let complete = complete && main_known;
    for AdminEntry { admin, checkout } in entries {
        // A symlink (or anything else that is not a real directory) where the
        // checkout was is not a live checkout: never follow it to decide.
        let prunable = !checkout.symlink_metadata().is_ok_and(|meta| meta.is_dir());
        registrations.push(Registration {
            path: norm_listed(&checkout),
            head: read_head(&admin.join("HEAD")),
            prunable,
            // Presence alone locks, so a dangling symlink named `locked` is a
            // lock too.
            locked: admin.join("locked").symlink_metadata().is_ok(),
            bare: false,
        });
    }
    Ok(RegistrationSnapshot {
        registrations,
        complete,
    })
}

/// Largest git metadata file (`gitdir`, `HEAD`, a `.git` file) ever read.
const MAX_METADATA_BYTES: u64 = 4 * 1024;

/// Reads at most [`MAX_METADATA_BYTES`] of a small git metadata file, never
/// following a symlink.
///
/// `None` when the file cannot be read, is larger than that, or is not a
/// regular file: such a file is never one git wrote, and reading it whole is
/// never needed to refuse it. See [`read_metadata`].
pub(crate) fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    read_metadata(path, false)
}

/// [`read_bounded`], following a symlink whose target is a regular file.
///
/// For `gitdir` and `HEAD`, which git itself reads through a symlink.
pub(crate) fn read_bounded_following(path: &Path) -> Option<Vec<u8>> {
    read_metadata(path, true)
}

/// Reads a small regular file without ever blocking in `open`.
///
/// The type is checked first (following symlinks only when `follow`), so a
/// FIFO, a device or a refused symlink is normally never opened. That check
/// and the open are separate steps, though, so on Unix the open itself also
/// cannot block or follow what it must not: it uses `O_NONBLOCK` (a FIFO swapped
/// in meanwhile opens at once instead of waiting for a writer) and, when not
/// `follow`, `O_NOFOLLOW`. The opened descriptor is then checked again and
/// anything that is not a regular file is refused before a byte is read.
fn read_metadata(path: &Path, follow: bool) -> Option<Vec<u8>> {
    use std::io::Read;
    let meta = if follow {
        path.metadata()
    } else {
        path.symlink_metadata()
    };
    if !meta.is_ok_and(|meta| meta.file_type().is_file()) {
        return None;
    }
    let file = open_nonblocking(path, follow).ok()?;
    if !file.metadata().is_ok_and(|meta| meta.file_type().is_file()) {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_METADATA_BYTES).then_some(bytes)
}

/// Opens `path` read-only with `O_NONBLOCK`, plus `O_NOFOLLOW` unless
/// `follow`.
#[cfg(unix)]
fn open_nonblocking(path: &Path, follow: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut flags = libc::O_NONBLOCK;
    if !follow {
        flags |= libc::O_NOFOLLOW;
    }
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
}

/// Opens `path` read-only; other platforms have no FIFOs to block on.
#[cfg(not(unix))]
fn open_nonblocking(path: &Path, _follow: bool) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Reads a `HEAD` file (a symlink to a regular file is followed, as git does).
/// Unreadable, oversized or unparseable is [`Head::Unknown`].
fn read_head(path: &Path) -> Head {
    let Some(text) = read_bounded_following(path).and_then(|bytes| String::from_utf8(bytes).ok())
    else {
        return Head::Unknown;
    };
    let text = text.trim();
    match text.strip_prefix("ref: ") {
        Some(reference) if !reference.is_empty() => Head::Branch(
            reference
                .strip_prefix("refs/heads/")
                .unwrap_or(reference)
                .to_owned(),
        ),
        Some(_) => Head::Unknown,
        None if is_object_id(text) && !is_null_object_id(text) => Head::Detached(text.to_owned()),
        None => Head::Unknown,
    }
}

/// Whether `text` is the all-zero object id, of any length.
///
/// Git prints it for a detached `HEAD` it could not resolve; it never names a
/// commit, so it is never "detached at a commit some ref reaches".
fn is_null_object_id(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte == b'0')
}

/// Whether `text` is a full SHA-1 or SHA-256 object id.
fn is_object_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Parses `git worktree list --porcelain -z` output.
///
/// Fields are NUL-separated and an empty field ends a record. The path is taken
/// as bytes, so a directory name holding a newline or invalid UTF-8 is still
/// one path rather than two lines, and is kept as is (never filtered); only
/// the non-Unix fallback decodes lossily.
/// A record with neither `branch` nor `detached` (and a detached one whose
/// `HEAD` is missing, not a full SHA-1 or SHA-256 hex object id, or the
/// all-zero id) has an unknown `HEAD`. A `bare`
/// field marks a bare repository's main registration.
pub fn parse_registrations(out: &[u8]) -> Vec<Registration> {
    /// One record while its fields are read.
    struct Partial {
        path: PathBuf,
        branch: Option<String>,
        sha: Option<String>,
        detached: bool,
        prunable: bool,
        locked: bool,
        bare: bool,
    }
    impl Partial {
        fn finish(self) -> Registration {
            let head = match (self.branch, self.sha, self.detached) {
                (Some(branch), _, _) => Head::Branch(branch),
                (None, Some(sha), true) if is_object_id(&sha) && !is_null_object_id(&sha) => {
                    Head::Detached(sha)
                }
                _ => Head::Unknown,
            };
            Registration {
                path: self.path,
                head,
                prunable: self.prunable,
                locked: self.locked,
                bare: self.bare,
            }
        }
    }

    let mut registrations = Vec::new();
    let mut current: Option<Partial> = None;
    for field in out.split(|byte| *byte == 0) {
        if field.is_empty() {
            if let Some(partial) = current.take() {
                registrations.push(partial.finish());
            }
            continue;
        }
        if let Some(value) = field.strip_prefix(b"worktree ") {
            // A field may only start a record, but flushing here also keeps a
            // malformed stream from merging two worktrees into one.
            if let Some(partial) = current.take() {
                registrations.push(partial.finish());
            }
            current = Some(Partial {
                path: norm_listed(&path_from_bytes(value)),
                branch: None,
                sha: None,
                detached: false,
                prunable: false,
                locked: false,
                bare: false,
            });
            continue;
        }
        let Some(partial) = current.as_mut() else {
            continue;
        };
        if let Some(value) = field.strip_prefix(b"branch ") {
            let value = String::from_utf8_lossy(value);
            partial.branch = Some(
                value
                    .strip_prefix("refs/heads/")
                    .unwrap_or(&value)
                    .to_owned(),
            );
        } else if let Some(value) = field.strip_prefix(b"HEAD ") {
            partial.sha = Some(String::from_utf8_lossy(value).trim().to_owned());
        } else if field == b"detached" {
            partial.detached = true;
        } else if field == b"bare" {
            partial.bare = true;
        } else if field.starts_with(b"prunable") {
            partial.prunable = true;
        } else if field == b"locked" || field.starts_with(b"locked ") {
            partial.locked = true;
        }
    }
    if let Some(partial) = current.take() {
        registrations.push(partial.finish());
    }
    registrations
}

/// Where a nested `.git` file points: its `gitdir:` target, resolved against
/// the file's own directory when relative. `None` when the file cannot be read,
/// is not a gitfile, or is larger than [`MAX_METADATA_BYTES`] (a real gitfile
/// is one short line; reading a huge one is never needed to refuse it).
pub fn gitfile_target(file: &Path) -> Option<PathBuf> {
    let bytes = read_bounded(file)?;
    let mut target = bytes.strip_prefix(b"gitdir: ")?;
    while let Some(rest) = target
        .strip_suffix(b"\n")
        .or_else(|| target.strip_suffix(b"\r"))
    {
        target = rest;
    }
    if target.is_empty() {
        return None;
    }
    Some(norm(&file.parent()?.join(path_from_bytes(target))))
}

/// The first file in the checkout at `worktree` that git hides from `status`.
///
/// `git ls-files -v` tags an assume-unchanged entry with a lowercase letter
/// and a skip-worktree entry with `S`; an edit to either never shows up as
/// uncommitted. An assume-unchanged entry always counts: git does not look at
/// the file at all, so even its deletion is hidden. A skip-worktree entry
/// counts only when its file is on disk, so a sparse checkout's absent files do
/// not. Runs with [`SAFETY_CONFIG`]; bounded by `limit`.
pub fn hidden_file(worktree: &Path, limit: Duration) -> Result<Option<PathBuf>, String> {
    let args = SAFETY_CONFIG.into_iter().chain(["ls-files", "-v", "-z"]);
    let out = run_bytes_with_timeout(worktree, args, limit)?;
    for record in out.split(|byte| *byte == 0) {
        let [tag, b' ', name @ ..] = record else {
            continue;
        };
        let file = worktree.join(path_from_bytes(name));
        if tag.is_ascii_lowercase() || (*tag == b'S' && file.symlink_metadata().is_ok()) {
            return Ok(Some(file));
        }
    }
    Ok(None)
}

/// Every gitlink (mode `160000`, a submodule) in the index of the checkout at
/// `worktree`, as paths relative to it. Bounded by `limit`.
pub fn gitlinks(worktree: &Path, limit: Duration) -> Result<HashSet<PathBuf>, String> {
    let out = run_bytes_with_timeout(worktree, ["ls-files", "-s", "-z"], limit)?;
    Ok(out
        .split(|byte| *byte == 0)
        .filter_map(|record| {
            // `<mode> <object> <stage>\t<path>`
            let tab = record.iter().position(|byte| *byte == b'\t')?;
            let (meta, name) = (&record[..tab], &record[tab + 1..]);
            meta.starts_with(b"160000 ").then(|| path_from_bytes(name))
        })
        .collect())
}

/// A path from the raw bytes git printed.
#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec()))
}

/// A path from the raw bytes git printed; other platforms decode lossily.
#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
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
///
/// `Err` when git fails or warns on stderr (a broken ref is skipped with a
/// warning, not reported as a failure): a partial answer must never make a
/// listed branch look deleted. Bounded by [`OBSERVATION_GIT_TIMEOUT`].
pub fn h4rn_branches(repo: &Path) -> Result<HashSet<String>, String> {
    let (out, stderr) = run_capture_with_timeout(
        repo,
        ["for-each-ref", "--format=%(refname)", "refs/heads/h4rn/"],
        OBSERVATION_GIT_TIMEOUT,
    )?;
    if !stderr.is_empty() {
        return Err(stderr);
    }
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.strip_prefix("refs/heads/"))
        .map(str::to_owned)
        .collect())
}

/// [`run_with_timeout`], also returning git's trimmed stderr on success.
///
/// Some commands (`for-each-ref` over a broken ref) succeed while warning
/// that they skipped something; callers that must not read a skip as absence
/// look at the stderr.
pub fn run_capture_with_timeout<I, S>(
    dir: &Path,
    args: I,
    limit: Duration,
) -> Result<(String, String), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (status, stdout, stderr) = run_raw(dir, args, limit)?;
    if !status.success() {
        return Err(failure_message(status, &stderr));
    }
    Ok((
        String::from_utf8_lossy(&stdout).trim().to_owned(),
        String::from_utf8_lossy(&stderr).trim().to_owned(),
    ))
}

/// `git status --porcelain` in `worktree`, as clearing's dirty check runs it.
///
/// Untracked files always count, whatever the user's `status.showUntrackedFiles`
/// config says, and submodules are inspected rather than skipped: a file the
/// user asked git to hide from `status` is still work that removing the
/// checkout would lose. Runs with [`SAFETY_CONFIG`]; bounded by `limit`.
pub fn status(worktree: &Path, limit: Duration) -> Result<String, String> {
    let args = SAFETY_CONFIG.into_iter().chain([
        "status",
        "--porcelain",
        "--untracked-files=all",
        "--ignore-submodules=none",
    ]);
    run_with_timeout(worktree, args, limit)
}

#[cfg(test)]
thread_local! {
    /// Test-only: a shorter observation deadline for [`is_dirty`] on this
    /// thread.
    pub static OBSERVATION_LIMIT: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// [`OBSERVATION_GIT_TIMEOUT`], or a test's shorter one.
fn observation_timeout() -> Duration {
    #[cfg(test)]
    if let Some(limit) = OBSERVATION_LIMIT.with(std::cell::Cell::get) {
        return limit;
    }
    OBSERVATION_GIT_TIMEOUT
}

/// Whether the worktree has uncommitted changes (see [`status`]); `None` when
/// git failed or outlived [`OBSERVATION_GIT_TIMEOUT`].
pub fn is_dirty(worktree: &Path) -> Option<bool> {
    status(worktree, observation_timeout())
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
