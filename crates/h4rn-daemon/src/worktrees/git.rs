//! Blocking git helpers for worktree tracking.
//!
//! Every function here shells out to `git` and blocks. Callers on the async
//! runtime must run them inside `tokio::task::spawn_blocking`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::io::Write;
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
///
/// It also carries where each linked registration's admin dir is (see
/// [`AdminDirs`]), read in the same pass, so a clear never walks
/// `<common>/worktrees` again to find one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistrationSnapshot {
    /// Every registration that could be read, main worktree included.
    pub registrations: Vec<Registration>,
    /// Whether every registration could be read.
    pub complete: bool,
    /// The admin dirs found while taking this snapshot.
    pub admins: AdminDirs,
}

/// Where the admin dirs (`<common>/worktrees/<id>`) of a snapshot's linked
/// registrations are, as the snapshot read them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdminDirs {
    /// Every admin dir whose `gitdir` could be read, by the checkout path it
    /// names (normalised as registration paths are). `complete` is false when
    /// some admin entry could not be read: a path missing from the map may be
    /// exactly that entry's.
    Read {
        /// Admin dirs by checkout path.
        by_checkout: HashMap<PathBuf, Vec<PathBuf>>,
        /// Whether every admin entry could be read.
        complete: bool,
    },
    /// The deadline passed while enumerating them; holds the
    /// `git timed out after Ns` message.
    TimedOut(String),
    /// `<common>/worktrees` could not be listed.
    Unreadable,
}

/// Why [`RegistrationSnapshot::admin_dirs`] has no answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdminLookup {
    /// Every admin entry was read and none names the checkout.
    NotFound,
    /// The enumeration ran out of time; holds the timeout message.
    TimedOut(String),
    /// The enumeration could not read everything, and nothing that was read
    /// names the checkout.
    Unreadable,
}

impl RegistrationSnapshot {
    /// The admin dirs whose `gitdir` names `checkout` (a normalised
    /// registration path), as this snapshot read them. Never empty when `Ok`.
    ///
    /// A timed-out enumeration never answers, even for a path it did read:
    /// a second admin dir naming the same checkout may be the one it missed.
    pub fn admin_dirs(&self, checkout: &Path) -> Result<&[PathBuf], AdminLookup> {
        match &self.admins {
            AdminDirs::Read {
                by_checkout,
                complete,
            } => match by_checkout.get(checkout) {
                Some(admins) if !admins.is_empty() => Ok(admins),
                _ if *complete => Err(AdminLookup::NotFound),
                _ => Err(AdminLookup::Unreadable),
            },
            AdminDirs::TimedOut(message) => Err(AdminLookup::TimedOut(message.clone())),
            AdminDirs::Unreadable => Err(AdminLookup::Unreadable),
        }
    }
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
    /// Test-only: treat every checkout as on a different filesystem from its
    /// admin dir on this thread.
    pub static FORCE_FOREIGN_FILESYSTEM: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    /// Test-only: probe every filesystem as lacking executable bits and
    /// symlinks on this thread.
    pub static FORCE_NO_MODE_SUPPORT: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    /// Test-only: the mode probe sees a filesystem that ignores chmod and
    /// reports `0777` (WSL drvfs, FAT) on this thread.
    pub static FORCE_SYNTHESIZED_MODE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    /// Test-only: the symlink probe fails with this errno on this thread.
    pub static FORCE_SYMLINK_ERRNO: std::cell::Cell<Option<i32>> =
        const { std::cell::Cell::new(None) };
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

/// Variables that point git at a repository, index or object store other than
/// the one each call names with `-C`/`--git-dir`.
///
/// A daemon started from a hook, a wrapper or `git rebase --exec` can inherit
/// them; an inherited `GIT_INDEX_FILE` would make the staged-work check read a
/// clean index while the registration's own index holds staged changes.
const LOCATION_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// Config overrides for every git call that decides whether work would be
/// lost (`status`, `ls-files -v`, `worktree remove`).
///
/// A filesystem monitor (`core.fsmonitor`) or the untracked cache lets git
/// skip looking at files it was told have not changed. A hook that reports
/// nothing, or a stale cache, would then hide an edit or a new file, so these
/// calls always look at the disk themselves. Reduced stat checks
/// (`core.trustctime=false`, `core.checkStat=minimal`) would likewise let a
/// same-size edit that kept its indexed mtime read as clean, so the full stat
/// comparison is forced. `core.ignoreCase=true` on a case-sensitive
/// filesystem would make an untracked `README` beside a tracked `readme`
/// invisible, so case is always significant here (on a case-insensitive
/// filesystem this can only over-report, which skips). `-c` reaches the
/// `git status` that `git worktree remove` runs in a child process too.
pub const SAFETY_CONFIG: [&str; 10] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
    "-c",
    "core.trustctime=true",
    "-c",
    "core.checkStat=default",
    "-c",
    "core.ignoreCase=false",
];

/// Strips inherited variables that would redirect git or add noise, and sets
/// the ones every tracker call relies on. Also used for the subagent worktree
/// commands, which must target the repository they name.
pub(crate) fn sanitize(command: &mut Command) {
    // Read-only inspection must never wait on the index lock or a prompt.
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        // A replacement ref (`refs/replace/*`) can make history look like it
        // contains a commit it does not: an unmerged branch would pass the
        // ancestry check and a detached commit would look reachable.
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        // Grafts (`<common>/info/grafts`, or an inherited `GIT_GRAFT_FILE`)
        // rewrite parent links the same way and are honoured regardless of
        // `GIT_NO_REPLACE_OBJECTS`; point git at an empty graft file.
        .env("GIT_GRAFT_FILE", EMPTY_FILE)
        // Git's messages are parsed (push classification, URL redaction, the
        // tracker's error triage) and localized catalogs change their quotes,
        // spacing and words, so every call speaks the C locale.
        .env("LC_ALL", "C")
        .env("LANG", "C");
    command.env_remove("LANGUAGE");
    for name in TRACE_VARS.iter().chain(LOCATION_VARS).chain(
        [
            "GIT_REPLACE_REF_BASE",
            // Inherited `-c` / `GIT_CONFIG_KEY_n` config, and redirected
            // global/system config files. The safety-relevant keys are forced
            // with `-c` and would win anyway, but a redirected global config
            // can set keys we do not force (`core.excludesFile` hiding every
            // untracked file); dropping these leaves git reading the user's
            // normal configuration, as for a daemon started normally.
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_NOSYSTEM",
        ]
        .iter(),
    ) {
        command.env_remove(name);
    }
}

/// A path that always reads as an empty file.
#[cfg(not(windows))]
const EMPTY_FILE: &str = "/dev/null";
#[cfg(windows)]
const EMPTY_FILE: &str = "NUL";

fn command(dir: &Path) -> Command {
    let mut command = Command::new(program());
    sanitize(&mut command);
    command.arg("-C").arg(dir);
    command
}

/// A `git` command that ignores inherited repository-location variables, for
/// callers outside this module (thread and subagent worktrees).
pub(crate) fn git_command() -> Command {
    let mut command = Command::new(program());
    sanitize(&mut command);
    command
}

