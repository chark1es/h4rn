//! The thread manager: live harness sessions, lifecycle projection and event
//! fan-out.
//!
//! One [`ThreadRecord`] owns the durable metadata and lifecycle state of a
//! thread, plus its live [`h4rn_harness::HarnessSession`]. Harness events are
//! normalised once, translated into `h4rn-core` lifecycle events and projected
//! into protocol events, appended to the journal, and broadcast to subscribers.
//!
//! Cadence rules:
//! - Commands (`CreateThread`, `SendMessage`, ...) answer immediately; the
//!   stream catches up through events.
//! - `ThreadSummary.tab` always comes from [`h4rn_core::tab_for`], never from a
//!   local guess, so daemon and UI share one predicate set (ADR-004).
//! - A background ticker emits a new projected summary when a clock-derived
//!   transition happens: Working -> Done after the exit grace, auto-settle, or
//!   snooze expiry.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use h4rn_core::{
    LifecycleSettings, ThreadEvent, ThreadState, apply_all, is_snoozed, needs_attention, tab_for,
};
use h4rn_harness::{
    AttachmentRef, HarnessEvent, HarnessInstall, HarnessSession, SessionSpec, UserInput,
};
use h4rn_meter::{LimitHit, MeterHub, TurnContext, TurnMeter};
use h4rn_protocol::{
    AgentState, ApprovalDecision, ApprovalKind, Attachment, AttachmentId, DeviceId, Event,
    HarnessKind, HostId, Item, LimitBrief, LimitSource, MessageRole, ProtoError, ProtoErrorCode,
    ResumeFailover, ResumeSchedule, ResumeState, RuntimeMode, SubagentId, ThreadFilter, ThreadId,
    ThreadSummary, ThreadTab, ToolCallStatus, TurnId, TurnStatus, Usage, UsageReport,
    WorktreeRequest,
};
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

use crate::blobs::{AttachmentResolver, BlobError};
use crate::journal::{Journal, ThreadMeta, WireEvent};

/// Capacity of the live event broadcast. A subscriber that falls further behind
/// than this is dropped with a resume hint rather than slowing other clients.
pub const BROADCAST_CAPACITY: usize = 512;

/// What a harness session needs to reach the daemon's MCP hub.
#[derive(Clone, Debug)]
pub struct McpSession {
    /// Loopback Streamable-HTTP endpoint.
    pub endpoint: String,
    /// Bearer token minted for this session only.
    pub token: String,
}

/// Supplies the per-session MCP endpoint and token (ADR-006).
///
/// Kept as a trait so the thread manager does not depend on the daemon's MCP
/// service; the daemon installs its hub-backed provider after construction.
pub trait McpSessionProvider: Send + Sync {
    /// Returns the injection for one harness session, or `None` before the
    /// endpoint is bound.
    fn prepare(&self, harness: HarnessKind, thread_id: &ThreadId) -> Option<McpSession>;
}

/// Options for the manager.
#[derive(Clone, Debug)]
pub struct ThreadManagerOptions {
    /// Lifecycle timings (exit grace, auto-settle, stale bypass).
    pub settings: LifecycleSettings,
    /// How often derived placement is re-evaluated.
    pub tick_interval: Duration,
}

impl Default for ThreadManagerOptions {
    fn default() -> Self {
        Self {
            settings: LifecycleSettings::default(),
            tick_interval: Duration::from_millis(250),
        }
    }
}

/// A request to create a thread (the protocol fields without the wire id).
#[derive(Clone, Debug)]
pub struct NewThread {
    /// Thread id to use. Subagents pre-allocate it so their records can be
    /// inserted before the first child event races past.
    pub thread_id: Option<ThreadId>,
    /// Thread that delegated this one, when it is a subagent child.
    pub parent_thread_id: Option<ThreadId>,
    /// Subagent record this child belongs to.
    pub subagent_id: Option<SubagentId>,
    /// Host that runs the harness.
    pub host: HostId,
    /// Harness adapter.
    pub harness: HarnessKind,
    /// Absolute workspace path.
    pub workspace_path: String,
    /// Model slug.
    pub model: String,
    /// First user prompt.
    pub prompt: String,
    /// Attachments uploaded before this request.
    pub attachments: Vec<AttachmentId>,
    /// Permission autonomy for the whole thread.
    pub runtime_mode: RuntimeMode,
    /// Worktree creation intent.
    pub worktree: Option<WorktreeRequest>,
}

/// Result of creating a thread: the row, its first turn and the last sequence.
#[derive(Clone, Debug)]
pub struct CreatedThread {
    /// New thread id.
    pub thread_id: ThreadId,
    /// Turn enqueued for the first prompt.
    pub turn_id: TurnId,
    /// Current projection.
    pub summary: ThreadSummary,
    /// Last durable sequence written for the create.
    pub seq: u64,
}

/// Result of appending a user message.
#[derive(Clone, Debug)]
pub struct AcceptedMessage {
    /// Thread that accepted the message.
    pub thread_id: ThreadId,
    /// Turn the message belongs to.
    pub turn_id: TurnId,
    /// Last durable sequence written for the message.
    pub seq: u64,
}

/// A thread snapshot with the events a client has not seen.
#[derive(Clone, Debug)]
pub struct ThreadSnapshot {
    /// Current projection.
    pub summary: ThreadSummary,
    /// Projected events after `since_seq`.
    pub events: Vec<WireEvent>,
    /// Next sequence the client should ask for.
    pub next_seq: u64,
}

/// Manager failures, mapped to protocol errors at the server boundary.
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    /// The thread or host does not exist.
    #[error("{0}")]
    NotFound(String),
    /// The request conflicts with current state.
    #[error("{0}")]
    Conflict(String),
    /// The request is malformed.
    #[error("{0}")]
    InvalidRequest(String),
    /// The request needs a later phase or another host.
    #[error("{0}")]
    Unsupported(String),
    /// The request is part of the contract but not implemented by this build.
    #[error("{0}")]
    NotImplemented(String),
    /// An internal failure.
    #[error("{0}")]
    Internal(String),
}

impl ManagerError {
    /// Converts to the wire error, preserving a user-facing hint.
    pub fn into_proto(self) -> ProtoError {
        match self {
            Self::NotFound(message) => ProtoError::new(
                ProtoErrorCode::NotFound,
                message,
                "Refresh the thread list and try again",
            ),
            Self::Conflict(message) => ProtoError::new(
                ProtoErrorCode::Conflict,
                message,
                "Wait for the current turn to finish or interrupt it first",
            ),
            Self::InvalidRequest(message) => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                message,
                "Fix the request payload and retry",
            ),
            Self::Unsupported(message) => ProtoError::new(
                ProtoErrorCode::Unsupported,
                message,
                "This daemon build cannot serve that request yet",
            ),
            Self::NotImplemented(message) => ProtoError::new(
                ProtoErrorCode::NotImplemented,
                message,
                "Upgrade the daemon when this feature ships; no state was changed",
            ),
            Self::Internal(message) => {
                ProtoError::new(ProtoErrorCode::Internal, message, "Retry the request")
            }
        }
    }
}

/// The daemon-wide thread manager.
#[derive(Clone)]
pub struct ThreadManager {
    inner: Arc<Inner>,
}

struct Inner {
    journal: Arc<dyn Journal>,
    harnesses: Arc<dyn crate::harnesses::HarnessCatalog>,
    attachments: Option<Arc<dyn AttachmentResolver>>,
    local_host: HostId,
    options: ThreadManagerOptions,
    events: broadcast::Sender<WireEvent>,
    emit_lock: Arc<Mutex<()>>,
    threads: Mutex<HashMap<String, ThreadHandle>>,
    ticker: std::sync::Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
    /// Pre-turn file snapshots for "diff vs agent changes" (IDE mode).
    snapshots: std::sync::OnceLock<Arc<crate::fs::SnapshotStore>>,
    /// CLIProxyAPI routing, when profiles are configured (ADR-015).
    proxy: std::sync::OnceLock<Arc<h4rn_proxy::ProxyService>>,
    /// MCP endpoint/token injection, installed once the daemon binds.
    mcp: std::sync::OnceLock<Arc<dyn McpSessionProvider>>,
    /// Durable metering, limits and auto-resume (ADR-024).
    meter: std::sync::OnceLock<Arc<MeterHub>>,
    /// Throttles the due-schedule scan to about once a second.
    last_schedule_check: std::sync::Mutex<Option<std::time::Instant>>,
}

type ThreadHandle = Arc<Mutex<ThreadRecord>>;

/// A harness session shared with the task that is writing to it.
///
/// `send` can take as long as the adapter likes; keeping it behind its own lock
/// means the thread record stays available for snapshots, listings and settings
/// requests while a harness accepts input.
type SharedSession = Arc<Mutex<Box<dyn HarnessSession>>>;

struct ThreadRecord {
    meta: ThreadMeta,
    state: ThreadState,
    active: Option<ActiveTurn>,
    last_turn_id: Option<TurnId>,
    session: Option<SharedSession>,
    pump: Option<JoinHandle<()>>,
    pending_approvals: BTreeMap<String, PendingApproval>,
    queued: VecDeque<UserInput>,
    last_signature: Option<SummarySignature>,
    /// A limit hit whose auto-resume schedule is armed for this thread.
    scheduled_limit: Option<ScheduledLimit>,
    /// Scheduled auto-resume target shown on the thread summary (ADR-024).
    resume_at: Option<DateTime<Utc>>,
    /// Latest limit observation shown on the thread summary (ADR-024).
    limit: Option<LimitBrief>,
    /// Time to first token of the most recent metered turn (ADR-024).
    ttft_ms: Option<u64>,
    /// Context window of the model the session is running (ADR-015/024).
    context_window: Option<u64>,
    /// Git branch of the workspace, cached for the thread header.
    branch: Option<String>,
    closed: bool,
}

struct ActiveTurn {
    turn_id: TurnId,
    item_id: String,
    text: String,
    usage: Usage,
    meter: TurnMeter,
    /// Tool call id -> whether the call is currently counted as running.
    tools: HashMap<String, bool>,
    last_tick: Option<std::time::Instant>,
}

/// The limit observation that keeps a thread in `Scheduled`.
#[derive(Clone, Debug)]
struct ScheduledLimit {
    reset_at: Option<DateTime<Utc>>,
    auto_resume: bool,
}

struct PendingApproval {
    kind: ApprovalKind,
    /// Where this approval is mirrored when a subagent raised it.
    forward: Option<ForwardedApproval>,
}

/// A mirrored approval that lets the parent inbox and the child session answer
/// each other.
#[derive(Clone, Debug)]
struct ForwardedApproval {
    /// The thread on the other side of the mirror.
    thread_id: ThreadId,
    /// The approval id that thread's UI or session uses.
    approval_id: String,
}

