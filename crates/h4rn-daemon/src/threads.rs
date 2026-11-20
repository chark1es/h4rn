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
    FsTurnChanges, HarnessKind, HostId, Item, LimitBrief, LimitSource, MessageRole, ProtoError,
    ProtoErrorCode, ResumeFailover, ResumeSchedule, ResumeState, RuntimeMode, SubagentId,
    ThreadFilter, ThreadId, ThreadSummary, ThreadTab, ToolCallStatus, TurnId, TurnStatus, Usage,
    UsageReport, WorktreeRequest,
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

/// The source of "now" for every lifecycle decision.
///
/// Production uses [`SystemClock`]. Tests inject a manual clock so a grace,
/// auto-settle or snooze window passes when the test says so, not when a wall
/// clock and a ticker happen to line up.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// The current instant.
    fn now(&self) -> DateTime<Utc>;
}

/// The wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Options for the manager.
#[derive(Clone, Debug)]
pub struct ThreadManagerOptions {
    /// Lifecycle timings (exit grace, auto-settle, stale bypass).
    pub settings: LifecycleSettings,
    /// How often derived placement is re-evaluated.
    pub tick_interval: Duration,
    /// Clock for derived placement. Defaults to the wall clock; tests pass a
    /// manual clock so timing assertions do not depend on wall-clock latency.
    pub clock: Option<Arc<dyn Clock>>,
}

impl Default for ThreadManagerOptions {
    fn default() -> Self {
        Self {
            settings: LifecycleSettings::default(),
            tick_interval: Duration::from_millis(250),
            clock: None,
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

/// What [`ThreadManager::restart_sessions`] did.
#[derive(Clone, Debug, Default)]
pub struct RestartedSessions {
    /// Threads whose live session was dropped.
    pub restarted: Vec<ThreadId>,
    /// The subset that had a running turn, which was interrupted.
    pub interrupted: Vec<ThreadId>,
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
    last_schedule_check: std::sync::Mutex<Option<DateTime<Utc>>>,
    /// The clock every lifecycle decision reads.
    clock: Arc<dyn Clock>,
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
    /// The thread's one harness session: none, being started, or live.
    ///
    /// The slot is what keeps two startups from running at once: a Send or a
    /// queued turn that finds it `Starting` leaves its prompt pending, and the
    /// startup that is already running delivers it once the session is live.
    session: SessionSlot,
    /// The last generation handed to a session startup. Every startup gets a
    /// fresh, larger number so a startup that returns late can prove the slot
    /// is still the one it claimed.
    session_gen: u64,
    pump: Option<JoinHandle<()>>,
    pending_approvals: BTreeMap<String, PendingApproval>,
    queued: VecDeque<UserInput>,
    last_signature: Option<SummarySignature>,
    /// The most recent turn whose failure was already published as one error
    /// row, so a second report of the same failure is a duplicate.
    turn_error_reported: Option<TurnId>,
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
    last_tick: Option<DateTime<Utc>>,
    /// The turn's one dispatch record, shared with every would-be writer and
    /// with Stop.
    dispatch: Arc<TurnDispatch>,
}

/// The thread's harness session, or the fact that one is being started.
///
/// Every startup gets its own generation ([`ThreadRecord::session_gen`]). A
/// startup that returns after the slot moved on (a restart, a shutdown, a
/// newer startup) must discard the session it made instead of attaching it,
/// and every path that clears a live session must name the generation it
/// inspected so it cannot detach a session it never saw.
enum SessionSlot {
    /// No session, and no startup in flight.
    None,
    /// A startup with this generation is in flight. Prompts stay pending; it
    /// delivers them.
    Starting(u64),
    /// A live session a prompt may be written to.
    Live {
        generation: u64,
        session: SharedSession,
    },
}

impl SessionSlot {
    fn live(&self) -> Option<&SharedSession> {
        match self {
            Self::Live { session, .. } => Some(session),
            Self::None | Self::Starting(_) => None,
        }
    }

    /// The generation this slot belongs to, when it has one.
    fn generation(&self) -> Option<u64> {
        match self {
            Self::None => None,
            Self::Starting(generation) | Self::Live { generation, .. } => Some(*generation),
        }
    }

    /// Whether this slot is still the startup for `generation`.
    fn is_starting(&self, generation: u64) -> bool {
        matches!(self, Self::Starting(current) if *current == generation)
    }

    fn is_live(&self, session: &SharedSession) -> bool {
        matches!(self, Self::Live { session: current, .. } if Arc::ptr_eq(current, session))
    }
}

/// One turn's dispatch state, created with the turn.
///
/// Every path that may write the turn's prompt (a fresh session's first input,
/// a queued turn, a send on a live session) goes through [`Self::claim`], and
/// Stop goes through [`Self::stop`]. Both take the one small mutex, so the
/// transition is atomic no matter which other locks the caller holds. Because
/// the claim and the write happen under the thread's session lock, and Stop
/// needs that same lock before sending a cancel, the two can never interleave
/// in the middle of a prompt.
struct TurnDispatch {
    turn_id: TurnId,
    /// The prompt this turn will write once it is claimed.
    input: UserInput,
    state: std::sync::Mutex<DispatchState>,
}

/// What happened to a turn's prompt so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DispatchState {
    /// Recorded and waiting for a session.
    Pending,
    /// A writer has taken it; the prompt is on its way to the harness.
    Claimed,
    /// Stop arrived before any writer claimed it. The prompt is dead.
    Stopped,
    /// The turn ended: whichever path settled it won, every later end is a
    /// no-op. Stop and failure therefore race to one winner, never two.
    Settled,
}

/// What a Stop did to a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopOutcome {
    /// The prompt had not been claimed, so it will never be written.
    StoppedBeforeSend,
    /// The prompt was claimed; the harness needs a cancel.
    AlreadySent,
    /// An earlier Stop already ended this dispatch.
    AlreadyStopped,
}

/// The facts a turn-end path hands to [`Inner::settle_turn`].
///
/// The settler owns everything a turn end publishes, so every path reaches
/// exactly one `TurnCompleted` and at most one error row.
struct TurnEnd {
    /// How the turn finished.
    status: TurnStatus,
    /// Usage for this turn.
    usage: Usage,
    /// Files this turn changed, when snapshots were recorded.
    changes: Option<FsTurnChanges>,
    /// Rows published just before the turn's `TurnCompleted` (an assistant
    /// message, or nothing).
    wire: Vec<Event>,
    /// Lifecycle events applied just after the turn's `TurnCompleted` (for
    /// example a preserved usage limit).
    domain: Vec<ThreadEvent>,
    /// The failure row this settlement wants to publish. It is emitted only
    /// when no other path already reported this turn's failure.
    error: Option<TurnError>,
}

impl TurnEnd {
    /// A turn that ended without an error row.
    fn new(status: TurnStatus, usage: Usage) -> Self {
        Self {
            status,
            usage,
            changes: None,
            wire: Vec::new(),
            domain: Vec::new(),
            error: None,
        }
    }

    /// A turn that ended because something failed.
    fn failed(message: String, hint: Option<String>, retryable: bool) -> Self {
        Self {
            error: Some(TurnError {
                message,
                hint,
                retryable,
            }),
            ..Self::new(TurnStatus::Failed, Usage::default())
        }
    }
}

/// One error row, the only one a turn may publish.
struct TurnError {
    message: String,
    hint: Option<String>,
    retryable: bool,
}

impl TurnDispatch {
    fn new(turn_id: TurnId, input: UserInput) -> Self {
        Self {
            turn_id,
            input,
            state: std::sync::Mutex::new(DispatchState::Pending),
        }
    }

    fn state(&self) -> DispatchState {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Claims the turn for delivery. `false` when Stop won or a writer already
    /// has it, in which case the caller must not write the prompt.
    fn claim(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *state == DispatchState::Pending {
            *state = DispatchState::Claimed;
            true
        } else {
            false
        }
    }

    /// Applies a Stop to this turn.
    fn stop(&self) -> StopOutcome {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match *state {
            DispatchState::Pending => {
                *state = DispatchState::Stopped;
                StopOutcome::StoppedBeforeSend
            }
            DispatchState::Claimed => StopOutcome::AlreadySent,
            DispatchState::Stopped | DispatchState::Settled => StopOutcome::AlreadyStopped,
        }
    }

    /// Ends this dispatch exactly once.
    ///
    /// `false` when another path already settled it; the caller must then
    /// publish nothing. This is the Stop-versus-failure race: whichever
    /// settles first wins, and the loser is a no-op.
    fn settle(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *state == DispatchState::Settled {
            false
        } else {
            *state = DispatchState::Settled;
            true
        }
    }

    /// The prompt, cloned for the one writer that claimed it.
    fn input(&self) -> UserInput {
        self.input.clone()
    }
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
    fn summary(&self, settings: &LifecycleSettings, now: DateTime<Utc>) -> ThreadSummary {
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
            context_fill_percent: self.meta.usage.context_fill_percent(self.context_window),
            needs_attention: needs_attention(&self.state, now, settings),
            unread: tab == ThreadTab::Done && !self.state.acknowledged,
            resume_at: self.resume_at,
            limit: self.limit.clone(),
        }
    }