/// Whether `path` is inside a git work tree.
pub(crate) fn is_git_repo(path: &Path) -> bool {
    git_command()
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
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

/// [`run_bytes_with_timeout`] with `GIT_INDEX_FILE` pointed at a scratch index.
///
/// Only the checkpoint capture needs this: it stages the whole worktree in a
/// copy of the real index, so the checkout's own index and lock are never
/// touched.
fn run_bytes_with_index(
    dir: &Path,
    index: &Path,
    args: &[&str],
    limit: Duration,
) -> Result<Vec<u8>, String> {
    run_bytes_with_index_input(dir, index, args, None, limit)
}

/// [`run_bytes_with_index`] with bytes handed to the child's stdin.
fn run_bytes_with_index_input(
    dir: &Path,
    index: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    limit: Duration,
) -> Result<Vec<u8>, String> {
    let mut command = command(dir);
    command.env("GIT_INDEX_FILE", index).args(args);
    let (status, stdout, stderr) = run_raw_command_with_input(&mut command, input, limit)?;
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
    let mut command = command(dir);
    command.args(args);
    run_raw_command(&mut command, limit)
}

/// [`run_raw`] for a caller that prepared the command itself (an extra
/// environment variable, say), keeping the same deadline and pipe draining.
fn run_raw_command(
    command: &mut Command,
    limit: Duration,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), String> {
    run_raw_command_with_input(command, None, limit)
}

/// [`run_raw_command`] with bytes handed to the child's stdin.
///
/// The write runs on its own thread: a child that fills its stdout pipe before
/// reading all of stdin would otherwise deadlock both sides.
fn run_raw_command_with_input(
    command: &mut Command,
    input: Option<&[u8]>,
    limit: Duration,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), String> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = spawn(command).map_err(|error| format!("could not run git: {error}"))?;
    let writer = match (input, child.stdin.take()) {
        (Some(bytes), Some(mut pipe)) => {
            let bytes = bytes.to_vec();
            Some(std::thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            }))
        }
        _ => None,
    };
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
    // The child is gone, so its stdin pipe is closed and the writer finishes.
    if let Some(writer) = writer {
        let _ = writer.join();
    }
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
    repo_of_with(workspace, &CommonDirs::default())
}