/// One child approval on its way to the parent's inbox.
struct ForwardedRequest {
    parent_thread_id: ThreadId,
    parent_approval_id: String,
    child_thread_id: ThreadId,
    child_approval_id: String,
    subagent_id: Option<SubagentId>,
    kind: ApprovalKind,
    prompt: String,
    options: Vec<h4rn_protocol::ApprovalOption>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SummarySignature {
    tab: ThreadTab,
    pinned: bool,
    needs_attention: bool,
    unread: bool,
    agent_state: AgentState,
    snoozed_until: Option<DateTime<Utc>>,
    resume_at: Option<DateTime<Utc>>,
    limit: Option<LimitBrief>,
}

impl SummarySignature {
    fn of(summary: &ThreadSummary) -> Self {
        Self {
            tab: summary.tab,
            pinned: summary.pinned,
            needs_attention: summary.needs_attention,
            unread: summary.unread,
            agent_state: summary.agent_state,
            snoozed_until: summary.snoozed_until,
            resume_at: summary.resume_at,
            limit: summary.limit.clone(),
        }
    }
}

impl ThreadRecord {
    fn summary(&self, settings: &LifecycleSettings) -> ThreadSummary {
        let now = Utc::now();
        let tab = tab_for(&self.state, now, settings);
        ThreadSummary {
            thread_id: self.meta.thread_id.clone(),
            parent_thread_id: self.meta.parent_thread_id.clone(),
            host: self.meta.host.clone(),
            harness: self.meta.harness,
            title: self.meta.title.clone(),
            workspace_path: self.meta.workspace_path.clone(),
            branch: self.branch.clone(),
            model: self.meta.model.clone(),
            tab,
            pinned: self.state.pinned(),
            snoozed_until: is_snoozed(&self.state, now)
                .then_some(self.state.snoozed_until)
                .flatten(),
            last_activity: self.state.last_activity,
            agent_state: self.state.agent_state,
            usage: self.meta.usage.clone(),
            ttft_ms: self.ttft_ms,
            context_window: self.context_window,
            needs_attention: needs_attention(&self.state, now, settings),
            unread: tab == ThreadTab::Done && !self.state.acknowledged,
            resume_at: self.resume_at,
            limit: self.limit.clone(),
        }
    }

    fn signature(&self, settings: &LifecycleSettings) -> SummarySignature {
        SummarySignature::of(&self.summary(settings))
    }

    fn begin_turn(&mut self, _at: DateTime<Utc>) -> (TurnId, String) {
        // A new turn is a fresh attempt: the previous limit and any resume
        // target no longer describe the thread.
        self.resume_at = None;
        self.limit = None;
        self.branch = crate::fs::git::branch(std::path::Path::new(&self.meta.workspace_path));
        let turn_id = TurnId::new_uuid_v7();
        let item_id = assistant_item_id(&turn_id);
        self.active = Some(ActiveTurn {
            turn_id: turn_id.clone(),
            item_id: item_id.clone(),
            text: String::new(),
            usage: Usage::default(),
            meter: TurnMeter::new(_at),
            tools: HashMap::new(),
            last_tick: None,
        });
        self.last_turn_id = Some(turn_id.clone());
        (turn_id, item_id)
    }

    fn turn_id_for_event(&self) -> TurnId {
        self.active
            .as_ref()
            .map(|active| active.turn_id.clone())
            .or_else(|| self.last_turn_id.clone())
            .unwrap_or_else(TurnId::new_uuid_v7)
    }
}

impl ThreadManager {
    /// Opens the manager and restores durable thread projections.
    pub async fn open(
        journal: Arc<dyn Journal>,
        harnesses: Arc<dyn crate::harnesses::HarnessCatalog>,
        local_host: HostId,
        options: ThreadManagerOptions,
        attachments: Option<Arc<dyn AttachmentResolver>>,
    ) -> Result<Self, ManagerError> {
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let manager = Self {
            inner: Arc::new(Inner {
                journal,
                harnesses,
                attachments,
                local_host,
                options,
                events,
                emit_lock: Arc::new(Mutex::new(())),
                threads: Mutex::new(HashMap::new()),
                ticker: std::sync::Mutex::new(None),
                closed: AtomicBool::new(false),
                snapshots: std::sync::OnceLock::new(),
                proxy: std::sync::OnceLock::new(),
                mcp: std::sync::OnceLock::new(),
                meter: std::sync::OnceLock::new(),
                last_schedule_check: std::sync::Mutex::new(None),
            }),
        };
        manager.inner.restore().await?;
        Ok(manager)
    }

    /// Subscribes to the live projected event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<WireEvent> {
        self.inner.events.subscribe()
    }

    /// Attaches CLIProxyAPI routing so new sessions receive harness overrides.
    ///
    /// Called once during daemon assembly; a second call is ignored so a
    /// session cannot observe two different proxy services mid-flight.
    pub fn set_proxy(&self, proxy: Arc<h4rn_proxy::ProxyService>) {
        let _ = self.inner.proxy.set(proxy);
    }

    /// Installs the MCP endpoint/token provider. Called once by the daemon
    /// after the hub's loopback endpoint is bound.
    pub fn set_mcp_provider(&self, provider: Arc<dyn McpSessionProvider>) {
        let _ = self.inner.mcp.set(provider);
    }

    /// Attaches durable metering, limits and auto-resume (ADR-024).
    ///
    /// Called once during daemon assembly. The ticker keeps running before this
    /// so a slow ledger open never delays restarted thread projections.
    pub fn set_meter(&self, meter: Arc<MeterHub>) {
        let _ = self.inner.meter.set(meter);
    }

    /// The metering hub, when one has been attached.
    pub fn meter(&self) -> Option<Arc<MeterHub>> {
        self.inner.meter.get().cloned()
    }

    /// Restores summary-visible schedule and limit state from the ledger.
    ///
    /// Schedules and limit history outlive the thread journal, so a restarted
    /// daemon re-reads them once the hub is attached. Without this the inbox
    /// would show a scheduled thread as merely rate-limited until its next
    /// tick writes a fresh projection.
    pub async fn resync_meter_state(&self) {
        let Some(meter) = self.meter() else {
            return;
        };
        let schedules = match meter.schedules().await {
            Ok(schedules) => schedules,
            Err(error) => {
                tracing::warn!(%error, "could not re-read auto-resume schedules");
                return;
            }
        };
        let limits = match meter.limits_report().await {
            Ok(report) => report.limits,
            Err(error) => {
                tracing::warn!(%error, "could not re-read the usage limit history");
                return;
            }
        };

        let mut armed: HashMap<String, (DateTime<Utc>, Option<DateTime<Utc>>)> = HashMap::new();
        for schedule in schedules {
            if matches!(
                schedule.state,
                ResumeState::Scheduled | ResumeState::Waiting
            ) {
                let reset_at = limits
                    .iter()
                    .find(|limit| limit.thread_id.as_ref() == Some(&schedule.thread_id))
                    .and_then(|limit| limit.reset_at);
                armed
                    .entry(schedule.thread_id.as_str().to_owned())
                    .or_insert((schedule.resume_at, reset_at));
            }
        }
        let mut latest: HashMap<String, LimitBrief> = HashMap::new();
        for limit in limits {
            let Some(thread_id) = &limit.thread_id else {
                continue;
            };
            latest
                .entry(thread_id.as_str().to_owned())
                .or_insert(LimitBrief {
                    harness: limit.harness,
                    reset_at: limit.reset_at,
                    message: limit.message,
                });
        }

        let handles: Vec<ThreadHandle> = {
            let threads = self.inner.threads.lock().await;
            threads.values().cloned().collect()
        };
        for handle in handles {
            let mut record = handle.lock().await;
            let key = record.meta.thread_id.as_str().to_owned();
            if let Some((resume_at, reset_at)) = armed.remove(&key) {
                record.resume_at = Some(resume_at);
                record.scheduled_limit = Some(ScheduledLimit {
                    reset_at,
                    auto_resume: true,
                });
            }
            // Only show a limit on a thread the limit still explains.
            record.limit = if record.state.phase == h4rn_core::Phase::RateLimited
                || record.scheduled_limit.is_some()
            {
                latest.remove(&key)
            } else {
                None
            };
            record.last_signature = None;
        }
    }

    /// Durable metadata for one thread, cloned for callers outside the manager.
    pub async fn thread_meta(&self, thread_id: &ThreadId) -> Option<ThreadMeta> {
        let handle = self.inner.handle(thread_id).await.ok()?;
        let record = handle.lock().await;
        Some(record.meta.clone())
    }

    /// Current projection of one thread.
    pub async fn thread_summary(&self, thread_id: &ThreadId) -> Option<ThreadSummary> {
        let handle = self.inner.handle(thread_id).await.ok()?;
        let record = handle.lock().await;
        Some(record.summary(&self.inner.options.settings))
    }

    /// The turn a thread would attach a new item to.
    pub async fn thread_turn_id(&self, thread_id: &ThreadId) -> Option<TurnId> {
        let handle = self.inner.handle(thread_id).await.ok()?;
        let record = handle.lock().await;
        Some(record.turn_id_for_event())
    }

