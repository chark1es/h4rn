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
    /// Normalised `<data_dir>/subagents`; everything under it is h4rn's own.
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
        self.list_with(true).await
    }

    /// [`Self::list`], optionally without the dirty check.
    ///
    /// Clearing first lists without `git status` for every entry because the
    /// per-entry removal checks dirtiness again anyway; the final response
    /// listing asks git for it.
    async fn list_with(&self, with_dirty: bool) -> Vec<TrackedWorktree> {
        let input = self.inner.list_input().await;
        let inner = Arc::clone(&self.inner);
        let computed = tokio::time::timeout(
            GIT_BUDGET,
            tokio::task::spawn_blocking(move || compute(&input, with_dirty)),
        )
        .await;
        let (entries, gone) = match computed {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                tracing::warn!(%error, "worktree listing task failed");
                return Vec::new();
            }
            Err(_) => {
                tracing::warn!("worktree listing timed out");
                return Vec::new();
            }
        };
        // A harness worktree whose directory is gone has nothing left to clear.
        for path in gone {
            if let Err(error) = inner.store.delete(&path).await {
                tracing::warn!(%error, %path, "could not forget a vanished worktree");
            }
        }
        entries
    }

    /// Removes tracked worktrees from disk.
    ///
    /// `paths: None` targets every non-active entry except thread workspaces,
    /// which are only removed when named explicitly. Active entries are never
    /// removed. Without `force`, dirty worktrees and owned branches with
    /// unmerged commits are skipped.
    pub async fn clear(&self, paths: Option<Vec<String>>, force: bool) -> ClearOutcome {
        let _guard = self.inner.clear_lock.lock().await;
        let entries = self.list_with(false).await;
        let selection = select_entries(&entries, paths.as_deref());
        let mut removed = Vec::new();
        let mut skipped = selection.skipped;
        // One entry at a time: the listing is a snapshot, so each removal runs
        // a blocking preflight, then asks the live thread and subagent state
        // again, and only then touches the disk. Both spawn_blocking calls are
        // awaited rather than timed out.
        for entry in selection.targets {
            let entry = entry.clone();
            let path = entry.path.clone();
            match run_clear_entry(&self.inner, entry, force, |_| std::future::ready(())).await {
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
    async fn still_active(self: &Arc<Self>, entry: &TrackedWorktree) -> Option<TrackedWorktree> {
        let mut fresh = entry.clone();
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
        // Any thread working in the entry, listed or not.
        let threads = self.threads.list_threads(&thread_filter()).await;
        let normalised = tokio::task::spawn_blocking(move || {
            threads
                .into_iter()
                .map(|summary| {
                    let workspace = git::norm(Path::new(&summary.workspace_path));
                    (summary, workspace)
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        for (summary, workspace) in normalised {
            if workspace != target && !workspace.starts_with(&target) {
                continue;
            }
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
        if entry.owned
            && let Some(id) = &entry.subagent_id
            && self
                .subagents
                .get(id)
                .await
                .is_some_and(|view| !view.is_terminal())
        {
            active = true;
        }
        active.then_some(fresh)
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
fn subagent_repo(input: &ListInput, view: &SubagentView, path: &Path) -> Option<PathBuf> {
    if let Some(stored) = input.subagent_repos.get(view.subagent_id.as_str())
        && let Some(repo) = repo_through_subagents(input, stored)
    {
        return Some(repo);
    }
    if let Some(workspace) = input.parent_workspaces.get(view.parent_thread_id.as_str())
        && let Some(repo) = repo_through_subagents(input, Path::new(workspace))
    {
        return Some(repo);
    }
    git::repo_of(path)
}

/// The repository at `path`, following h4rn's own subagent worktrees.
///
/// A nested child's parent workspace is `<data_dir>/subagents/<id>/worktree`,
/// which is deleted when that subagent settles. Its recorded repository is the
/// only reliable answer then, so follow it, bounded against a stale loop.
fn repo_through_subagents(input: &ListInput, path: &Path) -> Option<PathBuf> {
    let mut path = git::norm(path);
    for _ in 0..MAX_REPO_HOPS {
        match subagent_worktree_owner(&path, &input.subagents_root) {
            Some(id) => match input.subagent_repos.get(&id) {
                Some(stored) => path = git::norm(stored),
                None => return git::repo_of(&path),
            },
            None => return git::repo_of(&path),
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
/// `None` when git could not answer. A caller must never treat `None` as
/// absence: a failed snapshot says nothing about what is still registered.
///
/// `lists` caches one snapshot per repository for the listing, so a repository
/// with many vanished observations is asked only once.
fn registration_exists(
    repo: &Path,
    path: &Path,
    lists: &mut HashMap<PathBuf, Option<Vec<git::Registration>>>,
) -> Option<bool> {
    if repo.as_os_str().is_empty() {
        return Some(false);
    }
    lists
        .entry(repo.to_path_buf())
        .or_insert_with(|| git::registrations(repo).ok())
        .as_ref()
        .map(|registrations| {
            registrations
                .iter()
                .any(|registration| registration.path == path)
        })
}

/// Computes the tracked list from plain data. Blocking (shells out to git).
///
/// Returns the entries, sorted, and the stored harness paths whose directory is
/// gone and can be forgotten. `with_dirty` asks git for uncommitted changes;
/// clearing's selection listing skips it because `clear_one` checks again.
pub(crate) fn compute(input: &ListInput, with_dirty: bool) -> (Vec<TrackedWorktree>, Vec<String>) {
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
    let mut registration_lists: HashMap<PathBuf, Option<Vec<git::Registration>>> = HashMap::new();

    // 1. Worktrees h4rn made for isolated subagents.
    for view in &input.subagents {
        let path = git::norm(Path::new(&view.workspace_path));
        let repo = subagent_repo(input, view, &path);
        // The protocol carries paths as strings, so a path that cannot
        // round-trip through UTF-8 is never listed.
        if path.to_str().is_none() || repo.as_ref().is_some_and(|repo| repo.to_str().is_none()) {
            continue;
        }
        let exists = path.is_dir();
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
                match registration_exists(repo, &path, &mut registration_lists) {
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
    for info in &input.threads {
        let workspace = git::norm(Path::new(&info.workspace));
        if workspace.starts_with(&input.subagents_root) || !workspace.is_dir() {
            continue;
        }
        let top = tops
            .entry(workspace.clone())
            .or_insert_with(|| {
                git::is_linked_worktree(&workspace)
                    .then(|| git::toplevel(&workspace))
                    .flatten()
            })
            .clone();
        if let Some(top) = top {
            by_workspace.entry(top).or_default().push(info);
        }
    }
    for (path, group) in by_workspace {
        if path.starts_with(&input.subagents_root) || index.contains_key(&path) {
            continue;
        }
        let Some(repo) = git::repo_of(&path) else {
            continue;
        };
        if path.to_str().is_none() || repo.to_str().is_none() {
            continue;
        }
        let list = repo_lists
            .entry(repo.clone())
            .or_insert_with(|| git::list_worktrees(&repo).unwrap_or_default());
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
        let path = git::norm(Path::new(&stored.path));
        // A row under h4rn's own subagent worktrees is derived live, and a row
        // whose repository itself is gone can never be cleared through git:
        // forget both (the store only; nothing on disk is touched).
        if path.starts_with(&input.subagents_root) || !Path::new(&stored.repo).is_dir() {
            gone.push(stored.path.clone());
            continue;
        }
        let exists = path.is_dir();
        // A checkout whose directory is gone is still clearable while its own
        // registration remains: removing that registration is the only cleanup
        // left, and forgetting the row here would report it as untracked.
        // Only a successful snapshot that no longer names the path may forget
        // it; a failed snapshot keeps the row (exists: false) for a later try.
        if !exists {
            match registration_exists(Path::new(&stored.repo), &path, &mut registration_lists) {
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
        entry.exists = Path::new(&entry.path).is_dir();
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
    (entries, gone)
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

/// Another registered worktree nested inside `path`, from one snapshot.
fn contained_worktree(registrations: &[git::Registration], path: &Path) -> Option<PathBuf> {
    let selected = git::norm(path);
    registrations
        .iter()
        .find(|registration| {
            registration.path != selected && registration.path.starts_with(&selected)
        })
        .map(|registration| registration.path.clone())
}

/// The registration for exactly `path`, from one snapshot.
fn exact_registration<'a>(
    registrations: &'a [git::Registration],
    path: &Path,
) -> Option<&'a git::Registration> {
    let selected = git::norm(path);
    registrations
        .iter()
        .find(|registration| registration.path == selected)
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

/// The reason for an owned checkout that reappeared after prepare saw it gone
/// with no branch left (never forceable).
const RECREATED: &str = "the checkout was recreated during the clear";

/// The reason for a missing checkout whose registration `HEAD` is unknown.
const UNKNOWN_HEAD: &str = "could not read the registration's HEAD";

/// A rebase or bisect in progress in any worktree of `repo` that names `branch`.
///
/// The main git dir is `<common>` and every linked worktree's admin dir is
/// under `<common>/worktrees`, so both are inspected for the files git writes
/// while a rebase or bisect is running.
fn branch_in_progress(repo: &Path, branch: &str) -> Result<Option<String>, String> {
    let common = git::common_dir(repo, CLEAR_GIT_TIMEOUT)?;
    let mut dirs = vec![common.clone()];
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

/// Whether `path` exists and has a whole (trimmed) line equal to a needle.
fn file_has_line(path: &Path, needles: &[&str]) -> bool {
    std::fs::read_to_string(path)
        .is_ok_and(|text| text.lines().any(|line| needles.contains(&line.trim())))
}

/// Whether `path` exists and has a whole whitespace-separated token equal to a
/// needle.
fn file_has_token(path: &Path, needles: &[&str]) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| {
        text.split_whitespace()
            .any(|token| needles.contains(&token))
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
fn worktree_remove(repo: &Path, path: &Path, force: bool) -> Result<(), String> {
    let mut args: Vec<&OsStr> = [
        "-c",
        "status.showUntrackedFiles=all",
        "-c",
        "diff.ignoreSubmodules=none",
        "worktree",
        "remove",
    ]
    .map(OsStr::new)
    .to_vec();
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
/// An existing checkout must hold no registered worktree (from
/// `registrations`), and, when `thorough`, no nested checkout of any
/// repository. Without force it must still be on its listed branch with no
/// unreachable detached work and no file git hides from `status`
/// (assume-unchanged or skip-worktree). For a missing checkout, without force,
/// its own registration's `HEAD` must be known, and a detached one reachable
/// from some ref.
///
/// `thorough` gates only the nested-checkout scan, a whole tree walk: it runs
/// in [`prepare`] and again on a state flip, never in the final pass before an
/// unchanged checkout's removal, which must stay short. The hidden-file check
/// is one `ls-files` and runs in every pass.
fn verify_current(
    entry: &TrackedWorktree,
    registrations: &[git::Registration],
    exists: bool,
    force: bool,
    thorough: bool,
) -> Result<(), String> {
    let path = PathBuf::from(&entry.path);
    let repo = PathBuf::from(&entry.repo);
    if exists {
        // `git worktree remove` would leave a nested registration pointing
        // into a directory that no longer exists, or delete another
        // repository's clone outright; neither is ever forceable.
        if let Some(inner) = contained_worktree(registrations, &path) {
            return Err(format!("contains another worktree at {}", inner.display()));
        }
        if thorough {
            let common = git::common_dir(&repo, CLEAR_GIT_TIMEOUT)?;
            if let Some(nested) = nested_checkout(&path, &common, nested_scan_limit())? {
                return Err(format!(
                    "contains another git checkout at {}",
                    nested.display()
                ));
            }
        }
        if !force {
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
    } else if !force && let Some(registration) = exact_registration(registrations, &path) {
        // Only the registration is left. Its HEAD may hold a commit that no
        // branch, remote-tracking branch or tag reaches; deleting the
        // registration would lose the only name for that work.
        verify_missing_registration(&repo, registration)?;
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
async fn run_clear_entry<F, Fut>(
    inner: &Arc<Inner>,
    entry: TrackedWorktree,
    force: bool,
    after_preflight: F,
) -> Result<ClearOne, String>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let path = entry.path.clone();
    let preflight_entry = entry.clone();
    let plan = match tokio::task::spawn_blocking(move || preflight(&preflight_entry, force)).await {
        Ok(Ok(plan)) => plan,
        Ok(Err(reason)) => return Err(reason),
        Err(error) => {
            tracing::warn!(%error, %path, "worktree preflight task failed");
            return Err("the clear task failed".into());
        }
    };
    after_preflight(path.clone()).await;
    let prepare_entry = entry.clone();
    let prepared = match tokio::task::spawn_blocking(move || prepare(&prepare_entry, force)).await {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(reason)) => return Err(reason),
        Err(error) => {
            tracing::warn!(%error, %path, "worktree prepare task failed");
            return Err("the clear task failed".into());
        }
    };
    if let Some(active) = inner.still_active(&entry).await {
        return Err(in_use_reason(&active));
    }
    let data_dir = inner.data_dir.clone();
    match tokio::task::spawn_blocking(move || execute(&entry, force, &data_dir, &plan, &prepared))
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
fn preflight(entry: &TrackedWorktree, force: bool) -> Result<Preflight, String> {
    let path = PathBuf::from(&entry.path);
    if entry.repo.is_empty() {
        return Err("its repository could not be found".into());
    }
    let repo = PathBuf::from(&entry.repo);
    if git::norm(&path) == git::norm(&repo) {
        return Err("is the repository's main worktree".into());
    }
    // One snapshot answers both the nested-worktree check and the missing
    // checkout's own registration.
    let registrations = git::registrations_with_timeout(&repo, CLEAR_GIT_TIMEOUT)?;
    // Git would happily remove a checkout that holds another worktree, leaving
    // the nested registration pointing into a directory that no longer exists.
    // That checkout is never removed, with or without force. Only an existing
    // checkout can hold a nested one: when the selected directory is gone there
    // is no working tree to protect, and dropping its own registration must
    // leave the nested registration alone.
    if path.is_dir()
        && let Some(inner) = contained_worktree(&registrations, &path)
    {
        return Err(format!("contains another worktree at {}", inner.display()));
    }
    let mut branch = None;
    if !force {
        // The listing is only a snapshot: a turn may have written since. Ask
        // git again here, and never read "could not ask" as clean.
        // `status --porcelain` with untracked files always counted, whatever
        // the user's config says. Anything written after this is caught by
        // git's own refusal in `worktree_remove`.
        match git::run_with_timeout(
            &path,
            [
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
            CLEAR_GIT_TIMEOUT,
        ) {
            Ok(out) if !out.is_empty() => return Err("has uncommitted changes".into()),
            Ok(_) => {}
            Err(error) if git::timed_out(&error) => return Err(error),
            Err(_) if path.is_dir() => {
                return Err("could not check for uncommitted changes".into());
            }
            Err(_) => {}
        }
        if path.is_dir() {
            // The checkout at this path must still be the one the listing
            // described. A replacement checkout recreated here on another
            // branch would otherwise be removed as if it were that entry.
            verify_checkout(&path, &repo, entry.branch.as_deref())?;
        } else if let Some(registration) = exact_registration(&registrations, &path) {
            // Only the registration is left. Its HEAD may hold a commit that no
            // branch, remote-tracking branch or tag reaches; deleting the
            // registration would lose the only name for that work.
            verify_missing_registration(&repo, registration)?;
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
/// from `status`, and a missing checkout's registration `HEAD`. The live
/// activity check runs after this, and only then does [`execute`] touch the
/// disk.
fn prepare(entry: &TrackedWorktree, force: bool) -> Result<Prepared, String> {
    let repo = PathBuf::from(&entry.repo);
    let registrations = git::registrations_with_timeout(&repo, CLEAR_GIT_TIMEOUT)?;
    let was_dir = Path::new(&entry.path).is_dir();
    verify_current(entry, &registrations, was_dir, force, true)?;
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
/// [`verify_current`]), and an owned checkout with no branch that reappeared is
/// refused outright. A missing checkout with no
/// registration in that snapshot is reported removed only because both were
/// read after the activity check: no directory and no registration remain.
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
) -> Result<ClearOne, String> {
    let path = PathBuf::from(&entry.path);
    let repo = PathBuf::from(&entry.repo);
    // The snapshot first, then the directory: whatever the snapshot does not
    // show (a nested worktree, the checkout's own registration) was registered
    // after this point, and the directory check below is newer still.
    let registrations = git::registrations_with_timeout(&repo, CLEAR_GIT_TIMEOUT)?;
    let exists = path.is_dir();
    let flipped = exists != prepared.was_dir;
    // An owned checkout with no branch left had nothing on disk to protect;
    // a directory there now is someone else's new checkout, whatever force says.
    if flipped && exists && entry.owned && entry.branch.is_none() {
        return Err(RECREATED.into());
    }
    // Every pass checks the state that exists now against this snapshot: a
    // missing checkout's registration (already missing at prepare or just
    // vanished) gets its HEAD checks here too, so a ref deleted since prepare
    // is seen. Only a flip repeats the nested-checkout scan.
    verify_current(entry, &registrations, exists, force, flipped)?;
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
            branch_failure = delete_owned_branch(&repo, branch, plan, force)?;
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
/// from a fresh registration snapshot: occupancy, an in-progress rebase or
/// bisect, and the verified sha's ancestry in `HEAD`. A forced clear keeps
/// git's own `branch -D`. `Err` is a forceable skip; `Ok(Some(reason))` is a
/// branch left in place that force cannot fix.
fn delete_owned_branch(
    repo: &Path,
    branch: &str,
    plan: &Preflight,
    force: bool,
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
    let registrations = match git::registrations_with_timeout(repo, CLEAR_GIT_TIMEOUT) {
        Ok(registrations) => registrations,
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
            match branch_in_progress(repo, branch) {
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

/// Picks the entries a clear request names.
fn select_entries<'a>(entries: &'a [TrackedWorktree], paths: Option<&[String]>) -> Selection<'a> {
    let mut targets: Vec<&TrackedWorktree> = Vec::new();
    let mut skipped = Vec::new();
    match paths {
        // A thread workspace belongs to whoever made it (another tool, the
        // user) and the thread may still be resumed in it, so clearing
        // everything leaves it alone; only an explicit path removes one.
        None => targets.extend(entries.iter().filter(|entry| {
            entry.status != WorktreeStatus::Active && entry.origin != WorktreeOrigin::Thread
        })),
        Some(paths) => {
            let mut seen = HashSet::new();
            for requested in paths {
                let wanted = git::norm(Path::new(requested));
                if !seen.insert(wanted.clone()) {
                    continue;
                }
                match entries
                    .iter()
                    .find(|entry| Path::new(&entry.path) == wanted.as_path())
                {
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