/// [`repo_of`], asking whether the common dir is bare through `commons` (so a
/// listing asks once per common dir).
pub(crate) fn repo_of_with(workspace: &Path, commons: &CommonDirs) -> Option<PathBuf> {
    let common = observed_path(workspace, "--git-common-dir")?;
    let common = norm(&workspace.join(common));
    if commons.is_bare(&common, OBSERVATION_GIT_TIMEOUT).ok()? {
        return Some(common);
    }
    if common.file_name()? == ".git" {
        common.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

/// The repository facts one containment decision may trust.
pub(crate) struct Layout {
    /// The common git directory.
    pub common: PathBuf,
    /// The real main working tree, never the metadata directory.
    pub main: PathBuf,
    /// Every registration, complete or not.
    pub snapshot: RegistrationSnapshot,
}

/// One complete, cross-checked view of the repository containing `workspace`.
///
/// Containment decisions use this rather than taking separate snapshots:
/// `complete` is false when an admin entry could not be read, so a missing
/// sibling can never be mistaken for an absent one.
///
/// The main checkout is never guessed. It is `core.worktree`, the workspace
/// itself when its `.git` file points at the common dir, a nearby directory
/// whose `.git` does, or a registration whose own `.git` proves it. A
/// registration alone proves nothing: `git worktree list` reports the metadata
/// directory as the main entry of a `--separate-git-dir` repository, and a
/// linked worktree is registered too. When none of those resolves, this is an
/// error rather than the first non-bare registration, because a checkout
/// placed against the wrong folder could land inside the real main checkout.
pub(crate) fn layout(workspace: &Path) -> Result<Layout, String> {
    let common = common_dir(workspace, OBSERVATION_GIT_TIMEOUT)?;
    let snapshot = registrations_in(
        workspace,
        &common,
        OBSERVATION_GIT_TIMEOUT,
        &CommonDirs::default(),
    )?;
    if !snapshot.complete {
        return Err("git could not read every worktree registration of this repository".to_owned());
    }
    let main = if let Some(repo) = repo_of(workspace).filter(|repo| {
        // `repo_of` answers a bare repository with its common dir itself,
        // which is git's fact rather than an inference. Any other answer is
        // the parent of a common dir named `.git`, which a metadata directory
        // can carry while the real checkout lives elsewhere, so it counts
        // only when that folder proves it holds the working tree.
        *repo == common
            || holds_working_tree(repo, workspace, &common, &snapshot, OBSERVATION_GIT_TIMEOUT)
    }) {
        repo
    } else if let Some(worktree) = core_worktree(&common, OBSERVATION_GIT_TIMEOUT)? {
        worktree
    } else if gitdir_points_at(workspace, &common) {
        // The main checkout of a `--separate-git-dir` repository (or a
        // submodule) is the directory whose `.git` file names the common dir.
        workspace.to_path_buf()
    } else if let Some(found) = find_checkout_pointing_at(&common) {
        found
    } else if let Some(registered) = snapshot
        .iter()
        .filter(|registration| !registration.bare && registration.path != common)
        .find(|registration| gitdir_points_at(&registration.path, &common))
        .map(|registration| registration.path.clone())
    {
        // A registration counts only when its own `.git` still points at this
        // common dir, which a linked worktree's never does.
        registered
    } else {
        return Err("the project's main checkout could not be found".to_owned());
    };
    Ok(Layout {
        common,
        main: norm(&main),
        snapshot,
    })
}

/// Whether `candidate` really holds the working tree of the repository whose
/// common dir is `common`.
///
/// [`repo_of`] infers a non-bare repository's main checkout as the parent of a
/// common dir named `.git`, and `git worktree list` reports a
/// `--separate-git-dir` repository's metadata directory the same way. A
/// metadata directory is not a working tree, so the inference counts only when
/// the workspace is itself the main checkout (a shortcut), or `core.worktree`
/// is unset and the candidate holds the working tree: its `.git` resolves to
/// the common dir, or a tracked path exists there as the candidate's own file.
/// A linked worktree nested inside its own main checkout passes on the tracked
/// path; a metadata parent with a linked worktree below it does not, because
/// the tracked name resolves into that worktree.
fn holds_working_tree(
    candidate: &Path,
    workspace: &Path,
    common: &Path,
    snapshot: &RegistrationSnapshot,
    limit: Duration,
) -> bool {
    // Being inside the candidate proves nothing by itself: a linked worktree
    // can sit inside the metadata directory's parent, which `repo_of` names
    // after the common dir's `.git`. The workspace being the main worktree
    // (its own git dir is the common dir; a linked worktree's is its admin
    // dir) is a shortcut; otherwise the candidate still has to prove it holds
    // the working tree below, which a linked worktree nested in its own main
    // checkout (`repo/.worktrees/one`) does and a metadata parent does not.
    if norm(workspace).starts_with(norm(candidate)) && workspace_is_main(workspace, common, limit) {
        return true;
    }
    match core_worktree(common, limit) {
        // Unset: the `.git` name is all the inference had, so the folder
        // itself has to prove it holds the checkout.
        Ok(None) => {}
        // Set (or unreadable): the repository names its own working tree, and
        // an inferred parent must never override it.
        Ok(Some(_)) | Err(_) => return false,
    }
    // Only a `.git` *file* proves a checkout whose git directory lives
    // elsewhere. A `.git` directory equal to the common dir proves nothing:
    // the parent of a metadata dir named `.git` has exactly that shape too.
    if gitdir_points_at(candidate, common) {
        return true;
    }
    tracked_path_exists(candidate, workspace, snapshot, common, limit)
}

/// Whether `workspace` is the repository's main worktree: its own git dir is
/// the common dir rather than an admin dir under `<common>/worktrees`.
fn workspace_is_main(workspace: &Path, common: &Path, limit: Duration) -> bool {
    match run_path_with_timeout(workspace, ["rev-parse", "--git-dir"], limit) {
        Ok(git_dir) => norm(&workspace.join(git_dir)) == norm(common),
        Err(_) => false,
    }
}

/// Whether a path tracked by the repository's `HEAD` exists in `candidate` as
/// the candidate's own file.
///
/// The last proof that a folder holds a working tree: a metadata directory
/// named `.git` holds none of the repository's files beside its git directory.
/// A tracked name can also resolve into a checkout that merely sits below the
/// candidate: with the metadata dir at `meta/.git` and a linked worktree at
/// `meta/docs`, the tracked `docs/README.md` is the worktree's own
/// `meta/docs/README.md`. Such a match says nothing about the candidate, so a
/// name counts only when it resolves inside the candidate and outside the
/// workspace and every registered worktree that lies strictly below the
/// candidate. The candidate itself, and a worktree that contains it, are not
/// excluded: its own tracked files still prove it, as for a linked worktree
/// nested inside its main checkout (`repo/.worktrees/one`) or a main checkout
/// moved inside its own linked worktree.
fn tracked_path_exists(
    candidate: &Path,
    workspace: &Path,
    snapshot: &RegistrationSnapshot,
    common: &Path,
    limit: Duration,
) -> bool {
    let Ok(listing) = run_bytes_with_timeout(
        common,
        ["--git-dir=.", "ls-tree", "-r", "-z", "--name-only", "HEAD"],
        limit,
    ) else {
        return false;
    };
    let candidate = norm(candidate);
    let workspace = norm(workspace);
    // Only a worktree strictly below the candidate can lend it a file. The
    // candidate itself and any worktree that contains it (a main checkout
    // moved inside its own linked worktree) are not excluded, or the
    // candidate's own tree could never count. Registration paths are already
    // normalised as listings are (see `norm_listed`).
    let below_candidate = |path: &Path| path != candidate && path.starts_with(&candidate);
    let excluded: Vec<&Path> = std::iter::once(workspace.as_path())
        .chain(
            snapshot
                .iter()
                .map(|registration| registration.path.as_path()),
        )
        .filter(|path| below_candidate(path))
        .collect();
    listing.split(|byte| *byte == 0).any(|name| {
        let Ok(name) = std::str::from_utf8(name) else {
            return false;
        };
        if name.is_empty() {
            return false;
        }
        let path = candidate.join(name);
        if !path.exists() {
            return false;
        }
        // Resolve the match before asking where it lives, so a symlink cannot
        // smuggle a link to another checkout in as the candidate's own file.
        let resolved = norm(&path);
        resolved.starts_with(&candidate)
            && !excluded.iter().any(|other| resolved.starts_with(other))
    })
}

/// Whether `checkout/.git` is a file whose target is `common`.
fn gitdir_points_at(checkout: &Path, common: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(checkout.join(".git")) else {
        return false;
    };
    let Some(target) = text.trim().strip_prefix("gitdir:") else {
        return false;
    };
    let target = Path::new(target.trim());
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        checkout.join(target)
    };
    norm(&target) == norm(common)
}

/// A nearby directory whose `.git` file points at `common`.
///
/// This covers the main checkout of a `--separate-git-dir` repository when the
/// caller started in a linked worktree and `core.worktree` is unset. Only
/// immediate children of the metadata directory's parent are considered, and
/// a candidate must still exist as a directory.
fn find_checkout_pointing_at(common: &Path) -> Option<PathBuf> {
    let parent = common.parent()?;
    for entry in std::fs::read_dir(parent).ok()?.flatten() {
        let path = entry.path();
        if path.symlink_metadata().is_ok_and(|meta| meta.is_dir())
            && gitdir_points_at(&path, common)
        {
            return Some(path);
        }
    }
    None
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
/// would cost one git spawn each. Whether each common dir is bare (see
/// [`is_bare`]) is kept next to it, for [`repo_of_with`] and the metadata
/// fallback. Only successful lookups are kept, so a timeout is retried by the
/// next caller.
#[derive(Debug, Default)]
pub struct CommonDirs(std::sync::Mutex<CommonCache>);

/// What [`CommonDirs`] has learned so far.
#[derive(Debug, Default)]
pub struct CommonCache {
    /// Common dir by repository.
    dirs: HashMap<PathBuf, PathBuf>,
    /// Bareness by common dir.
    bare: HashMap<PathBuf, bool>,
}

impl CommonDirs {
    /// The common dir of `repo`, asking git (bounded by `limit`) only the
    /// first time.
    pub fn get(&self, repo: &Path, limit: Duration) -> Result<PathBuf, String> {
        if let Some(common) = self.lock().dirs.get(repo) {
            return Ok(common.clone());
        }
        let common = common_dir(repo, limit)?;
        self.lock().dirs.insert(repo.to_path_buf(), common.clone());
        Ok(common)
    }

    /// Whether the repository whose common dir is `common` is bare (see
    /// [`is_bare`]), asking git (bounded by `limit`) only the first time.
    pub fn is_bare(&self, common: &Path, limit: Duration) -> Result<bool, String> {
        if let Some(bare) = self.lock().bare.get(common) {
            return Ok(*bare);
        }
        let bare = is_bare(common, limit)?;
        self.lock().bare.insert(common.to_path_buf(), bare);
        Ok(bare)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CommonCache> {
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

/// One complete pre-turn checkpoint of a worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// The tree of the whole worktree, as a git tree hash.
    pub tree: String,
    /// Repository-relative paths (or folders, with a trailing `/`) git ignored
    /// when the tree was taken, sorted.
    ///
    /// Ignored content is outside the tree by design, so its absence from the
    /// tree says nothing about whether it existed. Recording what was ignored
    /// keeps a restore from deleting it on the tree's word.
    pub ignored: Vec<String>,
}

/// Most ignored paths one checkpoint records.
///
/// Past this the listing is not complete, and a restore cannot tell an ignored
/// path from one the tree simply never saw, so the checkpoint is refused.
pub const MAX_CHECKPOINT_IGNORED: usize = 10_000;

/// The whole worktree at this instant, as a checkpoint.
///
/// The tree is built with plumbing in a fresh scratch index (see
/// [`worktree_tree`]), so it covers every tracked file and every untracked file
/// git does not ignore, keeps git's modes (the executable bit, a symlink's
/// target) and already records a rename as the old path restored plus the new
/// path removed. The real index and its lock are never touched. `Ok(None)` when
/// the repository has no commit yet or git fails: such a turn has no complete
/// checkpoint to rewind to.
///
/// No attribute source can convert the stored bytes: not `.gitattributes`, not
/// `.git/info/attributes`, and not the global `core.attributesFile`. A clean
/// filter and `core.autocrlf` never run either. `Err` names why a checkpoint
/// cannot be complete — more ignored paths than [`MAX_CHECKPOINT_IGNORED`] —
/// and the caller records that reason so a restore refuses with it.
///
/// Ignored files are outside the tree by design (they are build output, not
/// work), so the checkpoint records which ignored paths were present (see
/// [`Checkpoint::ignored`]). A restore never touches those paths.
pub fn checkpoint_commit(workspace: &Path) -> Result<Option<Checkpoint>, String> {
    if run_with_timeout(workspace, ["rev-parse", "HEAD"], OBSERVATION_GIT_TIMEOUT)
        .ok()
        .filter(|head| !head.is_empty())
        .is_none()
    {
        return Ok(None);
    }
    let Some(repo) = toplevel(workspace) else {
        return Ok(None);
    };
    let Ok(Some(tree)) = worktree_tree(&repo) else {
        return Ok(None);
    };
    let ignored = ignored_paths(workspace)
        .map_err(|error| format!("git could not list this checkout's ignored paths: {error}"))?;
    Ok(Some(Checkpoint { tree, ignored }))
}

/// The tree of the whole worktree at `repo` as it is now, written with
/// plumbing into a fresh scratch index.
///
/// Every path is hashed with `hash-object --no-filters` and entered with
/// `update-index --index-info`, so the stored bytes are exactly the bytes on
/// disk whatever the attributes say. The checkpoint and the restore plan both
/// use this one builder, so the plan compares two trees made the same way and
/// never reads the working tree through git's conversion rules.
///
/// Regular files are hashed in one `--stdin-paths` process; a path that
/// protocol cannot carry (one with a newline, or starting with a quote) and
/// every symlink target get their own. `Ok(None)` when the tree could not be
/// written; `Err` only when git could not even list the worktree.
fn worktree_tree(repo: &Path) -> Result<Option<String>, String> {
    let listed = run_bytes_with_timeout(
        repo,
        [
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--full-name",
        ],
        OBSERVATION_GIT_TIMEOUT,
    )?;
    let paths = nul_paths(&listed)?;
    // `core.filemode=false` makes git ignore the executable bit; the tree must
    // record what git would have recorded.
    let filemode = run_with_timeout(
        repo,
        ["config", "--bool", "core.filemode"],
        OBSERVATION_GIT_TIMEOUT,
    )
    .map_or(true, |value| value != "false");
    let indexed = indexed_gitlinks(repo)?;
    let unique: std::collections::BTreeSet<&str> = paths.iter().map(String::as_str).collect();
    let mut entries: Vec<(&str, u32, String)> = Vec::with_capacity(unique.len());
    let mut files: Vec<(&str, u32)> = Vec::new();
    for path in unique {
        // `ls-files` names an untracked nested repository with a trailing
        // slash; the tree entry is the repository itself.
        let tree_path = path.strip_suffix('/').unwrap_or(path);
        if tree_path.is_empty() {
            continue;
        }
        let absolute = repo.join(tree_path);
        // A path that disappeared while the tree was being built is left out:
        // `git add -A` would have recorded it as removed.
        let Ok(meta) = absolute.symlink_metadata() else {
            continue;
        };
        if meta.file_type().is_symlink() {
            let Ok(target) = std::fs::read_link(&absolute) else {
                continue;
            };
            let Ok(object) = hash_bytes(repo, &path_to_bytes(&target)) else {
                continue;
            };
            entries.push((tree_path, MODE_SYMLINK, object));
        } else if meta.is_file() {
            let mode = if filemode && is_executable(&meta) {
                MODE_EXECUTABLE
            } else {
                0o100644
            };
            files.push((tree_path, mode));
        } else if meta.is_dir() {
            if let Some(object) = gitlink_object(&absolute, tree_path, &indexed) {
                entries.push((tree_path, MODE_GITLINK, object));
            }
        }
    }
    for ((path, mode), object) in files.iter().copied().zip(hash_files(repo, &files)) {
        if let Some(object) = object {
            entries.push((path, mode, object));
        }
    }
    let mut index_info = Vec::new();
    for (path, mode, object) in &entries {
        index_info.extend_from_slice(format!("{mode:o} {object}\t").as_bytes());
        index_info.extend_from_slice(path.as_bytes());
        index_info.push(0);
    }
    let Ok(scratch) = tempfile::Builder::new()
        .prefix("h4rn-checkpoint-")
        .tempdir()
    else {
        return Ok(None);
    };
    let staged = scratch.path().join("index");
    if run_bytes_with_index_input(
        repo,
        &staged,
        &["update-index", "-z", "--index-info"],
        Some(&index_info),
        OBSERVATION_GIT_TIMEOUT,
    )
    .is_err()
    {
        return Ok(None);
    }
    let Ok(tree) = run_bytes_with_index(repo, &staged, &["write-tree"], OBSERVATION_GIT_TIMEOUT)
    else {
        return Ok(None);
    };
    let Ok(tree) = String::from_utf8(tree) else {
        return Ok(None);
    };
    let tree = tree.trim().to_owned();
    Ok((!tree.is_empty()).then_some(tree))
}

/// The gitlink a folder in the worktree stands for, if any.
///
/// A folder is its own repository only when `rev-parse --show-toplevel` run
/// inside it names the folder itself; an uninitialised submodule is an empty
/// folder, and git run there walks up to the parent repository. Then the real
/// index's gitlink is the pointer, as `git add -A` would keep it. A plain
/// folder with neither is not a tree entry.
fn gitlink_object(
    absolute: &Path,
    tree_path: &str,
    indexed: &HashMap<String, String>,
) -> Option<String> {
    let own_repository = toplevel(absolute).is_some_and(|top| top == norm(absolute));
    if own_repository {
        if let Ok(head) = run_with_timeout(absolute, ["rev-parse", "HEAD"], OBSERVATION_GIT_TIMEOUT)
        {
            if is_object_id(&head) {
                return Some(head);
            }
        }
    }
    indexed.get(tree_path).cloned()
}

/// One blob id per file, in order, each hashed with no conversion or filter.
///
/// One `hash-object --stdin-paths` process hashes every path it can read by
/// line. A path with a newline, or starting with a quote (which the line
/// protocol would unquote), is hashed by itself. If the batch fails — a file
/// disappeared halfway, say — every path falls back to its own process, and a
/// path that still fails is left out, as one that vanished would be.
fn hash_files(repo: &Path, files: &[(&str, u32)]) -> Vec<Option<String>> {
    let by_line = |path: &str| !path.contains('\n') && !path.starts_with('"');
    let mut input = Vec::new();
    for (path, _) in files {
        if by_line(path) {
            input.extend_from_slice(path.as_bytes());
            input.push(b'\n');
        }
    }
    let batch = if input.is_empty() {
        Some(Vec::new())
    } else {
        let mut command = command(repo);
        command.args(["hash-object", "-w", "--no-filters", "--stdin-paths"]);
        run_raw_command_with_input(&mut command, Some(&input), OBSERVATION_GIT_TIMEOUT)
            .ok()
            .filter(|(status, _, _)| status.success())
            .and_then(|(_, stdout, _)| String::from_utf8(stdout).ok())
            .map(|stdout| stdout.lines().map(str::to_owned).collect::<Vec<_>>())
    };
    let expected = files.iter().filter(|(path, _)| by_line(path)).count();
    let mut batch = batch
        .filter(|objects| objects.len() == expected && objects.iter().all(|id| is_object_id(id)))
        .map(Vec::into_iter);
    files
        .iter()
        .map(|(path, _)| match batch.as_mut() {
            Some(objects) if by_line(path) => objects.next(),
            _ => hash_one_file(repo, path),
        })
        .collect()
}

/// One file's blob id, hashed by its own `hash-object` process.
fn hash_one_file(repo: &Path, path: &str) -> Option<String> {
    run_with_timeout(
        repo,
        ["hash-object", "-w", "--no-filters", "--", path],
        OBSERVATION_GIT_TIMEOUT,
    )
    .ok()
    .filter(|object| is_object_id(object))
}

/// A blob written from `bytes` with no conversion or filter.
fn hash_bytes(repo: &Path, bytes: &[u8]) -> Result<String, String> {
    let mut command = command(repo);
    command.args(["hash-object", "-w", "--no-filters", "--stdin"]);
    let (status, stdout, stderr) =
        run_raw_command_with_input(&mut command, Some(bytes), OBSERVATION_GIT_TIMEOUT)?;
    if !status.success() {
        return Err(failure_message(status, &stderr));
    }
    String::from_utf8(stdout)
        .map(|object| object.trim().to_owned())
        .map_err(|_| "git printed an object id that is not valid UTF-8".to_owned())
}

/// A path's bytes, as git stores a symlink's target.
fn path_to_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().into_owned().into_bytes()
    }
}

/// Repository-relative paths git ignores in this worktree, sorted.
///
/// `--directory` collapses an ignored folder to one entry ending in `/`, so a
/// build directory with thousands of objects costs one line. Names only: the
/// checkpoint must not depend on reading the ignored content. Past
/// [`MAX_CHECKPOINT_IGNORED`] the listing is refused: a partial one cannot tell
/// a missing ignored path from one that was never listed.
fn ignored_paths(workspace: &Path) -> Result<Vec<String>, String> {
    let raw = run_bytes_with_timeout(
        workspace,
        [
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "--full-name",
        ],
        OBSERVATION_GIT_TIMEOUT,
    )?;
    let mut paths = nul_paths(&raw)?;
    if paths.len() > MAX_CHECKPOINT_IGNORED {
        return Err(format!(
            "this checkout has {} ignored paths, more than the {MAX_CHECKPOINT_IGNORED} a checkpoint records, so this message's files cannot be restored",
            paths.len()
        ));
    }
    paths.sort();
    Ok(paths)
}

/// How one path stood at a checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointValue {
    /// A regular file with these bytes and this executable bit.
    File {
        /// The bytes the file held.
        bytes: Vec<u8>,
        /// Whether git recorded the file as executable.
        executable: bool,
    },
    /// A symbolic link to this target.
    Symlink {
        /// The link's target, exactly as git stored it.
        target: PathBuf,
    },
}

/// One path the restore must put back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointChange {
    /// Path relative to the repository root, as git prints it.
    pub path: String,
    /// The checkpoint's state; `None` removes the path, which did not exist
    /// then.
    pub value: Option<CheckpointValue>,
}