    fn signature(&self, settings: &LifecycleSettings, now: DateTime<Utc>) -> SummarySignature {
        SummarySignature::of(&self.summary(settings, now))
    }

    /// Records a new turn and returns its dispatch.
    ///
    /// The dispatch carries the prompt, so the same object a Stop may stop is
    /// the one the eventual writer must claim.
    fn begin_turn(&mut self, at: DateTime<Utc>, input: UserInput) -> Arc<TurnDispatch> {
        // A new turn is a fresh attempt: the previous limit and any resume
        // target no longer describe the thread.
        self.resume_at = None;
        self.limit = None;
        self.turn_error_reported = None;
        self.branch = crate::fs::git::branch(std::path::Path::new(&self.meta.workspace_path));
        let turn_id = TurnId::new_uuid_v7();
        let item_id = assistant_item_id(&turn_id);
        let dispatch = Arc::new(TurnDispatch::new(turn_id.clone(), input));
        self.active = Some(ActiveTurn {
            turn_id: turn_id.clone(),
            item_id: item_id.clone(),
            text: String::new(),
            usage: Usage::default(),
            meter: TurnMeter::new(at),
            tools: HashMap::new(),
            last_tick: None,
            dispatch: Arc::clone(&dispatch),
        });
        self.last_turn_id = Some(turn_id);
        dispatch
    }

    /// Allocates the next session generation for a startup.
    fn next_session_gen(&mut self) -> u64 {
        self.session_gen = self.session_gen.saturating_add(1);
        self.session_gen
    }

    /// Removes the live session when the slot still holds `generation`.
    ///
    /// The caller holds the record lock, so the check and the clear are one
    /// step: a session started in the meantime (a newer generation) is never
    /// detached by an older path.
    fn detach_session(&mut self, generation: u64) -> Option<SharedSession> {
        match &self.session {
            SessionSlot::Live {
                generation: current,
                ..
            } if *current == generation => {}
            SessionSlot::None | SessionSlot::Starting(_) | SessionSlot::Live { .. } => {
                return None;
            }
        }
        match std::mem::replace(&mut self.session, SessionSlot::None) {
            SessionSlot::Live { session, .. } => Some(session),
            SessionSlot::None | SessionSlot::Starting(_) => None,
        }
    }

    /// Marks `turn_id` as the turn whose one failure row was already published.
    ///
    /// `false` means that row exists, so this report is a duplicate.
    fn note_error(&mut self, turn_id: &TurnId) -> bool {
        if self.turn_error_reported.as_ref() == Some(turn_id) {
            return false;
        }
        self.turn_error_reported = Some(turn_id.clone());
        true
    }

    /// The live session, when there is one.
    fn live_session(&self) -> Option<&SharedSession> {
        self.session.live()
    }

