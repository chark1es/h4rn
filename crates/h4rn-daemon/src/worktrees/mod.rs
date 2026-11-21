//! Worktree tracking (subagent isolation, thread workspaces, harness-made).
//!
//! h4rn creates git worktrees for isolated subagents, and threads (or the
//! harness CLIs under them) work in or create others. The [`WorktreeTracker`]
//! keeps a picture of all of them so Settings can show what is in use, what is
//! settled, and clear the leftovers.
//!
//! Three origins feed the picture:
//!
//! - **subagent**: derived live from [`SubagentManager`]; h4rn owns the
//!   directory and the `h4rn/sa-*` branch.
//! - **thread**: derived live from every thread whose workspace is a linked
//!   worktree h4rn did not create.
//! - **harness**: observed by a watcher over the thread event stream (a turn
//!   that leaves a new linked worktree behind, or a tool call that touches
//!   one) and persisted in `worktrees.db`.
//!
//! Listing is computed fresh on every call; only harness observations are
//! stored. Clearing removes checkouts from disk and, for owned entries, the
//! `h4rn/` branch h4rn created.

pub(crate) mod git;
mod store;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use h4rn_protocol::{
    AgentState, Event, Item, SkippedWorktree, SubagentId, SubagentIsolation, SubagentView,
    ThreadFilter, ThreadId, ThreadTab, TrackedWorktree, WorktreeAccess, WorktreeOrigin,
    WorktreeStatus,
};
use store::{StoredWorktree, WorktreeStore};
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

use crate::journal::WireEvent;
use crate::subagents::SubagentManager;
use crate::threads::ThreadManager;

/// Minimum gap between `git worktree list` refreshes for one repository.
const CACHE_REFRESH: Duration = Duration::from_secs(2);

/// Minimum gap between persisted writes for one (worktree, thread) pair.
const RECORD_THROTTLE: Duration = Duration::from_secs(15);

/// Entries the throttle map may hold before stale ones are dropped.
const RECENT_LIMIT: usize = 1024;

/// How long an unfinished turn's snapshot can make overlapping turns shared.
const SNAPSHOT_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// Longest a listing may spend in git before giving up. Clearing is bounded by
/// the removal itself, so it has no budget.
const GIT_BUDGET: Duration = Duration::from_secs(60);

/// How many nested subagent worktrees a repository lookup may follow.
const MAX_REPO_HOPS: usize = 8;

/// How often the first turn's thread row may still be missing.
const META_RETRIES: usize = 20;

/// Wait between those retries.
const META_RETRY_GAP: Duration = Duration::from_millis(25);

/// Failures opening the tracker.
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    /// The observation store could not be opened.
    #[error("could not open the worktree store: {0}")]
    Store(String),
}

/// The result of a clear request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClearOutcome {
    /// Paths removed.
    pub removed: Vec<String>,
    /// Paths left in place, with reasons.
    pub skipped: Vec<SkippedWorktree>,
    /// The fresh list after clearing.
    pub worktrees: Vec<TrackedWorktree>,
}

/// The daemon-wide worktree tracker.
#[derive(Clone)]
pub struct WorktreeTracker {
    inner: Arc<Inner>,
}

struct Inner {
    threads: Arc<ThreadManager>,
    subagents: Arc<SubagentManager>,
    data_dir: PathBuf,
    /// Normalised `<data_dir>/subagents`; worktrees under it are h4rn's own.
    subagents_root: PathBuf,
    store: WorktreeStore,
    /// Worktree lists taken at turn start, keyed by thread id.
    snapshots: std::sync::Mutex<HashMap<String, Snapshot>>,
    /// Per-repository worktree lists for tool-call matching.
    repo_cache: RepoCaches,
    /// Thread id to the main worktree of its repository and the top level of
    /// its own checkout.
    thread_repos: std::sync::Mutex<HashMap<String, ThreadRepo>>,
    /// Last persisted write per (worktree, thread).
    recent: std::sync::Mutex<HashMap<(String, String), Instant>>,
    /// Serialises clears so two requests cannot race on one directory.
    clear_lock: Mutex<()>,
    watcher: std::sync::Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
}

#[derive(Clone)]
struct ThreadRepo {
    /// Main worktree of the repository.
    repo: PathBuf,
    /// Top level of the checkout the thread itself works in.
    own: PathBuf,
}

struct Snapshot {
    /// The repository the snapshot belongs to.
    repo: PathBuf,
    worktrees: Vec<git::GitWorktree>,
    /// Another turn on the same repository overlapped this one, so a new
    /// worktree cannot be attributed to this thread alone.
    shared: bool,
    /// When the turn started; a snapshot whose turn never reported completion
    /// stops counting as an overlapping turn after [`SNAPSHOT_TTL`].
    opened: Instant,
}

struct RepoCache {
    worktrees: Vec<git::GitWorktree>,
    refreshed: Instant,
}

/// Per-repository worktree lists, refreshed at most once per
/// [`CACHE_REFRESH`], so tool-call matching never shells out per event.
#[derive(Default)]
struct RepoCaches(std::sync::Mutex<HashMap<PathBuf, RepoCache>>);

impl RepoCaches {
    fn put(&self, repo: &Path, worktrees: Vec<git::GitWorktree>) {
        lock(&self.0).insert(
            repo.to_path_buf(),
            RepoCache {
                worktrees,
                refreshed: Instant::now(),
            },
        );
    }

    fn clear(&self) {
        lock(&self.0).clear();
    }

    fn matching(&self, repo: &Path, candidate: &Path) -> Option<git::GitWorktree> {
        lock(&self.0)
            .get(repo)?
            .worktrees
            .iter()
            .find(|worktree| candidate.starts_with(&worktree.path))
            .cloned()
    }

    fn stale(&self, repo: &Path) -> bool {
        lock(&self.0)
            .get(repo)
            .is_none_or(|cache| cache.refreshed.elapsed() >= CACHE_REFRESH)
    }

    /// Linked worktrees of `repo` that contain one of `candidates`.
    ///
    /// Blocking. Only shells out when a candidate sits under the repository's
    /// parent tree (or the call mentions `worktree`), matches no cached
    /// worktree and the cache is stale. A failed refresh still counts as an
    /// attempt, so a missing repository is not retried on every event.
    fn match_candidates(
        &self,
        repo: &Path,
        candidates: &[String],
        mentions: bool,
    ) -> Vec<git::GitWorktree> {
        let parent = repo.parent().map(Path::to_path_buf);
        let mut hits: Vec<git::GitWorktree> = Vec::new();
        let mut refreshed = false;
        for candidate in candidates {
            let candidate = git::norm(Path::new(candidate));
            let mut hit = self.matching(repo, &candidate);
            if hit.is_none()
                && !refreshed
                && (mentions || parent.as_ref().is_some_and(|p| candidate.starts_with(p)))
                && self.stale(repo)
            {
                refreshed = true;
                match git::list_worktrees(repo) {
                    Ok(list) => self.put(repo, list),
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            repo = %repo.display(),
                            "could not refresh a worktree list for tool-call matching"
                        );
                        // A failed refresh still counts as an attempt, so a
                        // missing repository is not retried on every event.
                        self.put(repo, Vec::new());
                    }
                }
                hit = self.matching(repo, &candidate);
            }
            if let Some(hit) = hit
                && hits.iter().all(|seen| seen.path != hit.path)
            {
                hits.push(hit);
            }
        }
        hits
    }
}

/// What one listing needs from the async world, as plain data.
pub(crate) struct ListInput {
    pub subagents: Vec<SubagentView>,
    /// Parent thread id to its workspace path.
    pub parent_workspaces: HashMap<String, String>,
    /// Subagent id to the repository stored when it was spawned.
    pub subagent_repos: HashMap<String, PathBuf>,
    pub threads: Vec<ThreadInfo>,
    pub stored: Vec<StoredWorktree>,
    pub subagents_root: PathBuf,
}

/// One thread as the listing sees it.
#[derive(Clone, Debug)]
pub(crate) struct ThreadInfo {
    pub thread_id: ThreadId,
    pub title: String,
    pub workspace: String,
    pub working: bool,
    pub last_activity: DateTime<Utc>,
}

/// One thread's workspace, in every form clearing compares entries against.
#[derive(Clone, Debug)]
pub(crate) struct ThreadWorkspace {
    pub thread_id: ThreadId,
    /// See [`workspace_forms`]; the listing also adds the top level of the
    /// checkout the workspace is in when that is a linked worktree.
    pub forms: Vec<PathBuf>,
}

/// Every form of a recorded thread workspace a comparison must consider.
///
/// The normalised path, plus, when the recorded path does not exist (a branch
/// switch removed `src/`, a symlinked parent now points elsewhere), the
/// recorded path with `.`/`..` folded lexically: the checkout it was recorded
/// in still holds it, whatever the filesystem resolves it to now. Blocking
/// (touches the filesystem). Both the Clear-all selection and the bulk live
/// re-check use exactly this, with [`workspace_inside`].
pub(crate) fn workspace_forms(recorded: &Path) -> Vec<PathBuf> {
    let mut forms = vec![git::norm(recorded)];
    if !recorded.exists() {
        let folded = git::fold(recorded.to_path_buf());
        if !forms.contains(&folded) {
            forms.push(folded);
        }
    }
    forms
}

/// Whether any form of a thread workspace is `entry` or lies inside it, by
/// whole path components (a sibling `wt-a2` is not inside `wt-a`).
fn workspace_inside(forms: &[PathBuf], entry: &Path) -> bool {
    forms.iter().any(|form| form.starts_with(entry))
}

/// The threads whose workspace never protects `entry`: an owned entry's own
/// subagent child, which works in exactly the worktree it leaves behind.
///
/// Recognised as the accessor carrying the entry's own subagent id. Every
/// other thread counts, and a non-owned entry exempts none.
fn own_child_threads(entry: &TrackedWorktree) -> HashSet<ThreadId> {
    if !entry.owned {
        return HashSet::new();
    }
    entry
        .accessed_by
        .iter()
        .filter(|access| access.subagent_id.is_some() && access.subagent_id == entry.subagent_id)
        .map(|access| access.thread_id.clone())
        .collect()
}

/// What the live re-check found using an entry.
enum LiveUse {
    /// Something is busy in it; the entry with fresh `working` flags.
    Active(TrackedWorktree),
    /// Nothing is busy, but a bulk clear found a thread working in it.
    ThreadWorkspace,
}

/// The reason for an entry a bulk clear leaves alone because it is (or holds)
/// a thread's workspace (never forceable).
const THREAD_WORKSPACE: &str = "is a thread workspace";

/// A thread that might still use an entry, gathered from one of the sources a
/// live re-check consults. Its activity is read only once all are collected.
struct Candidate {
    thread_id: ThreadId,
    title: String,
    subagent_id: Option<SubagentId>,
    last_seen: DateTime<Utc>,
}

impl WorktreeTracker {
    /// Opens the tracker over `<data_dir>/worktrees.db`.
    ///
    /// Call [`Self::start`] once the daemon is ready.
    pub async fn open(
        threads: Arc<ThreadManager>,
        subagents: Arc<SubagentManager>,
        data_dir: PathBuf,
    ) -> Result<Self, WorktreeError> {
        let store = WorktreeStore::connect(data_dir.join("worktrees.db"))
            .await
            .map_err(|error| WorktreeError::Store(error.to_string()))?;
        Ok(Self::with_store(threads, subagents, data_dir, store))
    }

    /// An in-memory tracker for tests.
    pub async fn in_memory(
        threads: Arc<ThreadManager>,
        subagents: Arc<SubagentManager>,
        data_dir: PathBuf,
    ) -> Result<Self, WorktreeError> {
        let store = WorktreeStore::in_memory()
            .await
            .map_err(|error| WorktreeError::Store(error.to_string()))?;
        Ok(Self::with_store(threads, subagents, data_dir, store))
    }