/// Git's file-mode bits that a restore has to carry.
const MODE_EXECUTABLE: u32 = 0o100755;
const MODE_SYMLINK: u32 = 0o120000;
const MODE_GITLINK: u32 = 0o160000;

/// One entry of the checkpoint tree.
struct TreeEntry {
    object: String,
    mode: u32,
}

/// Every path where the worktree differs from `checkpoint`, with the
/// checkpoint's own state.
///
/// The plan is relative to the repository root, as git prints paths. A path
/// present now but absent from the tree is a deletion; a blob the object store
/// cannot hand back is an error, never a deletion. Paths are compared by
/// content, so a file that was changed and then changed back is not in the
/// plan at all.
pub fn checkpoint_changes(
    workspace: &Path,
    checkpoint: &str,
) -> Result<Vec<CheckpointChange>, String> {
    let repo = toplevel(workspace)
        .ok_or_else(|| format!("{} is not inside a git worktree", workspace.display()))?;
    // The worktree as it is now, built by the same attribute-free plumbing as
    // the checkpoint. `git diff <checkpoint>` would read the working tree
    // through `.git/info/attributes` and the global `core.attributesFile`, so
    // a turn that only changed a file's line endings could look unchanged.
    // Two trees built the same way are compared by their raw bytes.
    let now = worktree_tree(&repo)?
        .ok_or_else(|| "git could not record the worktree as it is now".to_owned())?;
    // Rename detection would report a staged rename as only its new path, and
    // restoring that path would delete the file without putting the old one
    // back. Asking for every path on both sides undoes a rename as the old path
    // restored plus the new path removed.
    let changed = run_bytes_with_timeout(
        &repo,
        [
            "diff-tree",
            "-r",
            "--no-renames",
            "-z",
            checkpoint,
            &now,
            "--",
        ],
        OBSERVATION_GIT_TIMEOUT,
    )?;
    // An untracked nested repository is a gitlink in both trees that the real
    // index does not have. It is not the person's work and cannot be put back,
    // so it stays out of the plan: a gitlink the real index does name still
    // refuses, because its pointer really did change.
    let indexed = indexed_gitlinks(&repo)?;
    let mut plan: BTreeMap<String, Option<CheckpointValue>> = BTreeMap::new();
    for change in raw_tree_changes(&changed)? {
        let untracked_gitlink =
            |mode: u32| mode == MODE_GITLINK && !indexed.contains_key(&change.path);
        if untracked_gitlink(change.before.mode) || untracked_gitlink(change.after_mode) {
            continue;
        }
        let value = if change.before.mode == 0 {
            None
        } else {
            Some(checkpoint_value(&repo, &change.path, &change.before)?)
        };
        plan.insert(change.path, value);
    }
    Ok(plan
        .into_iter()
        .map(|(path, value)| CheckpointChange { path, value })
        .collect())
}

