//! Cross-harness subagents (ADR-038, ADR-039).
//!
//! A subagent is a child thread, not a second kind of session. Delegation
//! therefore inherits everything threads already do: durable events, resume,
//! usage accounting, lifecycle placement, approvals and the same MCP hub. This
//! module adds the part that is genuinely new:
//!
//! - **policy**: harness availability, nesting depth and budgets;
//! - **isolation**: a shared workspace or a git worktree under the data
//!   directory;
//! - **context handoff**: none, a caller summary, or selected workspace files;
//! - **supervision**: one background watcher enforces budgets, folds terminal
//!   turns into a result and cleans the worktree up;
//! - **presentation**: durable spawn/finish rows on the parent and transient
//!   progress updates for the subagent panel.
//!
//! The parent agent reaches the same operations through the `h4rn__…` MCP tools
//! in [`provider`], scoped by the session token to its own thread.

mod provider;
mod store;
pub mod worktree;

pub use provider::SubagentProvider;
use store::{StoredRecord, SubagentStore};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use h4rn_protocol::{
    AgentState, DEFAULT_MAX_DEPTH, DEFAULT_MAX_TOKENS, DEFAULT_MAX_TURNS,
    DEFAULT_MAX_WALL_TIME_SECS, Event, Item, MessageRole, ProtoError, ProtoErrorCode,
    SubagentBudget, SubagentContext, SubagentCredential, SubagentId, SubagentIsolation,
    SubagentProgress, SubagentResult, SubagentState, SubagentView, ThreadId, TurnId, TurnStatus,
};
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

use crate::harnesses::HarnessCatalog;
use crate::journal::WireEvent;
use crate::threads::{ManagerError, NewThread, ThreadManager};

/// Largest context block handed to a child, across every selected file.
const MAX_CONTEXT_BYTES: usize = 128 * 1024;

/// How often the supervisor refreshes wall-clock counters.
const DEFAULT_TICK: Duration = Duration::from_millis(500);

/// Supervisor policy.
#[derive(Clone, Debug)]
pub struct SubagentOptions {
    /// Deepest allowed nesting; 2 means children and grandchildren.
    pub max_depth: u32,
    /// Budget applied when the caller omits a field.
    pub default_budget: SubagentBudget,
    /// Wall-clock refresh interval.
    pub tick_interval: Duration,
}

impl Default for SubagentOptions {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            default_budget: SubagentBudget {
                max_turns: Some(DEFAULT_MAX_TURNS),
                max_wall_time_secs: Some(DEFAULT_MAX_WALL_TIME_SECS),
                max_tokens: Some(DEFAULT_MAX_TOKENS),
            },
            tick_interval: DEFAULT_TICK,
        }
    }
}

/// One subagent change the panel renders live.
#[derive(Clone, Debug)]
pub enum SubagentEvent {
    /// Progress, state or budget counters changed.
    Updated(SubagentView),
}

/// Failures mapped to protocol errors with an actionable hint.
#[derive(Debug, thiserror::Error)]
pub enum SubagentError {
    /// The request is malformed or violates a policy.
    #[error("{0}")]
    Invalid(String),
    /// The subagent does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The request conflicts with current state.
    #[error("{0}")]
    Conflict(String),
    /// The daemon build or host cannot serve it.
    #[error("{0}")]
    Unsupported(String),
    /// An internal failure.
    #[error("{0}")]
    Internal(String),
}

impl SubagentError {
    /// A user-facing next step for this class of failure.
    pub fn hint(&self) -> &'static str {
        match self {
            Self::Invalid(_) => {
                "Pass a non-empty task, a harness this daemon can run, and positive budget values"
            }
            Self::NotFound(_) => "List the parent's subagents and retry with a known id",
            Self::Conflict(_) => "Wait for the child to finish, or cancel it and spawn a new one",
            Self::Unsupported(_) => {
                "Pick a harness registered on this host, or run the child on the host that has it"
            }
            Self::Internal(_) => "Retry the request; if it persists, check the daemon log",
        }
    }

    /// Converts to the wire error, keeping the hint.
    pub fn into_proto(self) -> ProtoError {
        let hint = self.hint().to_owned();
        match self {
            Self::Invalid(message) => {
                ProtoError::new(ProtoErrorCode::InvalidRequest, message, hint)
            }
            Self::NotFound(message) => ProtoError::new(ProtoErrorCode::NotFound, message, hint),
            Self::Conflict(message) => ProtoError::new(ProtoErrorCode::Conflict, message, hint),
            Self::Unsupported(message) => {
                ProtoError::new(ProtoErrorCode::Unsupported, message, hint)
            }
            Self::Internal(message) => ProtoError::new(ProtoErrorCode::Internal, message, hint),
        }
    }
}

impl From<ManagerError> for SubagentError {
    fn from(error: ManagerError) -> Self {
        match error {
            ManagerError::NotFound(message) => Self::NotFound(message),
            ManagerError::Conflict(message) => Self::Conflict(message),
            ManagerError::InvalidRequest(message) => Self::Invalid(message),
            ManagerError::Unsupported(message) => Self::Unsupported(message),
            ManagerError::NotImplemented(message) => Self::Unsupported(message),
            ManagerError::Internal(message) => Self::Internal(message),
        }
    }
}

/// The daemon-wide subagent supervisor.
#[derive(Clone)]
pub struct SubagentManager {
    inner: Arc<Inner>,
}

struct Inner {
    threads: Arc<ThreadManager>,
    catalog: Arc<dyn HarnessCatalog>,
    data_dir: PathBuf,
    store: SubagentStore,
    options: SubagentOptions,
    records: Mutex<HashMap<String, Record>>,
    /// Serialises record writes. Without it a progress update that read the
    /// record before a terminal transition could land after the terminal
    /// write, and a restart would resurrect the child as still running.
    persist_lock: Mutex<()>,
    events: broadcast::Sender<SubagentEvent>,
    watcher: std::sync::Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
}

