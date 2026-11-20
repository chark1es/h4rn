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

mod git;
mod store;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use h4rn_protocol::{
    Event, Item, SkippedWorktree, SubagentId, SubagentIsolation, SubagentView, ThreadFilter,
    ThreadId, ThreadTab, TrackedWorktree, WorktreeAccess, WorktreeOrigin, WorktreeStatus,
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

/// Longest a listing or clear may spend in git before giving up.
const GIT_BUDGET: Duration = Duration::from_secs(60);

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
    worktrees: Vec<git::GitWorktree>,
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
                self.put(repo, git::list_worktrees(repo).unwrap_or_default());
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
        let input = self.inner.list_input().await;
        let inner = Arc::clone(&self.inner);
        let computed = tokio::time::timeout(
            GIT_BUDGET,
            tokio::task::spawn_blocking(move || compute(&input)),
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
        let entries = self.list().await;
        let data_dir = self.inner.data_dir.clone();
        let raw = tokio::time::timeout(
            GIT_BUDGET,
            tokio::task::spawn_blocking(move || {
                clear_entries(&entries, paths.as_deref(), force, &data_dir)
            }),
        )
        .await;
        let (removed, skipped) = match raw {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                tracing::warn!(%error, "worktree clear task failed");
                (Vec::new(), Vec::new())
            }
            Err(_) => {
                tracing::warn!("worktree clear timed out");
                (Vec::new(), Vec::new())
            }
        };
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

// ---------------------------------------------------------------------------
// Observation
// ---------------------------------------------------------------------------

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
        let workspace = PathBuf::from(meta.workspace_path);
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
        let Some(repo) = self.repo_for_thread(thread_id).await else {
            return;
        };
        let Some(worktrees) = self.list_repo(&repo).await else {
            return;
        };
        lock(&self.snapshots).insert(thread_id.as_str().to_owned(), Snapshot { worktrees });
    }

    async fn turn_completed(self: &Arc<Self>, thread_id: &ThreadId) {
        let Some(snapshot) = lock(&self.snapshots).remove(thread_id.as_str()) else {
            return;
        };
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
        if self
            .thread_repo(thread)
            .await
            .is_some_and(|found| found.own == worktree.path)
        {
            return;
        }
        let key = (
            worktree.path.to_string_lossy().into_owned(),
            thread.as_str().to_owned(),
        );
        {
            let mut recent = lock(&self.recent);
            if recent
                .get(&key)
                .is_some_and(|at| at.elapsed() < RECORD_THROTTLE)
            {
                return;
            }
            recent.insert(key.clone(), Instant::now());
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
                &repo.to_string_lossy(),
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
        let filter = ThreadFilter {
            include_snoozed: true,
            include_children: true,
            ..ThreadFilter::default()
        };
        let threads = self
            .threads
            .list_threads(&filter)
            .await
            .into_iter()
            .map(|summary| ThreadInfo {
                thread_id: summary.thread_id,
                title: summary.title,
                workspace: summary.workspace_path,
                working: summary.tab == ThreadTab::Working,
                last_activity: summary.last_activity,
            })
            .collect();
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
            threads,
            stored,
            subagents_root: self.subagents_root.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

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

/// Computes the tracked list from plain data. Blocking (shells out to git).
///
/// Returns the entries, sorted, and the stored harness paths whose directory is
/// gone and can be forgotten.
pub(crate) fn compute(input: &ListInput) -> (Vec<TrackedWorktree>, Vec<String>) {
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

    // 1. Worktrees h4rn made for isolated subagents.
    for view in &input.subagents {
        let path = git::norm(Path::new(&view.workspace_path));
        let repo = input
            .parent_workspaces
            .get(view.parent_thread_id.as_str())
            .map(|workspace| {
                let workspace = git::norm(Path::new(workspace));
                git::repo_of(&workspace).unwrap_or(workspace)
            })
            .or_else(|| git::repo_of(&path));
        let exists = path.is_dir();
        let branch_exists = match (&repo, &view.branch) {
            (Some(repo), Some(branch)) => git::branch_exists(repo, branch),
            _ => false,
        };
        // Nothing left to clear.
        if !exists && !branch_exists {
            continue;
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
            branch: view.branch.clone(),
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
        if path.starts_with(&input.subagents_root) || !path.is_dir() {
            gone.push(stored.path.clone());
            continue;
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
                    exists: true,
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
        entry.dirty = if entry.exists {
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

/// Removes one entry from disk. Blocking.
fn clear_one(entry: &TrackedWorktree, force: bool, data_dir: &Path) -> Result<(), String> {
    let path = PathBuf::from(&entry.path);
    if entry.repo.is_empty() {
        return Err("its repository could not be found".into());
    }
    let repo = PathBuf::from(&entry.repo);
    if git::norm(&path) == git::norm(&repo) {
        return Err("is the repository's main worktree".into());
    }
    if !force {
        if entry.dirty == Some(true) {
            return Err("has uncommitted changes".into());
        }
        if entry.owned
            && let Some(branch) = &entry.branch
            && git::branch_exists(&repo, branch)
            && !git::is_merged(&repo, branch)
        {
            return Err(format!("branch {branch} has unmerged commits"));
        }
    }
    if path.is_dir() {
        let mut args = vec!["worktree", "remove"];
        if force || entry.owned {
            args.push("--force");
        }
        let path_text = path.to_string_lossy();
        args.push(&path_text);
        git::run(&repo, args)?;
    }
    // A directory that vanished still leaves git metadata behind.
    if let Err(error) = git::run(&repo, ["worktree", "prune"]) {
        tracing::warn!(%error, repo = %repo.display(), "git worktree prune failed");
    }
    if entry.owned
        && let Some(branch) = &entry.branch
        && branch.starts_with("h4rn/")
        && git::branch_exists(&repo, branch)
        && let Err(error) = git::run(&repo, ["branch", "-D", branch])
    {
        tracing::warn!(%error, %branch, "could not delete the subagent branch");
    }
    if let Some(subagent_id) = &entry.subagent_id {
        // `remove_dir` only succeeds on an empty directory, which is the point.
        let _ = std::fs::remove_dir(data_dir.join("subagents").join(subagent_id.as_str()));
    }
    Ok(())
}

/// Selects and clears entries. Blocking.
///
/// Returns the removed paths and the skipped entries with reasons.
pub(crate) fn clear_entries(
    entries: &[TrackedWorktree],
    paths: Option<&[String]>,
    force: bool,
    data_dir: &Path,
) -> (Vec<String>, Vec<SkippedWorktree>) {
    let mut removed = Vec::new();
    let mut skipped = Vec::new();
    let mut targets: Vec<&TrackedWorktree> = Vec::new();
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
    for entry in targets {
        if entry.status == WorktreeStatus::Active {
            skipped.push(SkippedWorktree {
                path: entry.path.clone(),
                reason: in_use_reason(entry),
            });
            continue;
        }
        match clear_one(entry, force, data_dir) {
            Ok(()) => removed.push(entry.path.clone()),
            Err(reason) => skipped.push(SkippedWorktree {
                path: entry.path.clone(),
                reason,
            }),
        }
    }
    (removed, skipped)
}