/// One path `diff-tree` reported between the checkpoint and the worktree now.
struct TreeChange {
    path: String,
    /// The checkpoint's entry; mode 0 when the path was absent then.
    before: TreeEntry,
    /// The mode now; 0 when the path is gone.
    after_mode: u32,
}

/// Parses `diff-tree -r -z` raw output: `:<old mode> <new mode> <old id> <new
/// id> <status>` then the path, each ending in NUL.
fn raw_tree_changes(raw: &[u8]) -> Result<Vec<TreeChange>, String> {
    let mut changes = Vec::new();
    let mut fields = raw.split(|byte| *byte == 0);
    while let Some(header) = fields.next() {
        if header.is_empty() {
            continue;
        }
        let header = std::str::from_utf8(header)
            .map_err(|_| "git diff-tree printed a header that is not valid UTF-8".to_owned())?;
        let mut parts = header.trim_start_matches(':').split(' ');
        let mut mode = || {
            parts
                .next()
                .and_then(|mode| u32::from_str_radix(mode, 8).ok())
                .ok_or_else(|| format!("git diff-tree printed an unreadable record: {header}"))
        };
        let before_mode = mode()?;
        let after_mode = mode()?;
        let before_object = parts
            .next()
            .ok_or_else(|| format!("git diff-tree printed an unreadable record: {header}"))?;
        let path = fields
            .next()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "git diff-tree printed a record without a path".to_owned())?;
        let path = std::str::from_utf8(path).map_err(|_| {
            "a changed path is not valid UTF-8, so it cannot be restored".to_owned()
        })?;
        changes.push(TreeChange {
            path: path.to_owned(),
            before: TreeEntry {
                object: before_object.to_owned(),
                mode: before_mode,
            },
            after_mode,
        });
    }
    Ok(changes)
}