#[derive(Clone)]
struct Record {
    view: SubagentView,
    created: Instant,
    turns_started: u32,
    turns_completed: u32,
    cancel_requested: bool,
    budget_error: Option<String>,
    last_error: Option<String>,
    finalizing: bool,
    /// Parent repository, when this child runs in a worktree.
    repo: Option<PathBuf>,
    /// Absolute worktree path, when isolated.
    worktree: Option<PathBuf>,
    /// Wall-clock time already spent paused on a usage limit.
    paused: Duration,
    /// Start of the pause in progress.
    paused_since: Option<Instant>,
}

impl Record {
    /// Active wall-clock time, excluding time paused on a usage limit.
    fn elapsed(&self, now: Instant) -> Duration {
        let since_start = now.saturating_duration_since(self.created);
        let running = self
            .paused_since
            .map(|since| now.saturating_duration_since(since))
            .unwrap_or_default();
        since_start
            .saturating_sub(self.paused)
            .saturating_sub(running)
    }

    fn stored(&self) -> StoredRecord {
        StoredRecord {
            view: self.view.clone(),
            turns_started: self.turns_started,
            turns_completed: self.turns_completed,
            cancel_requested: self.cancel_requested,
            budget_error: self.budget_error.clone(),
            last_error: self.last_error.clone(),
            repo: self.repo.clone(),
            worktree: self.worktree.clone(),
            paused_ms: self.paused.as_millis() as u64,
            paused_since_ms: self
                .paused_since
                .map(|since| (Utc::now() - since.elapsed()).timestamp_millis()),
        }
    }
}

impl SubagentManager {
    /// Opens the supervisor and restores records from `subagents.db`.
    ///
    /// Call [`Self::start`] once the daemon is ready to receive delegation.
    /// Restoration happens before the watcher starts, so a rebuilt card cannot
    /// race a fresh child event.
    pub async fn open(
        threads: Arc<ThreadManager>,
        catalog: Arc<dyn HarnessCatalog>,
        data_dir: PathBuf,
        options: SubagentOptions,
    ) -> Result<Self, SubagentError> {
        let store = SubagentStore::connect(data_dir.join("subagents.db"))
            .await
            .map_err(|error| {
                SubagentError::Internal(format!("could not open the subagent store: {error}"))
            })?;
        let manager = Self::with_store(threads, catalog, data_dir, store, options);
        manager.inner.restore().await;
        Ok(manager)
    }

    /// An in-memory supervisor for tests.
    pub async fn in_memory(
        threads: Arc<ThreadManager>,
        catalog: Arc<dyn HarnessCatalog>,
        data_dir: PathBuf,
        options: SubagentOptions,
    ) -> Result<Self, SubagentError> {
        let store = SubagentStore::in_memory().await.map_err(|error| {
            SubagentError::Internal(format!("could not open the subagent store: {error}"))
        })?;
        Ok(Self::with_store(threads, catalog, data_dir, store, options))
    }