    /// Appends projected events to one thread without a lifecycle transition.
    ///
    /// Subagent delegation uses this for its durable spawn/finish rows: they
    /// belong in the parent's log but do not change the parent's tab.
    pub async fn append_wire_only(
        &self,
        thread_id: &ThreadId,
        wire: Vec<Event>,
    ) -> Result<u64, ManagerError> {
        if wire.is_empty() {
            return Ok(0);
        }
        let handle = self.inner.handle(thread_id).await?;
        let record = handle.lock().await;
        let _guard = self.inner.emit_lock.lock().await;
        let seqs = self
            .inner
            .journal
            .append_wire(&record.meta.thread_id, &wire)
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))?;
        self.inner.broadcast(&record.meta.thread_id, &wire, &seqs);
        Ok(seqs.last().copied().unwrap_or(0))
    }

    /// Drops approvals that a finished child leaves behind.
    ///
    /// Both sides of the mirror are cleared: the parent's forwarded hand and
    /// any hand still pending on the child's own thread. Without this a
    /// cancelled or completed child leaves an answerable-looking cue that no
    /// session can ever resolve.
    pub async fn clear_approvals_for_child(
        &self,
        parent_thread_id: &ThreadId,
        child_thread_id: &ThreadId,
    ) -> Result<(), ManagerError> {
        let parent = self.inner.handle(parent_thread_id).await?;
        {
            let mut record = parent.lock().await;
            let pending: Vec<(String, ApprovalKind)> = record
                .pending_approvals
                .iter()
                .filter(|(_, pending)| {
                    pending
                        .forward
                        .as_ref()
                        .is_some_and(|forward| &forward.thread_id == child_thread_id)
                })
                .map(|(approval_id, pending)| (approval_id.clone(), pending.kind))
                .collect();
            if !pending.is_empty() {
                let now = Utc::now();
                let mut domain = Vec::new();
                let mut wire = Vec::new();
                for (approval_id, kind) in pending {
                    record.pending_approvals.remove(&approval_id);
                    domain.push(if kind == ApprovalKind::Question {
                        ThreadEvent::QuestionAnswered { at: now }
                    } else {
                        ThreadEvent::ApprovalResolved { at: now }
                    });
                    wire.push(Event::ApprovalResolved {
                        thread_id: parent_thread_id.clone(),
                        approval_id,
                        option_id: None,
                        by_device: None,
                    });
                }
                self.inner.emit(&mut record, domain, wire).await?;
            }
        }

        let Ok(child) = self.inner.handle(child_thread_id).await else {
            return Ok(());
        };
        let mut record = child.lock().await;
        let pending: Vec<(String, ApprovalKind)> = record
            .pending_approvals
            .iter()
            .map(|(approval_id, pending)| (approval_id.clone(), pending.kind))
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let now = Utc::now();
        let mut domain = Vec::new();
        let mut wire = Vec::new();
        for (approval_id, kind) in pending {
            record.pending_approvals.remove(&approval_id);
            domain.push(if kind == ApprovalKind::Question {
                ThreadEvent::QuestionAnswered { at: now }
            } else {
                ThreadEvent::ApprovalResolved { at: now }
            });
            wire.push(Event::ApprovalResolved {
                thread_id: child_thread_id.clone(),
                approval_id,
                option_id: None,
                by_device: None,
            });
        }
        self.inner.emit(&mut record, domain, wire).await?;
        Ok(())
    }

    /// Acquires the append lock so a global replay is a gap-free prefix.
    ///
    /// While the guard is held no event can be appended; the caller reads the
    /// journal, queues the replay, and marks the connection ready before
    /// releasing it.
    pub async fn lock_append(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.inner.emit_lock).lock_owned().await
    }

    /// Projected events for every thread with `seq > after`.
    ///
    /// The caller must hold [`ThreadManager::lock_append`] for gap-free
    /// semantics; this method does not take the lock itself so it can be used
    /// from inside a replay already holding it.
    pub async fn replay_global_since(&self, after: u64) -> Result<Vec<WireEvent>, ManagerError> {
        self.inner
            .journal
            .read_wire_since_global(after)
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))
    }

    /// Starts the background placement ticker.
    pub fn start_ticker(&self) {
        let mut slot = self
            .inner
            .ticker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_some() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let interval = self.inner.options.tick_interval;
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                if inner.closed.load(Ordering::Relaxed) {
                    break;
                }
                inner.tick().await;
            }
        });
        *slot = Some(handle);
    }

    /// Creates a thread, enqueues its first turn and starts the harness.
    ///
    /// Returns as soon as the thread row and the turn start are durable; harness
    /// output follows as events.
    pub async fn create_thread(&self, request: NewThread) -> Result<CreatedThread, ManagerError> {
        if !self.inner.harnesses.get(request.harness).is_some() {
            return Err(ManagerError::Unsupported(format!(
                "no {:?} adapter is available in this daemon build",
                request.harness
            )));
        }
        if !self.inner.local_host_is(&request.host) {
            return Err(ManagerError::Unsupported(format!(
                "threads can only run on this daemon's host ({}) in phase 1b",
                self.inner.local_host
            )));
        }
        if request.workspace_path.trim().is_empty()
            || !std::path::Path::new(&request.workspace_path).is_absolute()
        {
            return Err(ManagerError::InvalidRequest(
                "workspace_path must be an absolute path on the host".into(),
            ));
        }
        if request
            .worktree
            .as_ref()
            .is_some_and(WorktreeRequest::is_requested)
        {
            // The intent is recorded on the CreateThread request so the wire
            // contract is stable, but this phase does not create worktrees.
            return Err(ManagerError::NotImplemented(
                "worktree creation is not implemented in this daemon".into(),
            ));
        }

        let now = Utc::now();
        let thread_id = request
            .thread_id
            .clone()
            .unwrap_or_else(ThreadId::new_uuid_v7);
        let resolved_attachments = self
            .inner
            .resolve_attachments(&thread_id, &request.attachments)
            .await?;
        let harness_attachments = harness_refs(&resolved_attachments);
        let meta = ThreadMeta {
            thread_id: thread_id.clone(),
            parent_thread_id: request.parent_thread_id.clone(),
            subagent_id: request.subagent_id.clone(),
            host: request.host,
            harness: request.harness,
            title: title_from_prompt(&request.prompt, &request.workspace_path),
            workspace_path: request.workspace_path,
            model: request.model,
            runtime_mode: request.runtime_mode,
            worktree: request.worktree,
            created_at: now,
            usage: Usage::default(),
            resume_id: None,
        };

        let mut record = ThreadRecord {
            meta,
            state: ThreadState::new(now),
            active: None,
            last_turn_id: None,
            session: None,
            pump: None,
            pending_approvals: BTreeMap::new(),
            queued: VecDeque::new(),
            last_signature: None,
            scheduled_limit: None,
            resume_at: None,
            limit: None,
            ttft_ms: None,
            context_window: None,
            branch: None,
            closed: false,
        };
        let (turn_id, _) = record.begin_turn(now);

        let domain = vec![
            ThreadEvent::MessageReceived {
                source: h4rn_core::MessageSource::User,
                at: now,
            },
            ThreadEvent::TurnQueued { at: now },
            ThreadEvent::TurnStarted { at: now },
        ];
        let wire = vec![
            Event::ItemCompleted {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                item: Item::Message {
                    item_id: user_item_id(&turn_id),
                    role: MessageRole::User,
                    text: request.prompt.clone(),
                    attachments: resolved_attachments,
                },
            },
            Event::TurnStarted {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                started_at: now,
            },
        ];

        let handle: ThreadHandle = Arc::new(Mutex::new(record));
        let (summary, seq) = {
            let mut record = handle.lock().await;
            self.inner.emit(&mut record, domain, wire).await?
        };
        self.inner
            .threads
            .lock()
            .await
            .insert(thread_id.as_str().to_owned(), handle.clone());

        self.inner.spawn_session(
            handle,
            Some(UserInput {
                text: request.prompt,
                attachments: harness_attachments,
            }),
        );
        Ok(CreatedThread {
            thread_id,
            turn_id,
            summary,
            seq,
        })
    }

    /// Appends a user message and starts or queues its turn.
    pub async fn send_message(
        &self,
        thread_id: &ThreadId,
        text: String,
        attachments: Vec<AttachmentId>,
    ) -> Result<AcceptedMessage, ManagerError> {
        let handle = self.inner.handle(thread_id).await?;
        let resolved_attachments = self
            .inner
            .resolve_attachments(thread_id, &attachments)
            .await?;
        let harness_attachments = harness_refs(&resolved_attachments);
        let mut record = handle.lock().await;
        if record.closed {
            return Err(ManagerError::NotFound(format!(
                "thread {thread_id} is closed"
            )));
        }

        let now = Utc::now();
        if let Some(active_turn_id) = record.active.as_ref().map(|active| active.turn_id.clone()) {
            // A turn is running: the message waits its turn, and the thread
            // stays Working. `MessageReceived` records the user activity.
            record.queued.push_back(UserInput {
                text: text.clone(),
                attachments: harness_attachments,
            });
            let domain = vec![ThreadEvent::MessageReceived {
                source: h4rn_core::MessageSource::User,
                at: now,
            }];
            let wire = vec![Event::ItemCompleted {
                thread_id: thread_id.clone(),
                turn_id: active_turn_id.clone(),
                item: Item::Message {
                    item_id: fresh_item_id("user"),
                    role: MessageRole::User,
                    text,
                    attachments: resolved_attachments,
                },
            }];
            let (_, seq) = self.inner.emit(&mut record, domain, wire).await?;
            return Ok(AcceptedMessage {
                thread_id: thread_id.clone(),
                turn_id: active_turn_id,
                seq,
            });
        }

        let (turn_id, _) = record.begin_turn(now);
        let stale_schedule = record.scheduled_limit.take().is_some();
        if stale_schedule && let Some(meter) = self.inner.meter.get() {
            let _ = meter.cancel_schedule(thread_id).await;
        }
        let domain = vec![
            ThreadEvent::MessageReceived {
                source: h4rn_core::MessageSource::User,
                at: now,
            },
            ThreadEvent::TurnQueued { at: now },
            ThreadEvent::TurnStarted { at: now },
        ];
        let wire = vec![
            Event::ItemCompleted {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                item: Item::Message {
                    item_id: user_item_id(&turn_id),
                    role: MessageRole::User,
                    text: text.clone(),
                    attachments: resolved_attachments,
                },
            },
            Event::TurnStarted {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                started_at: now,
            },
        ];
        let (_, seq) = self.inner.emit(&mut record, domain, wire).await?;

        let input = UserInput {
            text,
            attachments: harness_attachments,
        };
        if record.session.is_some() {
            drop(record);
            // The message and its turn are already recorded; let the adapter
            // accept the text in the background so the connection can answer
            // other requests (snapshots, listings, settings) meanwhile.
            let this = Arc::clone(&self.inner);
            tokio::spawn(async move {
                this.deliver(handle, input).await;
            });
        } else {
            drop(record);
            self.inner.spawn_session(handle, Some(input));
        }
        Ok(AcceptedMessage {
            thread_id: thread_id.clone(),
            turn_id,
            seq,
        })
    }

    /// Interrupts the running turn.
    pub async fn interrupt(&self, thread_id: &ThreadId) -> Result<(), ManagerError> {
        let handle = self.inner.handle(thread_id).await?;
        let session = {
            let record = handle.lock().await;
            if record.active.is_none() {
                return Err(ManagerError::Conflict(format!(
                    "thread {thread_id} has no running turn"
                )));
            }
            record.session.clone()
        };
        let Some(session) = session else {
            return Err(ManagerError::Conflict(format!(
                "thread {thread_id} has no live harness session"
            )));
        };
        session
            .lock()
            .await
            .interrupt()
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))
    }

    /// Answers a pending approval or question.
    ///
    /// Approvals raised by a subagent are mirrored onto its parent. Answering
    /// either the parent's namespaced id or the child's own id reaches the same
    /// harness session and resolves both cues.
    pub async fn respond_approval(
        &self,
        thread_id: &ThreadId,
        approval_id: &str,
        decision: ApprovalDecision,
        option_id: Option<String>,
        by_device: Option<DeviceId>,
    ) -> Result<(), ManagerError> {
        let handle = self.inner.handle(thread_id).await?;
        let (pending, is_child) = {
            let mut record = handle.lock().await;
            let Some(pending) = record.pending_approvals.remove(approval_id) else {
                return Err(ManagerError::NotFound(format!(
                    "approval {approval_id} is not pending on thread {thread_id}"
                )));
            };
            (pending, record.meta.parent_thread_id.is_some())
        };

        // Which record owns the live harness session, and which approval ids
        // does each side of the mirror use?
        let (target_thread, target_approval, mirror) = if is_child {
            (
                thread_id.clone(),
                approval_id.to_owned(),
                pending
                    .forward
                    .as_ref()
                    .map(|forward| (forward.thread_id.clone(), forward.approval_id.clone())),
            )
        } else if let Some(forward) = &pending.forward {
            (
                forward.thread_id.clone(),
                forward.approval_id.clone(),
                Some((thread_id.clone(), approval_id.to_owned())),
            )
        } else {
            (thread_id.clone(), approval_id.to_owned(), None)
        };

        let target = self.inner.handle(&target_thread).await?;
        let response = {
            let target_session = {
                let target_record = target.lock().await;
                target_record.session.clone()
            };
            match target_session {
                Some(session) => Some(
                    session
                        .lock()
                        .await
                        .respond_approval(&target_approval, decision, option_id.as_deref())
                        .await,
                ),
                None => None,
            }
        };

        match response {
            None => {
                let mut record = handle.lock().await;
                record
                    .pending_approvals
                    .insert(approval_id.to_owned(), pending);
                Err(ManagerError::Conflict(format!(
                    "thread {target_thread} lost its harness session"
                )))
            }
            Some(Err(error)) => {
                let mut record = handle.lock().await;
                record
                    .pending_approvals
                    .insert(approval_id.to_owned(), pending);
                Err(ManagerError::Internal(error.to_string()))
            }
            Some(Ok(())) => {
                let mut target_record = target.lock().await;
                target_record.pending_approvals.remove(&target_approval);
                let kind = pending.kind;
                let now = Utc::now();
                let domain = if kind == ApprovalKind::Question {
                    vec![ThreadEvent::QuestionAnswered { at: now }]
                } else {
                    vec![ThreadEvent::ApprovalResolved { at: now }]
                };
                let wire = vec![Event::ApprovalResolved {
                    thread_id: target_thread.clone(),
                    approval_id: target_approval,
                    option_id: option_id.clone(),
                    by_device: by_device.clone(),
                }];
                self.inner.emit(&mut target_record, domain, wire).await?;
                drop(target_record);

                if let Some((mirror_thread, mirror_approval)) = mirror {
                    let mirror_handle = self.inner.handle(&mirror_thread).await?;
                    let mut mirror_record = mirror_handle.lock().await;
                    mirror_record.pending_approvals.remove(&mirror_approval);
                    let domain = if kind == ApprovalKind::Question {
                        vec![ThreadEvent::QuestionAnswered { at: now }]
                    } else {
                        vec![ThreadEvent::ApprovalResolved { at: now }]
                    };
                    let wire = vec![Event::ApprovalResolved {
                        thread_id: mirror_thread,
                        approval_id: mirror_approval,
                        option_id,
                        by_device,
                    }];
                    let _ = self.inner.emit(&mut mirror_record, domain, wire).await;
                }
                Ok(())
            }
        }
    }

    /// Moves a thread by hand (ADR-004).
    pub async fn set_thread_tab(
        &self,
        thread_id: &ThreadId,
        tab: ThreadTab,
        pinned: bool,
    ) -> Result<ThreadSummary, ManagerError> {
        let handle = self.inner.handle(thread_id).await?;
        let mut record = handle.lock().await;
        let domain = vec![ThreadEvent::ManualMove {
            tab,
            pinned,
            at: Utc::now(),
        }];
        self.inner
            .emit(&mut record, domain, Vec::new())
            .await
            .map(|(summary, _)| summary)
    }

    /// Snoozes a thread until `until`, or wakes it when `None`.
    pub async fn snooze(
        &self,
        thread_id: &ThreadId,
        until: Option<DateTime<Utc>>,
    ) -> Result<ThreadSummary, ManagerError> {
        let handle = self.inner.handle(thread_id).await?;
        let mut record = handle.lock().await;
        let domain = vec![ThreadEvent::Snooze {
            until,
            at: Utc::now(),
        }];
        self.inner
            .emit(&mut record, domain, Vec::new())
            .await
            .map(|(summary, _)| summary)
    }

    /// Returns a thread projection and the events after `since_seq`.
    pub async fn get_thread(
        &self,
        thread_id: &ThreadId,
        since_seq: u64,
    ) -> Result<ThreadSnapshot, ManagerError> {
        let handle = self.inner.handle(thread_id).await?;
        let summary = {
            let record = handle.lock().await;
            record.summary(&self.inner.options.settings)
        };
        let events = self
            .inner
            .journal
            .read_wire_since(thread_id, since_seq)
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))?;
        let next_seq = events.last().map(|event| event.seq).unwrap_or(since_seq);
        Ok(ThreadSnapshot {
            summary,
            events,
            next_seq,
        })
    }

    /// Installs the snapshot store used to record pre-turn file content.
    ///
    /// Called once by the daemon builder; later calls are ignored so the pump
    /// never observes two stores.
    pub fn set_snapshot_store(&self, store: Arc<crate::fs::SnapshotStore>) {
        let _ = self.inner.snapshots.set(store);
    }

    /// Every distinct workspace path any thread has used.
    ///
    /// The filesystem service authorises these roots so IDE mode can open the
    /// repositories the user already has threads for, and nothing else.
    pub async fn workspace_paths(&self) -> Vec<String> {
        let handles: Vec<ThreadHandle> = {
            let threads = self.inner.threads.lock().await;
            threads.values().cloned().collect()
        };
        let mut paths = Vec::new();
        for handle in handles {
            let record = handle.lock().await;
            let workspace = record.meta.workspace_path.clone();
            if !workspace.is_empty() && !paths.contains(&workspace) {
                paths.push(workspace);
            }
        }
        paths
    }

    /// Lists thread projections with the protocol filter applied.
    pub async fn list_threads(&self, filter: &ThreadFilter) -> Vec<ThreadSummary> {
        let handles: Vec<ThreadHandle> = {
            let threads = self.inner.threads.lock().await;
            threads.values().cloned().collect()
        };

        let mut summaries = Vec::with_capacity(handles.len());
        for handle in handles {
            let record = handle.lock().await;
            let summary = record.summary(&self.inner.options.settings);
            let snoozed = is_snoozed(&record.state, Utc::now());
            drop(record);

            if snoozed && !filter.include_snoozed {
                continue;
            }
            if !filter.include_children && summary.parent_thread_id.is_some() {
                continue;
            }
            if !filter.tabs.is_empty() && !filter.tabs.contains(&summary.tab) {
                continue;
            }
            if let Some(host) = &filter.host
                && &summary.host != host
            {
                continue;
            }
            if let Some(harness) = filter.harness
                && summary.harness != harness
            {
                continue;
            }
            if let Some(search) = &filter.search {
                let needle = search.to_lowercase();
                if !needle.is_empty() && !summary.title.to_lowercase().contains(&needle) {
                    continue;
                }
            }
            summaries.push(summary);
        }

        summaries.sort_by_key(|left| std::cmp::Reverse(left.last_activity));
        summaries
    }

    /// Aggregated usage from the local ledger.
    pub async fn usage_report(
        &self,
        query: h4rn_meter::ReportQuery,
    ) -> Result<UsageReport, ManagerError> {
        let meter = self
            .meter()
            .ok_or_else(|| ManagerError::Unsupported("metering is not enabled".into()))?;
        meter
            .report(query)
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))
    }

    /// Metered token total for one thread, from the ledger when available.
    ///
    /// Budget enforcement asks the ledger rather than the thread projection
    /// because the ledger is the source of truth for usage (ADR-024); the
    /// projection only caches what the harness last reported.
    pub async fn metered_tokens(&self, thread_id: &ThreadId) -> Option<u64> {
        let meter = self.meter()?;
        let report = meter
            .report(h4rn_meter::ReportQuery {
                scope: h4rn_protocol::UsageScope::Thread,
                scope_id: Some(thread_id.as_str().to_owned()),
                include_children: false,
                range: Default::default(),
                group_by: h4rn_protocol::UsageGroupBy::None,
                bucket: None,
                series_group_by: None,
            })
            .await
            .ok()?;
        Some(
            report
                .totals
                .input_tokens
                .saturating_add(report.totals.output_tokens),
        )
    }

    /// Known limits plus the auto-resume policy.
    pub async fn limits_report(&self) -> Result<h4rn_protocol::LimitsReport, ManagerError> {
        let meter = self
            .meter()
            .ok_or_else(|| ManagerError::Unsupported("metering is not enabled".into()))?;
        meter
            .limits_report()
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))
    }

    /// Persisted schedules and limit history.
    pub async fn resets(&self) -> Result<h4rn_protocol::Resets, ManagerError> {
        let meter = self
            .meter()
            .ok_or_else(|| ManagerError::Unsupported("metering is not enabled".into()))?;
        meter
            .resets()
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))
    }

    /// Replaces the daemon-wide auto-resume policy.
    pub fn set_auto_resume_policy(&self, policy: h4rn_protocol::AutoResumePolicy) {
        if let Some(meter) = self.meter() {
            meter.set_policy(policy);
        }
    }

    /// Cancels auto-resume for one thread.
    pub async fn cancel_resume(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<ResumeSchedule>, ManagerError> {
        let meter = self
            .meter()
            .ok_or_else(|| ManagerError::Unsupported("metering is not enabled".into()))?;
        let schedule = meter
            .cancel_schedule(thread_id)
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))?;
        if let Ok(handle) = self.inner.handle(thread_id).await {
            let mut record = handle.lock().await;
            record.scheduled_limit = None;
            record.resume_at = None;
            if record.state.phase == h4rn_core::Phase::RateLimited {
                self.inner
                    .emit(
                        &mut record,
                        vec![ThreadEvent::LimitAutoResumeSet {
                            auto_resume: false,
                            at: Utc::now(),
                        }],
                        Vec::new(),
                    )
                    .await?;
            }
        }
        Ok(schedule)
    }

    /// Fires a thread's schedule immediately.
    pub async fn resume_now(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<ResumeSchedule>, ManagerError> {
        let meter = self
            .meter()
            .ok_or_else(|| ManagerError::Unsupported("metering is not enabled".into()))?;
        let mut schedule = meter
            .schedules()
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))?
            .into_iter()
            .find(|schedule| &schedule.thread_id == thread_id)
            .ok_or_else(|| {
                ManagerError::NotFound(format!("thread {thread_id} has no resume schedule"))
            })?;
        if !matches!(
            schedule.state,
            ResumeState::Scheduled | ResumeState::Waiting
        ) {
            return Err(ManagerError::Conflict(format!(
                "thread {thread_id}'s resume schedule is already {:?}",
                schedule.state
            )));
        }
        schedule.resume_at = Utc::now();
        self.inner
            .fire_schedule(schedule)
            .await
            .map_err(ManagerError::Internal)
    }

    /// Where a limit-hit thread could fail over to, default first.
    ///
    /// The candidates come from the proxy profile that serves the thread's
    /// harness. With no such profile there is nothing to fail over to, and the
    /// reply says why so the UI can hide the action with a reason.
    pub async fn failover_options(
        &self,
        thread_id: &ThreadId,
    ) -> Result<h4rn_protocol::FailoverOptions, ManagerError> {
        let handle = self.inner.handle(thread_id).await?;
        let (harness, model) = {
            let record = handle.lock().await;
            (record.meta.harness, record.meta.model.clone())
        };
        let nothing = |reason: String| h4rn_protocol::FailoverOptions {
            thread_id: thread_id.clone(),
            current_model: model.clone(),
            default: None,
            options: Vec::new(),
            unavailable: Some(reason),
        };
        let Some(proxy) = self.inner.proxy.get() else {
            return Ok(nothing(
                "No proxy provider is configured on this host".into(),
            ));
        };
        let Some(applied) = proxy.override_for(harness, &model) else {
            return Ok(nothing(format!(
                "No proxy profile routes the `{}` harness through a provider",
                harness.slug()
            )));
        };
        let options = match proxy
            .eligible_failovers(
                &applied.profile_id,
                &model,
                &h4rn_protocol::ProxyFailoverPolicy::default(),
            )
            .await
        {
            Ok(options) => options,
            Err(error) => return Ok(nothing(format!("The proxy could not be read: {error}"))),
        };
        if options.is_empty() {
            return Ok(nothing(
                "Every other model is excluded or cooling down".into(),
            ));
        }
        Ok(h4rn_protocol::FailoverOptions {
            thread_id: thread_id.clone(),
            current_model: model,
            default: options.first().cloned(),
            options,
            unavailable: None,
        })
    }

    /// Resumes a limit-hit thread now on another model or account.
    ///
    /// `model` of `None` takes the daemon's default. A named model must be one
    /// of [`Self::failover_options`]; the schedule keeps the choice (shown on
    /// the card and in history) and fires immediately.
    pub async fn fail_over(
        &self,
        thread_id: &ThreadId,
        model: Option<String>,
        account: Option<String>,
    ) -> Result<Option<ResumeSchedule>, ManagerError> {
        let meter = self
            .meter()
            .ok_or_else(|| ManagerError::Unsupported("metering is not enabled".into()))?;
        let options = self.failover_options(thread_id).await?;
        let choice = match model {
            None => options.default.clone().ok_or_else(|| {
                ManagerError::Conflict(
                    options
                        .unavailable
                        .clone()
                        .unwrap_or_else(|| "there is no model to fail over to".into()),
                )
            })?,
            Some(model) => options
                .options
                .iter()
                .find(|candidate| {
                    candidate.model == model && (account.is_none() || candidate.account == account)
                })
                .cloned()
                .ok_or_else(|| {
                    ManagerError::InvalidRequest(format!(
                        "`{model}` is not an eligible failover for thread {thread_id}"
                    ))
                })?,
        };
        let mut schedule = meter
            .schedules()
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))?
            .into_iter()
            .find(|schedule| &schedule.thread_id == thread_id)
            .ok_or_else(|| {
                ManagerError::NotFound(format!("thread {thread_id} has no resume schedule"))
            })?;
        if !matches!(
            schedule.state,
            ResumeState::Scheduled | ResumeState::Waiting
        ) {
            return Err(ManagerError::Conflict(format!(
                "thread {thread_id}'s resume schedule is already {:?}",
                schedule.state
            )));
        }
        schedule.failover = Some(ResumeFailover {
            model: choice.model,
            account: choice.account,
            reason: format!("chosen now: {}", choice.reason),
        });
        schedule.resume_at = Utc::now();
        self.inner
            .fire_schedule(schedule)
            .await
            .map_err(ManagerError::Internal)
    }

    /// Stops the ticker, interrupts live turns and shuts every harness session.
    pub async fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        if let Some(handle) = self
            .inner
            .ticker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            handle.abort();
        }

        let handles: Vec<ThreadHandle> = {
            let threads = self.inner.threads.lock().await;
            threads.values().cloned().collect()
        };

        for handle in handles {
            let mut record = handle.lock().await;
            record.closed = true;
            if record.active.is_some() {
                let now = Utc::now();
                record.active = None;
                let domain = vec![ThreadEvent::TurnCompleted {
                    status: TurnStatus::Interrupted,
                    at: now,
                }];
                let wire = vec![Event::TurnCompleted {
                    thread_id: record.meta.thread_id.clone(),
                    turn_id: record.turn_id_for_event(),
                    usage: Usage::default(),
                    status: TurnStatus::Interrupted,
                    changes: None,
                }];
                let _ = self.inner.emit(&mut record, domain, wire).await;
            }
            if let Some(session) = record.session.take() {
                let mut session = session.lock().await;
                if let Err(error) = session.shutdown().await {
                    tracing::warn!(thread_id = %record.meta.thread_id, %error, "harness shutdown failed");
                }
            }
            if let Some(pump) = record.pump.take() {
                pump.abort();
            }
        }
    }
}