/// The gitlinks the checkout's real index records, by repository-relative
/// path, with the commit each points at.
///
/// Only these are submodules the person really has. A nested repository the
/// turn happens to contain is a gitlink in the trees only, and the restore
/// leaves it alone instead of refusing. The pointer is also what a checkpoint
/// records for an uninitialised submodule, whose empty folder has no HEAD.
fn indexed_gitlinks(repo: &Path) -> Result<HashMap<String, String>, String> {
    let raw = run_bytes_with_timeout(
        repo,
        ["ls-files", "-s", "-z", "--full-name"],
        OBSERVATION_GIT_TIMEOUT,
    )?;
    let mut links = HashMap::new();
    for record in raw.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            return Err("git ls-files printed a record without a path".to_owned());
        };
        let header = std::str::from_utf8(&record[..tab])
            .map_err(|_| "git ls-files printed a mode that is not valid UTF-8".to_owned())?;
        let mut fields = header.split(' ');
        if fields.next() != Some("160000") {
            continue;
        }
        let object = fields
            .next()
            .ok_or_else(|| "git ls-files printed a gitlink without an object id".to_owned())?;
        let path = std::str::from_utf8(&record[tab + 1..])
            .map_err(|_| "an indexed path is not valid UTF-8".to_owned())?
            .to_owned();
        links.insert(path, object.to_owned());
    }
    Ok(links)
}

/// The checkpoint's state for one tree entry.
///
/// Every blob is read raw. The checkpoint was taken without conversion or
/// filters, so the stored object is exactly what was on disk: a CRLF file's
/// CRLF and a filtered file's working-tree bytes. Reading it back raw and
/// writing it raw gives the same bytes, whatever the attributes say now.
fn checkpoint_value(
    workspace: &Path,
    path: &str,
    entry: &TreeEntry,
) -> Result<CheckpointValue, String> {
    if entry.mode == MODE_GITLINK {
        return Err(format!(
            "{path} is a submodule, and a submodule pointer cannot be put back"
        ));
    }
    let bytes = run_bytes_with_timeout(
        workspace,
        ["cat-file", "blob", &entry.object],
        OBSERVATION_GIT_TIMEOUT,
    )
    .map_err(|error| format!("could not read {path} from the checkpoint: {error}"))?;
    if entry.mode == MODE_SYMLINK {
        return Ok(CheckpointValue::Symlink {
            target: bytes_to_path(bytes)?,
        });
    }
    Ok(CheckpointValue::File {
        bytes,
        executable: entry.mode == MODE_EXECUTABLE,
    })
}

/// Whether git records the file on disk as executable: the owner's execute
/// bit, as git itself tests it, never a group or other bit.
#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o100 != 0
}

/// Whether the file on disk is executable.
#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

/// The path bytes git stored for a symlink, as a path this platform can make.
fn bytes_to_path(bytes: Vec<u8>) -> Result<PathBuf, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes)
            .map(PathBuf::from)
            .map_err(|_| "a symlink target in the checkpoint is not valid UTF-8".to_owned())
    }
}

/// NUL-separated git paths, refusing a path the protocol cannot carry.
fn nul_paths(raw: &[u8]) -> Result<Vec<String>, String> {
    let mut paths = Vec::new();
    for bytes in raw.split(|byte| *byte == 0) {
        if bytes.is_empty() {
            continue;
        }
        let path = std::str::from_utf8(bytes).map_err(|_| {
            "a changed path is not valid UTF-8, so it cannot be restored".to_owned()
        })?;
        paths.push(path.to_owned());
    }
    Ok(paths)
}

/// The exact registration of `workspace` inside its repository, when it is a
/// linked worktree.
///
/// Both paths are kept: a moved worktree that left a different checkout at the
/// old path has a different git dir even when its common dir is unchanged.
pub fn linked_identity(workspace: &Path) -> Option<crate::journal::WorktreeIdentity> {
    let (dir, common) = (
        observed_path(workspace, "--git-dir")?,
        observed_path(workspace, "--git-common-dir")?,
    );
    let git_dir = norm(&workspace.join(dir));
    let common_dir = norm(&workspace.join(common));
    (git_dir != common_dir).then(|| crate::journal::WorktreeIdentity {
        git_dir: git_dir.display().to_string(),
        common_dir: common_dir.display().to_string(),
    })
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
    Ok(
        registrations_in(repo, &common, OBSERVATION_GIT_TIMEOUT, commons)?
            .registrations
            .into_iter()
            .filter(|registration| registration.path != main)
            .filter(|registration| registration.path.to_str().is_some())
            .map(|registration| GitWorktree {
                branch: registration.branch().map(str::to_owned),
                path: registration.path,
            })
            .collect(),
    )
}