    fn with_store(
        threads: Arc<ThreadManager>,
        subagents: Arc<SubagentManager>,
        data_dir: PathBuf,
        store: WorktreeStore,
    ) -> Self {
        let subagents_root = git::norm(&data_dir.join("subagents"));
        Self {
            inner: Arc::new(Inner {
                threads,
                subagents,
                data_dir,
                subagents_root,
                store,
                snapshots: std::sync::Mutex::new(HashMap::new()),
                repo_cache: RepoCaches::default(),
                thread_repos: std::sync::Mutex::new(HashMap::new()),
                recent: std::sync::Mutex::new(HashMap::new()),
                clear_lock: Mutex::new(()),
                watcher: std::sync::Mutex::new(None),
                closed: AtomicBool::new(false),
            }),
        }
    }

    /// Starts the observation watcher over the thread event stream.
    pub fn start(&self) {
        let mut slot = lock(&self.inner.watcher);
        if slot.is_some() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let handle = tokio::spawn(async move {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let mut events = inner.threads.subscribe();
            drop(inner);
            loop {
                let item = events.recv().await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                if inner.closed.load(Ordering::Relaxed) {
                    return;
                }
                match item {
                    Ok(event) => inner.handle_wire(event).await,
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::debug!(missed, "worktree watcher skipped thread events");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        *slot = Some(handle);
    }

    /// Stops the watcher. Tracked worktrees stay on disk.
    pub async fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        if let Some(handle) = lock(&self.inner.watcher).take() {
            handle.abort();
        }
    }

    /// Lists every tracked worktree, computed fresh.
    ///
    /// Active first, then orphaned, then settled; newest `last_seen` first
    /// within each group.
    pub async fn list(&self) -> Vec<TrackedWorktree> {
        self.list_with(true).await.0
    }

    /// [`Self::list`], optionally without the dirty check.
    ///
    /// Clearing first lists without `git status` for every entry because the
    /// per-entry removal checks dirtiness again anyway; the final response
    /// listing asks git for it.
    ///
    /// Also returns every thread workspace (and its checkout's top level),
    /// which clearing everything must leave alone whatever row names it.
    async fn list_with(&self, with_dirty: bool) -> (Vec<TrackedWorktree>, Vec<ThreadWorkspace>) {
        let input = self.inner.list_input().await;
        let inner = Arc::clone(&self.inner);
        let computed = tokio::time::timeout(
            GIT_BUDGET,
            tokio::task::spawn_blocking(move || compute_listing(&input, with_dirty)),
        )
        .await;
        let (entries, gone, thread_workspaces) = match computed {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                tracing::warn!(%error, "worktree listing task failed");
                return (Vec::new(), Vec::new());
            }
            Err(_) => {
                tracing::warn!("worktree listing timed out");
                return (Vec::new(), Vec::new());
            }
        };
        // A harness worktree whose directory is gone has nothing left to clear.
        for path in gone {
            if let Err(error) = inner.store.delete(&path).await {
                tracing::warn!(%error, %path, "could not forget a vanished worktree");
            }
        }
        (entries, thread_workspaces)
    }

    /// Removes tracked worktrees from disk.
    ///
    /// `paths: None` targets every non-active entry except thread workspaces
    /// (by origin, or a row that is a listed thread's workspace or holds one),
    /// which are only removed when named explicitly. `bulk` applies that
    /// thread-workspace exclusion to the given paths too (Settings' Clear all
    /// sends the paths it showed with `bulk: true`). Every entry the exclusion
    /// leaves out is reported as skipped, and a bulk clear's live re-check
    /// leaves out an entry that any thread, busy or settled, now works in.
    /// Active entries are never removed. Without `force`, dirty worktrees and
    /// owned branches with unmerged commits are skipped.
    pub async fn clear(&self, paths: Option<Vec<String>>, force: bool, bulk: bool) -> ClearOutcome {
        self.clear_with(paths, force, bulk, |_| std::future::ready(()))
            .await
    }

    /// [`Self::clear`], running `after_preflight` for every entry between its
    /// preflight and prepare (see [`run_clear_entry`]); tests use it to change
    /// one entry's surroundings while another is being cleared.
    async fn clear_with<F, Fut>(
        &self,
        paths: Option<Vec<String>>,
        force: bool,
        bulk: bool,
        mut after_preflight: F,
    ) -> ClearOutcome
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _guard = self.inner.clear_lock.lock().await;
        let bulk = bulk || paths.is_none();
        let (entries, thread_workspaces) = self.list_with(false).await;
        let selection = select_entries(&entries, paths.as_deref(), bulk, &thread_workspaces);
        let mut removed = Vec::new();
        let mut skipped = selection.skipped;
        // Each repository's common dir is resolved once for the whole clear.
        let commons = Arc::new(git::CommonDirs::default());
        // One entry at a time: the listing is a snapshot, so each removal runs
        // a blocking preflight, then asks the live thread and subagent state
        // again, and only then touches the disk. Both spawn_blocking calls are
        // awaited rather than timed out.
        for entry in selection.targets {
            let entry = entry.clone();
            let path = entry.path.clone();
            let commons = Arc::clone(&commons);
            match run_clear_entry(
                &self.inner,
                entry,
                force,
                bulk,
                commons,
                &mut after_preflight,
            )
            .await
            {
                Ok(ClearOne::Removed) => removed.push(path),
                Ok(ClearOne::RemovedWithBranchFailure(reason)) => {
                    skipped.push(SkippedWorktree { path, reason });
                }
                Err(reason) => skipped.push(SkippedWorktree { path, reason }),
            }
        }
        for path in &removed {
            if let Err(error) = self.inner.store.delete(path).await {
                tracing::warn!(%error, %path, "could not forget a cleared worktree");
            }
        }
        self.inner.repo_cache.clear();
        let worktrees = self.list().await;
        ClearOutcome {
            removed,
            skipped,
            worktrees,
        }
    }
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Every thread the listing considers, children and snoozed included.
fn thread_filter() -> ThreadFilter {
    ThreadFilter {
        include_snoozed: true,
        include_children: true,
        ..ThreadFilter::default()
    }
}

// ---------------------------------------------------------------------------
// Observation
// ---------------------------------------------------------------------------

/// Opens a turn-start snapshot for `thread`, returning whether another turn on
/// `repo` is already running.
///
/// Both overlapping snapshots become `shared`: neither turn can claim a
/// worktree that appears while the other is running.
fn open_snapshot(
    snapshots: &mut HashMap<String, Snapshot>,
    thread: &str,
    repo: &Path,
    worktrees: Vec<git::GitWorktree>,
) -> bool {
    // A turn that never reported completion (a crashed harness, a deleted
    // thread) must not mark every later turn on its repository as shared.
    snapshots.retain(|_, snapshot| snapshot.opened.elapsed() < SNAPSHOT_TTL);
    let mut shared = false;
    for snapshot in snapshots.values_mut() {
        if snapshot.repo == repo {
            snapshot.shared = true;
            shared = true;
        }
    }
    snapshots.insert(
        thread.to_owned(),
        Snapshot {
            repo: repo.to_path_buf(),
            worktrees,
            shared,
            opened: Instant::now(),
        },
    );
    shared
}

/// Whether a turn's new worktrees can be attributed to its own thread.
fn diff_attributable(snapshot: &Snapshot) -> bool {
    !snapshot.shared
}

impl Inner {
    async fn handle_wire(self: &Arc<Self>, wire: WireEvent) {
        match wire.event {
            Event::TurnStarted { thread_id, .. } => self.turn_started(&thread_id).await,
            Event::TurnCompleted { thread_id, .. } => self.turn_completed(&thread_id).await,
            Event::ToolCall {
                thread_id,
                tool_call,
                ..
            } => {
                if let Some(input) = tool_call.input {
                    self.tool_input(&thread_id, &input).await;
                }
            }
            Event::ItemCompleted {
                thread_id,
                item: Item::ToolCall { tool_call, .. },
                ..
            } => {
                if let Some(input) = tool_call.input {
                    self.tool_input(&thread_id, &input).await;
                }
            }
            _ => {}
        }
    }

    /// The main worktree of the repository a thread works in.
    async fn repo_for_thread(self: &Arc<Self>, thread_id: &ThreadId) -> Option<PathBuf> {
        self.thread_repo(thread_id).await.map(|found| found.repo)
    }

    async fn thread_repo(self: &Arc<Self>, thread_id: &ThreadId) -> Option<ThreadRepo> {
        if let Some(found) = lock(&self.thread_repos).get(thread_id.as_str()) {
            return Some(found.clone());
        }
        let meta = self.threads.thread_meta(thread_id).await?;
        self.thread_repo_at(thread_id, &meta.workspace_path).await
    }

    /// The repository and checkout top for a thread working at `workspace`.
    async fn thread_repo_at(
        self: &Arc<Self>,
        thread_id: &ThreadId,
        workspace: &str,
    ) -> Option<ThreadRepo> {
        let workspace = PathBuf::from(workspace);
        let found = tokio::task::spawn_blocking(move || {
            let repo = git::repo_of(&workspace)?;
            let own = git::toplevel(&workspace).unwrap_or_else(|| git::norm(&workspace));
            Some(ThreadRepo { repo, own })
        })
        .await
        .ok()
        .flatten()?;
        lock(&self.thread_repos).insert(thread_id.as_str().to_owned(), found.clone());
        Some(found)
    }

    /// [`Self::thread_repo`], but waits out the window where a brand-new
    /// thread has broadcast `TurnStarted` before its row exists.
    async fn thread_repo_when_known(self: &Arc<Self>, thread_id: &ThreadId) -> Option<ThreadRepo> {
        for attempt in 0..META_RETRIES {
            if let Some(meta) = self.threads.thread_meta(thread_id).await {
                return self.thread_repo_at(thread_id, &meta.workspace_path).await;
            }
            if attempt + 1 < META_RETRIES {
                tokio::time::sleep(META_RETRY_GAP).await;
            }
        }
        None
    }

    async fn list_repo(self: &Arc<Self>, repo: &Path) -> Option<Vec<git::GitWorktree>> {
        let repo = repo.to_path_buf();
        let listed = {
            let repo = repo.clone();
            tokio::task::spawn_blocking(move || git::list_worktrees(&repo))
                .await
                .ok()?
        };
        match listed {
            Ok(list) => {
                self.repo_cache.put(&repo, list.clone());
                Some(list)
            }
            Err(error) => {
                tracing::debug!(%error, repo = %repo.display(), "could not list worktrees");
                None
            }
        }
    }

    async fn turn_started(self: &Arc<Self>, thread_id: &ThreadId) {
        // `create_thread` broadcasts `TurnStarted` before it inserts the thread,
        // so the very first turn can arrive before `thread_meta` knows it.
        let Some(found) = self.thread_repo_when_known(thread_id).await else {
            return;
        };
        let repo = found.repo;
        let Some(worktrees) = self.list_repo(&repo).await else {
            return;
        };
        let mut snapshots = lock(&self.snapshots);
        open_snapshot(&mut snapshots, thread_id.as_str(), &repo, worktrees);
    }

    async fn turn_completed(self: &Arc<Self>, thread_id: &ThreadId) {
        let Some(snapshot) = lock(&self.snapshots).remove(thread_id.as_str()) else {
            return;
        };
        // Another turn on the same repository overlapped this one, so a
        // worktree that appeared belongs to neither thread alone. Tool calls
        // still record the accesses they can prove.
        if !diff_attributable(&snapshot) {
            return;
        }
        let Some(repo) = self.repo_for_thread(thread_id).await else {
            return;
        };
        let Some(after) = self.list_repo(&repo).await else {
            return;
        };
        for worktree in git::new_worktrees(&snapshot.worktrees, &after) {
            self.record(&repo, &worktree, thread_id).await;
        }
    }

    async fn tool_input(self: &Arc<Self>, thread_id: &ThreadId, input: &serde_json::Value) {
        let candidates = git::path_candidates(input);
        if candidates.is_empty() {
            return;
        }
        let mentions = git::mentions_worktree(input);
        let Some(repo) = self.repo_for_thread(thread_id).await else {
            return;
        };
        let inner = Arc::clone(self);
        let search_repo = repo.clone();
        let hits = tokio::task::spawn_blocking(move || {
            inner
                .repo_cache
                .match_candidates(&search_repo, &candidates, mentions)
        })
        .await
        .unwrap_or_default();
        for worktree in hits {
            self.record(&repo, &worktree, thread_id).await;
        }
    }

    /// Persists one access. Best-effort: failures are logged.
    async fn record(self: &Arc<Self>, repo: &Path, worktree: &git::GitWorktree, thread: &ThreadId) {
        // h4rn's own subagent worktrees are origin `subagent`, and a thread's
        // own checkout is origin `thread`; both are derived live.
        if worktree.path.starts_with(&self.subagents_root) || worktree.path == repo {
            return;
        }
        // The protocol carries paths as strings. A path that cannot round-trip
        // through UTF-8 is never offered lossily, so it is never stored either.
        let (Some(path_text), Some(repo_text)) = (worktree.path.to_str(), repo.to_str()) else {
            return;
        };
        if self
            .thread_repo(thread)
            .await
            .is_some_and(|found| found.own == worktree.path)
        {
            return;
        }
        let key = (path_text.to_owned(), thread.as_str().to_owned());
        {
            let mut recent = lock(&self.recent);
            if recent
                .get(&key)
                .is_some_and(|at| at.elapsed() < RECORD_THROTTLE)
            {
                return;
            }
            recent.insert(key.clone(), Instant::now());
            if recent.len() > RECENT_LIMIT {
                recent.retain(|_, at| at.elapsed() < RECORD_THROTTLE);
            }
        }
        let subagent_id = self
            .threads
            .thread_meta(thread)
            .await
            .and_then(|meta| meta.subagent_id);
        if let Err(error) = self
            .store
            .record(
                &key.0,
                repo_text,
                worktree.branch.as_deref(),
                thread.as_str(),
                subagent_id.as_ref().map(SubagentId::as_str),
                Utc::now(),
            )
            .await
        {
            tracing::warn!(%error, path = %key.0, "could not record a worktree observation");
        }
    }

    async fn list_input(&self) -> ListInput {
        let subagents: Vec<SubagentView> = self
            .subagents
            .list(None, true)
            .await
            .into_iter()
            .filter(|view| view.isolation == SubagentIsolation::Worktree)
            .collect();
        let mut parent_workspaces = HashMap::new();
        for view in &subagents {
            let parent = view.parent_thread_id.as_str();
            if parent_workspaces.contains_key(parent) {
                continue;
            }
            if let Some(meta) = self.threads.thread_meta(&view.parent_thread_id).await {
                parent_workspaces.insert(parent.to_owned(), meta.workspace_path);
            }
        }
        let subagent_repos = self.subagents.worktree_repos().await;
        let mut threads = Vec::new();
        for summary in self.threads.list_threads(&thread_filter()).await {
            let turn_in_flight = self.threads.turn_in_flight(&summary.thread_id).await;
            threads.push(ThreadInfo {
                thread_id: summary.thread_id,
                title: summary.title,
                workspace: summary.workspace_path,
                working: is_busy(summary.tab, summary.agent_state, turn_in_flight),
                last_activity: summary.last_activity,
            });
        }
        let stored = match self.store.load().await {
            Ok(stored) => stored,
            Err(error) => {
                tracing::warn!(%error, "could not load worktree observations");
                Vec::new()
            }
        };
        ListInput {
            subagents,
            parent_workspaces,
            subagent_repos,
            threads,
            stored,
            subagents_root: self.subagents_root.clone(),
        }
    }

    /// Re-reads an entry's activity from the live thread and subagent state.
    ///
    /// Returns the entry with fresh `working` flags when something still uses
    /// it, `None` when nothing does. The listing is only a snapshot, so this
    /// also looks for users the listing did not know: every thread whose
    /// workspace is the entry or inside it, and every access recorded in the
    /// store since the listing was taken. Those sources only *collect*
    /// candidates; activity is checked once, at the end, over the whole set, so
    /// a thread found by one source while it was idle cannot be dropped before
    /// a later source would have seen it busy.
    ///
    /// In a `bulk` clear, a thread whose workspace is the entry or inside it
    /// protects the entry even while it is settled (the Clear all exclusion,
    /// checked live, with the same comparison as the selection: see
    /// [`workspace_forms`]): the answer is then [`LiveUse::ThreadWorkspace`]
    /// unless something is busy. Only an owned entry's own subagent child is
    /// exempt from that (it works in the very worktree it leaves behind); its
    /// activity still counts like any thread's.
    async fn still_active(
        self: &Arc<Self>,
        entry: &TrackedWorktree,
        bulk: bool,
    ) -> Option<LiveUse> {
        let mut fresh = entry.clone();
        // The owned entry's subagent, read once: its child is exempt from the
        // thread-workspace protection, and a live subagent keeps it active.
        let view = match (&entry.subagent_id, entry.owned) {
            (Some(id), true) => self.subagents.get(id).await,
            _ => None,
        };
        let mut own_child = own_child_threads(entry);
        if let Some(child) = view.as_ref().and_then(|view| view.child_thread_id.clone()) {
            own_child.insert(child);
        }
        // Normalising touches the filesystem, so it stays off the async runtime.
        let entry_path = entry.path.clone();
        let target = tokio::task::spawn_blocking(move || git::norm(Path::new(&entry_path)))
            .await
            .unwrap_or_else(|_| PathBuf::from(&entry.path));
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for access in &entry.accessed_by {
            if seen.insert(access.thread_id.as_str().to_owned()) {
                candidates.push(Candidate {
                    thread_id: access.thread_id.clone(),
                    title: access.title.clone(),
                    subagent_id: access.subagent_id.clone(),
                    last_seen: access.last_seen,
                });
            }
        }
        // Accesses persisted since the listing was taken.
        match self.store.load().await {
            Ok(stored) => {
                let wanted = target.clone();
                let matching = tokio::task::spawn_blocking(move || {
                    stored
                        .into_iter()
                        .filter(|observation| git::norm(Path::new(&observation.path)) == wanted)
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default();
                for observation in matching {
                    for access in observation.accesses {
                        if seen.insert(access.thread_id.clone()) {
                            candidates.push(Candidate {
                                thread_id: ThreadId::new(access.thread_id),
                                title: String::new(),
                                subagent_id: access.subagent_id.map(SubagentId::new),
                                last_seen: access.last_seen,
                            });
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not reload worktree observations");
            }
        }
        // Any thread working in the entry, listed or not, compared exactly as
        // the selection compares (the listed path; its normalised form too).
        let mut workspace_thread = false;
        let threads = self.threads.list_threads(&thread_filter()).await;
        let listed = PathBuf::from(&entry.path);
        let inside = tokio::task::spawn_blocking(move || {
            threads
                .into_iter()
                .filter(|summary| {
                    let forms = workspace_forms(Path::new(&summary.workspace_path));
                    workspace_inside(&forms, &listed) || workspace_inside(&forms, &target)
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        for summary in inside {
            workspace_thread |= !own_child.contains(&summary.thread_id);
            if seen.insert(summary.thread_id.as_str().to_owned()) {
                let subagent_id = self
                    .threads
                    .thread_meta(&summary.thread_id)
                    .await
                    .and_then(|meta| meta.subagent_id);
                candidates.push(Candidate {
                    thread_id: summary.thread_id,
                    title: summary.title,
                    subagent_id,
                    last_seen: summary.last_activity,
                });
            }
        }
        // The one activity check, with every candidate known.
        let mut active = false;
        for candidate in candidates {
            let mut access = WorktreeAccess {
                thread_id: candidate.thread_id,
                title: candidate.title,
                subagent_id: candidate.subagent_id,
                working: false,
                last_seen: candidate.last_seen,
            };
            self.refresh_access(&mut access).await;
            active |= access.working;
            match fresh
                .accessed_by
                .iter_mut()
                .find(|seen| seen.thread_id == access.thread_id)
            {
                Some(existing) => {
                    existing.working = access.working;
                    if !access.title.is_empty() {
                        existing.title = access.title;
                    }
                }
                None if access.working => fresh.accessed_by.push(access),
                None => {}
            }
        }
        if view.is_some_and(|view| !view.is_terminal()) {
            active = true;
        }
        if active {
            Some(LiveUse::Active(fresh))
        } else if bulk && workspace_thread {
            Some(LiveUse::ThreadWorkspace)
        } else {
            None
        }
    }

    /// Refreshes one accessor's busy flag and title from the live thread.
    async fn refresh_access(&self, access: &mut WorktreeAccess) {
        let Some(summary) = self.threads.thread_summary(&access.thread_id).await else {
            access.working = false;
            return;
        };
        let turn_in_flight = self.threads.turn_in_flight(&access.thread_id).await;
        access.working = is_busy(summary.tab, summary.agent_state, turn_in_flight);
        if !summary.title.is_empty() {
            access.title = summary.title;
        }
    }
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// Whether a thread is actually busy, not merely sitting in a Working tab.
///
/// A turn in flight means the thread can still write, whatever the agent state
/// says: a failed or denied tool call sets `Error` while the turn keeps going.
/// Beyond that, a pinned or never-run thread keeps the Working tab while it is
/// Idle, and an auto-resume armed for a rate limit is not running yet. So the
/// thread is busy when a turn is in flight, when it is `Idle` in the Working
/// tab (pinned or never run), or when its agent state is running, queued,
/// scheduled or waiting on the user. `Done`, `Error` and `Unreachable` are
/// never busy on their own, even inside the Working-tab exit grace.
fn is_busy(tab: ThreadTab, agent_state: AgentState, turn_in_flight: bool) -> bool {
    turn_in_flight
        || (tab == ThreadTab::Working && agent_state == AgentState::Idle)
        || matches!(
            agent_state,
            AgentState::Queued
                | AgentState::Scheduled
                | AgentState::Thinking
                | AgentState::Searching
                | AgentState::Reading
                | AgentState::Editing
                | AgentState::RunningTool
                | AgentState::AwaitingApproval
                | AgentState::RateLimited
        )
}

/// The main worktree of the repository an isolated subagent's worktree is in.
///
/// The stored repository comes first, because an entry outlives its directory:
/// a child whose parent workspace is a removed subagent worktree only knows its
/// repository from the record written at spawn. Then the parent thread's
/// workspace, then git itself.
fn subagent_repo(
    input: &ListInput,
    view: &SubagentView,
    path: &Path,
    commons: &git::CommonDirs,
) -> Option<PathBuf> {
    if let Some(stored) = input.subagent_repos.get(view.subagent_id.as_str())
        && let Some(repo) = repo_through_subagents(input, stored, commons)
    {
        return Some(repo);
    }
    if let Some(workspace) = input.parent_workspaces.get(view.parent_thread_id.as_str())
        && let Some(repo) = repo_through_subagents(input, Path::new(workspace), commons)
    {
        return Some(repo);
    }
    git::repo_of_with(path, commons)
}

/// The repository at `path`, following h4rn's own subagent worktrees.
///
/// A nested child's parent workspace is `<data_dir>/subagents/<id>/worktree`,
/// which is deleted when that subagent settles. Its recorded repository is the
/// only reliable answer then, so follow it, bounded against a stale loop.
fn repo_through_subagents(
    input: &ListInput,
    path: &Path,
    commons: &git::CommonDirs,
) -> Option<PathBuf> {
    let mut path = git::norm(path);
    for _ in 0..MAX_REPO_HOPS {
        match subagent_worktree_owner(&path, &input.subagents_root) {
            Some(id) => match input.subagent_repos.get(&id) {
                Some(stored) => path = git::norm(stored),
                None => return git::repo_of_with(&path, commons),
            },
            None => return git::repo_of_with(&path, commons),
        }
    }
    None
}

/// The subagent whose worktree is at `path`, when it is one of h4rn's.
///
/// h4rn puts them at `<data_dir>/subagents/<id>/worktree`.
fn subagent_worktree_owner(path: &Path, subagents_root: &Path) -> Option<String> {
    let mut parts = path.strip_prefix(subagents_root).ok()?.components();
    let id = parts.next()?.as_os_str().to_str()?.to_owned();
    (parts.next()?.as_os_str() == "worktree").then_some(id)
}

fn status_rank(status: WorktreeStatus) -> u8 {
    match status {
        WorktreeStatus::Active => 0,
        WorktreeStatus::Orphaned => 1,
        WorktreeStatus::Settled => 2,
    }
}

fn access_of(
    info: Option<&ThreadInfo>,
    thread_id: &ThreadId,
    subagent_id: Option<SubagentId>,
    last_seen: DateTime<Utc>,
) -> WorktreeAccess {
    WorktreeAccess {
        thread_id: thread_id.clone(),
        title: info.map(|info| info.title.clone()).unwrap_or_default(),
        subagent_id,
        working: info.is_some_and(|info| info.working),
        last_seen,
    }
}

/// Whether `path` still has its own registration in `repo`.
///
/// `Some(true)` / `Some(false)` once the repository's snapshot was read;
/// `None` when git could not answer, or when the path is not in a snapshot
/// that is not complete. A caller must never treat `None` as absence: a failed
/// or partial snapshot says nothing about what is still registered.
///
/// `lists` caches one snapshot per repository for the listing, so a repository
/// with many vanished observations is asked only once.
fn registration_exists(
    repo: &Path,
    path: &Path,
    lists: &mut HashMap<PathBuf, Option<git::RegistrationSnapshot>>,
    commons: &git::CommonDirs,
) -> Option<bool> {
    if repo.as_os_str().is_empty() {
        return Some(false);
    }
    lists
        .entry(repo.to_path_buf())
        .or_insert_with(|| {
            let common = commons.get(repo, git::OBSERVATION_GIT_TIMEOUT).ok()?;
            git::registrations_in(repo, &common, git::OBSERVATION_GIT_TIMEOUT, commons).ok()
        })
        .as_ref()
        .and_then(|snapshot| {
            let found = snapshot
                .iter()
                .any(|registration| registration.path == path);
            (found || snapshot.complete).then_some(found)
        })
}

/// Whether a real directory is at `path`. A symlink there is never followed:
/// it is not a checkout, whatever it points at.
fn is_real_dir(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|meta| meta.is_dir())
}

/// [`compute_listing`] without the thread workspaces.
#[cfg(test)]
pub(crate) fn compute(input: &ListInput, with_dirty: bool) -> (Vec<TrackedWorktree>, Vec<String>) {
    let (entries, gone, _) = compute_listing(input, with_dirty);
    (entries, gone)
}

/// Computes the tracked list from plain data. Blocking (shells out to git).
///
/// Returns the entries, sorted; the stored harness paths whose directory is
/// gone and can be forgotten; and every listed thread's workspace (see
/// [`workspace_forms`]), with the top level of each one outside h4rn's own
/// subagent worktrees that is a linked worktree. Subagent children are
/// included: only the entry a child's own subagent left behind exempts it
/// (see [`own_child_threads`]). `with_dirty` asks git for uncommitted changes;
/// clearing's selection listing skips it because `clear_one` checks again.
pub(crate) fn compute_listing(
    input: &ListInput,
    with_dirty: bool,
) -> (Vec<TrackedWorktree>, Vec<String>, Vec<ThreadWorkspace>) {
    let now = Utc::now();
    let infos: HashMap<&str, &ThreadInfo> = input
        .threads
        .iter()
        .map(|info| (info.thread_id.as_str(), info))
        .collect();
    let child_subagent: HashMap<&str, SubagentId> = input
        .subagents
        .iter()
        .filter_map(|view| {
            view.child_thread_id
                .as_ref()
                .map(|child| (child.as_str(), view.subagent_id.clone()))
        })
        .collect();

    let mut entries: Vec<TrackedWorktree> = Vec::new();
    let mut index: HashMap<PathBuf, usize> = HashMap::new();
    let mut repo_lists: HashMap<PathBuf, Vec<git::GitWorktree>> = HashMap::new();
    // One `for-each-ref` per repository answers every owned branch lookup;
    // `None` records that git could not answer for that repository.
    let mut branch_lists: HashMap<PathBuf, Option<HashSet<String>>> = HashMap::new();
    // Registrations are asked for only when a stored path's directory is gone,
    // and then only once per repository.
    let mut registration_lists: HashMap<PathBuf, Option<git::RegistrationSnapshot>> =
        HashMap::new();
    // Each repository's common dir (and whether it is bare) is resolved once
    // for the whole listing.
    let commons = git::CommonDirs::default();

    // 1. Worktrees h4rn made for isolated subagents.
    for view in &input.subagents {
        // Listed under the recorded path with only its parent normalised:
        // clearing acts on the path as listed, and a recorded path that is now
        // a symlink must keep its own name (which `checkout_at` then refuses)
        // rather than turn into whatever the symlink points at. Normalising
        // the parent keeps a data directory reached through a symlink
        // clearable, matching how git records registration paths.
        let path = git::norm_listed(Path::new(&view.workspace_path));
        let repo = subagent_repo(input, view, &path, &commons);
        // The protocol carries paths as strings, so a path that cannot
        // round-trip through UTF-8 is never listed.
        if path.to_str().is_none() || repo.as_ref().is_some_and(|repo| repo.to_str().is_none()) {
            continue;
        }
        let exists = is_real_dir(&path);
        let mut branch = view.branch.clone();
        // A missing checkout is dropped only when git proved there is nothing
        // left to clear: the branch lookup succeeded without it AND a
        // successful snapshot holds no registration of its own. A lookup git
        // could not answer keeps the row with its recorded branch, and so does
        // a repository that could not be found at all (the row is listed with
        // an empty repo; clearing it says so).
        if !exists && let Some(repo) = &repo {
            // `Some(present)` once git answered; `None` when it could not.
            let branch_known = match &view.branch {
                Some(name) => branch_lists
                    .entry(repo.clone())
                    .or_insert_with(|| git::h4rn_branches(repo).ok())
                    .as_ref()
                    .map(|branches| branches.contains(name)),
                None => Some(false),
            };
            if branch_known == Some(false) {
                match registration_exists(repo, &path, &mut registration_lists, &commons) {
                    // Only the registration is left; never promise the branch.
                    Some(true) => branch = None,
                    None => {}
                    Some(false) => continue,
                }
            }
        }
        let status = if !view.is_terminal() {
            WorktreeStatus::Active
        } else if exists {
            WorktreeStatus::Orphaned
        } else {
            WorktreeStatus::Settled
        };
        let accessed_by = view
            .child_thread_id
            .as_ref()
            .map(|child| {
                vec![access_of(
                    infos.get(child.as_str()).copied(),
                    child,
                    Some(view.subagent_id.clone()),
                    view.finished_at.unwrap_or(now),
                )]
            })
            .unwrap_or_default();
        index.insert(path.clone(), entries.len());
        entries.push(TrackedWorktree {
            path: path.to_string_lossy().into_owned(),
            repo: repo
                .map(|repo| repo.to_string_lossy().into_owned())
                .unwrap_or_default(),
            branch,
            origin: WorktreeOrigin::Subagent,
            status,
            exists,
            dirty: None,
            owned: true,
            subagent_id: Some(view.subagent_id.clone()),
            task: Some(view.task.clone()),
            accessed_by,
            first_seen: view.created_at,
            last_seen: view.finished_at.unwrap_or(now),
        });
    }

    // 2. Thread workspaces that are linked worktrees h4rn did not create.
    // A workspace may be a subdirectory of its checkout; group by the
    // checkout's top level, which is what `git worktree remove` takes.
    let mut tops: HashMap<PathBuf, Option<PathBuf>> = HashMap::new();
    let mut by_workspace: HashMap<PathBuf, Vec<&ThreadInfo>> = HashMap::new();
    let mut thread_workspaces: Vec<ThreadWorkspace> = Vec::new();
    for info in &input.threads {
        let mut forms = workspace_forms(Path::new(&info.workspace));
        let workspace = forms[0].clone();
        // A subagent child's workspace still protects other entries, but it
        // never makes a thread row: its worktree is listed as `subagent`.
        if !workspace.starts_with(&input.subagents_root) && workspace.is_dir() {
            let top = tops
                .entry(workspace.clone())
                .or_insert_with(|| {
                    git::is_linked_worktree(&workspace)
                        .then(|| git::toplevel(&workspace))
                        .flatten()
                })
                .clone();
            if let Some(top) = top {
                if !forms.contains(&top) {
                    forms.push(top.clone());
                }
                by_workspace.entry(top).or_default().push(info);
            }
        }
        thread_workspaces.push(ThreadWorkspace {
            thread_id: info.thread_id.clone(),
            forms,
        });
    }
    for (path, group) in by_workspace {
        if path.starts_with(&input.subagents_root) || index.contains_key(&path) {
            continue;
        }
        let Some(repo) = git::repo_of_with(&path, &commons) else {
            continue;
        };
        if path.to_str().is_none() || repo.to_str().is_none() {
            continue;
        }
        let list = repo_lists
            .entry(repo.clone())
            .or_insert_with(|| git::list_worktrees_with(&repo, &commons).unwrap_or_default());
        let branch = list
            .iter()
            .find(|worktree| worktree.path == path)
            .and_then(|worktree| worktree.branch.clone());
        let accessed_by: Vec<WorktreeAccess> = group
            .iter()
            .map(|info| {
                access_of(
                    Some(info),
                    &info.thread_id,
                    child_subagent.get(info.thread_id.as_str()).cloned(),
                    info.last_activity,
                )
            })
            .collect();
        let first_seen = group.iter().map(|info| info.last_activity).min();
        let last_seen = group.iter().map(|info| info.last_activity).max();
        let working = accessed_by.iter().any(|access| access.working);
        index.insert(path.clone(), entries.len());
        entries.push(TrackedWorktree {
            path: path.to_string_lossy().into_owned(),
            repo: repo.to_string_lossy().into_owned(),
            branch,
            origin: WorktreeOrigin::Thread,
            status: if working {
                WorktreeStatus::Active
            } else {
                WorktreeStatus::Settled
            },
            exists: true,
            dirty: None,
            owned: false,
            subagent_id: None,
            task: None,
            accessed_by,
            first_seen: first_seen.unwrap_or(now),
            last_seen: last_seen.unwrap_or(now),
        });
    }

    // 3. Observed harness worktrees, merged by path.
    let mut gone = Vec::new();
    for stored in &input.stored {
        // Listed under exactly the stored path: clearing acts on the path as
        // listed, so it is never re-canonicalised (through a symlink that
        // replaced the checkout, say) into some other path.
        let path = PathBuf::from(&stored.path);
        // A row under h4rn's own subagent worktrees is derived live, and a row
        // whose repository itself is gone can never be cleared through git:
        // forget both (the store only; nothing on disk is touched).
        if path.starts_with(&input.subagents_root) || !Path::new(&stored.repo).is_dir() {
            gone.push(stored.path.clone());
            continue;
        }
        let exists = is_real_dir(&path);
        // A checkout whose directory is gone is still clearable while its own
        // registration remains: removing that registration is the only cleanup
        // left, and forgetting the row here would report it as untracked.
        // Only a successful snapshot that no longer names the path may forget
        // it; a failed snapshot keeps the row (exists: false) for a later try.
        if !exists {
            match registration_exists(
                Path::new(&stored.repo),
                &path,
                &mut registration_lists,
                &commons,
            ) {
                Some(true) => {}
                Some(false) => {
                    gone.push(stored.path.clone());
                    continue;
                }
                None => {}
            }
        }
        let accesses: Vec<WorktreeAccess> = stored
            .accesses
            .iter()
            .map(|access| {
                let thread_id = ThreadId::new(access.thread_id.clone());
                access_of(
                    infos.get(access.thread_id.as_str()).copied(),
                    &thread_id,
                    access.subagent_id.clone().map(SubagentId::new),
                    access.last_seen,
                )
            })
            .collect();
        match index.get(&path).copied() {
            Some(position) => {
                let entry = &mut entries[position];
                for access in accesses {
                    if entry
                        .accessed_by
                        .iter()
                        .all(|seen| seen.thread_id != access.thread_id)
                    {
                        entry.accessed_by.push(access);
                    }
                }
                entry.last_seen = entry.last_seen.max(stored.last_seen);
                if entry.origin != WorktreeOrigin::Subagent
                    && entry.accessed_by.iter().any(|access| access.working)
                {
                    entry.status = WorktreeStatus::Active;
                }
            }
            None => {
                let working = accesses.iter().any(|access| access.working);
                index.insert(path.clone(), entries.len());
                entries.push(TrackedWorktree {
                    path: path.to_string_lossy().into_owned(),
                    repo: stored.repo.clone(),
                    branch: stored.branch.clone(),
                    origin: WorktreeOrigin::Harness,
                    status: if working {
                        WorktreeStatus::Active
                    } else {
                        WorktreeStatus::Settled
                    },
                    exists,
                    dirty: None,
                    owned: false,
                    subagent_id: None,
                    task: None,
                    accessed_by: accesses,
                    first_seen: stored.first_seen,
                    last_seen: stored.last_seen,
                });
            }
        }
    }

    for entry in &mut entries {
        entry.exists = is_real_dir(Path::new(&entry.path));
        entry.dirty = if with_dirty && entry.exists {
            git::is_dirty(Path::new(&entry.path))
        } else {
            None
        };
    }

    entries.sort_by(|a, b| {
        status_rank(a.status)
            .cmp(&status_rank(b.status))
            .then(b.last_seen.cmp(&a.last_seen))
    });
    (entries, gone, thread_workspaces)
}

// ---------------------------------------------------------------------------
// Clearing
// ---------------------------------------------------------------------------

fn in_use_reason(entry: &TrackedWorktree) -> String {
    let titles: Vec<&str> = entry
        .accessed_by
        .iter()
        .filter(|access| access.working)
        .map(|access| access.title.as_str())
        .filter(|title| !title.is_empty())
        .collect();
    if titles.is_empty() {
        match &entry.task {
            Some(task) => format!("in use by subagent: {task}"),
            None => "in use".to_owned(),
        }
    } else {
        format!("in use by {}", titles.join(", "))
    }
}

/// What clearing one entry did.
#[derive(Debug)]
enum ClearOne {
    /// The checkout is gone (and its branch, when h4rn owned one).
    Removed,
    /// The checkout is gone but the owned branch could not be deleted.
    RemovedWithBranchFailure(String),
}

/// Deadline for each git command run while clearing one entry.
const CLEAR_GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The reason for a decision a partial registration snapshot cannot make
/// (blocked).
const INCOMPLETE_SNAPSHOT: &str = "could not read every worktree registration";

/// The common dir of `repo` (resolved once per clear through `commons`) and a
/// fresh registration snapshot taken with it, carrying the checkout→admin dir
/// mapping read in the same pass.
fn snapshot(
    repo: &Path,
    commons: &git::CommonDirs,
) -> Result<(PathBuf, git::RegistrationSnapshot), String> {
    let common = commons.get(repo, CLEAR_GIT_TIMEOUT)?;
    let registrations = git::registrations_in(repo, &common, CLEAR_GIT_TIMEOUT, commons)?;
    Ok((common, registrations))
}

/// The reason for a listed registration whose admin dir the snapshot's
/// mapping does not hold (forceable).
const REGISTRATION_LOST: &str = "the registration could no longer be located";

/// The reason for a missing checkout whose registration's index differs from
/// its `HEAD` (forceable).
const STAGED_CHANGES: &str = "has staged changes";

/// The admin dirs of the registration at `path`, from the mapping `snapshot`
/// carries (never a fresh walk of `<common>/worktrees`).
///
/// A complete mapping without the path is [`REGISTRATION_LOST`]; a timed-out
/// enumeration is its `git timed out after Ns` reason (blocked, like every
/// timeout); any other read failure is `unreadable`, the caller's own
/// forceable reason.
fn own_admin_dirs<'a>(
    snapshot: &'a git::RegistrationSnapshot,
    path: &Path,
    unreadable: &str,
) -> Result<&'a [PathBuf], String> {
    snapshot.admin_dirs(path).map_err(|lookup| match lookup {
        git::AdminLookup::NotFound => REGISTRATION_LOST.to_owned(),
        git::AdminLookup::TimedOut(message) => message,
        git::AdminLookup::Unreadable => unreadable.to_owned(),
    })
}

/// Refuses a missing checkout whose registration's index holds staged work.
/// Blocking.
///
/// With the checkout directory gone, git skips its cleanliness check and
/// deletes the admin dir, index included, so the index is compared with the
/// registration's `HEAD` first (see [`git::index_matches`]): its detached
/// commit, or its branch's current tip. Anything that differs, and anything
/// unknown (an unknown `HEAD`, an unborn branch, an index that cannot be
/// read, any git failure), is [`STAGED_CHANGES`]; a timeout is its own
/// (blocked) reason. A missing `index` file is clean: nothing is staged.
fn verify_nothing_staged(
    snapshot: &git::RegistrationSnapshot,
    path: &Path,
    registration: &git::Registration,
) -> Result<(), String> {
    let admins = own_admin_dirs(snapshot, path, STAGED_CHANGES)?;
    let treeish = match &registration.head {
        git::Head::Detached(sha) => sha.clone(),
        git::Head::Branch(branch) => format!("refs/heads/{branch}"),
        git::Head::Unknown => return Err(STAGED_CHANGES.into()),
    };
    for admin in admins {
        match git::index_matches(admin, &treeish, CLEAR_GIT_TIMEOUT) {
            Ok(true) => {}
            Ok(false) => return Err(STAGED_CHANGES.into()),
            Err(error) if git::timed_out(&error) => return Err(error),
            Err(_) => return Err(STAGED_CHANGES.into()),
        }
    }
    Ok(())
}

/// The reason for a checkout whose registration has a git operation in
/// progress (forceable).
const OPERATION_IN_PROGRESS: &str = "has a git operation in progress";

/// What git leaves in a worktree's own admin dir while an operation is
/// unfinished: a rebase (its autostash included), a merge, a cherry-pick, a
/// revert, a bisect, a sequencer run, or a merge's conflicted result.
const OPERATION_STATE: [&str; 8] = [
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "sequencer",
    "AUTO_MERGE",
];

/// Refuses a registered checkout (existing or missing) whose own admin dir
/// shows a git operation in progress. Blocking.
///
/// The admin dir is `<common>/worktrees/<id>` whose `gitdir` names `path`
/// (the main worktree is refused long before this), looked up in the mapping
/// `snapshot` carries (see [`own_admin_dirs`]): a registration the mapping no
/// longer holds is [`REGISTRATION_LOST`], a timed-out enumeration its timeout
/// reason. State is checked by presence only, never following a symlink:
/// whatever is there means the operation is unfinished, and removing the
/// registration would throw away its state (a rebase's autostash, say). An
/// admin dir that cannot be read is refused with the same (forceable) reason.
fn verify_no_operation(snapshot: &git::RegistrationSnapshot, path: &Path) -> Result<(), String> {
    let admins = own_admin_dirs(snapshot, path, OPERATION_IN_PROGRESS)?;
    for admin in admins {
        if !admin.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
            return Err(OPERATION_IN_PROGRESS.into());
        }
        for name in OPERATION_STATE {
            match admin.join(name).symlink_metadata() {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                // Present, or unknown: either way not provably idle.
                _ => return Err(OPERATION_IN_PROGRESS.into()),
            }
        }
    }
    Ok(())
}

/// Checks an existing checkout against one snapshot: it is registered at
/// exactly `path` and holds no other registered worktree.
///
/// Containment is a decision by absence (no registration lies inside), so a
/// snapshot that is not complete refuses: the unreadable registration may be
/// the nested one, or this checkout's own.
fn verify_registered(snapshot: &git::RegistrationSnapshot, path: &Path) -> Result<(), String> {
    if !snapshot.complete {
        return Err(INCOMPLETE_SNAPSHOT.into());
    }
    // `git worktree remove` acts on whatever registration git resolves the
    // path to; only exactly this path's own registration may be removed.
    if exact_registration(snapshot, path).is_none() {
        return Err(NOT_REGISTERED.into());
    }
    // `git worktree remove` would leave a nested registration pointing into a
    // directory that no longer exists, or delete another repository's clone
    // outright; neither is ever forceable.
    if let Some(inner) = contained_worktree(snapshot, path) {
        return Err(format!("contains another worktree at {}", inner.display()));
    }
    Ok(())
}

/// A missing checkout's own registration, from one snapshot.
///
/// `Ok(None)` means none is left, which only a complete snapshot can say;
/// otherwise the absence is unknown and refused.
fn own_registration<'a>(
    snapshot: &'a git::RegistrationSnapshot,
    path: &Path,
) -> Result<Option<&'a git::Registration>, String> {
    match exact_registration(snapshot, path) {
        Some(registration) => Ok(Some(registration)),
        None if snapshot.complete => Ok(None),
        None => Err(INCOMPLETE_SNAPSHOT.into()),
    }
}

/// Another registered worktree nested inside `path`, from one snapshot.
fn contained_worktree(registrations: &[git::Registration], path: &Path) -> Option<PathBuf> {
    registrations
        .iter()
        .find(|registration| registration.path != path && registration.path.starts_with(path))
        .map(|registration| registration.path.clone())
}

/// The registration for exactly `path`, from one snapshot.
///
/// Exact: `path` is the listed path a clear acts on, never re-canonicalised.
fn exact_registration<'a>(
    registrations: &'a [git::Registration],
    path: &Path,
) -> Option<&'a git::Registration> {
    registrations
        .iter()
        .find(|registration| registration.path == path)
}

/// The reason for a path that is not exactly a registered worktree (blocked).
const NOT_REGISTERED: &str = "not a registered worktree";

/// What is at a selected path: `Ok(true)` a real directory, `Ok(false)`
/// nothing at all.
///
/// Git resolves every path it is given through symlinks, so a path that is a
/// symlink (or a file), or that resolves elsewhere through a symlinked parent,
/// is never handed to it: git would act on whatever the symlink points at,
/// which may be another worktree entirely. Such a path is refused as
/// [`NOT_REGISTERED`].
fn checkout_at(path: &Path) -> Result<bool, String> {
    let exists = match path.symlink_metadata() {
        Ok(meta) if meta.is_dir() => true,
        Ok(_) => return Err(NOT_REGISTERED.into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(format!("could not check the checkout: {error}")),
    };
    if git::norm(path) != path {
        return Err(NOT_REGISTERED.into());
    }
    Ok(exists)
}

/// Whether the selected row was an owned checkout with nothing left on disk or
/// in its branch: a directory at its path now is someone else's new checkout.
fn recreated(entry: &TrackedWorktree, exists: bool) -> bool {
    exists && !entry.exists && entry.owned && entry.branch.is_none()
}

/// An owned branch and the way preflight verified it.
enum BranchDelete {
    /// Delete only while the ref still points at this sha.
    Verified { sha: String },
    /// The ref is symbolic; leave it, and its target, alone.
    Symbolic,
}

/// What preflight verified, for removal to act on.
struct Preflight {
    /// The owned `h4rn/` branch to handle once the checkout is gone.
    branch: Option<BranchDelete>,
}

/// What the removal phase saw, for [`execute`] to compare against.
///
/// Prepared before the (async) live activity check, so the destructive command
/// can run immediately after it.
struct Prepared {
    /// Whether the checkout's directory existed when [`prepare`] checked it.
    was_dir: bool,
}

/// The sha `refs/heads/<branch>` points at; `None` when the branch is gone.
///
/// `for-each-ref` prints nothing for an absent ref and fails only when git
/// itself fails, so an error is never mistaken for a missing branch. It also
/// skips a broken ref with only a warning on stderr: no matching line plus
/// anything on stderr is an error too, never "absent".
fn branch_tip(repo: &Path, branch: &str) -> Result<Option<String>, String> {
    let reference = format!("refs/heads/{branch}");
    let (out, stderr) = git::run_capture_with_timeout(
        repo,
        [
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            &reference,
        ],
        CLEAR_GIT_TIMEOUT,
    )?;
    let tip = out.lines().find_map(|line| {
        let (sha, name) = line.split_once(' ')?;
        (name == reference).then(|| sha.to_owned())
    });
    if tip.is_none() && !stderr.is_empty() {
        return Err(stderr);
    }
    Ok(tip)
}

/// Whether `refs/heads/<branch>` is a symbolic ref.
fn branch_is_symbolic(repo: &Path, branch: &str) -> Result<bool, String> {
    match git::run_with_timeout(
        repo,
        ["symbolic-ref", "-q", &format!("refs/heads/{branch}")],
        CLEAR_GIT_TIMEOUT,
    ) {
        Ok(_) => Ok(true),
        Err(error) if git::timed_out(&error) => Err(error),
        Err(_) => Ok(false),
    }
}

/// Whether the verified `sha` is fully merged into `HEAD`.
fn branch_is_merged(repo: &Path, sha: &str) -> Result<bool, String> {
    match git::run_with_timeout(
        repo,
        ["merge-base", "--is-ancestor", sha, "HEAD"],
        CLEAR_GIT_TIMEOUT,
    ) {
        Ok(_) => Ok(true),
        Err(error) if git::timed_out(&error) => Err(error),
        Err(_) => Ok(false),
    }
}

/// The owned branch, verified down to the sha preflight must delete.
///
/// A symbolic ref is recorded instead of resolved: it is never deleted, and the
/// branch part is reported as skipped so the user sees which ref was left
/// behind (its target is untouched too). Resolving to a sha first means an
/// unmerged branch is refused before the checkout is touched, and the sha then
/// binds the deletion to exactly what was checked.
fn verified_branch(repo: &Path, branch: &str) -> Result<Option<BranchDelete>, String> {
    if branch_is_symbolic(repo, branch)? {
        return Ok(Some(BranchDelete::Symbolic));
    }
    let tip = branch_tip(repo, branch).map_err(|error| {
        if git::timed_out(&error) {
            error
        } else {
            format!("branch {branch} could not be checked: {error}")
        }
    })?;
    let Some(sha) = tip else {
        return Ok(None);
    };
    if !branch_is_merged(repo, &sha)? {
        return Err(format!("branch {branch} has unmerged commits"));
    }
    Ok(Some(BranchDelete::Verified { sha }))
}

/// Whether any branch, remote-tracking branch or tag reaches `sha`.
fn any_ref_contains(repo: &Path, sha: &str) -> Result<bool, String> {
    let refs = git::run_with_timeout(
        repo,
        [
            "for-each-ref",
            "--contains",
            sha,
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
        CLEAR_GIT_TIMEOUT,
    )?;
    Ok(!refs.trim().is_empty())
}

/// Checks an existing checkout is still on its listed branch and holds no
/// detached commit that no ref reaches.
///
/// Preflight runs this, and removal runs it again immediately before
/// `git worktree remove`, so a checkout detached or replaced after preflight is
/// still caught. Both callers report the same reasons.
fn verify_checkout(path: &Path, repo: &Path, branch: Option<&str>) -> Result<(), String> {
    match git::run_with_timeout(
        path,
        ["symbolic-ref", "--short", "-q", "HEAD"],
        CLEAR_GIT_TIMEOUT,
    ) {
        Ok(current) => {
            if let Some(branch) = branch
                && current != *branch
            {
                return Err(format!("checkout is no longer on {branch}"));
            }
        }
        Err(error) if git::timed_out(&error) => return Err(error),
        // A detached HEAD can hold commits no branch reaches; removing the
        // checkout would leave them unreachable.
        Err(_) => {
            let sha = git::run_with_timeout(path, ["rev-parse", "HEAD"], CLEAR_GIT_TIMEOUT)?;
            if !any_ref_contains(repo, &sha)? {
                return Err("detached HEAD has commits on no branch".into());
            }
        }
    }
    Ok(())
}

/// Checks a registration whose directory is gone for detached work no ref
/// reaches; removing it would lose the only name for that commit. A `HEAD`
/// that could not be read is unknown, never "nothing to protect".
fn verify_missing_registration(
    repo: &Path,
    registration: &git::Registration,
) -> Result<(), String> {
    match &registration.head {
        git::Head::Branch(_) => Ok(()),
        git::Head::Detached(sha) => {
            if any_ref_contains(repo, sha)? {
                Ok(())
            } else {
                Err("detached HEAD has commits on no branch".into())
            }
        }
        git::Head::Unknown => Err(UNKNOWN_HEAD.into()),
    }
}

/// The reason for an owned checkout whose selected row had no directory and no
/// branch left, but whose directory exists at prepare or execute (never
/// forceable).
const RECREATED: &str = "the checkout was recreated during the clear";

/// The reason for a missing checkout whose registration `HEAD` is unknown.
const UNKNOWN_HEAD: &str = "could not read the registration's HEAD";

/// A rebase or bisect in progress in any worktree of the repository whose
/// common dir is `common` that names `branch`.
///
/// The main git dir is `<common>` and every linked worktree's admin dir is
/// under `<common>/worktrees`, so both are inspected for the files git writes
/// while a rebase or bisect is running.
fn branch_in_progress(common: &Path, branch: &str) -> Result<Option<String>, String> {
    let mut dirs = vec![common.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(common.join("worktrees")) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    let reference = format!("refs/heads/{branch}");
    // Git writes the checked-out ref to `head-name`, `update-refs` lines and
    // `BISECT_START`; matching must be by whole line or whole token, never a
    // substring, or a rebase on `h4rn/sa-abc2` would block `h4rn/sa-abc`.
    let needles = [reference.as_str(), branch];
    for dir in dirs {
        for name in ["rebase-merge/head-name", "rebase-apply/head-name"] {
            if file_has_line(&dir.join(name), &needles) {
                return Ok(Some("it is in use by a rebase".into()));
            }
        }
        if file_has_token(&dir.join("rebase-merge/update-refs"), &needles) {
            return Ok(Some("it is in use by a rebase".into()));
        }
        if file_has_line(&dir.join("BISECT_START"), &needles) {
            return Ok(Some("it is in use by a bisect".into()));
        }
    }
    Ok(None)
}

/// One rebase or bisect state file: `None` when it does not exist, `Some(None)`
/// when something is there but cannot be read safely, `Some(Some(text))`
/// otherwise.
///
/// Only a regular file of at most 4 KiB is read (see [`git::read_bounded`]);
/// a FIFO, a symlink, a directory, an oversized or unreadable file is
/// "something there, unknown", which callers read as in progress.
fn state_file(path: &Path) -> Option<Option<String>> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        _ => {
            Some(git::read_bounded(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
        }
    }
}

/// Whether `path` exists and has a whole (trimmed) line equal to a needle; a
/// state file that cannot be read safely counts as a match.
fn file_has_line(path: &Path, needles: &[&str]) -> bool {
    state_file(path).is_some_and(|text| {
        text.is_none_or(|text| text.lines().any(|line| needles.contains(&line.trim())))
    })
}

/// Whether `path` exists and has a whole whitespace-separated token equal to a
/// needle; a state file that cannot be read safely counts as a match.
fn file_has_token(path: &Path, needles: &[&str]) -> bool {
    state_file(path).is_some_and(|text| {
        text.is_none_or(|text| {
            text.split_whitespace()
                .any(|token| needles.contains(&token))
        })
    })
}

/// The standard wording for a branch the checkout removal left behind.
fn branch_delete_failure(branch: &str, why: &str) -> String {
    format!("checkout removed, but branch {branch} could not be deleted: {why}")
}

/// `git worktree remove [--force] <path>`, run in `repo`; never prunes.
///
/// Git's own dirtiness refusal is the backstop for anything written after
/// preflight. It runs `git status` under the user's config, so the config is
/// overridden for that call: untracked files and submodules always count.
/// [`git::SAFETY_CONFIG`] keeps a filesystem monitor or the untracked cache
/// from hiding anything from that `git status` either.
fn worktree_remove(repo: &Path, path: &Path, force: bool) -> Result<(), String> {
    let mut args: Vec<&OsStr> = git::SAFETY_CONFIG
        .into_iter()
        .chain([
            "-c",
            "status.showUntrackedFiles=all",
            "-c",
            "diff.ignoreSubmodules=none",
            "worktree",
            "remove",
        ])
        .map(OsStr::new)
        .collect();
    if force {
        args.push(OsStr::new("--force"));
    }
    args.push(path.as_os_str());
    git::run_with_timeout(repo, args, CLEAR_GIT_TIMEOUT).map(|_| ())
}

/// The reason a nested-checkout scan could not finish (forceable).
const NESTED_SCAN_UNKNOWN: &str = "could not check for nested checkouts";

#[cfg(test)]
thread_local! {
    /// Test-only: the nested-checkout scan's deadline on this thread.
    pub(crate) static NESTED_SCAN_LIMIT: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// The nested-checkout scan's deadline: the per-command clear deadline.
fn nested_scan_limit() -> Duration {
    #[cfg(test)]
    if let Some(limit) = NESTED_SCAN_LIMIT.with(std::cell::Cell::get) {
        return limit;
    }
    CLEAR_GIT_TIMEOUT
}

/// Whether a nested `.git` file's target is a submodule repository of this
/// one.
///
/// Git keeps a submodule's repository under the superproject's common dir:
/// `<common>/modules/...`, or `<common>/worktrees/<id>/modules/...` for a
/// submodule initialised in a linked worktree. A `worktrees` component after
/// `modules` is a linked worktree *of* that submodule, a separate checkout. A
/// gitfile pointing anywhere else (or one that cannot be read) is another
/// repository's checkout.
fn is_own_submodule(gitfile: &Path, common: &Path) -> bool {
    let Some(target) = git::gitfile_target(gitfile) else {
        return false;
    };
    let under_modules = match target.strip_prefix(common.join("modules")) {
        Ok(rest) => rest,
        Err(_) => {
            let Ok(rest) = target.strip_prefix(common.join("worktrees")) else {
                return false;
            };
            let mut parts = rest.components();
            if parts.next().is_none()
                || parts
                    .next()
                    .is_none_or(|part| part.as_os_str() != "modules")
            {
                return false;
            }
            parts.as_path()
        }
    };
    !under_modules.as_os_str().is_empty()
        && under_modules
            .components()
            .all(|part| part.as_os_str() != "worktrees")
}

/// The first checkout nested inside `root`, of any repository.
///
/// Walks the tree without following symlinks and looks for a `.git` file or
/// directory other than the checkout's own top-level one, so an ignored clone
/// of an unrelated repository is found too. A `.git` file is a submodule of
/// this checkout, not a hit, only when both hold: its directory is a gitlink
/// (mode `160000`) in the checkout's index (one `git ls-files -s`, run the
/// first time a nested `.git` file is met and bounded by the time left), and
/// its `gitdir:` is a submodule repository under this repository's common dir
/// (`common`, see [`is_own_submodule`]). Git itself refuses (without force) to
/// remove a checkout holding a submodule. Bounded by `limit`, checked for every directory entry
/// and once more before answering "nothing found"; running out of time, or a
/// directory that cannot be read, is "could not check", never "nothing
/// found".
fn nested_checkout(root: &Path, common: &Path, limit: Duration) -> Result<Option<PathBuf>, String> {
    let deadline = Instant::now() + limit;
    let expired = || Instant::now() >= deadline;
    let mut stack = vec![root.to_path_buf()];
    // Checkouts whose index decides which nested `.git` files are submodules:
    // the checkout itself, then every submodule already found exempt, so a
    // submodule of a submodule is judged by its own superproject's index.
    // The walk is depth-first and a directory's `.git` is seen before its
    // subdirectories are popped, so a parent is exempt before its children.
    let mut exempt: Vec<PathBuf> = vec![root.to_path_buf()];
    // Asked for lazily, per superproject: most checkouts hold no nested `.git`
    // at all. A failed lookup proves no gitlink, so the nested checkout is
    // refused.
    let mut gitlinks: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|_| NESTED_SCAN_UNKNOWN.to_owned())?;
        for entry in entries {
            if expired() {
                return Err(NESTED_SCAN_UNKNOWN.into());
            }
            let entry = entry.map_err(|_| NESTED_SCAN_UNKNOWN.to_owned())?;
            // `DirEntry::file_type` does not follow symlinks.
            let file_type = entry
                .file_type()
                .map_err(|_| NESTED_SCAN_UNKNOWN.to_owned())?;
            if entry.file_name() == ".git" {
                if dir == root {
                    continue;
                }
                if file_type.is_file() {
                    // The nearest enclosing checkout (the root or an exempt
                    // submodule) whose index may list `dir` as a gitlink.
                    let parent = exempt
                        .iter()
                        .filter(|candidate| dir != **candidate && dir.starts_with(candidate))
                        .max_by_key(|candidate| candidate.components().count())
                        .cloned()
                        .unwrap_or_else(|| root.to_path_buf());
                    let links = gitlinks.entry(parent.clone()).or_insert_with(|| {
                        git::gitlinks(&parent, deadline.saturating_duration_since(Instant::now()))
                            .unwrap_or_default()
                    });
                    let gitlink = dir
                        .strip_prefix(&parent)
                        .is_ok_and(|relative| links.contains(relative));
                    if gitlink && is_own_submodule(&entry.path(), common) {
                        exempt.push(dir.clone());
                        continue;
                    }
                }
                return Ok(Some(dir));
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    if expired() {
        return Err(NESTED_SCAN_UNKNOWN.into());
    }
    Ok(None)
}

/// The reason for a checkout holding files git hides from `status`.
const HIDDEN_FILES: &str = "has files hidden from git status";

/// The checks for the checkout's state as it is now. Blocking.
///
/// An existing checkout must be registered at exactly its path and hold no
/// registered worktree (both from `snapshot`, which must be complete; see
/// [`verify_registered`]), and, when `thorough`, no
/// nested checkout of any
/// repository. Without force it must still be on its listed branch with no
/// unreachable detached work and no file git hides from `status`
/// (assume-unchanged or skip-worktree). For a missing checkout, a partial
/// snapshot without its registration is refused (see [`own_registration`]);
/// without force, its own registration's `HEAD` must be known, a detached one
/// reachable from some ref, and its index must hold nothing staged (see
/// [`verify_nothing_staged`]).
///
/// Without force, a registered checkout (existing or missing) must have no git
/// operation in progress in its own admin dir (see [`verify_no_operation`]).
///
/// `thorough` gates only the nested-checkout scan, a whole tree walk: it runs
/// in [`prepare`] and again on a state flip, never in the final pass before an
/// unchanged checkout's removal, which must stay short. The hidden-file check
/// is one `ls-files` and runs in every pass, as does the operation check.
/// `common` is the repository's common dir, resolved once per clear.
fn verify_current(
    entry: &TrackedWorktree,
    snapshot: &git::RegistrationSnapshot,
    common: &Path,
    exists: bool,
    force: bool,
    thorough: bool,
) -> Result<(), String> {
    let path = PathBuf::from(&entry.path);
    let repo = PathBuf::from(&entry.repo);
    if exists {
        verify_registered(snapshot, &path)?;
        if thorough && let Some(nested) = nested_checkout(&path, common, nested_scan_limit())? {
            return Err(format!(
                "contains another git checkout at {}",
                nested.display()
            ));
        }
        if !force {
            verify_no_operation(snapshot, &path)?;
            verify_checkout(&path, &repo, entry.branch.as_deref())?;
            // An edit to an assume-unchanged or skip-worktree file never shows
            // in `git status`, so neither our dirty check nor git's removal
            // backstop would see it.
            match git::hidden_file(&path, CLEAR_GIT_TIMEOUT) {
                Ok(None) => {}
                Ok(Some(_)) => return Err(HIDDEN_FILES.into()),
                Err(error) if git::timed_out(&error) => return Err(error),
                Err(_) => return Err("could not check for uncommitted changes".into()),
            }
        }
    } else if let Some(registration) = own_registration(snapshot, &path)?
        && !force
    {
        verify_no_operation(snapshot, &path)?;
        // Only the registration is left. Its HEAD may hold a commit that no
        // branch, remote-tracking branch or tag reaches; deleting the
        // registration would lose the only name for that work.
        verify_missing_registration(&repo, registration)?;
        // Removing it also deletes its index: staged work there is lost.
        verify_nothing_staged(snapshot, &path, registration)?;
    }
    Ok(())
}

/// Runs one entry's clear: preflight, prepare, live re-check, destructive act.
///
/// The order matters. Preflight refuses what can be refused cheaply; the test
/// hook then runs; `prepare` re-runs every check that can go stale behind
/// preflight (the checkout's branch, containment, the nested-checkout scan,
/// hidden files, detached reachability); only then does the async activity
/// check run; and `execute` takes the registration snapshot the removal acts
/// on and launches the destructive command right after its short re-check.
///
/// `after_preflight` runs between preflight and prepare; a test uses it to
/// change the checkout in that window and prove prepare still catches it.
/// `bulk` makes the live check leave out an entry any thread works in (see
/// [`Inner::still_active`]).
async fn run_clear_entry<F, Fut>(
    inner: &Arc<Inner>,
    entry: TrackedWorktree,
    force: bool,
    bulk: bool,
    commons: Arc<git::CommonDirs>,
    after_preflight: F,
) -> Result<ClearOne, String>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let path = entry.path.clone();
    let preflight_entry = entry.clone();
    let preflight_commons = Arc::clone(&commons);
    let plan = match tokio::task::spawn_blocking(move || {
        preflight(&preflight_entry, force, &preflight_commons)
    })
    .await
    {
        Ok(Ok(plan)) => plan,
        Ok(Err(reason)) => return Err(reason),
        Err(error) => {
            tracing::warn!(%error, %path, "worktree preflight task failed");
            return Err("the clear task failed".into());
        }
    };
    after_preflight(path.clone()).await;
    let prepare_entry = entry.clone();
    let prepare_commons = Arc::clone(&commons);
    let prepared =
        match tokio::task::spawn_blocking(move || prepare(&prepare_entry, force, &prepare_commons))
            .await
        {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(reason)) => return Err(reason),
            Err(error) => {
                tracing::warn!(%error, %path, "worktree prepare task failed");
                return Err("the clear task failed".into());
            }
        };
    match inner.still_active(&entry, bulk).await {
        Some(LiveUse::Active(active)) => return Err(in_use_reason(&active)),
        Some(LiveUse::ThreadWorkspace) => return Err(THREAD_WORKSPACE.into()),
        None => {}
    }
    let data_dir = inner.data_dir.clone();
    match tokio::task::spawn_blocking(move || {
        execute(&entry, force, &data_dir, &plan, &prepared, &commons)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(%error, %path, "worktree clear task failed");
            Err("the clear task failed".into())
        }
    }
}

/// Whether an entry may be removed, without touching the disk. Blocking.
///
/// Every git call is bounded by [`CLEAR_GIT_TIMEOUT`]; a timeout is a skip with
/// that reason. A directory that vanished between the listing and now still
/// passes (a removed harness checkout leaves only the branch), after its own
/// registration has been checked for detached work no ref reaches. The owned
/// branch is resolved to a sha here so removal deletes exactly the tip this
/// phase verified.
fn preflight(
    entry: &TrackedWorktree,
    force: bool,
    commons: &git::CommonDirs,
) -> Result<Preflight, String> {
    let path = PathBuf::from(&entry.path);
    if entry.repo.is_empty() {
        return Err("its repository could not be found".into());
    }
    let repo = PathBuf::from(&entry.repo);
    if git::norm(&path) == git::norm(&repo) {
        return Err("is the repository's main worktree".into());
    }
    let exists = checkout_at(&path)?;
    // One snapshot answers the exact-registration check, the nested-worktree
    // check and the missing checkout's own registration.
    let (_, registrations) = snapshot(&repo, commons)?;
    // Git would happily remove a checkout that holds another worktree, leaving
    // the nested registration pointing into a directory that no longer exists.
    // That checkout is never removed, with or without force. Only an existing
    // checkout can hold a nested one: when the selected directory is gone there
    // is no working tree to protect, and dropping its own registration must
    // leave the nested registration alone.
    if exists {
        verify_registered(&registrations, &path)?;
    } else {
        own_registration(&registrations, &path)?;
    }
    let mut branch = None;
    if !force {
        // A paused rebase, merge, cherry-pick, revert or bisect keeps its
        // state (a rebase's autostash, say) in the registration's admin dir;
        // removing the registration would throw it away.
        if exists || exact_registration(&registrations, &path).is_some() {
            verify_no_operation(&registrations, &path)?;
        }
        // The listing is only a snapshot: a turn may have written since. Ask
        // git again here, and never read "could not ask" as clean.
        // `status --porcelain` with untracked files always counted, whatever
        // the user's config says, and no filesystem monitor or untracked cache
        // trusted. Anything written after this is caught by git's own refusal
        // in `worktree_remove`.
        match git::status(&path, CLEAR_GIT_TIMEOUT) {
            Ok(out) if !out.is_empty() => return Err("has uncommitted changes".into()),
            Ok(_) => {}
            Err(error) if git::timed_out(&error) => return Err(error),
            Err(_) if exists => {
                return Err("could not check for uncommitted changes".into());
            }
            Err(_) => {}
        }
        if exists {
            // The checkout at this path must still be the one the listing
            // described. A replacement checkout recreated here on another
            // branch would otherwise be removed as if it were that entry.
            verify_checkout(&path, &repo, entry.branch.as_deref())?;
        } else if let Some(registration) = exact_registration(&registrations, &path) {
            // Only the registration is left. Its HEAD may hold a commit that no
            // branch, remote-tracking branch or tag reaches; deleting the
            // registration would lose the only name for that work.
            verify_missing_registration(&repo, registration)?;
            // Git skips its cleanliness check for a missing checkout and
            // deletes the admin dir, index included.
            verify_nothing_staged(&registrations, &path, registration)?;
        }
        if entry.owned
            && let Some(name) = &entry.branch
            && name.starts_with("h4rn/")
        {
            branch = verified_branch(&repo, name)?;
        }
    }
    Ok(Preflight { branch })
}

/// The removal phase's own verification. Blocking.
///
/// Takes its own snapshot, then re-runs the checks that can go stale between
/// preflight and the destructive command: a checkout replaced on another
/// branch or detached with unreachable work, a nested worktree or checkout
/// created in the meantime (the nested-checkout scan runs here), files hidden
/// from `status`, and a missing checkout's registration `HEAD`. An owned row
/// with no branch and no directory whose directory exists now is refused as
/// recreated. The live activity check runs after this, and only then does
/// [`execute`] touch the disk.
fn prepare(
    entry: &TrackedWorktree,
    force: bool,
    commons: &git::CommonDirs,
) -> Result<Prepared, String> {
    let repo = PathBuf::from(&entry.repo);
    let (common, registrations) = snapshot(&repo, commons)?;
    let was_dir = checkout_at(Path::new(&entry.path))?;
    if recreated(entry, was_dir) {
        return Err(RECREATED.into());
    }
    verify_current(entry, &registrations, &common, was_dir, force, true)?;
    Ok(Prepared { was_dir })
}

/// Removes one entry from disk and settles its owned branch. Blocking.
///
/// Runs only after [`prepare`] passed and the live re-check found nothing using
/// the entry. Immediately before `git worktree remove` it takes a fresh
/// registration snapshot and then reads whether the directory exists. For an
/// existing checkout that prepare also saw, it re-checks containment, the
/// listed branch, detached reachability and hidden files against that
/// snapshot; for a missing checkout, its registration's `HEAD` (unknown or
/// detached and unreachable) from that snapshot; if the directory appeared or
/// vanished since prepare, every check for the state that exists now (see
/// [`verify_current`]), and an owned checkout with no branch whose selected
/// row had no directory is refused outright if one is there now. A path that
/// is not exactly a registered checkout (a symlink there, say) is never handed
/// to git (see [`checkout_at`]). A missing checkout with no
/// registration in that snapshot is reported removed only because both were
/// read after the activity check and the snapshot is complete: no directory
/// and no registration remain.
/// The branch delete that follows re-checks everything from another fresh
/// snapshot (see [`delete_owned_branch`]). Every git call is bounded by
/// [`CLEAR_GIT_TIMEOUT`]; a timeout is a skip with that reason, or a branch
/// failure once the checkout is already gone.
fn execute(
    entry: &TrackedWorktree,
    force: bool,
    data_dir: &Path,
    plan: &Preflight,
    prepared: &Prepared,
    commons: &git::CommonDirs,
) -> Result<ClearOne, String> {
    let path = PathBuf::from(&entry.path);
    let repo = PathBuf::from(&entry.repo);
    // The snapshot first, then the directory: whatever the snapshot does not
    // show (a nested worktree, the checkout's own registration) was registered
    // after this point, and the directory check below is newer still.
    let (common, registrations) = snapshot(&repo, commons)?;
    let exists = checkout_at(&path)?;
    let flipped = exists != prepared.was_dir;
    // The selected row was an owned checkout with no branch and no directory,
    // so nothing was on disk to protect; a directory there now is someone
    // else's new checkout, whatever force says.
    if recreated(entry, exists) {
        return Err(RECREATED.into());
    }
    // Every pass checks the state that exists now against this snapshot: a
    // missing checkout's registration (already missing at prepare or just
    // vanished) gets its HEAD checks here too, so a ref deleted since prepare
    // is seen. Only a flip repeats the nested-checkout scan.
    verify_current(entry, &registrations, &common, exists, force, flipped)?;
    if exists {
        // Git's own remove semantics: without `--force` it refuses a checkout
        // with modifications, which is what we want. Ignored files go with the
        // checkout either way; that is git's contract.
        if let Err(error) = worktree_remove(&repo, &path, force) {
            if locked_worktree(&error) {
                return Err("is locked; unlock it first".into());
            }
            // Git can find work our own checks did not (a fresh write, a
            // submodule). Report it as the forceable dirty reason so the UI
            // offers the same retry as any other uncommitted work.
            if !force && error.contains("contains modified or untracked files") {
                return Err("has uncommitted changes".into());
            }
            // Submodules of this repository are not foreign checkouts (the
            // nested scan lets them through); git itself refuses to remove a
            // checkout holding one unless forced.
            if !force && error.contains("containing submodules cannot be moved or removed") {
                return Err("contains submodules".into());
            }
            return Err(error);
        }
    } else if let Some(registration) = exact_registration(&registrations, &path) {
        // The directory is already gone (the supervisor removed it, or the user
        // did) but its own registration may still be there. Drop exactly that
        // registration; never prune the repository, which would also forget a
        // worktree whose directory is only temporarily away. A failure is a
        // reported skip rather than a removal: the row is still clearable.
        if registration.locked {
            return Err("is locked; unlock it first".into());
        }
        // A forced clear may need `--force` for a prunable registration; the
        // plain form is tried after in case git accepts only that.
        let removed = if force {
            worktree_remove(&repo, &path, true).or_else(|_| worktree_remove(&repo, &path, false))
        } else {
            worktree_remove(&repo, &path, false)
        };
        removed.map_err(|error| {
            if locked_worktree(&error) {
                "is locked; unlock it first".to_owned()
            } else {
                format!("registration left in place: {error}")
            }
        })?;
    }
    let mut branch_failure = None;
    if entry.owned
        && let Some(branch) = &entry.branch
        && branch.starts_with("h4rn/")
    {
        // The checkout is gone (or was never there), so every failure from
        // here on is a left-behind branch, timeouts included.
        let exists = match branch_tip(&repo, branch) {
            Ok(tip) => tip.is_some(),
            Err(error) => {
                return Ok(ClearOne::RemovedWithBranchFailure(format!(
                    "checkout removed, but branch {branch} could not be checked: {error}"
                )));
            }
        };
        if exists {
            branch_failure = delete_owned_branch(&repo, branch, plan, force, commons)?;
        }
    }
    if let Some(subagent_id) = &entry.subagent_id {
        // `remove_dir` only succeeds on an empty directory, which is the point.
        let _ = std::fs::remove_dir(data_dir.join("subagents").join(subagent_id.as_str()));
    }
    match branch_failure {
        Some(reason) => Ok(ClearOne::RemovedWithBranchFailure(reason)),
        None => Ok(ClearOne::Removed),
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only: runs once on this thread between the final branch checks
    /// and `update-ref`, so a test can move the ref in that window.
    pub(crate) static BEFORE_UPDATE_REF: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

/// Whether git refused because the worktree is locked.
fn locked_worktree(error: &str) -> bool {
    error.contains("cannot remove a locked working tree") || error.contains("is locked")
}

/// Deletes one owned `h4rn/` branch after its checkout is gone. Blocking.
///
/// An ordinary clear re-checks everything immediately before `update-ref`,
/// from a fresh registration snapshot: occupancy (refused when the snapshot is
/// not complete, since then no holder proves nothing, or when a registration
/// other than a bare main one has an unknown `HEAD`), an in-progress rebase or
/// bisect, and the verified sha's ancestry in `HEAD`. A forced clear keeps
/// git's own `branch -D`. `Err` is a forceable skip; `Ok(Some(reason))` is a
/// branch left in place that force cannot fix.
fn delete_owned_branch(
    repo: &Path,
    branch: &str,
    plan: &Preflight,
    force: bool,
    commons: &git::CommonDirs,
) -> Result<Option<String>, String> {
    if force {
        // Forced clears keep git's own command; it still refuses a branch
        // checked out in any worktree.
        return match git::run_with_timeout(repo, ["branch", "-D", branch], CLEAR_GIT_TIMEOUT) {
            Ok(_) => Ok(None),
            Err(error) => {
                tracing::warn!(%error, %branch, "could not delete the subagent branch");
                Ok(Some(branch_delete_failure(branch, &error)))
            }
        };
    }
    // The checkout just went away, so a fresh snapshot no longer contains its
    // registration but does show any worktree that took the branch over.
    let (common, registrations) = match snapshot(repo, commons) {
        Ok(found) => found,
        Err(error) => return Ok(Some(branch_delete_failure(branch, &error))),
    };
    match plan.branch.as_ref() {
        // The branch did not exist at preflight. If it does now, someone made
        // it after the checkout was removed; deleting it is not ours to do.
        None => Ok(Some(format!(
            "checkout removed, but branch {branch} was left in place"
        ))),
        Some(BranchDelete::Symbolic) => Ok(Some(format!("branch {branch} is a symbolic ref"))),
        Some(BranchDelete::Verified { sha }) => {
            if let Some(holder) = registrations
                .iter()
                .find(|registration| registration.branch() == Some(branch))
            {
                return Ok(Some(branch_delete_failure(
                    branch,
                    &format!("it is checked out in {}", holder.path.display()),
                )));
            }
            // No holder in a partial snapshot proves nothing: the unreadable
            // registration may have the branch checked out.
            if !registrations.complete {
                return Ok(Some(format!(
                    "checkout removed, but branch {branch} could not be checked: the worktree list \
                     is incomplete"
                )));
            }
            // Likewise a registration whose HEAD could not be read may be the
            // one holding the branch. A bare repository's main registration
            // has no working tree and never holds a branch, so its (always
            // unknown) HEAD proves nothing either way.
            if let Some(unknown) = registrations
                .iter()
                .find(|registration| !registration.bare && registration.head == git::Head::Unknown)
            {
                return Ok(Some(format!(
                    "checkout removed, but branch {branch} could not be checked: the HEAD of {} \
                     could not be read",
                    unknown.path.display()
                )));
            }
            match branch_in_progress(&common, branch) {
                Ok(Some(why)) => return Ok(Some(branch_delete_failure(branch, &why))),
                Ok(None) => {}
                Err(error) => return Ok(Some(branch_delete_failure(branch, &error))),
            }
            // The repository's HEAD can move behind an ordinary clear. If the
            // verified tip is no longer an ancestor, the branch gained work
            // that only force may discard.
            match branch_is_merged(repo, sha) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(format!("branch {branch} has unmerged commits"));
                }
                Err(error) => return Ok(Some(branch_delete_failure(branch, &error))),
            }
            #[cfg(test)]
            if let Some(hook) = BEFORE_UPDATE_REF.with(|hook| hook.borrow_mut().take()) {
                hook();
            }
            if let Err(error) = git::run_with_timeout(
                repo,
                [
                    "update-ref",
                    "--no-deref",
                    "-d",
                    &format!("refs/heads/{branch}"),
                    sha,
                ],
                CLEAR_GIT_TIMEOUT,
            ) {
                // The delete may have raced another writer. Re-read the ref:
                // gone means the branch is gone; a new tip already merged
                // into HEAD changed during the clear, and an unmerged one is
                // unmerged work (both forceable); anything else is an
                // ordinary branch failure.
                return match branch_tip(repo, branch) {
                    Ok(None) => Ok(None),
                    Ok(Some(current)) if current != *sha => {
                        match branch_is_merged(repo, &current) {
                            Ok(true) => Err(format!("branch {branch} changed during the clear")),
                            Ok(false) => Err(format!("branch {branch} has unmerged commits")),
                            Err(read_error) => Ok(Some(branch_delete_failure(branch, &read_error))),
                        }
                    }
                    Ok(Some(_)) => Ok(Some(branch_delete_failure(branch, &error))),
                    Err(read_error) => Ok(Some(branch_delete_failure(branch, &read_error))),
                };
            }
            Ok(None)
        }
    }
}

/// The entries a clear request targets, plus requested paths it cannot act on.
struct Selection<'a> {
    targets: Vec<&'a TrackedWorktree>,
    skipped: Vec<SkippedWorktree>,
}

/// `path` without trailing separators (unless that would leave nothing).
fn trim_separator(path: &str) -> &str {
    match path.trim_end_matches(std::path::is_separator) {
        "" => path,
        trimmed => trimmed,
    }
}

/// Whether clearing everything leaves `entry` alone because it is a thread
/// workspace.
///
/// A thread workspace belongs to whoever made it (another tool, the user) and
/// the thread may still be resumed in it, so only a non-bulk explicit path
/// removes one. That holds by path too: a row of any origin that is a listed
/// thread's workspace (or its checkout's top level), or that holds one
/// anywhere inside it, from the fresh listing, is left alone (see
/// [`workspace_inside`]). Only an owned entry's own subagent child does not
/// count (see [`own_child_threads`]).
fn thread_workspace(entry: &TrackedWorktree, thread_workspaces: &[ThreadWorkspace]) -> bool {
    if entry.origin == WorktreeOrigin::Thread {
        return true;
    }
    let path = Path::new(&entry.path);
    let own_child = own_child_threads(entry);
    thread_workspaces.iter().any(|workspace| {
        !own_child.contains(&workspace.thread_id) && workspace_inside(&workspace.forms, path)
    })
}

/// Picks the entries a clear request names.
///
/// Pure: a requested path matches a listed one only when the two strings are
/// byte-equal after trailing separators are trimmed. Nothing is normalised,
/// so a request can never reach a row through a symlink. A `bulk` request
/// (Clear all with the paths it showed) leaves thread workspaces out exactly
/// as `paths: None` does, and both report each one left out as skipped with
/// [`THREAD_WORKSPACE`] (active rows `paths: None` passes over are not
/// reported).
fn select_entries<'a>(
    entries: &'a [TrackedWorktree],
    paths: Option<&[String]>,
    bulk: bool,
    thread_workspaces: &[ThreadWorkspace],
) -> Selection<'a> {
    let mut targets: Vec<&TrackedWorktree> = Vec::new();
    let mut skipped = Vec::new();
    match paths {
        None => {
            for entry in entries {
                if entry.status == WorktreeStatus::Active {
                    continue;
                }
                if thread_workspace(entry, thread_workspaces) {
                    skipped.push(SkippedWorktree {
                        path: entry.path.clone(),
                        reason: THREAD_WORKSPACE.into(),
                    });
                } else {
                    targets.push(entry);
                }
            }
        }
        Some(paths) => {
            let mut seen = HashSet::new();
            for requested in paths {
                let wanted = trim_separator(requested);
                if !seen.insert(wanted) {
                    continue;
                }
                match entries
                    .iter()
                    .find(|entry| trim_separator(&entry.path) == wanted)
                {
                    Some(entry) if bulk && thread_workspace(entry, thread_workspaces) => {
                        skipped.push(SkippedWorktree {
                            path: requested.clone(),
                            reason: THREAD_WORKSPACE.into(),
                        });
                    }
                    Some(entry) => targets.push(entry),
                    None => skipped.push(SkippedWorktree {
                        path: requested.clone(),
                        reason: "not tracked".into(),
                    }),
                }
            }
        }
    }
    Selection { targets, skipped }
}