impl Inner {
    fn local_host_is(&self, host: &HostId) -> bool {
        &self.local_host == host
    }

    /// Resolves upload ids to committed blob metadata and materialises the
    /// per-thread copies the harness will see.
    async fn resolve_attachments(
        &self,
        thread_id: &ThreadId,
        ids: &[AttachmentId],
    ) -> Result<Vec<Attachment>, ManagerError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let Some(resolver) = &self.attachments else {
            return Err(ManagerError::Unsupported(
                "this daemon has no attachment store".into(),
            ));
        };
        resolver
            .resolve(thread_id, ids)
            .await
            .map_err(|error| match error {
                BlobError::NotFound(id) => ManagerError::InvalidRequest(format!(
                    "attachment {id} is no longer available; upload it again"
                )),
                other => ManagerError::Internal(other.to_string()),
            })
    }

    async fn handle(&self, thread_id: &ThreadId) -> Result<ThreadHandle, ManagerError> {
        self.threads
            .lock()
            .await
            .get(thread_id.as_str())
            .cloned()
            .ok_or_else(|| ManagerError::NotFound(format!("thread {thread_id} does not exist")))
    }

    /// Mirrors a child approval onto the parent thread's inbox.
    ///
    /// The parent's entry keeps a pointer back to the child so either side can
    /// answer; see [`ThreadManager::respond_approval`]. A missing parent (the
    /// parent was deleted, which the current daemon does not do) is ignored so
    /// the child can still proceed.
    async fn forward_approval(&self, request: ForwardedRequest) {
        let Ok(parent) = self.handle(&request.parent_thread_id).await else {
            return;
        };
        let mut record = parent.lock().await;
        record.pending_approvals.insert(
            request.parent_approval_id.clone(),
            PendingApproval {
                kind: request.kind,
                forward: Some(ForwardedApproval {
                    thread_id: request.child_thread_id.clone(),
                    approval_id: request.child_approval_id,
                }),
            },
        );
        let now = Utc::now();
        let domain = if request.kind == ApprovalKind::Question {
            vec![ThreadEvent::QuestionAsked { at: now }]
        } else {
            vec![ThreadEvent::ApprovalRequested { at: now }]
        };
        let wire = vec![Event::ApprovalRequested {
            thread_id: request.parent_thread_id,
            turn_id: record.turn_id_for_event(),
            approval_id: request.parent_approval_id,
            subagent_id: request.subagent_id,
            kind: request.kind,
            prompt: format!("[Subagent] {}", request.prompt),
            options: request.options,
        }];
        if let Err(error) = self.emit(&mut record, domain, wire).await {
            tracing::warn!(%error, "could not mirror a subagent approval onto its parent");
        }
    }

    /// Applies `domain` to the lifecycle state, appends both logs, broadcasts,
    /// and returns the new summary.
    async fn emit(
        &self,
        record: &mut ThreadRecord,
        domain: Vec<ThreadEvent>,
        mut wire: Vec<Event>,
    ) -> Result<(ThreadSummary, u64), ManagerError> {
        for event in &domain {
            record.state = record.state.clone().apply(event);
        }
        let summary = record.summary(&self.options.settings);
        wire.push(Event::ThreadUpserted(summary.clone()));
        record.last_signature = Some(SummarySignature::of(&summary));

        let _guard = self.emit_lock.lock().await;
        let seqs = self
            .journal
            .append(&record.meta, &domain, &wire)
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))?;
        self.broadcast(&record.meta.thread_id, &wire, &seqs);
        let seq = seqs.last().copied().unwrap_or(0);
        Ok((summary, seq))
    }

    fn broadcast(&self, thread_id: &ThreadId, wire: &[Event], seqs: &[u64]) {
        for (event, seq) in wire.iter().zip(seqs.iter()) {
            let _ = self.events.send(WireEvent {
                seq: *seq,
                thread_id: thread_id.clone(),
                event: event.clone(),
            });
        }
    }

    async fn restore(self: &Arc<Self>) -> Result<(), ManagerError> {
        let metas = self
            .journal
            .list_threads()
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))?;

        for meta in metas {
            let domain = self
                .journal
                .read_domain(&meta.thread_id)
                .await
                .map_err(|error| ManagerError::Internal(error.to_string()))?;
            let state = apply_all(ThreadState::new(meta.created_at), &domain);
            let branch = crate::fs::git::branch(std::path::Path::new(&meta.workspace_path));
            let mut record = ThreadRecord {
                meta,
                state,
                active: None,
                last_turn_id: None,
                session: None,
                pump: None,
                pending_approvals: BTreeMap::new(),
                queued: VecDeque::new(),
                last_signature: None,
                scheduled_limit: None,
                resume_at: None,
                limit: None,
                ttft_ms: None,
                context_window: None,
                branch,
                closed: false,
            };

            if record.state.phase.is_active() {
                // A daemon restart cannot resume an in-flight harness turn.
                // Surface that instead of leaving a phantom Working row.
                let wire = vec![Event::Error {
                    thread_id: Some(record.meta.thread_id.clone()),
                    message: "the daemon restarted while this turn was running".into(),
                    hint: Some("Send the message again to resume the work".into()),
                    retryable: true,
                }];
                self.emit(
                    &mut record,
                    vec![ThreadEvent::HostLost { at: Utc::now() }],
                    wire,
                )
                .await?;
            }

            record.last_signature = Some(record.signature(&self.options.settings));
            self.threads.lock().await.insert(
                record.meta.thread_id.as_str().to_owned(),
                Arc::new(Mutex::new(record)),
            );
        }
        Ok(())
    }

    fn spawn_session(self: &Arc<Self>, handle: ThreadHandle, first_input: Option<UserInput>) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            inner.start_session(handle, first_input).await;
        });
    }

    async fn start_session(self: &Arc<Self>, handle: ThreadHandle, first_input: Option<UserInput>) {
        let harness_kind = {
            let record = handle.lock().await;
            record.meta.harness
        };
        let (harness, spec) = {
            let record = handle.lock().await;
            let Some(harness) = self.harnesses.get(harness_kind) else {
                drop(record);
                self.fail_session(
                    handle,
                    format!("no {harness_kind:?} adapter is available in this daemon build"),
                )
                .await;
                return;
            };
            let injection = self
                .mcp
                .get()
                .and_then(|provider| provider.prepare(harness_kind, &record.meta.thread_id));
            let mut env_overrides = std::collections::BTreeMap::new();
            let mcp_endpoint = injection.as_ref().map(|session| session.endpoint.clone());
            if let Some(session) = injection {
                env_overrides.insert(h4rn_harness::ENV_MCP_AUTH_TOKEN.to_owned(), session.token);
            }
            let spec = SessionSpec {
                workspace: std::path::PathBuf::from(&record.meta.workspace_path),
                model: record.meta.model.clone(),
                runtime_mode: record.meta.runtime_mode,
                resume_id: record.meta.resume_id.clone(),
                mcp_endpoint,
                env_overrides,
                attachments: Vec::new(),
            };
            (harness, spec)
        };

        // CLIProxyAPI routing is applied only when the harness's toggle is on.
        // This is synchronous and offline: a probe never delays a session start.
        let mut spec = spec;
        let mut context_window = None;
        if let Some(proxy) = self.proxy.get()
            && let Some(applied) = proxy.override_for(harness_kind, &spec.model)
        {
            tracing::debug!(
                profile = %applied.profile_id,
                harness = ?harness_kind,
                model = %applied.model,
                verified = applied.verified,
                "routing a harness session through CLIProxyAPI"
            );
            // The cached catalogue is the only place a context window is
            // known; without it the footer simply omits the fill bar.
            context_window = proxy
                .cached_models(&applied.profile_id)
                .and_then(|catalog| {
                    catalog
                        .models
                        .into_iter()
                        .find(|model| model.id == applied.model)
                })
                .and_then(|model| model.context_length);
            spec.model = applied.model;
            spec.env_overrides = applied.env;
        }

        let mut session = match harness.start_session(spec).await {
            Ok(session) => session,
            Err(error) => {
                self.fail_session(handle, error.to_string()).await;
                return;
            }
        };

        let stream = session.events();
        let session: SharedSession = Arc::new(Mutex::new(session));
        let mut record = handle.lock().await;
        if record.closed || self.closed.load(Ordering::Relaxed) {
            drop(record);
            let _ = session.lock().await.shutdown().await;
            return;
        }
        record.session = Some(Arc::clone(&session));
        if context_window.is_some() {
            record.context_window = context_window;
        }
        let inner = Arc::clone(self);
        let pump_handle = handle.clone();
        record.pump = Some(tokio::spawn(async move {
            inner.run_pump(pump_handle, stream).await;
        }));
        drop(record);

        if let Some(input) = first_input {
            // The first prompt is written outside the record lock: a harness
            // that takes a while to accept it must not stall the thread.
            let sent = session.lock().await.send(input).await;
            if let Err(error) = sent {
                let mut record = handle.lock().await;
                let now = Utc::now();
                record.active = None;
                let wire = vec![Event::Error {
                    thread_id: Some(record.meta.thread_id.clone()),
                    message: format!("the harness rejected the message: {error}"),
                    hint: Some("Check the harness installation with `h4rnd doctor`".into()),
                    retryable: false,
                }];
                let _ = self
                    .emit(
                        &mut record,
                        vec![
                            ThreadEvent::FailureRaised { at: now },
                            ThreadEvent::TurnCompleted {
                                status: TurnStatus::Failed,
                                at: now,
                            },
                        ],
                        wire,
                    )
                    .await;
            }
        }
    }

    async fn fail_session(&self, handle: ThreadHandle, message: String) {
        let mut record = handle.lock().await;
        let now = Utc::now();
        record.active = None;
        let wire = vec![Event::Error {
            thread_id: Some(record.meta.thread_id.clone()),
            message: message.clone(),
            hint: Some("Install the harness or pick a different one, then retry".into()),
            retryable: false,
        }];
        let _ = self
            .emit(
                &mut record,
                vec![
                    ThreadEvent::FailureRaised { at: now },
                    ThreadEvent::TurnCompleted {
                        status: TurnStatus::Failed,
                        at: now,
                    },
                ],
                wire,
            )
            .await;
    }

    async fn deliver(self: &Arc<Self>, handle: ThreadHandle, input: UserInput) {
        let session = {
            let record = handle.lock().await;
            record.session.clone()
        };
        let Some(session) = session else {
            self.spawn_session(handle, Some(input));
            return;
        };
        // The record lock is released before the write: a harness that takes
        // seconds to accept input must not stall snapshots or settings calls
        // queued on the same connection.
        let sent = session.lock().await.send(input).await;
        if let Err(error) = sent {
            let mut record = handle.lock().await;
            let now = Utc::now();
            record.active = None;
            let wire = vec![Event::Error {
                thread_id: Some(record.meta.thread_id.clone()),
                message: format!("the harness rejected the message: {error}"),
                hint: Some("Check the harness installation with `h4rnd doctor`".into()),
                retryable: false,
            }];
            let _ = self
                .emit(
                    &mut record,
                    vec![
                        ThreadEvent::FailureRaised { at: now },
                        ThreadEvent::TurnCompleted {
                            status: TurnStatus::Failed,
                            at: now,
                        },
                    ],
                    wire,
                )
                .await;
        }
    }

    async fn run_pump(
        self: Arc<Self>,
        handle: ThreadHandle,
        mut stream: futures::stream::BoxStream<'static, HarnessEvent>,
    ) {
        while let Some(event) = stream.next().await {
            if self.closed.load(Ordering::Relaxed) {
                break;
            }
            let mut record = handle.lock().await;
            let now = Utc::now();
            let thread_id = record.meta.thread_id.clone();
            let mut domain = Vec::new();
            let mut wire = Vec::new();
            let mut completed = false;
            let mut forward_approval: Option<ForwardedRequest> = None;

            // `SessionStarted` normally carries the native id, but keep the
            // defaulted accessor in sync too so an adapter that only reports
            // it through `session_id` is still resumable after a restart.
            if record.meta.resume_id.is_none()
                && let Some(session_id) = record
                    .session
                    .as_ref()
                    .and_then(|session| session.try_lock().ok())
                    .and_then(|session| session.session_id())
            {
                record.meta.resume_id = Some(session_id);
            }

            match event {
                HarnessEvent::SessionStarted { session_id } => {
                    record.meta.resume_id = Some(session_id);
                }
                HarnessEvent::TextDelta { text } => {
                    if let Some(active) = record.active.as_mut() {
                        active.meter.first_token(now);
                        active.text.push_str(&text);
                        let turn_id = active.turn_id.clone();
                        let item_id = active.item_id.clone();
                        wire.push(Event::MessageDelta {
                            thread_id: thread_id.clone(),
                            turn_id,
                            item_id,
                            text,
                        });
                    }
                    if record.state.agent_state != AgentState::Thinking {
                        domain.push(ThreadEvent::AgentStateChanged {
                            state: AgentState::Thinking,
                            at: now,
                        });
                    }
                }
                HarnessEvent::ToolCall { tool_call } => {
                    if let Some(active) = record.active.as_mut() {
                        match tool_call.status {
                            ToolCallStatus::Running => {
                                if active.tools.insert(tool_call.tool_call_id.clone(), true)
                                    != Some(true)
                                {
                                    active.meter.tool_started(now);
                                }
                            }
                            ToolCallStatus::Completed
                            | ToolCallStatus::Failed
                            | ToolCallStatus::Denied => {
                                if active.tools.remove(&tool_call.tool_call_id).is_some() {
                                    active.meter.tool_finished(now);
                                }
                            }
                        }
                    }
                    let turn_id = record.turn_id_for_event();
                    wire.push(Event::ToolCall {
                        thread_id: thread_id.clone(),
                        turn_id: turn_id.clone(),
                        tool_call: tool_call.clone(),
                    });
                    // IDE mode's "diff vs agent changes": the first time a turn
                    // reports a file tool, capture the bytes it is about to
                    // change. The first observation wins, so the diff base is
                    // the pre-turn state even after several edits.
                    if tool_call.status == ToolCallStatus::Running
                        && let Some(store) = self.snapshots.get()
                    {
                        let mut paths = tool_call_file_paths(tool_call.input.as_ref());
                        if let Some(session) = record
                            .session
                            .as_ref()
                            .and_then(|session| session.try_lock().ok())
                        {
                            paths.extend(session.touched_paths());
                        }
                        paths.sort();
                        paths.dedup();
                        if !paths.is_empty() {
                            let workspace = std::path::PathBuf::from(&record.meta.workspace_path);
                            match store.capture(
                                &workspace,
                                &record.meta.thread_id,
                                &turn_id,
                                &paths,
                            ) {
                                Ok(0) => {}
                                Ok(count) => tracing::info!(
                                    thread_id = %thread_id,
                                    %turn_id,
                                    count,
                                    ?paths,
                                    "captured pre-turn file snapshots"
                                ),
                                Err(error) => tracing::warn!(
                                    thread_id = %thread_id,
                                    %error,
                                    "pre-turn snapshot capture failed"
                                ),
                            }
                        }
                    }
                    let desired = match tool_call.status {
                        ToolCallStatus::Running => AgentState::RunningTool,
                        ToolCallStatus::Completed => AgentState::Thinking,
                        ToolCallStatus::Failed | ToolCallStatus::Denied => AgentState::Error,
                    };
                    if record.state.agent_state != desired {
                        domain.push(ThreadEvent::AgentStateChanged {
                            state: desired,
                            at: now,
                        });
                    }
                }
                HarnessEvent::ApprovalRequested {
                    approval_id,
                    kind,
                    prompt,
                    options,
                } => {
                    let turn_id = record.turn_id_for_event();
                    let parent_thread_id = record.meta.parent_thread_id.clone();
                    let subagent_id = record.meta.subagent_id.clone();
                    let parent_approval_id = format!("sa:{approval_id}");
                    record.pending_approvals.insert(
                        approval_id.clone(),
                        PendingApproval {
                            kind,
                            forward: parent_thread_id.as_ref().map(|parent| ForwardedApproval {
                                thread_id: parent.clone(),
                                approval_id: parent_approval_id.clone(),
                            }),
                        },
                    );
                    if kind == ApprovalKind::Question {
                        domain.push(ThreadEvent::QuestionAsked { at: now });
                    } else {
                        domain.push(ThreadEvent::ApprovalRequested { at: now });
                    }
                    wire.push(Event::ApprovalRequested {
                        thread_id: thread_id.clone(),
                        turn_id,
                        approval_id: approval_id.clone(),
                        subagent_id,
                        kind,
                        prompt: prompt.clone(),
                        options: options.clone(),
                    });
                    if let Some(parent_thread_id) = parent_thread_id {
                        forward_approval = Some(ForwardedRequest {
                            parent_thread_id,
                            parent_approval_id,
                            child_thread_id: thread_id.clone(),
                            child_approval_id: approval_id,
                            subagent_id: record.meta.subagent_id.clone(),
                            kind,
                            prompt,
                            options,
                        });
                    }
                }
                HarnessEvent::Usage { usage } => {
                    if let Some(active) = record.active.as_mut() {
                        active.usage.merge(&usage);
                        active.meter.add_usage(&usage);
                    }
                }
                HarnessEvent::LimitHit {
                    reset_at,
                    auto_resume,
                } => {
                    record.limit = Some(LimitBrief {
                        harness: Some(record.meta.harness),
                        reset_at,
                        message: None,
                    });
                    if let Some(meter) = self.meter.get() {
                        let hit = LimitHit {
                            thread_id: Some(thread_id.clone()),
                            harness: Some(record.meta.harness),
                            account: None,
                            provider: provider_for_model(&record.meta.model),
                            model: Some(record.meta.model.clone()),
                            reset_at,
                            message: None,
                            consumed: None,
                            limit: None,
                            source: LimitSource::Harness,
                        };
                        if let Err(error) = meter.record_limit(hit).await {
                            tracing::warn!(%error, "could not record the usage limit");
                        }
                        if auto_resume {
                            let prompt = meter.policy().prompt;
                            match meter
                                .arm_schedule(&thread_id, reset_at, prompt, None, now)
                                .await
                            {
                                Ok(schedule) => {
                                    record.resume_at = Some(schedule.resume_at);
                                    record.scheduled_limit = Some(ScheduledLimit {
                                        reset_at,
                                        auto_resume,
                                    });
                                }
                                Err(error) => {
                                    let wire = vec![Event::Error {
                                        thread_id: Some(thread_id.clone()),
                                        message: "could not schedule the automatic resume".into(),
                                        hint: Some(format!(
                                            "The usage limit is still recorded; retry manually ({error})"
                                        )),
                                        retryable: true,
                                    }];
                                    let _ = self.emit(&mut record, Vec::new(), wire).await;
                                }
                            }
                        }
                        // The ledger row itself is written on TurnCompleted;
                        // the schedule survives the summary re-projection.
                    }
                    domain.push(ThreadEvent::UsageLimitHit {
                        reset_at,
                        auto_resume,
                        at: now,
                    });
                    wire.push(Event::UsageLimitHit {
                        thread_id: thread_id.clone(),
                        reset_at,
                        auto_resume,
                    });
                }
                HarnessEvent::TurnCompleted { status, usage } => {
                    completed = true;
                    // A completed turn clears a stale limit unless this turn
                    // armed an auto-resume: that schedule still explains why
                    // the thread is paused, so the row keeps its reset time.
                    if status == TurnStatus::Completed && record.scheduled_limit.is_none() {
                        record.limit = None;
                    }
                    // The summary footer shows TTFT after the turn ends, so
                    // read it from the live meter before the turn is taken.
                    if let Some(active) = record.active.as_ref()
                        && let Some(ttft) = active.meter.live(now).ttft_ms
                    {
                        record.ttft_ms = Some(ttft);
                    }
                    let mut active = record.active.take();
                    let (turn_id, item_id, text, turn_usage) = match &active {
                        Some(active) => (
                            active.turn_id.clone(),
                            active.item_id.clone(),
                            active.text.clone(),
                            active.usage.clone(),
                        ),
                        None => (
                            record.turn_id_for_event(),
                            fresh_item_id("assistant"),
                            String::new(),
                            Usage::default(),
                        ),
                    };
                    let usage = if usage.total_tokens() == 0
                        && usage.cost_micros == 0
                        && usage.duration_ms == 0
                    {
                        turn_usage.clone()
                    } else {
                        usage
                    };
                    // Fill gaps from the adapter's own turn report when the
                    // event carried no accounting (Claude reports it once).
                    let usage = if usage.total_tokens() == 0
                        && usage.cost_micros == 0
                        && usage.duration_ms == 0
                    {
                        record
                            .session
                            .as_ref()
                            .and_then(|session| session.try_lock().ok())
                            .and_then(|session| session.last_turn())
                            .map(|report| Usage {
                                cost_micros: report.cost_micros,
                                duration_ms: report.duration_ms,
                                ..Usage::default()
                            })
                            .unwrap_or(usage)
                    } else {
                        usage
                    };
                    record.meta.usage.merge(&usage);

                    // The ledger row is written before the turn completion is
                    // projected, so a client that reacts to `TurnCompleted`
                    // already sees the cost in `GetUsage`.
                    if let Some(meter) = self.meter.get() {
                        let context = TurnContext {
                            thread_id: Some(record.meta.thread_id.as_str().to_owned()),
                            parent_thread_id: record
                                .meta
                                .parent_thread_id
                                .as_ref()
                                .map(|parent| parent.as_str().to_owned()),
                            run_id: Some(turn_id.as_str().to_owned()),
                            workspace: Some(record.meta.workspace_path.clone()),
                            machine: Some(self.local_host.as_str().to_owned()),
                            account: None,
                            harness: Some(record.meta.harness.slug().to_owned()),
                            model: Some(record.meta.model.clone()),
                            provider: provider_for_model(&record.meta.model),
                            credential: None,
                        };
                        let metered = if let Some(active) = &mut active {
                            active.meter.set_final_usage(&usage);
                            Some(active.meter.finish(
                                now,
                                status,
                                meter.prices(),
                                &record.meta.model,
                            ))
                        } else {
                            let mut turn = TurnMeter::new(now);
                            turn.set_final_usage(&usage);
                            Some(turn.finish(now, status, meter.prices(), &record.meta.model))
                        };
                        if let Some(metered) = metered
                            && let Err(error) = meter.record_turn(context, &metered).await
                        {
                            tracing::warn!(%error, "could not write the usage ledger row");
                        }
                    }

                    domain.push(ThreadEvent::TurnCompleted { status, at: now });
                    // A limit hit earlier in this turn must survive the
                    // completion event: the thread stays Working/Scheduled
                    // until the scheduled resume starts.
                    if let Some(limit) = record.scheduled_limit.clone()
                        && limit.auto_resume
                    {
                        domain.push(ThreadEvent::UsageLimitHit {
                            reset_at: limit.reset_at,
                            auto_resume: true,
                            at: now,
                        });
                    }
                    if !text.is_empty() {
                        wire.push(Event::ItemCompleted {
                            thread_id: thread_id.clone(),
                            turn_id: turn_id.clone(),
                            item: Item::Message {
                                item_id,
                                role: MessageRole::Assistant,
                                text,
                                attachments: Vec::new(),
                            },
                        });
                    }
                    let changes = self.snapshots.get().map(|snapshots| {
                        let computed = snapshots.turn_changes(
                            std::path::Path::new(&record.meta.workspace_path),
                            &thread_id,
                            &turn_id,
                        );
                        tracing::info!(
                            thread_id = %thread_id,
                            %turn_id,
                            files = computed.files.len(),
                            "computed turn change set"
                        );
                        computed
                    });
                    wire.push(Event::TurnCompleted {
                        thread_id: thread_id.clone(),
                        turn_id,
                        usage,
                        status,
                        changes,
                    });
                }
                HarnessEvent::Error { message, retryable } => {
                    if record.active.is_some() {
                        domain.push(ThreadEvent::FailureRaised { at: now });
                    }
                    wire.push(Event::Error {
                        thread_id: Some(thread_id.clone()),
                        message,
                        hint: Some("Inspect the thread and retry when ready".into()),
                        retryable,
                    });
                }
            }

            if let Err(error) = self.emit(&mut record, domain, wire).await {
                tracing::error!(thread_id = %thread_id, %error, "failed to append harness event");
            }
            drop(record);

            if let Some(forwarded) = forward_approval {
                self.forward_approval(forwarded).await;
            }

            if completed {
                self.advance_queue(handle.clone()).await;
            }
        }

        let mut record = handle.lock().await;
        let stderr_tail = record
            .session
            .as_ref()
            .and_then(|session| session.try_lock().ok())
            .map(|session| session.stderr_tail())
            .unwrap_or_default();
        record.session = None;
        record.pump = None;
        if record.active.is_some() {
            let now = Utc::now();
            record.active = None;
            let turn_id = record.turn_id_for_event();
            let hint = if stderr_tail.trim().is_empty() {
                "Send the message again to resume".to_owned()
            } else {
                format!("Send the message again to resume. Harness stderr: {stderr_tail}")
            };
            let wire = vec![
                Event::Error {
                    thread_id: Some(record.meta.thread_id.clone()),
                    message: "the harness session ended before the turn completed".into(),
                    hint: Some(hint),
                    retryable: true,
                },
                Event::TurnCompleted {
                    thread_id: record.meta.thread_id.clone(),
                    turn_id,
                    usage: Usage::default(),
                    status: TurnStatus::Failed,
                    changes: None,
                },
            ];
            let _ = self
                .emit(
                    &mut record,
                    vec![
                        ThreadEvent::FailureRaised { at: now },
                        ThreadEvent::TurnCompleted {
                            status: TurnStatus::Failed,
                            at: now,
                        },
                    ],
                    wire,
                )
                .await;
        }
        drop(record);
        self.advance_queue(handle).await;
    }

    async fn advance_queue(self: &Arc<Self>, handle: ThreadHandle) {
        let (input, needs_session) = {
            let mut record = handle.lock().await;
            if record.closed || record.active.is_some() {
                return;
            }
            // A rate-limited thread keeps its queue until the scheduled resume
            // starts; otherwise the queued turn would hit the same limit now.
            if record.scheduled_limit.is_some() {
                return;
            }
            let Some(input) = record.queued.pop_front() else {
                return;
            };
            let now = Utc::now();
            let (turn_id, _) = record.begin_turn(now);
            let domain = vec![
                ThreadEvent::TurnQueued { at: now },
                ThreadEvent::TurnStarted { at: now },
            ];
            let wire = vec![Event::TurnStarted {
                thread_id: record.meta.thread_id.clone(),
                turn_id,
                started_at: now,
            }];
            if let Err(error) = self.emit(&mut record, domain, wire).await {
                tracing::error!(%error, "failed to start queued turn");
            }
            (input, record.session.is_none())
        };

        if needs_session {
            self.spawn_session(handle, Some(input));
        } else {
            // Never block the pump on a slow adapter write; the turn is
            // already recorded and its progress arrives as events.
            let this = Arc::clone(self);
            tokio::spawn(async move {
                this.deliver(handle, input).await;
            });
        }
    }

    /// Emits at most one `UsageTick` per second for each running turn.
    async fn emit_usage_ticks(&self) {
        let Some(meter) = self.meter.get().cloned() else {
            return;
        };
        let handles: Vec<ThreadHandle> = {
            let threads = self.threads.lock().await;
            threads.values().cloned().collect()
        };
        let now = Utc::now();
        for handle in handles {
            let mut record = handle.lock().await;
            let thread_id = record.meta.thread_id.clone();
            let Some(active) = record.active.as_mut() else {
                continue;
            };
            let due = active
                .last_tick
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(1));
            if !due {
                continue;
            }
            active.last_tick = Some(std::time::Instant::now());
            let live = active.meter.live(now);
            let run_id = Some(active.turn_id.as_str().to_owned());
            drop(record);
            meter.tick(&thread_id, run_id, live, now);
        }
    }

    /// Fires every schedule whose reset has passed.
    async fn fire_due_schedules(self: &Arc<Self>) {
        let Some(meter) = self.meter.get().cloned() else {
            return;
        };
        {
            let mut last = self
                .last_schedule_check
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if last.is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
                return;
            }
            *last = Some(std::time::Instant::now());
        }
        let due = match meter.due_schedules(Utc::now()).await {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!(%error, "could not read due auto-resume schedules");
                return;
            }
        };
        for schedule in due {
            if let Err(error) = self.fire_schedule(schedule).await {
                tracing::warn!(%error, "auto-resume failed");
            }
        }
    }

    /// Starts a scheduled thread's resumed turn.
    async fn fire_schedule(
        self: &Arc<Self>,
        mut schedule: ResumeSchedule,
    ) -> Result<Option<ResumeSchedule>, String> {
        let Some(meter) = self.meter.get().cloned() else {
            return Err("metering is not enabled".into());
        };
        let handle = match self.handle(&schedule.thread_id).await {
            Ok(handle) => handle,
            Err(error) => {
                let _ = meter
                    .finish_schedule(
                        schedule,
                        ResumeState::Cancelled,
                        Some(format!("the thread no longer exists: {error}")),
                    )
                    .await;
                return Ok(None);
            }
        };
        let mut record = handle.lock().await;
        if record.closed {
            let _ = meter
                .finish_schedule(
                    schedule,
                    ResumeState::Cancelled,
                    Some("the thread is closed".into()),
                )
                .await;
            return Ok(None);
        }
        if record.active.is_some() {
            let thread_id = record.meta.thread_id.clone();
            let _ = self
                .emit(
                    &mut record,
                    Vec::new(),
                    vec![Event::Error {
                        thread_id: Some(thread_id),
                        message: "auto-resume found the thread already running".into(),
                        hint: Some(
                            "The schedule was left in place; it retries on the next tick".into(),
                        ),
                        retryable: true,
                    }],
                )
                .await;
            return Ok(None);
        }

        // A failover choice is made when the reset fires, not when the limit is
        // observed: model cooldowns may have expired in the meantime.
        let policy = meter.policy();
        if schedule.failover.is_none()
            && policy.failover
            && let Some(proxy) = self.proxy.get()
            && let Some(applied) = proxy.override_for(record.meta.harness, &record.meta.model)
            && let Ok(Some(choice)) = proxy
                .pick_failover(
                    &applied.profile_id,
                    &record.meta.model,
                    &h4rn_protocol::ProxyFailoverPolicy::default(),
                )
                .await
        {
            schedule.failover = Some(ResumeFailover {
                model: choice.model,
                account: choice.account,
                reason: choice.reason,
            });
        }
        // Whoever chose it (the policy at reset time, or a person through
        // `FailOver`), a changed model needs a fresh session: the live one was
        // started on the limited model.
        if let Some(failover) = &schedule.failover
            && record.meta.model != failover.model
        {
            record.meta.model = failover.model.clone();
            if let Some(session) = record.session.take() {
                let mut session = session.lock().await;
                if let Err(error) = session.shutdown().await {
                    tracing::warn!(thread_id = %record.meta.thread_id, %error, "harness shutdown before failover failed");
                }
            }
            if let Some(pump) = record.pump.take() {
                pump.abort();
            }
        }

        let prompt = schedule.prompt.clone();
        let now = Utc::now();
        record.scheduled_limit = None;
        let (turn_id, _) = record.begin_turn(now);
        let domain = vec![
            ThreadEvent::MessageReceived {
                source: h4rn_core::MessageSource::User,
                at: now,
            },
            ThreadEvent::TurnQueued { at: now },
            ThreadEvent::TurnStarted { at: now },
        ];
        let wire = vec![
            Event::ItemCompleted {
                thread_id: record.meta.thread_id.clone(),
                turn_id: turn_id.clone(),
                item: Item::Message {
                    item_id: user_item_id(&turn_id),
                    role: MessageRole::User,
                    text: prompt.clone(),
                    attachments: Vec::new(),
                },
            },
            Event::TurnStarted {
                thread_id: record.meta.thread_id.clone(),
                turn_id,
                started_at: now,
            },
        ];
        if let Err(error) = self.emit(&mut record, domain, wire).await {
            let schedule = meter
                .finish_schedule(schedule, ResumeState::Waiting, Some(error.to_string()))
                .await
                .map_err(|error| error.to_string())?;
            return Ok(Some(schedule));
        }
        let needs_session = record.session.is_none();
        drop(record);

        let fired = meter
            .finish_schedule(schedule, ResumeState::Fired, None)
            .await
            .map_err(|error| error.to_string())?;
        let input = UserInput {
            text: prompt,
            attachments: Vec::new(),
        };
        if needs_session {
            self.spawn_session(handle, Some(input));
        } else {
            // Acknowledge the resume before the harness produces its first
            // token. `finish_schedule` above already emitted `ScheduleChanged`
            // and the thread events below report progress, so a slow first
            // send must not hold the `ResumeNow` request open (ADR-003).
            let this = Arc::clone(self);
            tokio::spawn(async move {
                this.deliver(handle, input).await;
            });
        }
        Ok(Some(fired))
    }

    async fn tick(self: &Arc<Self>) {
        let handles: Vec<ThreadHandle> = {
            let threads = self.threads.lock().await;
            threads.values().cloned().collect()
        };

        for handle in handles {
            if self.closed.load(Ordering::Relaxed) {
                return;
            }
            let mut record = handle.lock().await;
            let summary = record.summary(&self.options.settings);
            let signature = SummarySignature::of(&summary);
            if record.last_signature.as_ref() == Some(&signature) {
                continue;
            }
            record.last_signature = Some(signature);

            let wire = vec![Event::ThreadUpserted(summary)];
            let _guard = self.emit_lock.lock().await;
            match self
                .journal
                .append_wire(&record.meta.thread_id, &wire)
                .await
            {
                Ok(seqs) => self.broadcast(&record.meta.thread_id, &wire, &seqs),
                Err(error) => tracing::warn!(%error, "failed to append derived tab change"),
            }
        }

        self.emit_usage_ticks().await;
        self.fire_due_schedules().await;
    }
}