/// Every worktree registration of `repo`, main checkout included, resolving
/// its common dir first (see [`registrations_in`]).
pub fn registrations(repo: &Path) -> Result<RegistrationSnapshot, String> {
    let common = common_dir(repo, OBSERVATION_GIT_TIMEOUT)?;
    registrations_in(
        repo,
        &common,
        OBSERVATION_GIT_TIMEOUT,
        &CommonDirs::default(),
    )
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
/// dir (and its bareness) once (see [`CommonDirs`]).
///
/// `-z` (git 2.36+) is used when available. An older git rejects the switch,
/// and then git's own registration metadata is read instead (see
/// [`metadata_registrations`]); which form works is cached for the rest
/// of the process. Either way the admin entries are enumerated once, and the
/// snapshot carries the checkout→admin dir mapping built from them (see
/// [`AdminDirs`]). Bounded by `limit` overall.
pub fn registrations_in(
    repo: &Path,
    common: &Path,
    limit: Duration,
    commons: &CommonDirs,
) -> Result<RegistrationSnapshot, String> {
    let deadline = Instant::now() + limit;
    if forced_metadata_fallback() || PORCELAIN_Z.load(Ordering::Relaxed) == PORCELAIN_Z_UNSUPPORTED
    {
        return metadata_registrations(common, deadline, limit, commons);
    }
    match porcelain_z(repo, limit)? {
        // Git exits 0 even when it skipped an admin entry it could not read,
        // so the answer is complete only if it represents every admin entry.
        Some(registrations) => {
            PORCELAIN_Z.store(PORCELAIN_Z_WORKS, Ordering::Relaxed);
            let scan = admin_entries(common, deadline);
            let complete =
                represents_every_admin_entry(scan.as_ref().ok(), &registrations, deadline);
            Ok(RegistrationSnapshot {
                registrations,
                complete,
                admins: admin_dirs(scan, limit),
            })
        }
        None => {
            PORCELAIN_Z.store(PORCELAIN_Z_UNSUPPORTED, Ordering::Relaxed);
            metadata_registrations(common, deadline, limit, commons)
        }
    }
}

/// The checkout→admin dir mapping from one enumeration of the admin entries.
fn admin_dirs(scan: Result<AdminScan, String>, limit: Duration) -> AdminDirs {
    let Ok(scan) = scan else {
        return AdminDirs::Unreadable;
    };
    if scan.timed_out {
        return AdminDirs::TimedOut(timeout_message(limit));
    }
    let mut by_checkout: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for entry in scan.entries {
        by_checkout
            .entry(entry.listed)
            .or_default()
            .push(entry.admin);
    }
    AdminDirs::Read {
        by_checkout,
        complete: scan.complete,
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
/// `scan` is `None` when the admin directory could not be listed.
fn represents_every_admin_entry(
    scan: Option<&AdminScan>,
    registrations: &[Registration],
    deadline: Instant,
) -> bool {
    let Some(scan) = scan else {
        return false;
    };
    if !scan.complete {
        return false;
    }
    let listed: HashSet<&Path> = registrations
        .iter()
        .map(|registration| registration.path.as_path())
        .collect();
    for entry in &scan.entries {
        if Instant::now() >= deadline {
            return false;
        }
        if !listed.contains(entry.listed.as_path()) {
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
    /// `checkout` normalised as registration paths are (see [`norm_listed`]).
    listed: PathBuf,
}

/// One enumeration of `<common>/worktrees`.
struct AdminScan {
    /// Every admin entry whose `gitdir` could be read.
    entries: Vec<AdminEntry>,
    /// Whether every admin entry could be read (false after a timeout too).
    complete: bool,
    /// Whether the deadline cut the enumeration short.
    timed_out: bool,
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

/// Every admin directory under `<common>/worktrees` with a readable `gitdir`,
/// and whether every one could be read.
///
/// Bounded by `deadline`, checked per entry (before its checkout path is
/// normalised); running out of time stops the enumeration and reports it
/// incomplete and timed out. A stray plain file, or an admin
/// directory without any `gitdir`, holds no registration and is skipped as git
/// skips it. An entry that is neither a directory nor a plain file (checked
/// without following symlinks), or whose `gitdir` exists but cannot be read
/// as a regular file of at most [`MAX_METADATA_BYTES`] (a symlink to one is
/// followed, as git follows it) or is empty, makes the answer incomplete: its
/// registration is unknown. The checkout is derived like git does (see
/// [`checkout_of_gitdir`]). `Err` only when the directory itself cannot be
/// listed.
fn admin_entries(common: &Path, deadline: Instant) -> Result<AdminScan, String> {
    let admin_root = common.join("worktrees");
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(&admin_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AdminScan {
                entries: found,
                complete: true,
                timed_out: false,
            });
        }
        Err(error) => return Err(format!("could not read {}: {error}", admin_root.display())),
    };
    let mut complete = true;
    for entry in entries {
        if Instant::now() >= deadline {
            return Ok(AdminScan {
                entries: found,
                complete: false,
                timed_out: true,
            });
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
        let listed = norm_listed(&checkout);
        found.push(AdminEntry {
            admin,
            checkout,
            listed,
        });
    }
    Ok(AdminScan {
        entries: found,
        complete,
        timed_out: false,
    })
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
    metadata_registrations(&common, deadline, limit, &CommonDirs::default())
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
/// (see [`is_bare`], asked once per common dir through `commons`), never the
/// common dir's name. For a bare one the main
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
    limit: Duration,
    commons: &CommonDirs,
) -> Result<RegistrationSnapshot, String> {
    let left = || deadline.saturating_duration_since(Instant::now());
    let mut registrations = Vec::new();
    let mut main_known = true;
    if commons.is_bare(common, left())? {
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
    let scan = admin_entries(common, deadline)?;
    let complete = scan.complete && main_known;
    for entry in &scan.entries {
        let AdminEntry {
            admin,
            checkout,
            listed,
        } = entry;
        // A symlink (or anything else that is not a real directory) where the
        // checkout was is not a live checkout: never follow it to decide.
        let prunable = !checkout.symlink_metadata().is_ok_and(|meta| meta.is_dir());
        registrations.push(Registration {
            path: listed.clone(),
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
        admins: admin_dirs(Ok(scan), limit),
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
pub(crate) fn read_head(path: &Path) -> Head {
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

/// Whether the index in the linked registration's admin dir `admin` matches
/// `treeish` (the registration's detached commit, or `refs/heads/<branch>`).
///
/// For a registration whose checkout directory is gone: git skips its own
/// cleanliness check then, and removing the registration deletes the admin dir
/// and the index with it, so anything staged there would be lost. Runs
/// `git -C <admin> --git-dir=. diff-index --cached --quiet
/// --ignore-submodules=none <treeish> --`, bounded by `limit` (trace and
/// location variables stripped like every call here); a staged submodule update
/// counts even when the submodule is configured `ignore = all`. `Ok(true)` when
/// git exits 0, or when there is no `index` file at all (nothing is staged);
/// `Ok(false)` when it exits 1. Anything else (an index that is not a regular
/// file or cannot be checked, a tree-ish that does not resolve, such as an
/// unborn branch, any other failure) is `Err`, never "clean"; a timeout is the
/// usual `git timed out after Ns` error.
pub fn index_matches(admin: &Path, treeish: &str, limit: Duration) -> Result<bool, String> {
    match admin.join("index").symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err("the index is not a regular file".into()),
        Err(error) => return Err(format!("could not check the index: {error}")),
    }
    let (status, _, stderr) = run_raw(
        admin,
        [
            "--git-dir=.",
            "diff-index",
            "--cached",
            "--quiet",
            // A staged submodule update counts even when the submodule is
            // configured `ignore = all`.
            "--ignore-submodules=none",
            treeish,
            "--",
        ],
        limit,
    )?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(failure_message(status, &stderr)),
    }
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

/// Runs git with a deadline and reports whether it succeeded, with its stdout
/// and stderr either way.
///
/// A porcelain push prints the fate of every ref on stdout and still exits
/// non-zero when one is rejected, so a caller that must read that record
/// cannot use the helpers that throw stdout away on failure.
pub fn run_combined_with_timeout<I, S>(
    dir: &Path,
    args: I,
    limit: Duration,
) -> Result<(bool, String, String), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (status, stdout, stderr) = run_raw(dir, args, limit)?;
    Ok((
        status.success(),
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
    let args = SAFETY_CONFIG
        .into_iter()
        .chain(mode_config(worktree).iter().copied())
        .chain([
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ]);
    run_with_timeout(worktree, args, limit)
}

/// Forces git to compare executable bits and symlinks.
#[cfg(unix)]
const FULL_MODES: [&str; 4] = ["-c", "core.fileMode=true", "-c", "core.symlinks=true"];
#[cfg(unix)]
const FILE_MODE_ONLY: [&str; 2] = ["-c", "core.fileMode=true"];
const SYMLINKS_ONLY: [&str; 2] = ["-c", "core.symlinks=true"];

/// Extra config for the checks on `checkout` (the dirty check and git's
/// removal backstop).
///
/// `core.fileMode` and `core.symlinks` are probed by git once, for the
/// filesystem the repository was created on, shared by every linked worktree,
/// and can also be set by hand. A checkout whose filesystem supports executable
/// bits or symlinks while the setting says it does not would hide a `chmod +x`
/// or a symlink replaced by a plain file. So each comparison is forced wherever
/// the checkout's filesystem actually supports it: always for a checkout on a
/// different filesystem from its admin dir, and otherwise as a fresh probe in
/// the admin dir (same filesystem) finds. A probe that cannot run forces both,
/// which can only over-report and skip. A filesystem that genuinely lacks the
/// feature keeps git's `false`, so ordinary clears still work there.
#[cfg(unix)]
pub fn mode_config(checkout: &Path) -> &'static [&'static str] {
    let admin = gitfile_target(&checkout.join(".git")).unwrap_or_else(|| checkout.join(".git"));
    if foreign_filesystem(checkout, &admin) {
        return &FULL_MODES;
    }
    match mode_support(&admin) {
        (true, true) => &FULL_MODES,
        (true, false) => &FILE_MODE_ONLY,
        (false, true) => &SYMLINKS_ONLY,
        (false, false) => &[],
    }
}

/// Non-Unix: there is no stable volume id to compare, so the symlink comparison
/// is always forced (it can only over-report, for repositories that track
/// symlinks). `core.fileMode` keeps git's probe: Windows filesystems have no
/// executable bits, and forcing it would mark every file changed.
#[cfg(not(unix))]
pub fn mode_config(_checkout: &Path) -> &'static [&'static str] {
    &SYMLINKS_ONLY
}

#[cfg(unix)]
fn foreign_filesystem(checkout: &Path, admin: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    #[cfg(test)]
    if FORCE_FOREIGN_FILESYSTEM.with(std::cell::Cell::get) {
        return true;
    }
    match (checkout.metadata(), admin.metadata()) {
        (Ok(checkout), Ok(admin)) => checkout.dev() != admin.dev(),
        _ => true,
    }
}

/// Whether the filesystem holding `admin` keeps executable bits and symlinks,
/// probed like `git init` does (a scratch file and a scratch symlink in the git
/// directory, never in the working tree) and cached per admin dir. A probe that
/// cannot run answers "supported", so the comparison is forced.
#[cfg(unix)]
fn mode_support(admin: &Path) -> (bool, bool) {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    #[cfg(test)]
    if FORCE_NO_MODE_SUPPORT.with(std::cell::Cell::get) {
        return (false, false);
    }
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (bool, bool)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(found) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(admin)
    {
        return *found;
    }
    let (exec, symlinks) = probe_mode_support(admin);
    // Only a probe that established both answers is cached. An inconclusive
    // one (a read-only admin dir, a full disk) is answered as "supported" so
    // the comparison is forced, and is asked again next time: a transient
    // failure must never become a lasting "unsupported".
    let found = (exec.unwrap_or(true), symlinks.unwrap_or(true));
    if exec.is_some() && symlinks.is_some() {
        cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(admin.to_path_buf(), found);
    }
    found
}

/// Probes executable bits and symlinks in `admin`. `None` means the probe
/// could not establish the answer.
///
/// Like `git init`, the executable bit must actually toggle: a filesystem that
/// ignores `chmod` and reports a synthesised `0777` (WSL drvfs without
/// `metadata`, FAT) lacks executable bits even though every file looks
/// executable. A symlink failure means "unsupported" only for the errors a
/// filesystem without symlinks returns, after a plain file could be written in
/// the same place; anything else is inconclusive.
#[cfg(unix)]
fn probe_mode_support(admin: &Path) -> (Option<bool>, Option<bool>) {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stamp = format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let file = admin.join(format!("h4rn-mode-probe-{stamp}"));
    // `create_new`: a stale entry of the same name (a crashed probe whose pid
    // was reused) is never truncated or followed; it makes the probe
    // inconclusive instead.
    let wrote_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file)
        .is_ok();
    let exec = wrote_file
        .then(|| -> std::io::Result<bool> {
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644))?;
            let off = probe_mode(&file)? & 0o111 == 0;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))?;
            let on = probe_mode(&file)? & 0o111 != 0;
            Ok(off && on)
        })
        .and_then(Result::ok);
    let _ = std::fs::remove_file(&file);
    let link = admin.join(format!("h4rn-link-probe-{stamp}"));
    let symlinks = match probe_symlink(&link) {
        Ok(()) => match link.symlink_metadata() {
            Ok(meta) if meta.file_type().is_symlink() => Some(true),
            _ => None,
        },
        // The filesystem demonstrably takes files here but refuses symlinks.
        Err(error) if wrote_file && symlinks_unsupported(&error) => Some(false),
        // A full disk, a quota, a permission problem: proves nothing.
        Err(_) => None,
    };
    let _ = std::fs::remove_file(&link);
    (exec, symlinks)
}

/// The errors a filesystem without symlink support returns from `symlink(2)`.
///
/// `EPERM` is deliberately not among them: a filesystem without symlinks can
/// return it, but so can a security policy denying `symlinkat` on one that
/// stores symlinks fine, and treating that as "unsupported" would switch the
/// symlink comparison off. It is inconclusive (forced, not cached), so on a
/// FAT-like filesystem a repository that tracks symlinks needs "Remove anyway".
#[cfg(unix)]
fn symlinks_unsupported(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(code) if code == libc::ENOSYS || code == libc::EOPNOTSUPP)
        || error.kind() == std::io::ErrorKind::Unsupported
}

/// The probe file's mode (a test can simulate a filesystem that ignores chmod).
#[cfg(unix)]
fn probe_mode(file: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    #[cfg(test)]
    if FORCE_SYNTHESIZED_MODE.with(std::cell::Cell::get) {
        return Ok(0o100777);
    }
    Ok(std::fs::metadata(file)?.permissions().mode())
}

/// Creates the probe symlink (a test can inject the error it fails with).
#[cfg(unix)]
fn probe_symlink(link: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(errno) = FORCE_SYMLINK_ERRNO.with(std::cell::Cell::get) {
        return Err(std::io::Error::from_raw_os_error(errno));
    }
    std::os::unix::fs::symlink("h4rn-probe-target", link)
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