    fn with_store(
        threads: Arc<ThreadManager>,
        catalog: Arc<dyn HarnessCatalog>,
        data_dir: PathBuf,
        store: SubagentStore,
        options: SubagentOptions,
    ) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            inner: Arc::new(Inner {
                threads,
                catalog,
                data_dir,
                store,
                options,
                records: Mutex::new(HashMap::new()),
                persist_lock: Mutex::new(()),
                events,
                watcher: std::sync::Mutex::new(None),
                closed: AtomicBool::new(false),
            }),
        }
    }

    /// Subscribes to live progress updates.
    pub fn subscribe(&self) -> broadcast::Receiver<SubagentEvent> {
        self.inner.events.subscribe()
    }

    /// Starts the supervision watcher over the thread event stream.
    pub fn start(&self) {
        let mut slot = self
            .inner
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_some() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let tick_interval = self.inner.options.tick_interval;
        let handle = tokio::spawn(async move {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let mut events = inner.threads.subscribe();
            let mut ticker = tokio::time::interval(tick_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await;
            loop {
                if inner.closed.load(Ordering::Relaxed) {
                    return;
                }
                tokio::select! {
                    item = events.recv() => match item {
                        Ok(event) => inner.handle_wire(event).await,
                        Err(broadcast::error::RecvError::Lagged(missed)) => {
                            tracing::debug!(missed, "subagent watcher skipped thread events");
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                    _ = ticker.tick() => inner.tick().await,
                }
            }
        });
        *slot = Some(handle);
    }

    /// Stops the watcher. Active children keep their thread state and worktree;
    /// daemon shutdown is not a cancel.
    pub async fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        if let Some(handle) = self
            .inner
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            handle.abort();
        }
        // Flush the live map: a terminal card may have become visible before
        // its background write landed, and a record written as still running
        // would be folded into `Failed` by the next daemon's restore.
        self.inner.flush().await;
    }

    /// Spawns a child thread and starts its first turn.
    pub async fn spawn(
        &self,
        request: h4rn_protocol::SpawnSubagentRequest,
    ) -> Result<SubagentView, SubagentError> {
        self.inner.spawn(request).await
    }

    /// Lists subagents, optionally scoped to one parent.
    pub async fn list(
        &self,
        parent_thread: Option<&ThreadId>,
        include_finished: bool,
    ) -> Vec<SubagentView> {
        let records = self.inner.records.lock().await;
        let now = Instant::now();
        let mut views: Vec<SubagentView> = records
            .values()
            .filter(|record| {
                parent_thread.is_none_or(|parent| &record.view.parent_thread_id == parent)
            })
            .filter(|record| include_finished || !record.view.is_terminal())
            .map(|record| {
                let mut view = record.view.clone();
                view.progress.elapsed_ms = record.elapsed(now).as_millis() as u64;
                view
            })
            .collect();
        views.sort_by_key(|view| std::cmp::Reverse(view.created_at));
        views
    }

    /// Fetches one subagent.
    pub async fn get(&self, subagent_id: &SubagentId) -> Option<SubagentView> {
        let records = self.inner.records.lock().await;
        let record = records.get(subagent_id.as_str())?;
        let mut view = record.view.clone();
        view.progress.elapsed_ms = record.elapsed(Instant::now()).as_millis() as u64;
        Some(view)
    }

    /// The repository stored for every subagent that runs in a worktree,
    /// keyed by subagent id.
    pub async fn worktree_repos(&self) -> HashMap<String, PathBuf> {
        let records = self.inner.records.lock().await;
        records
            .iter()
            .filter_map(|(id, record)| record.repo.clone().map(|repo| (id.clone(), repo)))
            .collect()
    }

    /// Waits up to `wait` for a terminal state, then returns the current view.
    ///
    /// The caller (the MCP tool) clamps this well below its own timeout.
    pub async fn wait(
        &self,
        subagent_id: &SubagentId,
        wait: Duration,
    ) -> Result<SubagentView, SubagentError> {
        let deadline = Instant::now() + wait;
        loop {
            let view = self
                .get(subagent_id)
                .await
                .ok_or_else(|| Self::not_found(subagent_id))?;
            if view.is_terminal() || Instant::now() >= deadline {
                return Ok(view);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Cancels a child, interrupting its turn and deleting its worktree branch.
    pub async fn cancel(&self, subagent_id: &SubagentId) -> Result<SubagentView, SubagentError> {
        let (child, terminal) = {
            let mut records = self.inner.records.lock().await;
            let record = records
                .get_mut(subagent_id.as_str())
                .ok_or_else(|| Self::not_found(subagent_id))?;
            if record.view.is_terminal() {
                (record.view.child_thread_id.clone(), true)
            } else {
                record.cancel_requested = true;
                (record.view.child_thread_id.clone(), false)
            }
        };
        self.inner.persist_id(subagent_id.as_str()).await;
        if terminal {
            return self
                .get(subagent_id)
                .await
                .ok_or_else(|| Self::not_found(subagent_id));
        }

        if let Some(child) = child {
            if self.inner.threads.interrupt(&child).await.is_err() {
                self.inner
                    .finalize(subagent_id.as_str(), TurnStatus::Interrupted)
                    .await;
            }
        } else {
            self.inner
                .finalize(subagent_id.as_str(), TurnStatus::Interrupted)
                .await;
        }
        // The supervisor normally folds the interrupt's `TurnCompleted` into a
        // terminal view; a harness that never reports one must not leave the
        // cancel half-applied.
        let view = self.wait(subagent_id, Duration::from_secs(5)).await?;
        if view.is_terminal() {
            return Ok(view);
        }
        self.inner
            .finalize(subagent_id.as_str(), TurnStatus::Interrupted)
            .await;
        self.get(subagent_id)
            .await
            .ok_or_else(|| Self::not_found(subagent_id))
    }

    fn not_found(subagent_id: &SubagentId) -> SubagentError {
        SubagentError::NotFound(format!("subagent {subagent_id} does not exist"))
    }
}

impl Inner {
    fn broadcast(&self, view: SubagentView) {
        let _ = self.events.send(SubagentEvent::Updated(view));
    }

    /// Writes one record through to SQLite, logging rather than failing the
    /// live update: a transient write error must not strand a running child.
    async fn persist(&self, record: &Record) {
        if let Err(error) = self.store.upsert(&record.stored()).await {
            tracing::warn!(%error, subagent = %record.view.subagent_id, "could not persist a subagent record");
        }
    }

    /// Writes every live record through to the store, in map order.
    async fn flush(&self) {
        let ids: Vec<String> = {
            let records = self.records.lock().await;
            records.keys().cloned().collect()
        };
        for id in ids {
            self.persist_id(&id).await;
        }
    }

    /// Persists one record by id, if it still exists.
    ///
    /// Writes are serialised and each one re-reads the live record, so the last
    /// write always carries the newest state: a snapshot taken before a
    /// terminal transition can never overwrite it.
    async fn persist_id(&self, id: &str) {
        let _writing = self.persist_lock.lock().await;
        let record = {
            let records = self.records.lock().await;
            records.get(id).cloned()
        };
        if let Some(record) = record {
            self.persist(&record).await;
        }
    }

    /// Rebuilds the live map from `subagents.db` after a restart.
    ///
    /// Finished records keep their status and result verbatim. A record that
    /// was still running cannot be running any more: a daemon restart drops
    /// harness sessions, so it is folded into `Failed` with an explicit reason
    /// instead of leaving a phantom `Running` card.
    async fn restore(&self) {
        let stored = match self.store.load().await {
            Ok(stored) => stored,
            Err(error) => {
                tracing::warn!(%error, "could not restore subagent records");
                return;
            }
        };
        let now = Instant::now();
        let now_ms = Utc::now().timestamp_millis();
        let mut restored = Vec::with_capacity(stored.len());
        for stored in stored {
            let elapsed = (now_ms - stored.view.created_at.timestamp_millis()).max(0) as u64;
            let created = now
                .checked_sub(Duration::from_millis(elapsed))
                .unwrap_or(now);
            let paused_since = stored.paused_since_ms.and_then(|ms| {
                let age = (now_ms - ms).max(0) as u64;
                now.checked_sub(Duration::from_millis(age))
            });
            let mut record = Record {
                view: stored.view,
                created,
                turns_started: stored.turns_started,
                turns_completed: stored.turns_completed,
                cancel_requested: stored.cancel_requested,
                budget_error: stored.budget_error,
                last_error: stored.last_error,
                finalizing: false,
                repo: stored.repo,
                worktree: stored.worktree,
                paused: Duration::from_millis(stored.paused_ms),
                paused_since,
            };
            if !record.view.is_terminal() {
                let scheduled = match record.view.child_thread_id.as_ref() {
                    Some(child) => self
                        .threads
                        .thread_summary(child)
                        .await
                        .is_some_and(|summary| summary.agent_state == AgentState::Scheduled),
                    None => false,
                };
                if scheduled {
                    record.view.state = SubagentState::Scheduled;
                    record.view.agent_state = AgentState::Scheduled;
                } else {
                    let child = record.view.child_thread_id.clone();
                    let mut result = self.collect_result(child.as_ref()).await;
                    result.error = Some(
                        "the daemon restarted while the subagent was running; the child thread kept its transcript"
                            .into(),
                    );
                    record.view.state = SubagentState::Failed;
                    record.view.agent_state = AgentState::Error;
                    record.view.result = Some(result);
                    record.view.finished_at = Some(Utc::now());
                }
            }
            restored.push(record);
        }
        let mut records = self.records.lock().await;
        for record in &restored {
            records.insert(record.view.subagent_id.as_str().to_owned(), record.clone());
        }
        drop(records);
        for record in &restored {
            self.persist(record).await;
        }
    }

    /// Looks up the subagent that owns `child`.
    async fn subagent_for_child(&self, child: &ThreadId) -> Option<String> {
        let records = self.records.lock().await;
        records
            .iter()
            .find(|(_, record)| record.view.child_thread_id.as_ref() == Some(child))
            .map(|(id, _)| id.clone())
    }

    async fn spawn(
        self: &Arc<Self>,
        request: h4rn_protocol::SpawnSubagentRequest,
    ) -> Result<SubagentView, SubagentError> {
        let task = request.task.trim().to_owned();
        if task.is_empty() {
            return Err(SubagentError::Invalid(
                "a subagent needs a non-empty task".into(),
            ));
        }
        validate_budget(&request.budget)?;

        let parent = self
            .threads
            .thread_meta(&request.parent_thread)
            .await
            .ok_or_else(|| {
                SubagentError::NotFound(format!(
                    "parent thread {} does not exist",
                    request.parent_thread
                ))
            })?;

        let depth = self.depth_of(&request.parent_thread).await + 1;
        if !depth_allowed(depth, self.options.max_depth) {
            return Err(SubagentError::Conflict(format!(
                "nested subagents are limited to depth {}; this child would be depth {depth}",
                self.options.max_depth
            )));
        }

        let harness = request.harness.unwrap_or(parent.harness);
        if self.catalog.get(harness).is_none() {
            let available: Vec<&str> = self
                .catalog
                .available()
                .iter()
                .map(|kind| kind.slug())
                .collect();
            return Err(SubagentError::Unsupported(format!(
                "no {harness:?} adapter is available in this daemon; available: {}",
                if available.is_empty() {
                    "none".to_owned()
                } else {
                    available.join(", ")
                }
            )));
        }

        // TODO(remote-hosts): let the target daemon create the child and relay
        // its events back. This phase only delegates on the local host, so an
        // explicit remote request fails before any thread row exists.
        if let Some(host) = &request.host
            && host != &parent.host
        {
            return Err(SubagentError::Unsupported(format!(
                "running a subagent on host {host} is not implemented; \
                 this phase delegates on the local host only"
            )));
        }
        let host = request.host.unwrap_or(parent.host.clone());
        let model = request.model.unwrap_or(parent.model.clone());
        let prompt = compose_prompt(&task, &request.context, &parent.workspace_path)?;
        let budget = resolve_budget(&request.budget, &self.options.default_budget);

        let subagent_id = SubagentId::new_uuid_v7();
        let child_thread_id = ThreadId::new_uuid_v7();

        let (workspace_path, branch, repo, worktree_dir) = match request.isolation {
            SubagentIsolation::Shared => (parent.workspace_path.clone(), None, None, None),
            SubagentIsolation::Worktree => {
                let workspace = PathBuf::from(&parent.workspace_path);
                let branch = format!("h4rn/sa-{}", short_id(subagent_id.as_str()));
                let dest = worktree::worktree_path(&self.data_dir, subagent_id.as_str());
                // Every probe and `git worktree add` shells out and blocks, so
                // they run off the async runtime.
                let workspace_text = parent.workspace_path.clone();
                let (create_branch, create_dest) = (branch.clone(), dest.clone());
                let repo = tokio::task::spawn_blocking(move || {
                    if !worktree::is_git_repo(&workspace) {
                        return Err(SubagentError::Invalid(format!(
                            "worktree isolation needs a git repository; {workspace_text} is not one"
                        )));
                    }
                    // A nested child's parent workspace may itself be a
                    // subagent worktree (a linked worktree). Record the
                    // repository's main worktree so the branch can be found
                    // after that directory is removed. The worktree is still
                    // created from the parent's workspace so the child
                    // branches from the parent's HEAD.
                    let repo = crate::worktrees::git::repo_of(&workspace)
                        .unwrap_or_else(|| workspace.clone());
                    worktree::create(&workspace, &create_dest, &create_branch)
                        .map_err(SubagentError::Internal)?;
                    Ok(repo)
                })
                .await
                .map_err(|error| {
                    SubagentError::Internal(format!("the worktree setup task failed: {error}"))
                })??;
                (
                    dest.display().to_string(),
                    Some(branch),
                    Some(repo),
                    Some(dest),
                )
            }
        };

        let now = Utc::now();
        let view = SubagentView {
            subagent_id: subagent_id.clone(),
            parent_thread_id: request.parent_thread.clone(),
            child_thread_id: Some(child_thread_id.clone()),
            task: task.clone(),
            harness,
            host: host.clone(),
            model: model.clone(),
            isolation: request.isolation,
            workspace_path: workspace_path.clone(),
            branch: branch.clone(),
            depth,
            state: SubagentState::Starting,
            agent_state: AgentState::Queued,
            progress: SubagentProgress::default(),
            budget,
            context: request.context,
            credential: SubagentCredential {
                label: "parent session credential".into(),
                shared_with_parent: true,
            },
            created_at: now,
            started_at: None,
            finished_at: None,
            result: None,
        };
        {
            let mut records = self.records.lock().await;
            records.insert(
                subagent_id.as_str().to_owned(),
                Record {
                    view: view.clone(),
                    created: Instant::now(),
                    turns_started: 0,
                    turns_completed: 0,
                    cancel_requested: false,
                    budget_error: None,
                    last_error: None,
                    finalizing: false,
                    repo,
                    worktree: worktree_dir,
                    paused: Duration::ZERO,
                    paused_since: None,
                },
            );
        }
        self.persist_id(subagent_id.as_str()).await;

        let created = self
            .threads
            .create_thread(NewThread {
                thread_id: Some(child_thread_id.clone()),
                parent_thread_id: Some(request.parent_thread.clone()),
                subagent_id: Some(subagent_id.clone()),
                host,
                harness,
                agent_id: None,
                workspace_path,
                model,
                prompt,
                attachments: Vec::new(),
                runtime_mode: parent.runtime_mode,
                worktree: None,
            })
            .await;

        let created = match created {
            Ok(created) => created,
            Err(error) => {
                let record = self.records.lock().await.remove(subagent_id.as_str());
                let _ = self.store.remove(subagent_id.as_str()).await;
                if let Some(record) = record
                    && let (Some(repo), Some(worktree)) = (record.repo, record.worktree)
                {
                    let branch = record.view.branch;
                    // Shells out to git; keep it off the runtime.
                    let _ = tokio::task::spawn_blocking(move || {
                        worktree::remove(&repo, &worktree, branch.as_deref());
                    })
                    .await;
                }
                return Err(error.into());
            }
        };

        let (view, cancelled_while_starting) = {
            let mut records = self.records.lock().await;
            let record = records
                .get_mut(subagent_id.as_str())
                .expect("the record was inserted before the thread");
            if !record.view.is_terminal() {
                record.view.state = SubagentState::Running;
                record.view.agent_state = AgentState::Thinking;
                record.view.started_at = Some(Utc::now());
                record.view.progress.current = Some("working".into());
            }
            (record.view.clone(), record.view.is_terminal())
        };
        self.persist_id(subagent_id.as_str()).await;

        // The child thread exists now; deliver the delegation row to the parent
        // and finish the durable half of the spawn.
        let parent_turn = self
            .threads
            .thread_turn_id(&request.parent_thread)
            .await
            .unwrap_or_else(TurnId::new_uuid_v7);
        let item = Item::Delegated {
            item_id: format!("item_delegated_{}", short_id(subagent_id.as_str())),
            subagent_id: subagent_id.clone(),
            thread_id: Some(created.thread_id.clone()),
            task: task.clone(),
            harness,
            host: view.host.clone(),
            model: view.model.clone(),
            state: view.state,
            agent_state: view.agent_state,
        };
        let wire = vec![
            Event::ItemCompleted {
                thread_id: request.parent_thread.clone(),
                turn_id: parent_turn,
                item,
            },
            Event::SubagentSpawned {
                parent_thread_id: request.parent_thread.clone(),
                subagent: view.clone(),
            },
        ];
        if let Err(error) = self
            .threads
            .append_wire_only(&request.parent_thread, wire)
            .await
        {
            tracing::warn!(%error, "could not append the subagent spawn row");
        }
        self.broadcast(view.clone());

        if cancelled_while_starting {
            // A cancel raced the spawn; stop the child before its first turn
            // produces work nobody asked to keep.
            let _ = self.threads.interrupt(&created.thread_id).await;
        }
        Ok(view)
    }

    /// Number of parent links above `thread`.
    async fn depth_of(&self, thread: &ThreadId) -> u32 {
        let mut depth = 0;
        let mut current = thread.clone();
        let limit = self.options.max_depth.saturating_add(2);
        while depth < limit {
            let Some(meta) = self.threads.thread_meta(&current).await else {
                break;
            };
            let Some(parent) = meta.parent_thread_id else {
                break;
            };
            depth = depth.saturating_add(1);
            current = parent;
        }
        depth
    }

    async fn handle_wire(&self, event: WireEvent) {
        match event.event {
            Event::TurnStarted { thread_id, .. } => {
                let Some(id) = self.subagent_for_child(&thread_id).await else {
                    return;
                };
                let (view, exceeded) = {
                    let mut records = self.records.lock().await;
                    let Some(record) = records.get_mut(&id) else {
                        return;
                    };
                    if record.view.is_terminal() {
                        return;
                    }
                    record.turns_started = record.turns_started.saturating_add(1);
                    record.view.progress.turns_started = record.turns_started;
                    record.view.state = SubagentState::Running;
                    record.view.agent_state = AgentState::Thinking;
                    record.view.progress.current = Some("working".into());
                    let exceeded =
                        record.view.budget.max_turns.filter(|max| {
                            record.turns_started > *max && record.budget_error.is_none()
                        });
                    (record.view.clone(), exceeded)
                };
                self.persist_id(&id).await;
                self.broadcast(view);
                if let Some(max) = exceeded {
                    self.exceed(&id, format!("turn budget of {max} turns exceeded"))
                        .await;
                }
            }
            Event::ThreadUpserted(summary) => {
                let Some(id) = self.subagent_for_child(&summary.thread_id).await else {
                    return;
                };
                let (view, exceeded) = {
                    let mut records = self.records.lock().await;
                    let Some(record) = records.get_mut(&id) else {
                        return;
                    };
                    if record.view.is_terminal() {
                        return;
                    }
                    record.view.agent_state = summary.agent_state;
                    record.view.progress.tokens = summary.usage.total_tokens();
                    record.view.state = match summary.agent_state {
                        AgentState::AwaitingApproval => SubagentState::AwaitingApproval,
                        AgentState::Scheduled => SubagentState::Scheduled,
                        _ => SubagentState::Running,
                    };
                    let exceeded = record
                        .view
                        .budget
                        .max_tokens
                        .filter(|max| {
                            record.view.progress.tokens > *max && record.budget_error.is_none()
                        })
                        .map(|max| format!("token budget of {max} tokens exceeded"));
                    (record.view.clone(), exceeded)
                };
                let exceeded_now = self.refresh_tokens_for(&id).await;
                self.persist_id(&id).await;
                self.broadcast(view);
                if let Some(reason) = exceeded_now {
                    self.exceed(&id, reason).await;
                }
                if let Some(reason) = exceeded {
                    self.exceed(&id, reason).await;
                }
            }
            Event::ApprovalRequested { thread_id, .. } => {
                let Some(id) = self.subagent_for_child(&thread_id).await else {
                    return;
                };
                self.update_state(
                    &id,
                    SubagentState::AwaitingApproval,
                    AgentState::AwaitingApproval,
                )
                .await;
            }
            Event::ApprovalResolved { thread_id, .. } => {
                let Some(id) = self.subagent_for_child(&thread_id).await else {
                    return;
                };
                self.update_state(&id, SubagentState::Running, AgentState::Thinking)
                    .await;
            }
            Event::ToolCall {
                thread_id,
                tool_call,
                ..
            } => {
                let Some(id) = self.subagent_for_child(&thread_id).await else {
                    return;
                };
                let state = AgentState::RunningTool;
                let mut records = self.records.lock().await;
                let Some(record) = records.get_mut(&id) else {
                    return;
                };
                if record.view.is_terminal() {
                    return;
                }
                record.view.agent_state = state;
                record.view.progress.current = Some(format!("tool: {}", tool_call.name));
                let view = record.view.clone();
                drop(records);
                self.persist_id(&id).await;
                self.broadcast(view);
            }
            Event::Error {
                thread_id: Some(thread_id),
                message,
                ..
            } => {
                let Some(id) = self.subagent_for_child(&thread_id).await else {
                    return;
                };
                let mut records = self.records.lock().await;
                if let Some(record) = records.get_mut(&id) {
                    record.last_error = Some(message);
                }
                drop(records);
                self.persist_id(&id).await;
            }
            Event::TurnCompleted {
                thread_id, status, ..
            } => {
                let Some(id) = self.subagent_for_child(&thread_id).await else {
                    return;
                };
                {
                    let mut records = self.records.lock().await;
                    if let Some(record) = records.get_mut(&id) {
                        record.turns_completed = record.turns_completed.saturating_add(1);
                        record.view.progress.turns_completed = record.turns_completed;
                    }
                }
                self.refresh_tokens_for(&id).await;
                // A turn that ends because a usage limit fired is not the end
                // of the child: the thread's auto-resume schedule will start a
                // new turn. Report `Scheduled` instead of a premature failure.
                let scheduled = {
                    let records = self.records.lock().await;
                    records
                        .get(&id)
                        .is_some_and(|record| record.view.state == SubagentState::Scheduled)
                } || self
                    .threads
                    .thread_summary(&thread_id)
                    .await
                    .is_some_and(|summary| summary.agent_state == AgentState::Scheduled);
                if scheduled {
                    self.update_state(&id, SubagentState::Scheduled, AgentState::Scheduled)
                        .await;
                    return;
                }
                self.persist_id(&id).await;
                self.finalize(&id, status).await;
            }
            _ => {}
        }
    }

    async fn update_state(&self, id: &str, state: SubagentState, agent_state: AgentState) {
        let mut records = self.records.lock().await;
        let Some(record) = records.get_mut(id) else {
            return;
        };
        if record.view.is_terminal() {
            return;
        }
        record.view.state = state;
        record.view.agent_state = agent_state;
        let view = record.view.clone();
        drop(records);
        self.persist_id(id).await;
        self.broadcast(view);
    }

    /// Marks a budget overrun and interrupts the child.
    async fn exceed(&self, id: &str, reason: String) {
        let (child, terminal) = {
            let mut records = self.records.lock().await;
            let Some(record) = records.get_mut(id) else {
                return;
            };
            if record.view.is_terminal() {
                return;
            }
            if record.budget_error.is_none() {
                record.budget_error = Some(reason.clone());
            }
            (
                record.view.child_thread_id.clone(),
                record.view.is_terminal(),
            )
        };
        if terminal {
            return;
        }
        self.persist_id(id).await;
        match child {
            Some(child) => {
                if self.threads.interrupt(&child).await.is_err() {
                    self.finalize(id, TurnStatus::Interrupted).await;
                }
            }
            None => self.finalize(id, TurnStatus::Interrupted).await,
        }
    }

    /// Replaces the cached token count with the ledger's, when it has one.
    ///
    /// The ledger is the source of truth for usage (ADR-024), so a budget is
    /// enforced against metered tokens rather than only the last harness
    /// report carried on the thread projection. Returns the overrun reason the
    /// first time the budget is crossed.
    async fn refresh_tokens_for(&self, id: &str) -> Option<String> {
        let child = {
            let records = self.records.lock().await;
            records.get(id)?.view.child_thread_id.clone()
        };
        let child = child?;
        let tokens = self.threads.metered_tokens(&child).await?;
        let mut records = self.records.lock().await;
        let record = records.get_mut(id)?;
        record.view.progress.tokens = record.view.progress.tokens.max(tokens);
        let max = record.view.budget.max_tokens?;
        (record.view.progress.tokens > max && record.budget_error.is_none()).then(|| {
            record.budget_error = Some(format!("token budget of {max} tokens exceeded"));
            format!("token budget of {max} tokens exceeded")
        })
    }

    /// Refreshes elapsed counters and enforces the wall-clock and token
    /// budgets. Time spent waiting for an auto-resume does not count against
    /// the wall-clock budget, because the child cannot make progress then.
    async fn tick(&self) {
        let now = Instant::now();
        let snapshot: Vec<(String, Option<ThreadId>)> = {
            let records = self.records.lock().await;
            records
                .iter()
                .filter(|(_, record)| !record.view.is_terminal())
                .map(|(id, record)| (id.clone(), record.view.child_thread_id.clone()))
                .collect()
        };
        let mut metered = Vec::with_capacity(snapshot.len());
        for (id, child) in snapshot {
            let tokens = match child {
                Some(child) => self.threads.metered_tokens(&child).await,
                None => None,
            };
            metered.push((id, tokens));
        }

        let mut updates = Vec::new();
        let mut exceeded = Vec::new();
        {
            let mut records = self.records.lock().await;
            for (id, tokens) in metered {
                let Some(record) = records.get_mut(&id) else {
                    continue;
                };
                if record.view.is_terminal() {
                    continue;
                }
                if let Some(tokens) = tokens {
                    record.view.progress.tokens = record.view.progress.tokens.max(tokens);
                    if let Some(max) = record.view.budget.max_tokens
                        && record.view.progress.tokens > max
                        && record.budget_error.is_none()
                    {
                        let reason = format!("token budget of {max} tokens exceeded");
                        record.budget_error = Some(reason.clone());
                        exceeded.push((id.clone(), reason));
                    }
                }
                if record.view.state == SubagentState::Scheduled {
                    record.paused_since.get_or_insert(now);
                } else if let Some(since) = record.paused_since.take() {
                    record.paused = record
                        .paused
                        .saturating_add(now.saturating_duration_since(since));
                }
                let elapsed = record.elapsed(now);
                record.view.progress.elapsed_ms = elapsed.as_millis() as u64;
                if let Some(max) = record.view.budget.max_wall_time_secs
                    && elapsed.as_secs() > max
                    && record.budget_error.is_none()
                {
                    let reason = format!("wall-time budget of {max}s exceeded");
                    record.budget_error = Some(reason.clone());
                    exceeded.push((id.clone(), reason));
                }
                updates.push(record.view.clone());
            }
        }
        for (id, _) in &exceeded {
            self.persist_id(id).await;
        }
        for view in updates {
            self.broadcast(view);
        }
        for (id, reason) in exceeded {
            self.exceed(&id, reason).await;
        }
    }

    /// Folds a terminal turn into a result and cleans up the workspace.
    async fn finalize(&self, id: &str, status: TurnStatus) {
        let (view, repo, worktree, parent, should_finalize) = {
            let mut records = self.records.lock().await;
            let Some(record) = records.get_mut(id) else {
                return;
            };
            if record.view.is_terminal() || record.finalizing {
                return;
            }
            record.finalizing = true;
            (
                record.view.clone(),
                record.repo.clone(),
                record.worktree.clone(),
                record.view.parent_thread_id.clone(),
                true,
            )
        };
        if !should_finalize {
            return;
        }

        let state = {
            let records = self.records.lock().await;
            let record = records.get(id).expect("record exists");
            if record.budget_error.is_some() {
                SubagentState::BudgetExceeded
            } else if record.cancel_requested {
                SubagentState::Cancelled
            } else {
                match status {
                    TurnStatus::Completed => SubagentState::Completed,
                    TurnStatus::Failed | TurnStatus::TokenLimit | TurnStatus::TurnLimit => {
                        SubagentState::Failed
                    }
                    TurnStatus::Interrupted => SubagentState::Cancelled,
                }
            }
        };

        let mut result = self.collect_result(view.child_thread_id.as_ref()).await;
        if let Some(repo) = &repo
            && let Some(worktree) = &worktree
        {
            let delete_branch = matches!(
                state,
                SubagentState::Cancelled | SubagentState::BudgetExceeded
            );
            let (repo, worktree) = (repo.clone(), worktree.clone());
            let branch = delete_branch.then(|| view.branch.clone()).flatten();
            // Both shell out to git; keep them off the runtime.
            result.files_changed = tokio::task::spawn_blocking(move || {
                let changes = worktree::changes(&worktree);
                worktree::remove(&repo, &worktree, branch.as_deref());
                changes
            })
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "the subagent worktree cleanup task failed");
                Vec::new()
            });
        }
        if state == SubagentState::Failed || state == SubagentState::BudgetExceeded {
            let records = self.records.lock().await;
            result.error = records
                .get(id)
                .and_then(|record| record.budget_error.clone().or(record.last_error.clone()));
        }

        let final_view = {
            let mut records = self.records.lock().await;
            let Some(record) = records.get_mut(id) else {
                return;
            };
            record.finalizing = false;
            record.view.state = state;
            record.view.result = Some(result);
            record.view.finished_at = Some(Utc::now());
            record.view.progress.current = None;
            record.view.agent_state = match state {
                SubagentState::Completed => AgentState::Done,
                SubagentState::Failed | SubagentState::BudgetExceeded => AgentState::Error,
                SubagentState::Cancelled => AgentState::Idle,
                _ => record.view.agent_state,
            };
            record.view.clone()
        };

        if let Some(child) = &view.child_thread_id {
            let _ = self.threads.clear_approvals_for_child(&parent, child).await;
        }
        if let Err(error) = self
            .threads
            .append_wire_only(
                &parent,
                vec![Event::SubagentFinished {
                    parent_thread_id: parent.clone(),
                    subagent: final_view,
                }],
            )
            .await
        {
            tracing::warn!(%error, "could not append the subagent finish row");
        }
        self.persist_id(id).await;
    }

    async fn collect_result(&self, child: Option<&ThreadId>) -> SubagentResult {
        let Some(child) = child else {
            return SubagentResult::default();
        };
        let Ok(snapshot) = self.threads.get_thread(child, 0).await else {
            return SubagentResult::default();
        };
        let mut result = SubagentResult {
            usage: snapshot.summary.usage.clone(),
            ..SubagentResult::default()
        };
        for event in snapshot.events {
            if let Event::ItemCompleted {
                item:
                    Item::Message {
                        role: MessageRole::Assistant,
                        text,
                        ..
                    },
                ..
            } = event.event
                && !text.trim().is_empty()
            {
                result.text = text;
            }
        }
        result
    }
}