fn user_item_id(turn_id: &TurnId) -> String {
    format!("item_user_{}", turn_suffix(turn_id))
}

/// Converts committed attachments into the harness adapter's delivery refs.
fn harness_refs(attachments: &[Attachment]) -> Vec<AttachmentRef> {
    attachments
        .iter()
        .map(|attachment| AttachmentRef {
            id: attachment.id.clone(),
            path: std::path::PathBuf::from(&attachment.local_path),
            media_type: attachment.mime.clone(),
        })
        .collect()
}

fn assistant_item_id(turn_id: &TurnId) -> String {
    format!("item_assistant_{}", turn_suffix(turn_id))
}

fn turn_suffix(turn_id: &TurnId) -> &str {
    turn_id
        .as_str()
        .strip_prefix("turn_")
        .unwrap_or_else(|| turn_id.as_str())
}

fn fresh_item_id(role: &str) -> String {
    let id = ThreadId::new_uuid_v7();
    let suffix = id
        .as_str()
        .strip_prefix("th_")
        .unwrap_or_else(|| id.as_str());
    format!("item_{role}_{suffix}")
}

/// File paths a tool call names, for pre-turn snapshot capture.
///
/// Harnesses spell the field differently (`file_path`, `path`, `filePath`, a
/// `files` array, or `notebook_path`); this checks the shapes h4rn's adapters
/// emit and ignores anything else. Snapshot capture is best-effort, so a
/// missed spelling costs a diff base, not correctness.
fn tool_call_file_paths(input: Option<&serde_json::Value>) -> Vec<String> {
    let Some(serde_json::Value::Object(map)) = input else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for key in [
        "file_path",
        "filePath",
        "path",
        "filename",
        "notebook_path",
        "notebookPath",
    ] {
        if let Some(serde_json::Value::String(value)) = map.get(key) {
            paths.push(value.clone());
        }
    }
    for key in ["files", "paths", "file_paths"] {
        if let Some(serde_json::Value::Array(items)) = map.get(key) {
            for item in items {
                match item {
                    serde_json::Value::String(value) => paths.push(value.clone()),
                    serde_json::Value::Object(entry) => {
                        for inner in ["file_path", "filePath", "path"] {
                            if let Some(serde_json::Value::String(value)) = entry.get(inner) {
                                paths.push(value.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    paths
}

/// A best-effort provider label for dedupe and per-provider reporting.
fn provider_for_model(model: &str) -> Option<String> {
    let model = model.to_ascii_lowercase();
    if model.contains("claude") {
        Some("anthropic".into())
    } else if model.starts_with("gpt") || model.contains("openai") {
        Some("openai".into())
    } else if model.contains("gemini") {
        Some("google".into())
    } else if model.contains("grok") {
        Some("xai".into())
    } else {
        None
    }
}

fn title_from_prompt(prompt: &str, workspace: &str) -> String {
    let first = prompt.lines().next().unwrap_or_default().trim();
    if !first.is_empty() {
        return first.chars().take(80).collect();
    }
    std::path::Path::new(workspace)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Untitled thread".to_owned())
}

/// A detected harness for diagnostics, exposed through the manager's catalog.
pub fn detected_harness(
    catalog: &dyn crate::harnesses::HarnessCatalog,
    kind: HarnessKind,
) -> Option<HarnessInstall> {
    catalog.detect(kind)
}

/// An adapter's normalized event stream type, re-exported for tests.
pub type HarnessEventStream = futures::stream::BoxStream<'static, HarnessEvent>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_come_from_the_first_prompt_line_or_workspace() {
        assert_eq!(
            title_from_prompt("Fix the build\nand docs", "/tmp/repo"),
            "Fix the build"
        );
        assert_eq!(title_from_prompt("   ", "/tmp/repo"), "repo");
        assert_eq!(title_from_prompt("", "/"), "Untitled thread");
    }

    #[test]
    fn item_ids_are_stable_for_a_turn() {
        let turn = TurnId::new("turn_abc");
        assert_eq!(user_item_id(&turn), "item_user_abc");
        assert_eq!(assistant_item_id(&turn), "item_assistant_abc");
        assert!(fresh_item_id("user").starts_with("item_user_"));
    }
}