    /// The dispatch of `turn_id`, if it is still the active turn.
    fn dispatch_for(&self, turn_id: &TurnId) -> Option<Arc<TurnDispatch>> {
        self.active
            .as_ref()
            .filter(|active| &active.turn_id == turn_id)
            .map(|active| Arc::clone(&active.dispatch))
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
        let clock = options
            .clock
            .clone()
            .unwrap_or_else(|| Arc::new(SystemClock));
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
                clock,
            }),
        };
        manager.inner.restore().await?;
        Ok(manager)
    }

    /// Subscribes to the live projected event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<WireEvent> {
        self.inner.events.subscribe()
    }

    /// The manager's clock. Every lifecycle decision reads this so a test can
    /// move time without sleeping.
    fn now(&self) -> DateTime<Utc> {
        self.inner.clock.now()
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
        Some(record.summary(&self.inner.options.settings, self.now()))
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
                let now = self.now();
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
        let now = self.now();
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
    pub async fn create_thread(
        &self,
        mut request: NewThread,
    ) -> Result<CreatedThread, ManagerError> {
        request.workspace_path = expand_home(&request.workspace_path);
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

        let now = self.now();
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
            session: SessionSlot::None,
            session_gen: 0,
            pump: None,
            pending_approvals: BTreeMap::new(),
            queued: VecDeque::new(),
            last_signature: None,
            turn_error_reported: None,
            scheduled_limit: None,
            resume_at: None,
            limit: None,
            ttft_ms: None,
            context_window: None,
            branch: None,
            closed: false,
        };
        let dispatch = record.begin_turn(
            now,
            UserInput {
                text: request.prompt.clone(),
                attachments: harness_attachments,
            },
        );
        let turn_id = dispatch.turn_id.clone();

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

        self.inner.spawn_session(handle);
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

        let now = self.now();
        if let Some(active_turn_id) = record.active.as_ref().map(|active| active.turn_id.clone()) {
            // A turn is running: the message waits its turn, and the thread
            // stays Working. `MessageReceived` records the user activity. Any
            // armed resume is now stale (the new message is the next turn), so
            // cancel it rather than let it fire a duplicate prompt.
            if record.scheduled_limit.take().is_some()
                && let Some(meter) = self.inner.meter.get()
            {
                let _ = meter.cancel_schedule(thread_id).await;
            }
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

        let dispatch = record.begin_turn(
            now,
            UserInput {
                text: text.clone(),
                attachments: harness_attachments,
            },
        );
        let turn_id = dispatch.turn_id.clone();
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

        let live = record.live_session().cloned();
        drop(record);
        match live {
            Some(session) => {
                // The message and its turn are already recorded; let the
                // adapter accept the text in the background so the connection
                // can answer other requests (snapshots, listings, settings)
                // meanwhile.
                let this = Arc::clone(&self.inner);
                tokio::spawn(async move {
                    this.deliver(handle, dispatch, session).await;
                });
            }
            // No session yet, or one is still starting: that startup claims
            // the turn once it is live and writes this prompt then.
            None => self.inner.spawn_session(handle),
        }
        Ok(AcceptedMessage {
            thread_id: thread_id.clone(),
            turn_id,
            seq,
        })
    }

    /// Whether any thread on `kind` has a turn in flight.
    ///
    /// Updating a harness CLI replaces the binary a live session is running, so
    /// the update is refused until every turn on that harness has ended.
    pub async fn harness_busy(&self, kind: HarnessKind) -> bool {
        let handles: Vec<ThreadHandle> = {
            let threads = self.inner.threads.lock().await;
            threads.values().cloned().collect()
        };
        for handle in handles {
            let record = handle.lock().await;
            if record.meta.harness == kind && record.active.is_some() {
                return true;
            }
        }
        false
    }

    /// Drops the live harness session of every thread on one of `harnesses`,
    /// so its next message starts a session with the current proxy routing.
    ///
    /// A running turn is interrupted first, as if the person had pressed stop.
    /// A thread whose turn does not end within a few seconds keeps its session
    /// and is left out of the result.
    pub async fn restart_sessions(&self, harnesses: &[HarnessKind]) -> RestartedSessions {
        let handles: Vec<ThreadHandle> = {
            let threads = self.inner.threads.lock().await;
            threads.values().cloned().collect()
        };
        let mut result = RestartedSessions::default();
        for handle in handles {
            let (thread_id, was_running, generation) = {
                let record = handle.lock().await;
                if !harnesses.contains(&record.meta.harness) {
                    continue;
                }
                // Only a live session is this restart's to drop: a startup in
                // flight is already building a new one.
                let Some(generation) = record.session.generation() else {
                    continue;
                };
                if record.session.live().is_none() {
                    continue;
                }
                (
                    record.meta.thread_id.clone(),
                    record.active.is_some(),
                    generation,
                )
            };
            if was_running {
                if self.interrupt(&thread_id).await.is_err() {
                    continue;
                }
                let mut ended = false;
                for _ in 0..30 {
                    if handle.lock().await.active.is_none() {
                        ended = true;
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                if !ended {
                    continue;
                }
            }
            let detached = {
                let mut record = handle.lock().await;
                // Clear the session this restart inspected, and only that one:
                // a turn that ended while we waited may already have started a
                // fresh session, which belongs to its own startup.
                record
                    .detach_session(generation)
                    .map(|session| (session, record.pump.take()))
            };
            let Some((session, pump)) = detached else {
                continue;
            };
            if let Err(error) = session.lock().await.shutdown().await {
                tracing::warn!(%thread_id, %error, "harness shutdown before a proxy restart failed");
            }
            if let Some(pump) = pump {
                pump.abort();
            }
            if was_running {
                result.interrupted.push(thread_id.clone());
            }
            result.restarted.push(thread_id);
        }
        result
    }

    /// Interrupts the running turn.
    ///
    /// Stop targets one turn, never "whatever is running now". The turn's
    /// dispatch decides, atomically, whether the prompt can still be dropped
    /// (it was never claimed, including while the session is starting) or
    /// whether the harness needs a cancel (a writer already claimed it). A Stop
    /// that arrives after its turn ended is a no-op: it must not cancel the
    /// turn that took its place.
    pub async fn interrupt(&self, thread_id: &ThreadId) -> Result<(), ManagerError> {
        let handle = self.inner.handle(thread_id).await?;
        let (turn_id, dispatch) = {
            let record = handle.lock().await;
            let Some(active) = record.active.as_ref() else {
                return Err(ManagerError::Conflict(format!(
                    "thread {thread_id} has no running turn"
                )));
            };
            (active.turn_id.clone(), Arc::clone(&active.dispatch))
        };
        match dispatch.stop() {
            StopOutcome::StoppedBeforeSend => {
                // Whether the slot is None, Starting or Live, no prompt was
                // written and none will be: end the turn and move on.
                self.finish_stopped_turn(handle, turn_id).await;
                Ok(())
            }
            StopOutcome::AlreadyStopped => Ok(()),
            StopOutcome::AlreadySent => self.cancel_claimed_turn(handle, turn_id).await,
        }
    }

    /// Sends the harness cancel for a turn whose prompt is on the wire.
    ///
    /// The session lock is taken before the final check, so a prompt claimed by
    /// a later turn cannot slip in between the check and the cancel: delivery
    /// claims under that same lock.
    async fn cancel_claimed_turn(
        &self,
        handle: ThreadHandle,
        turn_id: TurnId,
    ) -> Result<(), ManagerError> {
        let shared = {
            let record = handle.lock().await;
            match record.session.live() {
                Some(session) if record.dispatch_for(&turn_id).is_some() => {
                    Some(Arc::clone(session))
                }
                _ => None,
            }
        };
        let Some(shared) = shared else {
            // The turn already ended (or its session went away); nothing to
            // cancel.
            return Ok(());
        };
        let mut session = shared.lock().await;
        {
            let record = handle.lock().await;
            if record.dispatch_for(&turn_id).is_none() || !record.session.is_live(&shared) {
                // The turn ended and another one may now own this session.
                return Ok(());
            }
        }
        match session.interrupt().await {
            Ok(()) => Ok(()),
            // A Stop that races the prompt write's timeout: the transport is
            // already dead, so there is no cancel to deliver and nothing the
            // person needs to be told.
            Err(error) if session.is_closed() => {
                tracing::debug!(%error, "the session died before the stop reached it");
                Ok(())
            }
            Err(error) => Err(ManagerError::Internal(error.to_string())),
        }
    }

    /// Ends a turn whose prompt was stopped before it reached the harness.
    async fn finish_stopped_turn(&self, handle: ThreadHandle, turn_id: TurnId) {
        self.inner
            .settle_turn(
                &handle,
                &turn_id,
                TurnEnd {
                    status: TurnStatus::Interrupted,
                    usage: Usage::default(),
                    changes: None,
                    wire: Vec::new(),
                    domain: Vec::new(),
                    error: None,
                },
            )
            .await;
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
                target_record.live_session().cloned()
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
                let now = self.now();
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
            at: self.now(),
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
            at: self.now(),
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
            record.summary(&self.inner.options.settings, self.now())
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
            let summary = record.summary(&self.inner.options.settings, self.now());
            let snoozed = is_snoozed(&record.state, self.now());
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
        // Take the record lock before touching the ledger. A fire claims its
        // attempt under this lock, so the lock orders the two conditional
        // writes: either the cancellation lands first and the fire's claim
        // fails, or the fire has claimed and the cancellation is too late.
        let handle = self.inner.handle(thread_id).await.ok();
        let mut record = match &handle {
            Some(handle) => Some(handle.lock().await),
            None => None,
        };
        let schedule = meter
            .cancel_schedule(thread_id)
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))?;
        if let Some(record) = record.as_mut() {
            record.scheduled_limit = None;
            record.resume_at = None;
            if record.state.phase == h4rn_core::Phase::RateLimited {
                self.inner
                    .emit(
                        record,
                        vec![ThreadEvent::LimitAutoResumeSet {
                            auto_resume: false,
                            at: self.now(),
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
        schedule.resume_at = self.now();
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
            account_label: choice.account_label,
            reason: format!("chosen now: {}", choice.reason),
        });
        schedule.resume_at = self.now();
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
            let (thread_id, session, pump) = {
                let mut record = handle.lock().await;
                record.closed = true;
                if let Some(turn_id) = record.active.as_ref().map(|active| active.turn_id.clone()) {
                    let _ = self
                        .inner
                        .settle_turn_locked(
                            &mut record,
                            &turn_id,
                            TurnEnd::new(TurnStatus::Interrupted, Usage::default()),
                        )
                        .await;
                }
                let thread_id = record.meta.thread_id.clone();
                let session = record
                    .session
                    .generation()
                    .and_then(|generation| record.detach_session(generation));
                let pump = if session.is_some() {
                    record.pump.take()
                } else {
                    None
                };
                (thread_id, session, pump)
            };
            if let Some(session) = session {
                // The record lock is released first: a lock order of
                // record-then-session would deadlock with a Stop.
                let mut session = session.lock().await;
                if let Err(error) = session.shutdown().await {
                    tracing::warn!(%thread_id, %error, "harness shutdown failed");
                }
            }
            if let Some(pump) = pump {
                pump.abort();
            }
        }
    }
}

impl Inner {
    /// The clock every lifecycle decision reads.
    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

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
        let now = self.now();
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
        let summary = record.summary(&self.options.settings, self.now());
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
                session: SessionSlot::None,
                session_gen: 0,
                pump: None,
                pending_approvals: BTreeMap::new(),
                queued: VecDeque::new(),
                last_signature: None,
                turn_error_reported: None,
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
                    vec![ThreadEvent::HostLost { at: self.now() }],
                    wire,
                )
                .await?;
            }

            record.last_signature = Some(record.signature(&self.options.settings, self.now()));
            self.threads.lock().await.insert(
                record.meta.thread_id.as_str().to_owned(),
                Arc::new(Mutex::new(record)),
            );
        }
        Ok(())
    }

    /// Ends `turn_id` exactly once.
    ///
    /// Every path that can end a turn comes here: the pump's completion, Stop
    /// before a prompt was written, a failed prompt write, a failed startup and
    /// a dead session. Under the record lock it checks the turn is still the
    /// active one, closes its dispatch, publishes exactly one `TurnCompleted`
    /// (and at most one error row) and promotes the next queued message before
    /// releasing the lock, so a Send that arrives meanwhile queues behind that
    /// message instead of jumping ahead of it. The promoted turn's delivery or
    /// startup is spawned after the lock is released.
    async fn settle_turn(
        self: &Arc<Self>,
        handle: &ThreadHandle,
        turn_id: &TurnId,
        end: TurnEnd,
    ) -> bool {
        let mut record = handle.lock().await;
        let settled = self.settle_turn_locked(&mut record, turn_id, end).await;
        drop(record);
        if settled {
            self.spawn_advance(handle.clone());
        }
        settled
    }

    /// [`Self::settle_turn`] for a caller that already holds the record lock.
    async fn settle_turn_locked(
        &self,
        record: &mut ThreadRecord,
        turn_id: &TurnId,
        end: TurnEnd,
    ) -> bool {
        let Some(active) = record.active.as_ref() else {
            return false;
        };
        if &active.turn_id != turn_id {
            // Someone else settled this turn (or a later one is running).
            return false;
        }
        if !active.dispatch.settle() {
            return false;
        }

        let now = self.now();
        let TurnEnd {
            status,
            usage,
            changes,
            mut wire,
            domain: mut extra_domain,
            error,
        } = end;
        let mut domain = Vec::new();
        if let Some(error) = error
            && record.note_error(turn_id)
        {
            domain.push(ThreadEvent::FailureRaised { at: now });
            wire.push(Event::Error {
                thread_id: Some(record.meta.thread_id.clone()),
                message: error.message,
                hint: error.hint,
                retryable: error.retryable,
            });
        }
        record.active = None;
        domain.push(ThreadEvent::TurnCompleted { status, at: now });
        // Events that must survive the completion (a usage limit that keeps the
        // thread scheduled) are applied after it, exactly as the lifecycle
        // projection orders them.
        domain.append(&mut extra_domain);
        wire.push(Event::TurnCompleted {
            thread_id: record.meta.thread_id.clone(),
            turn_id: turn_id.clone(),
            usage,
            status,
            changes,
        });

        // Promote the next queued message under this same lock: a Send that
        // arrives now must queue behind it, not become the active turn first.
        if !record.closed
            && record.scheduled_limit.is_none()
            && let Some(input) = record.queued.pop_front()
        {
            let next = record.begin_turn(now, input);
            let next_id = next.turn_id.clone();
            domain.push(ThreadEvent::TurnQueued { at: now });
            domain.push(ThreadEvent::TurnStarted { at: now });
            wire.push(Event::TurnStarted {
                thread_id: record.meta.thread_id.clone(),
                turn_id: next_id,
                started_at: now,
            });
        }

        if let Err(error) = self.emit(record, domain, wire).await {
            tracing::error!(%error, "failed to record a settled turn");
        }
        true
    }

    /// Starts a session for the thread unless it already has one.
    ///
    /// The startup is the only path that may create a session: when the slot is
    /// not `None` this returns immediately and leaves the turn to the startup
    /// that is already running, so a Send and a queued turn can never start two
    /// agents for one thread.
    fn spawn_session(self: &Arc<Self>, handle: ThreadHandle) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            inner.start_session(handle).await;
        });
    }

    async fn start_session(self: &Arc<Self>, handle: ThreadHandle) {
        // Build the spec, mark the slot and capture the turn this startup
        // carries under one lock. The generation is what lets this startup
        // prove, after the slow `start_session` call, that the slot is still
        // the one it claimed; the turn is used as captured, never re-read from
        // "whatever is active then".
        let (harness, mut spec, generation, turn) = {
            let mut record = handle.lock().await;
            if record.closed || self.closed.load(Ordering::Relaxed) {
                return;
            }
            if !matches!(record.session, SessionSlot::None) {
                // A session is live, or one is already starting and will
                // deliver this turn's prompt when it is.
                return;
            }
            let harness_kind = record.meta.harness;
            let Some(harness) = self.harnesses.get(harness_kind) else {
                drop(record);
                self.fail_session(
                    handle,
                    None,
                    None,
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
            let turn = record
                .active
                .as_ref()
                .map(|active| Arc::clone(&active.dispatch));
            let generation = record.next_session_gen();
            record.session = SessionSlot::Starting(generation);
            (harness, spec, generation, turn)
        };

        // CLIProxyAPI routing is applied only when the harness's toggle is on.
        // This is synchronous and offline: a probe never delays a session start.
        let mut context_window = None;
        if let Some(proxy) = self.proxy.get()
            && let Some(applied) = proxy.override_for(harness.kind(), &spec.model)
        {
            tracing::debug!(
                profile = %applied.profile_id,
                harness = ?harness.kind(),
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
                self.fail_session(handle, Some(generation), turn, error.to_string())
                    .await;
                return;
            }
        };

        let stream = session.events();
        let session: SharedSession = Arc::new(Mutex::new(session));
        {
            let mut record = handle.lock().await;
            if record.closed
                || self.closed.load(Ordering::Relaxed)
                || !record.session.is_starting(generation)
            {
                // The thread was restarted or shut down while the harness was
                // booting: this session belongs to nobody, and no turn is
                // touched on its behalf.
                drop(record);
                let _ = session.lock().await.shutdown().await;
                return;
            }
            record.session = SessionSlot::Live {
                generation,
                session: Arc::clone(&session),
            };
            if context_window.is_some() {
                record.context_window = context_window;
            }
            let inner = Arc::clone(self);
            let pump_handle = handle.clone();
            let pump_session = Arc::clone(&session);
            record.pump = Some(tokio::spawn(async move {
                inner
                    .run_pump(pump_handle, generation, pump_session, stream)
                    .await;
            }));
        }

        // Claim the turn this startup carried: a Stop that won during startup
        // left its dispatch stopped, so its prompt is dropped and the queue
        // moves on instead.
        if let Some(turn) = turn {
            let still_active = {
                let record = handle.lock().await;
                record.dispatch_for(&turn.turn_id).is_some()
            };
            if still_active {
                self.deliver(handle.clone(), turn, Arc::clone(&session))
                    .await;
            }
        }
        // The turn ended while the harness was booting (or nothing was active),
        // so deliver whatever is waiting now on this live session.
        self.advance_queue(handle).await;
    }

    /// Ends the active turn when a startup for `generation` failed.
    ///
    /// A failure from a startup that no longer owns the slot (a restart, a
    /// shutdown) changes nothing: that startup's turn belongs to another path.
    async fn fail_session(
        self: &Arc<Self>,
        handle: ThreadHandle,
        generation: Option<u64>,
        turn: Option<Arc<TurnDispatch>>,
        message: String,
    ) {
        let turn_id = {
            let mut record = handle.lock().await;
            if let Some(generation) = generation {
                if !record.session.is_starting(generation) {
                    return;
                }
                record.session = SessionSlot::None;
            }
            match &turn {
                Some(turn) => record
                    .dispatch_for(&turn.turn_id)
                    .map(|_| turn.turn_id.clone()),
                None => record.active.as_ref().map(|active| active.turn_id.clone()),
            }
        };
        if let Some(turn_id) = turn_id {
            self.settle_turn(
                &handle,
                &turn_id,
                TurnEnd::failed(
                    message,
                    Some("Install the harness or pick a different one, then retry".into()),
                    false,
                ),
            )
            .await;
        }
    }

    /// Ends `turn_id` after its prompt could not be written, at most once.
    ///
    /// The pump can notice the same dead session, and Stop or a queued turn can
    /// end this one first, so the settlement itself decides whether this path
    /// still owns the turn. A write error that killed the transport detaches
    /// the session (by generation) and shuts it down: the next message then
    /// starts a fresh one instead of writing into a closed pipe.
    async fn fail_prompt_write(
        self: &Arc<Self>,
        handle: ThreadHandle,
        turn_id: &TurnId,
        session: &SharedSession,
        message: String,
    ) {
        let dead = session.lock().await.is_closed();
        let mut detached = None;
        if dead {
            let mut record = handle.lock().await;
            if record.session.is_live(session)
                && let Some(generation) = record.session.generation()
            {
                detached = record.detach_session(generation);
                record.pump = None;
            }
        }
        // A dead transport also reports its own failure on the harness stream;
        // the settlement's row is deduplicated against it. If that report
        // arrives late, this is the one row, and it lands before the turn's
        // completion either way.
        let end = TurnEnd::failed(
            format!("the harness rejected the message: {message}"),
            Some("Check the harness installation with `h4rnd doctor`".into()),
            false,
        );
        self.settle_turn(&handle, turn_id, end).await;
        if let Some(detached) = detached {
            let _ = detached.lock().await.shutdown().await;
        }
    }

    /// Runs [`Self::advance_queue`] on its own task.
    ///
    /// The future is erased through a `dyn Future + Send` so the compiler does
    /// not have to reason about the mutually recursive shapes of "deliver ->
    /// fail write -> advance queue -> deliver".
    fn spawn_advance(self: &Arc<Self>, handle: ThreadHandle) {
        let this = Arc::clone(self);
        let future: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            Box::pin(async move {
                this.advance_queue(handle).await;
            });
        tokio::spawn(future);
    }

    /// Writes a turn's prompt on the session that is attached now.
    ///
    /// The session lock is held across the claim and the write, so a Stop is
    /// ordered either before the claim (where it drops the prompt) or after it
    /// is on the wire (where a cancel works). The claim is the only authority on
    /// whether the prompt may be written.
    async fn deliver(
        self: &Arc<Self>,
        handle: ThreadHandle,
        dispatch: Arc<TurnDispatch>,
        session: SharedSession,
    ) {
        if !self
            .owns_attached_session(&handle, &dispatch, &session)
            .await
        {
            self.recover_delivery(&handle, &dispatch).await;
            return;
        }
        let mut guard = session.lock().await;
        if !self
            .owns_attached_session(&handle, &dispatch, &session)
            .await
        {
            // The turn ended, or its session was replaced, while this waited
            // for the session lock.
            drop(guard);
            self.recover_delivery(&handle, &dispatch).await;
            return;
        }
        if !dispatch.claim() {
            return;
        }
        let turn_id = dispatch.turn_id.clone();
        let sent = guard.send(dispatch.input()).await;
        drop(guard);
        if let Err(error) = sent {
            self.fail_prompt_write(handle, &turn_id, &session, error.to_string())
                .await;
        }
    }

    /// Whether `dispatch` is still the active turn on `session`'s slot.
    async fn owns_attached_session(
        &self,
        handle: &ThreadHandle,
        dispatch: &Arc<TurnDispatch>,
        session: &SharedSession,
    ) -> bool {
        let record = handle.lock().await;
        record.dispatch_for(&dispatch.turn_id).is_some() && record.session.is_live(session)
    }

    /// Gets a still-pending turn onto a session after its delivery found the
    /// session detached.
    ///
    /// Returning here would leave the active turn Working forever: the startup
    /// that detached the slot never delivered it, and no turn end will come.
    /// A startup that is already in flight owns the turn instead.
    async fn recover_delivery(
        self: &Arc<Self>,
        handle: &ThreadHandle,
        dispatch: &Arc<TurnDispatch>,
    ) {
        let start = {
            let record = handle.lock().await;
            !record.closed
                && record.dispatch_for(&dispatch.turn_id).is_some()
                && dispatch.state() == DispatchState::Pending
                && matches!(record.session, SessionSlot::None)
        };
        if start {
            self.spawn_session(handle.clone());
        }
    }

    /// Delivers the active turn's prompt when it is still pending.
    ///
    /// Spawns the write so a slow adapter can never hold the caller (the pump,
    /// a Stop, a startup) for the length of one prompt.
    async fn schedule_active(self: &Arc<Self>, handle: &ThreadHandle) {
        let (session, dispatch) = {
            let record = handle.lock().await;
            let dispatch = record
                .active
                .as_ref()
                .map(|active| Arc::clone(&active.dispatch));
            (record.live_session().cloned(), dispatch)
        };
        let (Some(session), Some(dispatch)) = (session, dispatch) else {
            return;
        };
        if dispatch.state() != DispatchState::Pending {
            return;
        }
        let this = Arc::clone(self);
        let handle = handle.clone();
        tokio::spawn(async move {
            this.deliver(handle, dispatch, session).await;
        });
    }

    async fn run_pump(
        self: Arc<Self>,
        handle: ThreadHandle,
        generation: u64,
        session: SharedSession,
        mut stream: futures::stream::BoxStream<'static, HarnessEvent>,
    ) {
        while let Some(event) = stream.next().await {
            if self.closed.load(Ordering::Relaxed) {
                break;
            }
            // Sampling the transport's health needs the session lock, so do it
            // before the record lock (the same order Stop and delivery use).
            let session_closed = if matches!(&event, HarnessEvent::TurnCompleted { .. }) {
                session.lock().await.is_closed()
            } else {
                false
            };
            let mut record = handle.lock().await;
            let now = self.now();
            let thread_id = record.meta.thread_id.clone();
            let mut domain = Vec::new();
            let mut wire = Vec::new();
            let mut detached: Option<SharedSession> = None;
            let mut settled = false;
            let mut forward_approval: Option<ForwardedRequest> = None;

            // `SessionStarted` normally carries the native id, but keep the
            // defaulted accessor in sync too so an adapter that only reports
            // it through `session_id` is still resumable after a restart.
            if record.meta.resume_id.is_none()
                && let Some(session_id) = record
                    .live_session()
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
                            .live_session()
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
                    if record.active.is_none() {
                        // Another path already settled this turn (a Stop, a
                        // failed write, a dead session). A late harness
                        // completion must not publish a second one.
                        tracing::debug!(
                            thread_id = %thread_id,
                            "ignoring a completion for an already-settled turn"
                        );
                    } else {
                        // A transport that is already dead (a cancel that timed
                        // out, a writer I/O error) is detached before this turn
                        // ends, so promotion starts the next turn on a fresh
                        // session instead of writing into a closed client.
                        let closed = session_closed;
                        if closed
                            && record.session.is_live(&session)
                            && let Some(generation) = record.session.generation()
                        {
                            detached = record.detach_session(generation);
                            record.pump = None;
                        }

                        // A completed turn clears a stale limit unless this
                        // turn armed an auto-resume: that schedule still
                        // explains why the thread is paused, so the row keeps
                        // its reset time.
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
                        let (turn_id, item_id, text, turn_usage) = {
                            let active = record.active.as_ref().expect("checked above");
                            (
                                active.turn_id.clone(),
                                active.item_id.clone(),
                                active.text.clone(),
                                active.usage.clone(),
                            )
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
                                .live_session()
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

                        // The ledger row is written before the turn completion
                        // is projected, so a client that reacts to
                        // `TurnCompleted` already sees the cost in `GetUsage`.
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
                            let model = record.meta.model.clone();
                            let metered = record.active.as_mut().map(|active| {
                                active.meter.set_final_usage(&usage);
                                active.meter.finish(now, status, meter.prices(), &model)
                            });
                            if let Some(metered) = &metered {
                                // The meter owns the generation time (wall minus
                                // tool time); the thread's TPS derives from it.
                                record.meta.usage.generation_ms = record
                                    .meta
                                    .usage
                                    .generation_ms
                                    .saturating_add(metered.generation_ms);
                                record.meta.usage.refresh_tps();
                            }
                            if let Some(metered) = metered
                                && let Err(error) = meter.record_turn(context, &metered).await
                            {
                                tracing::warn!(%error, "could not write the usage ledger row");
                            }
                        }

                        let mut end = TurnEnd::new(status, usage);
                        if closed && status == TurnStatus::Failed {
                            // The transport died under the turn. If the bridge
                            // already reported it, the settlement's row is
                            // deduplicated; if it arrives late, this is the one
                            // row.
                            end.error = Some(TurnError {
                                message: "the harness transport closed mid-turn".into(),
                                hint: Some("Send the message again to resume".into()),
                                retryable: true,
                            });
                        }
                        // A limit hit earlier in this turn must survive the
                        // completion event: the thread stays Working/Scheduled
                        // until the scheduled resume starts.
                        if let Some(limit) = record.scheduled_limit.clone()
                            && limit.auto_resume
                        {
                            end.domain.push(ThreadEvent::UsageLimitHit {
                                reset_at: limit.reset_at,
                                auto_resume: true,
                                at: now,
                            });
                        }
                        if !text.is_empty() {
                            end.wire.push(Event::ItemCompleted {
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
                        end.changes = self.snapshots.get().map(|snapshots| {
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
                        settled = self.settle_turn_locked(&mut record, &turn_id, end).await;
                    }
                }
                HarnessEvent::Error { message, retryable } => {
                    // One turn may publish at most one error row: a transport
                    // failure forwarded by the bridge and a write failure
                    // noticed by the daemon are the same failure.
                    let turn_id = record.turn_id_for_event();
                    if !record.session.is_live(&session) {
                        // This pump's session was already detached, so whoever
                        // detached it reported the failure. A late error from a
                        // session nobody attaches to any more is a duplicate and
                        // must not be blamed on the turn that replaced it.
                        tracing::debug!(
                            thread_id = %thread_id,
                            "dropped a late error from a detached session"
                        );
                    } else if record.note_error(&turn_id) {
                        if record.active.is_some() {
                            domain.push(ThreadEvent::FailureRaised { at: now });
                        }
                        wire.push(Event::Error {
                            thread_id: Some(thread_id.clone()),
                            message,
                            hint: Some("Inspect the thread and retry when ready".into()),
                            retryable,
                        });
                    } else {
                        tracing::debug!(
                            thread_id = %thread_id,
                            "dropped a duplicate failure report for the same turn"
                        );
                    }
                }
            }

            if let Err(error) = self.emit(&mut record, domain, wire).await {
                tracing::error!(thread_id = %thread_id, %error, "failed to append harness event");
            }
            drop(record);

            if let Some(forwarded) = forward_approval {
                self.forward_approval(forwarded).await;
            }
            if let Some(session) = detached {
                // The record is unlocked: shutting a session down while holding
                // it would deadlock with a Stop or a delivery.
                let _ = session.lock().await.shutdown().await;
            }
            if settled {
                // A settled turn may have promoted the next queued message
                // under the same lock; get it onto a session now.
                self.spawn_advance(handle.clone());
            }
        }

        // The stream ended. Only the pump that still owns the attached session
        // may clear it: a failed write or a restart may already have detached
        // it and started another one.
        let turn = {
            let mut record = handle.lock().await;
            if !record.session.is_live(&session) || record.session.generation() != Some(generation)
            {
                return;
            }
            let stderr_tail = record
                .live_session()
                .and_then(|session| session.try_lock().ok())
                .map(|session| session.stderr_tail())
                .unwrap_or_default();
            record.detach_session(generation);
            record.pump = None;
            record.active.as_ref().map(|active| {
                let hint = if stderr_tail.trim().is_empty() {
                    "Send the message again to resume".to_owned()
                } else {
                    format!("Send the message again to resume. Harness stderr: {stderr_tail}")
                };
                (active.turn_id.clone(), hint)
            })
        };
        if let Some((turn_id, hint)) = turn {
            self.settle_turn(
                &handle,
                &turn_id,
                TurnEnd::failed(
                    "the harness session ended before the turn completed".into(),
                    Some(hint),
                    true,
                ),
            )
            .await;
        }
    }

    /// Gets the active turn onto a session, starting one when needed.
    ///
    /// A settled turn already promoted the next queued message under its own
    /// lock, so this only ensures the active turn's prompt reaches the harness:
    /// on the live session, or by starting one. A session that is still
    /// starting delivers it when it is ready, and this returns without starting
    /// a second one.
    async fn advance_queue(self: &Arc<Self>, handle: ThreadHandle) {
        let needs_session = {
            let record = handle.lock().await;
            if record.closed || record.active.is_none() {
                return;
            }
            matches!(record.session, SessionSlot::None)
        };
        if needs_session {
            self.spawn_session(handle);
        } else {
            self.schedule_active(&handle).await;
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
        let now = self.now();
        for handle in handles {
            let mut record = handle.lock().await;
            let thread_id = record.meta.thread_id.clone();
            let Some(active) = record.active.as_mut() else {
                continue;
            };
            let due = active.last_tick.is_none_or(|last| {
                let elapsed = now - last;
                elapsed < chrono::TimeDelta::zero() || elapsed >= chrono::TimeDelta::seconds(1)
            });
            if !due {
                continue;
            }
            active.last_tick = Some(now);
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
        let now = self.now();
        {
            let mut last = self
                .last_schedule_check
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // The scan reads the manager's clock, not a wall-clock stopwatch, so
            // a manual clock makes a due schedule fire on the test's step. A
            // clock that jumped backwards scans rather than waiting out the gap.
            if last.is_some_and(|at| {
                let elapsed = now - at;
                elapsed >= chrono::TimeDelta::zero() && elapsed < chrono::TimeDelta::seconds(1)
            }) {
                return;
            }
            *last = Some(now);
        }
        let due = match meter.due_schedules(now).await {
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
        // Snapshot what the decision needs, then let go of the record lock.
        // Everything below up to the re-checks is either network I/O (the
        // proxy calls) or harness I/O (session shutdown), and holding the lock
        // across it would freeze GetThread and SendMessage for this thread for
        // as long as the proxy or the harness takes to answer.
        let (harness, model, thread_id, closed, active) = {
            let record = handle.lock().await;
            (
                record.meta.harness,
                record.meta.model.clone(),
                record.meta.thread_id.clone(),
                record.closed,
                record.active.is_some(),
            )
        };
        if closed {
            let _ = meter
                .finish_schedule(
                    schedule,
                    ResumeState::Cancelled,
                    Some("the thread is closed".into()),
                )
                .await;
            return Ok(None);
        }
        if active {
            // A turn is already running (a manual resume, or the limited turn
            // itself has not finished). Wait quietly: the schedule stays armed
            // and is re-checked on the next tick, and `send` cancels it when a
            // new turn supersedes it. Emitting here would repeat every second.
            tracing::debug!(
                thread_id = %thread_id,
                "auto-resume due while the thread is active; waiting"
            );
            return Ok(None);
        }

        // A failover choice is made when the reset fires, not when the limit is
        // observed: model cooldowns may have expired in the meantime.
        let policy = meter.policy();
        if schedule.failover.is_none()
            && policy.failover
            && let Some(proxy) = self.proxy.get()
            && let Some(applied) = proxy.override_for(harness, &model)
            && let Ok(Some(choice)) = proxy
                .pick_failover(
                    &applied.profile_id,
                    &model,
                    &h4rn_protocol::ProxyFailoverPolicy::default(),
                )
                .await
        {
            schedule.failover = Some(ResumeFailover {
                model: choice.model,
                account: choice.account,
                account_label: choice.account_label,
                reason: choice.reason,
            });
        }

        // Whoever chose it (the policy at reset time, or a person through
        // `FailOver`), a changed model needs a fresh session: the live one was
        // started on the limited model. Claim the attempt first, under the
        // thread lock and before any model change: `cancel_resume` takes the
        // same lock before it writes `Cancelled`, so exactly one of the two
        // wins. A cancellation that landed while the proxy was thinking makes
        // the claim fail here and no turn runs.
        enum Apply {
            Closed,
            Active,
            Stale,
            Go(Option<SharedSession>, Option<JoinHandle<()>>),
        }
        let apply = {
            let mut record = handle.lock().await;
            if record.closed {
                Apply::Closed
            } else if record.active.is_some() {
                Apply::Active
            } else if !meter
                .claim_schedule(&schedule)
                .await
                .map_err(|error| error.to_string())?
            {
                Apply::Stale
            } else if let Some(failover) = &schedule.failover {
                if record.meta.model == failover.model {
                    Apply::Go(None, None)
                } else {
                    record.meta.model = failover.model.clone();
                    // The limited model's session is the one inspected under
                    // this lock; a newer generation is never this schedule's
                    // to drop.
                    let session = record
                        .session
                        .generation()
                        .and_then(|generation| record.detach_session(generation));
                    let pump = if session.is_some() {
                        record.pump.take()
                    } else {
                        None
                    };
                    Apply::Go(session, pump)
                }
            } else {
                Apply::Go(None, None)
            }
        };
        match apply {
            Apply::Closed => {
                let _ = meter
                    .finish_schedule(
                        schedule,
                        ResumeState::Cancelled,
                        Some("the thread is closed".into()),
                    )
                    .await;
                return Ok(None);
            }
            Apply::Active => {
                tracing::debug!(
                    thread_id = %thread_id,
                    "auto-resume due while the thread is active; waiting"
                );
                return Ok(None);
            }
            Apply::Stale => {
                tracing::debug!(
                    thread_id = %thread_id,
                    schedule = %schedule.schedule_id,
                    "auto-resume was cancelled or superseded before it claimed; not starting it"
                );
                return Ok(None);
            }
            Apply::Go(session, pump) => {
                if let Some(session) = session {
                    let mut session = session.lock().await;
                    if let Err(error) = session.shutdown().await {
                        tracing::warn!(%thread_id, %error, "harness shutdown before failover failed");
                    }
                }
                if let Some(pump) = pump {
                    pump.abort();
                }
            }
        }

        // Re-take the lock after the unlocked I/O and check that nothing
        // replaced this schedule meanwhile: `send` may have started a turn
        // (which cancels the schedule) or the daemon may have closed the thread.
        let mut record = handle.lock().await;
        if record.closed {
            drop(record);
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
            drop(record);
            tracing::debug!(
                thread_id = %thread_id,
                "auto-resume due while the thread is active; waiting"
            );
            return Ok(None);
        }

        let prompt = schedule.prompt.clone();
        let now = self.now();
        record.scheduled_limit = None;
        let dispatch = record.begin_turn(
            now,
            UserInput {
                text: prompt.clone(),
                attachments: Vec::new(),
            },
        );
        let turn_id = dispatch.turn_id.clone();
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
                turn_id: turn_id.clone(),
                started_at: now,
            },
        ];
        if let Err(error) = self.emit(&mut record, domain, wire).await {
            let schedule = meter
                .finish_schedule(schedule, ResumeState::Waiting, Some(error.to_string()))
                .await
                .map_err(|error| error.to_string())?;
            return Ok(schedule);
        }
        let needs_session = matches!(record.session, SessionSlot::None);
        drop(record);

        let fired = meter
            .finish_schedule(schedule, ResumeState::Fired, None)
            .await
            .map_err(|error| error.to_string())?;
        if needs_session {
            self.spawn_session(handle);
        } else {
            // Acknowledge the resume before the harness produces its first
            // token. `finish_schedule` above already emitted `ScheduleChanged`
            // and the thread events below report progress, so a slow first
            // send must not hold the `ResumeNow` request open (ADR-003).
            self.schedule_active(&handle).await;
        }
        Ok(fired)
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
            let summary = record.summary(&self.options.settings, self.now());
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

/// Expands a leading `~` to the daemon user's home, so `~/code/api` from a
/// client means the same folder it does in a shell on the host.
fn expand_home(path: &str) -> String {
    let trimmed = path.trim();
    let Some(home) = dirs::home_dir() else {
        return trimmed.to_owned();
    };
    if trimmed == "~" {
        home.display().to_string()
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        home.join(rest).display().to_string()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use h4rn_harness::FakeHarness;

    use super::*;
    use crate::harnesses::StaticHarnessCatalog;
    use crate::journal::SqliteJournal;

    /// Polls `ready` until it is true, for up to five seconds of wall clock.
    ///
    /// A shared counter is updated by another task, so the check cannot wait on
    /// anything but the runtime; the timeout keeps a regression a failure
    /// instead of a hang.
    async fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok()
    }

    /// The next projected event, failing the test instead of hanging when a
    /// regression leaves a turn's queue stuck.
    async fn next_event(events: &mut broadcast::Receiver<WireEvent>) -> WireEvent {
        tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("no event arrived; a turn-end path failed to advance the queue")
            .expect("event")
    }

    async fn next_turn_status(events: &mut broadcast::Receiver<WireEvent>) -> TurnStatus {
        next_turn_completion(events).await.1
    }

    async fn next_turn_completion(
        events: &mut broadcast::Receiver<WireEvent>,
    ) -> (TurnId, TurnStatus) {
        loop {
            let wire = next_event(events).await;
            if let Event::TurnCompleted {
                turn_id, status, ..
            } = wire.event
            {
                return (turn_id, status);
            }
        }
    }

    async fn open_test_manager(harness: FakeHarness, host: HostId) -> ThreadManager {
        ThreadManager::open(
            Arc::new(SqliteJournal::in_memory().await.expect("journal")),
            Arc::new(StaticHarnessCatalog::with(Arc::new(harness))),
            host,
            ThreadManagerOptions {
                // The manager only reacts to this test's own calls.
                tick_interval: Duration::from_secs(3600),
                ..ThreadManagerOptions::default()
            },
            None,
        )
        .await
        .expect("manager")
    }

    fn new_thread(host: HostId, workspace: &std::path::Path, prompt: &str) -> NewThread {
        NewThread {
            thread_id: None,
            parent_thread_id: None,
            subagent_id: None,
            host,
            harness: HarnessKind::ClaudeCode,
            workspace_path: workspace.display().to_string(),
            model: "fake-model".into(),
            prompt: prompt.into(),
            attachments: Vec::new(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            worktree: None,
        }
    }

    #[tokio::test]
    async fn a_timed_out_prompt_fails_once_and_the_next_message_starts_a_new_session() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, sessions) = FakeHarness::new().with_session_count();
        let (harness, sends) = harness.with_send_count();
        let harness = harness.with_dead_transport_after(1);
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);
        assert_eq!(sessions.get(), 1);
        assert_eq!(sends.get(), 1);

        // The prompt write fails, as it does when the ACP deadline kills the
        // transport: one failure row, the turn is over.
        manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        let mut errors = 0;
        loop {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::Error { .. } => errors += 1,
                Event::TurnCompleted { status, .. } => {
                    assert_eq!(status, TurnStatus::Failed);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(errors, 1, "one failure row for one prompt write");
        assert!(
            manager
                .inner
                .handle(&created.thread_id)
                .await
                .expect("handle")
                .lock()
                .await
                .active
                .is_none(),
            "the turn is no longer running"
        );

        manager
            .send_message(&created.thread_id, "third".into(), Vec::new())
            .await
            .expect("send");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);
        assert_eq!(sessions.get(), 2, "the next message starts a fresh session");
        assert_eq!(sends.get(), 3);
    }

    #[tokio::test]
    async fn a_stop_during_startup_drops_the_prompt_and_runs_the_follow_up() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_start_gate();
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert!(
            wait_until(|| sessions.get() == 1).await,
            "the startup has begun"
        );
        assert_eq!(sessions.get(), 1, "exactly one session start is in flight");

        // A follow-up arrives while the first prompt is still pending.
        let queued = manager
            .send_message(&created.thread_id, "follow-up".into(), Vec::new())
            .await
            .expect("send");
        assert_eq!(
            queued.turn_id, created.turn_id,
            "the message waits behind the first turn"
        );

        manager
            .interrupt(&created.thread_id)
            .await
            .expect("a stop during startup is accepted");
        let (stopped, status) = next_turn_completion(&mut events).await;
        assert_eq!(stopped, created.turn_id);
        assert_eq!(status, TurnStatus::Interrupted);

        // Let the session finish starting. The stopped prompt is dropped; the
        // follow-up is delivered on that one session, under its own turn id.
        gate.release();
        let mut deltas = Vec::new();
        let follow_up = loop {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::MessageDelta { text, .. } => deltas.push(text),
                Event::TurnCompleted {
                    turn_id, status, ..
                } => {
                    assert_eq!(status, TurnStatus::Completed, "the follow-up completes");
                    break turn_id;
                }
                _ => {}
            }
        };
        assert_ne!(
            follow_up, created.turn_id,
            "the follow-up runs under its own turn id"
        );
        assert_eq!(sessions.get(), 1, "exactly one agent process was started");
        assert_eq!(sends.get(), 1, "only the follow-up's prompt was written");
        assert!(
            deltas.iter().any(|text| text.contains("follow-up")),
            "the follow-up ran on the session the startup created"
        );
        assert!(
            !deltas.iter().any(|text| text.contains("echo: first")),
            "the stopped prompt never ran"
        );
    }

    /// A Stop that lands before the prompt is written drops it: no cancel, no
    /// prompt, one interrupted turn.
    #[tokio::test]
    async fn a_stop_before_attachment_drops_the_prompt() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_start_gate();
        let (harness, sends) = harness.with_send_count();
        let (harness, cancels) = harness.with_interrupt_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(handle.lock().await.session, SessionSlot::Starting(_)) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the startup is in flight, holding the start gate");

        // The Stop lands while the session is still starting, so the prompt can
        // never be claimed.
        manager
            .interrupt(&created.thread_id)
            .await
            .expect("a stop during startup is accepted");
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_eq!(turn, created.turn_id);
        assert_eq!(status, TurnStatus::Interrupted);
        assert_eq!(sends.get(), 0, "the stopped prompt was never written");
        assert_eq!(cancels.get(), 0, "nothing was on the wire to cancel");

        // Let the startup finish: it must not resurrect the stopped turn.
        gate.release();
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sends.get(),
            0,
            "the startup did not write the stopped prompt"
        );
        assert_eq!(cancels.get(), 0);
    }

    /// A Stop that lands after the prompt reached the harness cancels the turn
    /// that is on the wire, and the withheld completion cannot arrive as a
    /// second turn end.
    #[tokio::test]
    async fn a_stop_after_the_prompt_is_written_cancels_it_once() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_completion_gate();
        let (harness, sends) = harness.with_send_count();
        let (harness, cancels) = harness.with_interrupt_count();
        let (harness, sessions) = harness.with_session_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert!(
            wait_until(|| sends.get() == 1).await,
            "the prompt reached the harness"
        );
        assert_eq!(sessions.get(), 1);

        manager
            .interrupt(&created.thread_id)
            .await
            .expect("a stop after the prompt is on the wire is accepted");
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_eq!(turn, created.turn_id);
        assert_eq!(status, TurnStatus::Interrupted);
        assert_eq!(cancels.get(), 1, "the prompt on the wire was cancelled");

        // Releasing the withheld completion must not publish a second end.
        gate.release();
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                next_turn_completion(&mut events)
            )
            .await
            .is_err(),
            "the turn ended exactly once"
        );
    }

    #[tokio::test]
    async fn a_stop_for_an_ended_turn_does_not_cancel_the_next_one() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_send_gate();
        let (harness, sends) = harness.with_send_count();
        let (harness, cancels) = harness.with_interrupt_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        let (first_turn, status) = next_turn_completion(&mut events).await;
        assert_eq!(status, TurnStatus::Completed);
        assert_eq!(sends.get(), 1);

        // The next turn's prompt is claimed and its write is in flight.
        manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        assert!(
            wait_until(|| sends.get() == 2).await,
            "the next turn's prompt is on its way"
        );
        assert_eq!(sends.get(), 2, "the next turn's prompt was written");
        // A Stop captured for the ended turn (what `interrupt` reaches when it
        // loses the race with the turn's completion) must not cancel the turn
        // that replaced it.
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        manager
            .cancel_claimed_turn(handle, first_turn)
            .await
            .expect("a stop for an ended turn is a no-op");
        assert_eq!(cancels.get(), 0, "the next turn is not cancelled");

        gate.release();
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_ne!(turn, created.turn_id);
        assert_eq!(
            status,
            TurnStatus::Completed,
            "the next turn still completes"
        );
    }

    #[tokio::test]
    async fn a_failed_prompt_write_runs_the_queued_follow_up_on_a_fresh_session() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_send_gate();
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let harness = harness.with_dead_transport_after(1);
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);
        assert_eq!(sessions.get(), 1);

        // The second prompt's write is held at the gate; the third message
        // queues behind it.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        let third = manager
            .send_message(&created.thread_id, "third".into(), Vec::new())
            .await
            .expect("send");
        assert_eq!(
            third.turn_id, second.turn_id,
            "the third message waits behind the second turn"
        );
        gate.release();

        let mut errors = 0;
        let mut failed_second = false;
        let follow_up = loop {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::Error { .. } => errors += 1,
                Event::TurnCompleted {
                    turn_id, status, ..
                } => {
                    if turn_id == second.turn_id {
                        assert_eq!(status, TurnStatus::Failed);
                        failed_second = true;
                    } else {
                        assert_eq!(status, TurnStatus::Completed);
                        break turn_id;
                    }
                }
                _ => {}
            }
        };
        assert!(failed_second, "the failed turn ends once");
        assert_eq!(
            errors, 1,
            "one failure row for the dead transport, with no extra exit error"
        );
        assert_eq!(
            sessions.get(),
            2,
            "the follow-up starts a fresh session after the dead one is detached"
        );
        assert_eq!(sends.get(), 3, "the follow-up's prompt was written");
        assert_ne!(follow_up, second.turn_id);
    }

    #[tokio::test]
    async fn a_stop_before_dispatch_never_writes_the_prompt() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, sends) = FakeHarness::new().with_send_count();
        let host = HostId::new("host_test");
        let manager = ThreadManager::open(
            Arc::new(SqliteJournal::in_memory().await.expect("journal")),
            Arc::new(StaticHarnessCatalog::with(Arc::new(harness))),
            host.clone(),
            ThreadManagerOptions {
                // The manager only reacts to this test's own calls.
                tick_interval: Duration::from_secs(3600),
                ..ThreadManagerOptions::default()
            },
            None,
        )
        .await
        .expect("manager");
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(NewThread {
                thread_id: None,
                parent_thread_id: None,
                subagent_id: None,
                host,
                harness: HarnessKind::ClaudeCode,
                workspace_path: workspace.path().display().to_string(),
                model: "fake-model".into(),
                prompt: "first".into(),
                attachments: Vec::new(),
                runtime_mode: RuntimeMode::ApprovalRequired,
                worktree: None,
            })
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);
        assert_eq!(sends.get(), 1, "the first prompt reached the harness");

        // Record the turn, then Stop before the background delivery task has
        // had a chance to run. Nothing may be written, and the turn ends
        // interrupted instead of reporting a cancel the agent never saw.
        manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        manager.interrupt(&created.thread_id).await.expect("stop");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Interrupted);

        // Now let the queued delivery run: it must find the turn stopped.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(sends.get(), 1, "the stopped prompt was never written");
    }

    /// A startup that fails still ends its turn: one `TurnCompleted(Failed)`
    /// with one error row, and the next Send starts a fresh attempt.
    #[tokio::test]
    async fn a_startup_failure_emits_one_completion_and_advances() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let harness = FakeHarness::new().with_start_failure("the harness binary is missing");
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");

        let mut errors = 0;
        loop {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::Error { .. } => errors += 1,
                Event::TurnCompleted {
                    turn_id, status, ..
                } => {
                    assert_eq!(turn_id, created.turn_id);
                    assert_eq!(status, TurnStatus::Failed);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(errors, 1, "a failed startup reports one error row");

        // The turn ended, so a fresh Send runs its own turn (and fails again)
        // instead of leaving the previous one Working.
        manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_ne!(turn, created.turn_id);
        assert_eq!(status, TurnStatus::Failed);
    }

    /// A Stop and a startup failure race for the same turn: whichever settles
    /// first wins, and the turn still ends exactly once.
    #[tokio::test]
    async fn a_stop_racing_a_startup_failure_settles_the_turn_once() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_start_gate();
        let harness = harness.with_start_failure("the harness failed after the gate");
        let (harness, sessions) = harness.with_session_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert!(
            wait_until(|| sessions.get() == 1).await,
            "the startup is in flight"
        );

        manager
            .interrupt(&created.thread_id)
            .await
            .expect("a stop during startup is accepted");
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_eq!(turn, created.turn_id);
        assert_eq!(status, TurnStatus::Interrupted);

        // The startup now fails. The turn is already settled, so this must be a
        // no-op rather than a second completion or a second error row.
        gate.release();
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        let mut extra = Vec::new();
        while let Ok(wire) = tokio::time::timeout(Duration::from_millis(100), events.recv()).await {
            if let Ok(wire) = wire
                && matches!(wire.event, Event::TurnCompleted { .. })
            {
                extra.push(wire.event);
            }
        }
        assert!(extra.is_empty(), "the turn ended exactly once: {extra:?}");
    }

    /// A startup that returns after the slot moved on must shut the session it
    /// made down instead of attaching it or touching the turn.
    #[tokio::test]
    async fn a_stale_startup_shuts_its_session_down_and_does_not_attach() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_start_gate();
        let (harness, sessions) = harness.with_session_count();
        let (harness, shutdowns) = harness.with_shutdown_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        assert!(
            wait_until(|| sessions.get() == 1).await,
            "the startup is in flight"
        );

        // The slot moves on while the harness is still booting, as a restart
        // that cleared the startup would.
        {
            let mut record = handle.lock().await;
            record.session = SessionSlot::None;
        }
        gate.release();
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }

        assert_eq!(shutdowns.get(), 1, "the stale session was shut down");
        let record = handle.lock().await;
        assert!(
            matches!(record.session, SessionSlot::None),
            "the stale startup did not attach"
        );
        assert_eq!(
            record.active.as_ref().map(|active| &active.turn_id),
            Some(&created.turn_id),
            "the stale startup did not touch the turn"
        );
    }

    /// A delivery that finds its captured session detached gets the still
    /// pending turn onto a fresh session instead of leaving it Working forever.
    #[tokio::test]
    async fn a_delivery_whose_session_was_detached_starts_a_fresh_one() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, sessions) = FakeHarness::new().with_session_count();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);
        assert_eq!(sessions.get(), 1);
        assert_eq!(sends.get(), 1);

        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (dispatch, detached) = {
            let mut record = handle.lock().await;
            let generation = record.session.generation().expect("a live session");
            let detached = record.detach_session(generation).expect("the session");
            let dispatch = record.begin_turn(
                manager.now(),
                UserInput {
                    text: "second".into(),
                    attachments: Vec::new(),
                },
            );
            (dispatch, detached)
        };

        // The delivery was spawned for the old session; it arrives after the
        // detach.
        manager
            .inner
            .deliver(handle.clone(), dispatch, detached)
            .await;

        assert_eq!(
            next_turn_status(&mut events).await,
            TurnStatus::Completed,
            "the turn runs on a fresh session"
        );
        assert_eq!(sessions.get(), 2, "the delivery started a fresh session");
        assert_eq!(sends.get(), 2);
    }

    /// A Send that arrives while a turn is failing is promoted behind the
    /// message that was already queued, never ahead of it.
    #[tokio::test]
    async fn a_send_during_a_failure_queues_behind_the_queued_turn() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let harness = FakeHarness::new();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");

        // A is active (never delivered), B is queued behind it.
        {
            let mut record = handle.lock().await;
            record.begin_turn(
                manager.now(),
                UserInput {
                    text: "A".into(),
                    attachments: Vec::new(),
                },
            );
        }
        let b = manager
            .send_message(&created.thread_id, "B".into(), Vec::new())
            .await
            .expect("send B");
        let a_turn = b.turn_id.clone();

        // Hold the record so C's Send is waiting on it when A settles.
        {
            let mut record = handle.lock().await;
            let sender = manager.clone();
            let thread_id = created.thread_id.clone();
            let sending = tokio::spawn(async move {
                sender
                    .send_message(&thread_id, "C".into(), Vec::new())
                    .await
            });
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            manager
                .inner
                .settle_turn_locked(
                    &mut record,
                    &a_turn,
                    TurnEnd::new(TurnStatus::Interrupted, Usage::default()),
                )
                .await;
            let promoted = record
                .active
                .as_ref()
                .map(|active| active.turn_id.clone())
                .expect("B was promoted under the lock");
            drop(record);
            let c = sending.await.expect("send task").expect("send C");
            assert_eq!(
                c.turn_id, promoted,
                "C queued behind the promoted B, not in front of it"
            );
            assert_ne!(c.turn_id, a_turn);
        }
        // Let B (promoted) and C run in order.
        manager.inner.spawn_advance(handle.clone());

        let mut started = Vec::new();
        loop {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::TurnStarted { turn_id, .. } => started.push(turn_id),
                Event::TurnCompleted { status, .. } if started.len() >= 2 => {
                    assert_eq!(status, TurnStatus::Completed);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(started.len(), 2, "B and C each started exactly once");
        assert_ne!(started[0], started[1], "B and C are different turns");
    }

    /// A restart while a Send is in flight never leaves its turn Working with
    /// no session to run it.
    #[tokio::test]
    async fn restart_sessions_during_a_send_does_not_strand_the_turn() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_later_completion_gate();
        let (harness, sessions) = harness.with_session_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        let (first, status) = next_turn_completion(&mut events).await;
        assert_eq!(status, TurnStatus::Completed);
        assert_eq!(first, created.turn_id);

        // The send's write and the session restart race; whichever wins, the
        // turn must still end.
        let sender = manager.clone();
        let thread_id = created.thread_id.clone();
        let sending = tokio::spawn(async move {
            sender
                .send_message(&thread_id, "second".into(), Vec::new())
                .await
        });
        let restarted = manager.restart_sessions(&[HarnessKind::ClaudeCode]).await;
        sending.await.expect("send task").expect("send");
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        gate.release();
        let (_, status) = next_turn_completion(&mut events).await;
        assert!(
            matches!(status, TurnStatus::Interrupted | TurnStatus::Completed),
            "the turn ended instead of staying Working: {status:?}"
        );
        assert!(
            manager
                .inner
                .handle(&created.thread_id)
                .await
                .expect("handle")
                .lock()
                .await
                .active
                .is_none(),
            "no turn is stranded as active"
        );
        let _ = restarted;
        assert!(sessions.get() >= 1);
    }

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