/// Whether a new child at `depth` fits the daemon's nesting policy.
fn depth_allowed(depth: u32, max_depth: u32) -> bool {
    depth <= max_depth
}

/// Fills omitted budget fields from the daemon defaults.
fn resolve_budget(request: &SubagentBudget, defaults: &SubagentBudget) -> SubagentBudget {
    SubagentBudget {
        max_turns: request.max_turns.or(defaults.max_turns),
        max_wall_time_secs: request.max_wall_time_secs.or(defaults.max_wall_time_secs),
        max_tokens: request.max_tokens.or(defaults.max_tokens),
    }
}

fn validate_budget(budget: &SubagentBudget) -> Result<(), SubagentError> {
    if budget.max_turns == Some(0) {
        return Err(SubagentError::Invalid(
            "max_turns must be at least 1".into(),
        ));
    }
    if budget.max_wall_time_secs == Some(0) {
        return Err(SubagentError::Invalid(
            "max_wall_time_secs must be at least 1".into(),
        ));
    }
    if budget.max_tokens == Some(0) {
        return Err(SubagentError::Invalid(
            "max_tokens must be at least 1".into(),
        ));
    }
    Ok(())
}

/// Builds the first prompt, including the selected context handoff.
fn compose_prompt(
    task: &str,
    context: &SubagentContext,
    workspace: &str,
) -> Result<String, SubagentError> {
    match context {
        SubagentContext::None => Ok(task.trim().to_owned()),
        SubagentContext::Summary { summary } => Ok(format!(
            "{}\n\n## Context from the parent thread\n\n{}",
            task.trim(),
            summary.trim()
        )),
        SubagentContext::Files { paths, summary } => {
            if paths.is_empty() {
                return Err(SubagentError::Invalid(
                    "context.files needs at least one file path".into(),
                ));
            }
            let root = std::fs::canonicalize(workspace).map_err(|error| {
                SubagentError::Invalid(format!(
                    "the parent workspace {workspace} could not be opened: {error}"
                ))
            })?;
            let mut prompt = format!("{}\n\n## Context files\n", task.trim());
            let mut total = 0usize;
            for relative in paths {
                let path = root.join(relative);
                let canonical = std::fs::canonicalize(&path).map_err(|error| {
                    SubagentError::Invalid(format!(
                        "context file {relative} could not be read: {error}"
                    ))
                })?;
                if !canonical.starts_with(&root) {
                    return Err(SubagentError::Invalid(format!(
                        "context file {relative} is outside the parent workspace"
                    )));
                }
                let bytes = std::fs::read(&canonical).map_err(|error| {
                    SubagentError::Invalid(format!(
                        "context file {relative} could not be read: {error}"
                    ))
                })?;
                total = total.saturating_add(bytes.len());
                if total > MAX_CONTEXT_BYTES {
                    return Err(SubagentError::Invalid(format!(
                        "selected context files exceed {MAX_CONTEXT_BYTES} bytes; select fewer files"
                    )));
                }
                let text = String::from_utf8_lossy(&bytes);
                prompt.push_str(&format!("\n### {relative}\n\n```\n{text}\n```\n"));
            }
            if let Some(summary) = summary {
                prompt.push_str("\n## Summary\n\n");
                prompt.push_str(summary.trim());
            }
            Ok(prompt)
        }
    }
}

fn short_id(id: &str) -> String {
    id.rsplit_once('_')
        .map(|(_, suffix)| suffix)
        .unwrap_or(id)
        .chars()
        .take(8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budgets_fill_from_defaults_and_reject_zero() {
        let defaults = SubagentBudget {
            max_turns: Some(20),
            max_wall_time_secs: Some(900),
            max_tokens: Some(50_000),
        };
        let resolved = resolve_budget(
            &SubagentBudget {
                max_turns: Some(3),
                ..SubagentBudget::default()
            },
            &defaults,
        );
        assert_eq!(resolved.max_turns, Some(3));
        assert_eq!(resolved.max_tokens, Some(50_000));

        assert!(validate_budget(&SubagentBudget::default()).is_ok());
        assert!(
            validate_budget(&SubagentBudget {
                max_turns: Some(0),
                ..SubagentBudget::default()
            })
            .is_err()
        );
        assert!(
            validate_budget(&SubagentBudget {
                max_tokens: Some(0),
                ..SubagentBudget::default()
            })
            .is_err()
        );
    }

    #[test]
    fn short_ids_come_from_the_uuid_suffix() {
        assert_eq!(short_id("sa_0192ab"), "0192ab");
    }

    #[test]
    fn nesting_is_capped_at_the_configured_depth() {
        let options = SubagentOptions::default();
        assert_eq!(options.max_depth, DEFAULT_MAX_DEPTH);
        assert!(depth_allowed(1, options.max_depth));
        assert!(depth_allowed(2, options.max_depth));
        assert!(!depth_allowed(3, options.max_depth));
        assert!(!depth_allowed(1, 0));
    }

    #[test]
    fn context_prompts_include_the_handoff() {
        let prompt = compose_prompt(
            "Fix it",
            &SubagentContext::Summary {
                summary: "the build fails".into(),
            },
            "/tmp",
        )
        .unwrap();
        assert!(prompt.contains("Fix it"));
        assert!(prompt.contains("the build fails"));
        assert!(compose_prompt("Task", &SubagentContext::None, "/tmp").is_ok());
    }
}
