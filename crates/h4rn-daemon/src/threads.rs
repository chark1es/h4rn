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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures::StreamExt;
use h4rn_core::{
    LifecycleSettings, ThreadEvent, ThreadState, apply_all, is_snoozed, needs_attention, tab_for,
};
use h4rn_harness::{
    AttachmentRef, HarnessEvent, HarnessInstall, HarnessSession, LimitSignal, SessionSpec,
    TransportCloseKind, UserInput, merge_transport_detail, most_specific_detail,
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
use crate::journal::{Journal, ParkedAttachment, ParkedPrompt, ThreadMeta, TitleSource, WireEvent};
use crate::text_generation::{TextGeneration, TextGenerationError};

/// Capacity of the live event broadcast. A subscriber that falls further behind
/// than this is dropped with a resume hint rather than slowing other clients.
pub const BROADCAST_CAPACITY: usize = 512;

/// How long a death between turns waits for the exit report that carries the
/// agent's stderr and status.
///
/// A live adapter keeps its event sender alive for as long as the daemon holds
/// its session, so the end of that session's stream is not an event the pump
/// can wait for. The generic stdout-close notice therefore gets this bounded
/// window for the exit report to arrive and sharpen the row; after it, the
/// notice alone is published. The ACP adapter's exit report is the same
/// 500 ms window a failed prompt waits for (`DEATH_DETAIL_WAIT`).
const IDLE_DEATH_DETAIL_WAIT: Duration = Duration::from_millis(500);

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

    /// A monotonic reading for waits that must ignore wall-clock changes.
    ///
    /// Retry deadlines use this instead of [`Self::now`]: the wall clock can
    /// jump backwards (an NTP correction, a manual change) and must not stretch
    /// a one-second wait into an hour. A manual clock advances both readings by
    /// its own step so a test stays deterministic.
    fn monotonic(&self) -> Instant;
}

/// The wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn monotonic(&self) -> Instant {
        Instant::now()
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
    /// Title generation, installed once by the daemon assembly.
    text_generation: std::sync::OnceLock<Arc<dyn TextGeneration>>,
    /// Threads with a title generation attempt in flight, so a duplicate
    /// never starts a second model call.
    ///
    /// A blocking mutex: the insert happens before the task is spawned, so a
    /// test that waits for an idle set cannot miss the job it just started.
    title_jobs: std::sync::Mutex<HashSet<String>>,
    /// Signals that one title attempt finished (tests wait on it).
    title_settled: tokio::sync::Notify,
    /// Throttles the due-schedule scan to about once a second.
    last_schedule_check: std::sync::Mutex<Option<DateTime<Utc>>>,
    /// Set when the startup ledger resync could not read the ledger.
    ///
    /// A schedule that restored no id can never be fired (the record does not
    /// name it), so the resync is retried on the next ticks until both reads
    /// succeed instead of leaving every restored pause `Unnamed` forever.
    resync_pending: AtomicBool,
    /// When the failed startup resync may be retried, and how long to wait.
    ///
    /// A ledger read that keeps failing must not clear every thread's cached
    /// summary signature (and so re-append and broadcast it) on every tick, so
    /// the retry starts a second apart and doubles up to a minute. The deadline
    /// is monotonic: a wall-clock jump back must not stretch the wait.
    resync_retry: std::sync::Mutex<ResyncRetry>,
    /// The clock every lifecycle decision reads.
    clock: Arc<dyn Clock>,
    /// Pauses a restart between inspecting a thread and detaching its session.
    ///
    /// Test-only: this is the window a Send can claim the session in, and the
    /// race the restart's detach must re-check under its own lock.
    #[cfg(test)]
    restart_pause: std::sync::Mutex<Option<Arc<RestartPause>>>,
    /// Pauses a due-schedule fire between claiming its attempt and acting on
    /// the record.
    ///
    /// Test-only: a newer limit can replace the schedule in that window, and
    /// the old fire must not act on a pause that is no longer its own.
    #[cfg(test)]
    fire_pause: std::sync::Mutex<Option<Arc<FirePause>>>,
    /// Pauses the startup resync between reading the ledger and binding a
    /// record.
    ///
    /// Test-only: a newer limit can arm a schedule in that window, and the
    /// retry must not restore the older ledger snapshot over it.
    #[cfg(test)]
    resync_pause: std::sync::Mutex<Option<Arc<ResyncPause>>>,
}

/// A test's hold on the fire's claim-to-act window.
#[cfg(test)]
#[derive(Default)]
struct FirePause {
    claimed: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

/// A test's hold on the restart's inspect-to-detach window.
#[cfg(test)]
#[derive(Default)]
struct RestartPause {
    inspected: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

/// A test's hold on the resync's read-to-bind window.
#[cfg(test)]
#[derive(Default)]
struct ResyncPause {
    read: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

/// The wait before a failed startup ledger resync is tried again.
#[derive(Clone, Debug)]
struct ResyncRetry {
    /// When the next attempt may run. `None` means it may run now.
    ///
    /// A monotonic reading, so a wall-clock jump cannot move the wait.
    at: Option<Instant>,
    /// The wait that follows another failed attempt.
    backoff: Duration,
}

impl Default for ResyncRetry {
    fn default() -> Self {
        Self {
            at: None,
            backoff: Duration::from_secs(1),
        }
    }
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
    /// The most recent turn whose dead transport was already published as one
    /// error row.
    ///
    /// The ACP bridge reports a writer death on its own stream while the daemon
    /// reports the same death from `fail_prompt_write`; those two are one row.
    /// Every other harness error is its own row, so this marker is never used
    /// to hide an unrelated failure.
    transport_error_reported: Option<TurnId>,
    /// A transport death reported for the active turn, held until that turn
    /// settles.
    ///
    /// The bridge's generic stdout-close notice usually arrives before the
    /// prompt failure that carries the agent's stderr. Publishing the first
    /// report immediately would take the turn's one row and leave the stderr
    /// out; holding it lets the settlement (or a second report) merge the most
    /// specific detail into that one row.
    transport_report: Option<TransportReport>,
    /// Sessions detached the moment their transport was known dead between
    /// turns, keyed by generation and kept only for the exit report that
    /// follows.
    ///
    /// A read failure or EOF says the reader is gone, but the exit report that
    /// carries the status, stderr and provider reset may still arrive on the
    /// session's stream. Leaving the session in the slot for that bounded
    /// window let a Send in it claim a transport that could never take the
    /// prompt. Detaching here makes delivery start a fresh session at once,
    /// while each generation's pump keeps its own session and report.
    ///
    /// The map is what keeps two deaths from overwriting each other: a Send can
    /// replace the first dead session and that replacement can die too, before
    /// the first one's exit report has arrived. Each pump finalizes and removes
    /// only its own generation; shutdown tears all of them down.
    dying_sessions: BTreeMap<u64, DyingSession>,
    /// Wakes a test that waits for a session to leave the slot as dying.
    ///
    /// Test-only: the detach is the event a deterministic idle-read test waits
    /// for instead of polling the record.
    #[cfg(test)]
    dying_notify: Arc<tokio::sync::Notify>,
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
    /// Whether the active turn was rebuilt from the journal at startup.
    ///
    /// Such a turn is the prompt a pause was holding when the daemon stopped.
    /// The metering ledger is attached after the journal is restored, so this
    /// record's pause is only known once `resync_meter_state` runs: that call
    /// either binds the pause to this turn or, when no schedule survived,
    /// releases the turn for delivery.
    restored_parked: bool,
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
    /// The reset this turn already armed a schedule for, when it armed one,
    /// together with the session generation that reported it.
    ///
    /// This belongs to the turn, not to `scheduled_limit`: a person can cancel
    /// the resume while a dead transport's exit report is still arriving, and
    /// that report repeats the same reset. Reading the live schedule would find
    /// it empty after the cancellation and arm the pause again; the turn's own
    /// record still knows the reset was already applied. The generation is part
    /// of the marker: a report from a different (older) generation is not this
    /// turn's report even when it names the same reset, so it must not consume
    /// the live generation's own first report.
    applied_limit_reset: Option<(u64, Option<DateTime<Utc>>)>,
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
        /// The reset this session's typed limit already armed a schedule for.
        ///
        /// Kept on the live slot so a death between turns can carry it into
        /// its dying entry: the bridge can report `LimitHit` and only then
        /// `Exit` with the same reset, and if the person cancels the resume in
        /// between, the exit report must not arm that pause again.
        applied_limit_reset: Option<DateTime<Utc>>,
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
    /// The session generation that was started for this turn, once one was.
    ///
    /// A startup records its generation here before it tries to deliver the
    /// turn's first prompt. That is what lets the pump tell "the agent that was
    /// started for this turn exited before it accepted the prompt" (settle the
    /// turn) from "the previous turn's agent exited and left this promoted turn
    /// waiting" (start one fresh session for it). Without the distinction a
    /// harness whose sessions die at startup would be replaced forever.
    started_generation: AtomicU64,
    /// Pauses a failing write just before it decides what to do with the
    /// dispatch, so a test can land a Stop in that window.
    ///
    /// Test-only: the decision itself reads and writes the state under its one
    /// lock, but a Stop arriving before that lock is the interleaving the
    /// recovery must not split.
    #[cfg(test)]
    recover_pause: std::sync::Mutex<Option<Arc<RecoverPause>>>,
}

/// A test's hold on the failed-write recovery's inspect-to-decide window.
#[cfg(test)]
#[derive(Default)]
struct RecoverPause {
    /// Signalled once a failing write has inspected the dispatch.
    inspected: tokio::sync::Notify,
    /// The test's permission to act on it.
    resume: tokio::sync::Notify,
}

/// `started_generation` has no generation yet.
const NO_GENERATION: u64 = 0;

/// Where a prompt goes after the session that claimed it was detached.
enum DeliveryRetarget {
    /// No session: a fresh startup takes the prompt.
    Start,
    /// A live session can take it again.
    Live,
    /// A startup is already in flight and delivers it when it is live.
    Starting,
}

/// What happened to a turn's prompt so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DispatchState {
    /// Recorded and waiting for a session.
    Pending,
    /// A writer has taken it; the prompt is on its way to the harness.
    Claimed,
    /// Stop arrived after a writer claimed the prompt, and the cancel could
    /// not be delivered (the session had already gone away).
    ///
    /// The prompt is still in flight, so a failed write must settle it as
    /// interrupted instead of putting it back: a Stop the person already saw
    /// acknowledged must never replay on a fresh session.
    ClaimedStopped,
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
    /// The prompt was claimed; the harness needs a cancel. A turn whose earlier
    /// cancel failed is still claimed, so every Stop on it retries the cancel:
    /// only a cancel that could not be delivered counts as done.
    AlreadySent,
    /// The turn's dispatch already ended.
    AlreadyStopped,
}

/// What a write that lost its session may do with the turn's dispatch.
///
/// It is one decision, taken under the dispatch's own lock, so a Stop that
/// lands at any point leaves exactly one action: the prompt goes back to the
/// queue, or the turn ends interrupted, never neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryOutcome {
    /// The prompt is back in the queue and must be delivered again.
    Requeue,
    /// A Stop already owns the turn: it ends interrupted, and the prompt must
    /// never be written again.
    Stopped,
    /// Another path already ended this turn; publish nothing.
    Settled,
}

/// Which branch of the idle fallback's one poll was ready.
#[derive(Clone, Debug)]
enum IdleWait {
    /// The session's stream produced its next value, or ended.
    Event(Option<HarnessEvent>),
    /// The report window expired with nothing on the stream.
    Deadline,
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
                transport: false,
            }),
            ..Self::new(TurnStatus::Failed, Usage::default())
        }
    }

    /// A turn that ended because its transport died.
    ///
    /// Only these rows are deduplicated: the ACP bridge reports the same death
    /// on its harness stream, and the two reports are one failure.
    fn transport_failed(message: String, hint: Option<String>, retryable: bool) -> Self {
        let mut end = Self::failed(message, hint, retryable);
        if let Some(error) = end.error.as_mut() {
            error.transport = true;
        }
        end
    }
}

/// One error row, the only one a turn may publish.
struct TurnError {
    message: String,
    hint: Option<String>,
    retryable: bool,
    /// Whether this row reports a dead transport, the one failure the bridge
    /// can also report for the same turn.
    transport: bool,
}

/// One report of a turn's dead transport, held until the turn settles.
///
/// A death can be reported more than once (the bridge's stdout-close notice,
/// the prompt failure that carries stderr, an exit row). These reports are one
/// row, so they are merged into the most specific message rather than published
/// separately.
struct TransportReport {
    turn_id: TurnId,
    message: String,
    retryable: bool,
}

impl TransportReport {
    /// Folds a second report of the same death into this one.
    ///
    /// The exit/status report wins over the generic close notice even when its
    /// text is shorter; [`most_specific_detail`] only compares two reports of
    /// the same kind.
    fn merge(&mut self, message: String, retryable: bool) {
        let held = std::mem::take(&mut self.message);
        self.message = merge_transport_detail(held, self.retryable, message, retryable);
        // An exit report is the non-retryable account of the death, so the
        // merged row keeps that once one has reported it.
        self.retryable &= retryable;
    }

    /// The row this report becomes when the turn settles without a failure of
    /// its own.
    fn into_error(self) -> TurnError {
        TurnError {
            message: self.message,
            hint: Some("Inspect the thread and retry when ready".into()),
            retryable: self.retryable,
            transport: true,
        }
    }
}

/// A transport death reported while no turn was active.
///
/// It is held until the exit/status report arrives (or the bounded wait for it
/// expires), so the generic close and the exit report that follows it merge
/// into one row for one session generation.
///
/// The adapter reports that death twice: the generic stdout-close notice, then
/// the exit status together with the stderr it drained. Holding both lets the
/// one row for the death keep the exit status and the stderr instead of taking
/// the generic notice.
struct IdleTransportReport {
    message: String,
    retryable: bool,
}

impl IdleTransportReport {
    /// Folds a second report of the same death into this one.
    ///
    /// A stderr-less exit report is still the specific account of the death,
    /// so it wins over the generic close notice rather than the longer text
    /// doing so.
    fn merge(&mut self, message: String, retryable: bool) {
        let held = std::mem::take(&mut self.message);
        self.message = merge_transport_detail(held, self.retryable, message, retryable);
        self.retryable &= retryable;
    }
}

/// A session whose transport died between turns, detached from the slot for
/// delivery but kept by its pump for the exit report.
struct DyingSession {
    session: SharedSession,
    /// The death's one row, held for the report that can sharpen it.
    report: Option<IdleTransportReport>,
    /// The reset this death already armed a schedule for.
    ///
    /// This belongs to the death, not to `scheduled_limit`: the person can
    /// cancel the resume while the exit report is still arriving, and that
    /// report repeats the same reset. Reading the live schedule would find it
    /// empty after the cancellation and arm the pause again. A different,
    /// newer reset is a different pause and may still arm.
    applied_reset: Option<DateTime<Utc>>,
}

impl TurnDispatch {
    fn new(turn_id: TurnId, input: UserInput) -> Self {
        Self {
            turn_id,
            input,
            state: std::sync::Mutex::new(DispatchState::Pending),
            started_generation: AtomicU64::new(NO_GENERATION),
            #[cfg(test)]
            recover_pause: std::sync::Mutex::new(None),
        }
    }

    /// Records that `generation` is the session started for this turn.
    fn note_started_generation(&self, generation: u64) {
        self.started_generation.store(generation, Ordering::SeqCst);
    }

    /// The generation of the session started for this turn, when one was.
    fn started_generation(&self) -> Option<u64> {
        match self.started_generation.load(Ordering::SeqCst) {
            NO_GENERATION => None,
            generation => Some(generation),
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
    ///
    /// A Stop on a claimed prompt is recorded even when a cancel can still be
    /// sent: the session can detach before the cancel reaches it (a restart, a
    /// shutdown, a model change), and the failed write that follows must know
    /// the person already stopped this turn. It settles as interrupted instead
    /// of replaying the prompt on a fresh session.
    ///
    /// A turn that is already claimed-and-stopped is still claimed: if its
    /// earlier cancel failed with the session open (a transient interrupt
    /// error), the Stop the person pressed again must try that cancel again
    /// rather than report success while the turn keeps running.
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
            DispatchState::Claimed | DispatchState::ClaimedStopped => {
                *state = DispatchState::ClaimedStopped;
                StopOutcome::AlreadySent
            }
            DispatchState::Stopped | DispatchState::Settled => StopOutcome::AlreadyStopped,
        }
    }

    /// Takes a prompt whose session went away back from its writer.
    ///
    /// The Stop check and the requeue are one operation under this dispatch's
    /// lock: a Stop arriving anywhere in it leaves exactly one owner of the
    /// turn, either "the prompt is queued again" or "the turn ends interrupted".
    /// Split into a check and a later mutation, a Stop landing between them
    /// would leave the turn active with nobody delivering it.
    async fn recover_lost_session(&self) -> RecoveryOutcome {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match *state {
            DispatchState::Pending | DispatchState::Claimed => {
                *state = DispatchState::Pending;
                RecoveryOutcome::Requeue
            }
            DispatchState::ClaimedStopped => RecoveryOutcome::Stopped,
            DispatchState::Stopped | DispatchState::Settled => RecoveryOutcome::Settled,
        }
    }

    /// Installs a test's hold on the recovery's inspect-to-decide window.
    #[cfg(test)]
    fn pause_recovery_at(&self, pause: Arc<RecoverPause>) {
        *self
            .recover_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pause);
    }

    /// Waits at that window when a test installed one.
    #[cfg(test)]
    async fn pause_recovery(&self) {
        let pause = self
            .recover_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(pause) = pause {
            pause.inspected.notify_one();
            pause.resume.notified().await;
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
    /// The session generation whose report armed this schedule.
    ///
    /// A thread can have several dead generations waiting for their exit
    /// reports. A report from an older generation must not replace a newer
    /// generation's pause: its reset may be earlier and the thread would
    /// resume before the newer provider reset.
    generation: Option<u64>,
    /// The ledger schedule this record describes, when it was armed here.
    ///
    /// A fire claims its attempt and then acts on the record under a second
    /// lock. A newer limit in that window replaces the schedule, and the old
    /// fire must not clear the newer pause or deliver on its behalf. A record
    /// with no identity never matches a claimed fire, and a schedule restored
    /// from the ledger carries the id it was stored with.
    schedule_id: Option<String>,
    /// The dispatch this pause parked, when it was armed to hold a prompt a
    /// failed write put back.
    ///
    /// Only that turn waits for the schedule. A pause armed by a dying session
    /// says nothing about a different prompt a Send is starting, so that
    /// prompt is delivered exactly as it would be with no pause at all.
    parked_turn: Option<TurnId>,
}

/// One armed schedule read back from the ledger at startup.
///
/// The id is kept so the record describes the exact attempt a fire may claim;
/// without it a fire for a schedule the record does not name must do nothing.
struct RestoredSchedule {
    resume_at: DateTime<Utc>,
    reset_at: Option<DateTime<Utc>>,
    schedule_id: String,
}

/// One limit observation: which session generation reported it and what the
/// adapter asked for.
#[derive(Clone, Copy, Debug)]
struct LimitObservation {
    generation: Option<u64>,
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
            // The manual override sticks until the next turn starts, so this
            // is true exactly for the placement it caused: a lifecycle
            // transition to Done after a new turn reports false.
            placed_by_user: self
                .state
                .manual
                .as_ref()
                .is_some_and(|manual| manual.tab == tab),
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
        self.transport_error_reported = None;
        self.transport_report = None;
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
            applied_limit_reset: None,
        });
        self.last_turn_id = Some(turn_id);
        dispatch
    }

    /// Allocates the next session generation for a startup.
    fn next_session_gen(&mut self) -> u64 {
        self.session_gen = self.session_gen.saturating_add(1);
        self.session_gen
    }

    /// Removes the live session when the slot still holds `generation`, and
    /// stops its pump.
    ///
    /// The caller holds the record lock, so the check and the clear are one
    /// step: a session started in the meantime (a newer generation) is never
    /// detached by an older path.
    fn detach_session(&mut self, generation: u64) -> Option<SharedSession> {
        let session = self.clear_session(generation)?;
        if let Some(pump) = self.pump.take() {
            pump.abort();
        }
        Some(session)
    }

    /// Removes the live session when the slot still holds `generation`.
    ///
    /// Unlike [`Self::detach_session`] this leaves the pump alone: the pump
    /// itself calls it when it notices its own transport died, and aborting the
    /// task that is running this code would skip the settlement after it.
    fn clear_session(&mut self, generation: u64) -> Option<SharedSession> {
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

    /// Detaches the live session for `generation` because its transport is
    /// known dead between turns.
    ///
    /// The slot is emptied under the record lock, so a Send that arrives before
    /// the exit report starts a fresh session instead of claiming a transport
    /// whose reader already failed. The pump keeps rendering the exit report:
    /// it holds the session itself, and the record keeps
    /// [`Self::dying_sessions`] to recognise its events and publish the one row.
    ///
    /// The pump is this task, so the slot's handle is cleared rather than
    /// aborted; the pump ends when it publishes the report or its deadline
    /// expires.
    fn mark_session_dying(&mut self, generation: u64) {
        let SessionSlot::Live {
            generation: current,
            session,
            applied_limit_reset,
        } = std::mem::replace(&mut self.session, SessionSlot::None)
        else {
            return;
        };
        if current != generation {
            // Some other session already owns the slot; leave it alone.
            self.session = SessionSlot::Live {
                generation: current,
                session,
                applied_limit_reset,
            };
            return;
        }
        self.dying_sessions.insert(
            generation,
            DyingSession {
                session,
                report: None,
                applied_reset: applied_limit_reset,
            },
        );
        self.pump = None;
        #[cfg(test)]
        self.dying_notify.notify_one();
    }

    /// Whether `generation`'s session is one the pump detached as dying.
    fn is_dying_session(&self, generation: u64) -> bool {
        self.dying_sessions.contains_key(&generation)
    }

    /// Removes the session a restart inspected, unless a turn claimed it since.
    ///
    /// The restart inspects a thread under one record lock and detaches under
    /// another; a Send can begin and claim a turn on that session in between.
    /// Detaching it then would take the session away from a turn that is
    /// already writing to it, so the restart defers: the session keeps running
    /// and the next session start picks up the new proxy routing.
    fn detach_restart_session(&mut self, generation: u64) -> Option<SharedSession> {
        if self.active.is_some() {
            return None;
        }
        self.detach_session(generation)
    }

    /// Marks `turn_id` as the turn whose dead transport already has one row.
    ///
    /// `false` means that row exists, so this report is the second half of the
    /// same transport failure. Only transport failures use this marker.
    fn note_transport_error(&mut self, turn_id: &TurnId) -> bool {
        if self.transport_error_reported.as_ref() == Some(turn_id) {
            return false;
        }
        self.transport_error_reported = Some(turn_id.clone());
        true
    }

    /// Holds a transport-death report for `turn_id` until the turn settles.
    ///
    /// `false` when there is nothing new to hold: the turn's row is already
    /// published, or this report only sharpened the detail already held. The
    /// settlement publishes the merged report as the turn's one row.
    fn note_transport_report(
        &mut self,
        turn_id: &TurnId,
        message: String,
        retryable: bool,
    ) -> bool {
        if self.transport_error_reported.as_ref() == Some(turn_id) {
            return false;
        }
        match self
            .transport_report
            .as_mut()
            .filter(|report| &report.turn_id == turn_id)
        {
            Some(report) => {
                report.merge(message, retryable);
                false
            }
            None => {
                self.transport_report = Some(TransportReport {
                    turn_id: turn_id.clone(),
                    message,
                    retryable,
                });
                true
            }
        }
    }

    /// Takes the held report for `turn_id`, if it is that turn's.
    fn take_transport_report(&mut self, turn_id: &TurnId) -> Option<TransportReport> {
        let held = self
            .transport_report
            .as_ref()
            .is_some_and(|report| &report.turn_id == turn_id);
        if held {
            self.transport_report.take()
        } else {
            None
        }
    }

    /// Holds a transport death reported between turns for `generation`.
    ///
    /// A second report for the same generation only sharpens the held detail.
    /// A report for a death whose row is already published is dropped, exactly
    /// as the immediate path dropped it, so one death never gets two rows.
    fn hold_idle_transport_report(
        &mut self,
        generation: u64,
        turn_id: &TurnId,
        message: String,
        retryable: bool,
    ) {
        if let Some(report) = self
            .dying_sessions
            .get_mut(&generation)
            .and_then(|death| death.report.as_mut())
        {
            report.merge(message, retryable);
            return;
        }
        if !self.dying_sessions.contains_key(&generation) {
            return;
        }
        if !self.note_transport_error(turn_id) {
            return;
        }
        if let Some(death) = self.dying_sessions.get_mut(&generation) {
            death.report = Some(IdleTransportReport { message, retryable });
        }
    }

    /// Records that `generation`'s death already armed `reset_at`.
    ///
    /// `false` when that exact reset was applied before: the exit report is
    /// repeating a pause whose schedule the person may have cancelled since,
    /// and arming it again would undo the cancellation.
    fn note_idle_reset_applied(&mut self, generation: u64, reset_at: DateTime<Utc>) -> bool {
        let Some(death) = self.dying_sessions.get_mut(&generation) else {
            return false;
        };
        if death.applied_reset == Some(reset_at) {
            return false;
        }
        death.applied_reset = Some(reset_at);
        true
    }

    /// Whether a newer session generation already armed this thread's schedule.
    ///
    /// A thread can have several dead generations waiting for their exit
    /// reports. Only the newest one names the pause: an older generation's
    /// earlier reset must not pull the resume forward. A schedule with no known
    /// generation is treated as the newest, because there is nothing to prove
    /// the reporting generation is newer than it.
    fn newer_generation_armed_schedule(&self, generation: u64) -> bool {
        self.scheduled_limit.as_ref().is_some_and(|scheduled| {
            scheduled.auto_resume && scheduled.generation.is_none_or(|armed| armed > generation)
        })
    }

    /// Records a newer generation as the owner of a pause that already exists.
    ///
    /// A newer generation that confirms the same provider reset is the same
    /// pause, so it keeps the attempt and jitter already chosen; it now owns
    /// the schedule, though, so a delayed report from an older generation
    /// cannot replace it. Returns `true` when the armed schedule already covers
    /// this reset and nothing new must be armed.
    fn adopt_schedule_generation(
        &mut self,
        generation: Option<u64>,
        reset_at: Option<DateTime<Utc>>,
    ) -> bool {
        let Some(scheduled) = self.scheduled_limit.as_mut() else {
            return false;
        };
        if !scheduled.auto_resume || scheduled.reset_at != reset_at {
            return false;
        }
        // An unknown reset is a backoff attempt, not a provider pause two
        // generations can confirm, so only a named reset transfers ownership.
        if reset_at.is_some()
            && let Some(reporting) = generation
            && scheduled.generation.is_none_or(|armed| reporting > armed)
        {
            scheduled.generation = Some(reporting);
        }
        true
    }

    /// Whether the active turn's prompt is still waiting for a session.
    fn pending_turn(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.dispatch.state() == DispatchState::Pending)
    }

    /// Whether this thread's pause was armed to hold `turn_id`.
    ///
    /// A pause armed for a requeued write owns that one dispatch. Any other
    /// prompt (a Send that started a replacement while an old agent's exit
    /// report armed a pause) is delivered as it would be with no pause.
    fn pause_parks_turn(&self, turn_id: &TurnId) -> bool {
        self.scheduled_limit
            .as_ref()
            .is_some_and(|scheduled| scheduled.parked_turn.as_ref() == Some(turn_id))
    }

    /// Whether the active, still-pending turn is the one the pause parks.
    ///
    /// A fire may only release the prompt its own schedule was armed for. A
    /// turn a Send started while an old agent's exit armed a pause is pending
    /// too, but that pause does not name it, so the fire must leave it alone
    /// and spend no attempt on it.
    fn parks_active_turn(&self) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.dispatch.state() == DispatchState::Pending
                && self.pause_parks_turn(&active.turn_id)
        })
    }

    /// Whether the active, still-pending turn is the prompt stored on the row.
    ///
    /// The thread row's `parked` prompt is what a restart rebuilds the paused
    /// dispatch from. A record restored from the journal has no in-memory pause
    /// yet (the ledger is attached later), and one whose startup resync could
    /// not read the ledger never binds one, so [`Self::parks_active_turn`] is
    /// false for exactly the prompt that still needs a delivery owner. Shutdown
    /// must leave that turn and its saved prompt alone for the same reason it
    /// leaves an armed pause's turn alone.
    fn parks_persisted_prompt(&self) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.dispatch.state() == DispatchState::Pending
                && self
                    .meta
                    .parked
                    .as_ref()
                    .is_some_and(|parked| parked.turn_id == active.turn_id)
        })
    }

    /// Whether the thread's pause is the schedule with this ledger id.
    ///
    /// A fire claims an attempt in the ledger before it acts on the record, so
    /// the record has to name the attempt first. A record with no pause (a
    /// restarted daemon before the ledger resync) names nothing, and claiming
    /// on its behalf would consume the attempt without delivering a turn.
    fn names_schedule(&self, schedule_id: &str) -> bool {
        self.scheduled_limit
            .as_ref()
            .and_then(|scheduled| scheduled.schedule_id.as_deref())
            == Some(schedule_id)
    }

    /// The prompt a replacement schedule must keep parking, if any.
    ///
    /// A newer reset can replace the pause a failed write armed while that
    /// write's prompt is still the active, pending dispatch. The replacement
    /// has to name that prompt too, or a pump-end `advance_queue` would deliver
    /// it into the provider the newer reset paused.
    fn parked_turn_to_keep(&self) -> Option<TurnId> {
        let active = self.active.as_ref()?;
        if active.dispatch.state() != DispatchState::Pending {
            return None;
        }
        let named = self.pause_parks_turn(&active.turn_id)
            || self
                .meta
                .parked
                .as_ref()
                .is_some_and(|parked| parked.turn_id == active.turn_id);
        named.then(|| active.turn_id.clone())
    }

    /// The session generation whose prompt the active turn is.
    ///
    /// A startup records the generation it started for the turn; a turn
    /// delivered on an already-live session belongs to that session. A report
    /// from any other generation describes a different session, so it must not
    /// touch this turn's applied-reset marker.
    fn active_turn_generation(&self) -> Option<u64> {
        let active = self.active.as_ref()?;
        active
            .dispatch
            .started_generation()
            .or_else(|| self.session.generation())
    }

    /// Records that `generation`'s report already applied `reset_at`.
    ///
    /// The marker lives on the turn (so a cancelled schedule does not hide it)
    /// and on the live slot (so a death between turns carries it into its
    /// dying entry). Both name the generation, so an older generation's late
    /// report can never consume a newer one's own first report. The turn's
    /// marker is only written by the generation that owns the turn: a dying
    /// session's delayed report must not overwrite a newer turn's marker and
    /// make that turn's own completion arm the pause again after a cancel.
    fn note_applied_reset(&mut self, generation: Option<u64>, reset_at: Option<DateTime<Utc>>) {
        let Some(generation) = generation else {
            return;
        };
        if self.active.is_some() && self.active_turn_generation() == Some(generation) {
            if let Some(active) = self.active.as_mut() {
                active.applied_limit_reset = Some((generation, reset_at));
            }
        }
        if reset_at.is_some()
            && self.session.generation() == Some(generation)
            && let SessionSlot::Live {
                applied_limit_reset,
                ..
            } = &mut self.session
        {
            *applied_limit_reset = reset_at;
        }
    }

    /// Clears the thread's pause and any prompt it was holding for delivery.
    fn clear_pause(&mut self) {
        self.scheduled_limit = None;
        self.resume_at = None;
        self.meta.parked = None;
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
                text_generation: std::sync::OnceLock::new(),
                title_jobs: std::sync::Mutex::new(HashSet::new()),
                title_settled: tokio::sync::Notify::new(),
                last_schedule_check: std::sync::Mutex::new(None),
                resync_pending: AtomicBool::new(false),
                resync_retry: std::sync::Mutex::new(ResyncRetry::default()),
                clock,
                #[cfg(test)]
                restart_pause: std::sync::Mutex::new(None),
                #[cfg(test)]
                fire_pause: std::sync::Mutex::new(None),
                #[cfg(test)]
                resync_pause: std::sync::Mutex::new(None),
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
    /// Called once during daemon assembly, before the ticker starts: a fire
    /// only claims a schedule the thread's record names, and the record learns
    /// its schedule in [`Self::resync_meter_state`]. Callers must resync before
    /// starting the ticker.
    pub fn set_meter(&self, meter: Arc<MeterHub>) {
        let _ = self.inner.meter.set(meter);
    }

    /// Attaches the text-generation service used for thread titles.
    ///
    /// Called once during daemon assembly. A manager without one simply keeps
    /// every prompt-derived title, which is what unit tests and the mock
    /// harness want.
    pub fn set_text_generation(&self, service: Arc<dyn TextGeneration>) {
        let _ = self.inner.text_generation.set(service);
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
    /// tick writes a fresh projection. A read that fails is retried by the
    /// ticker (see [`Inner::resync_meter_state`]).
    pub async fn resync_meter_state(&self) {
        self.inner.resync_meter_state().await;
    }

    /// Durable metadata for one thread, cloned for callers outside the manager.
    pub async fn thread_meta(&self, thread_id: &ThreadId) -> Option<ThreadMeta> {
        let handle = self.inner.handle(thread_id).await.ok()?;
        let record = handle.lock().await;
        Some(record.meta.clone())
    }

    /// Whether a turn is in flight for this thread.
    ///
    /// True from the moment `begin_turn` runs until the record completes and
    /// clears it. This is independent of the agent state: a failed or denied
    /// tool call sets `AgentState::Error` while the turn keeps running, so a
    /// caller asking "could this thread still write?" must ask both.
    pub async fn turn_in_flight(&self, thread_id: &ThreadId) -> bool {
        let Ok(handle) = self.inner.handle(thread_id).await else {
            return false;
        };
        let record = handle.lock().await;
        record.active.is_some()
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

    /// The journal head, for the subscribe answer's replay boundary.
    ///
    /// The caller must hold [`ThreadManager::lock_append`] so no event can be
    /// allocated above the returned sequence between the read and going live.
    pub async fn journal_head(&self) -> Result<u64, ManagerError> {
        self.inner
            .journal
            .head()
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
        let title_eligible = request.parent_thread_id.is_none() && request.subagent_id.is_none();
        let title_prompt = request.prompt.clone();
        // The default title mode uses this thread's own harness, so a prompt is
        // never handed to a different provider unasked.
        let title_harness = request.harness;
        let meta = ThreadMeta {
            thread_id: thread_id.clone(),
            parent_thread_id: request.parent_thread_id.clone(),
            subagent_id: request.subagent_id.clone(),
            host: request.host,
            harness: request.harness,
            title: title_from_prompt(&request.prompt, &request.workspace_path),
            title_source: TitleSource::Prompt,
            workspace_path: request.workspace_path,
            model: request.model,
            runtime_mode: request.runtime_mode,
            worktree: request.worktree,
            created_at: now,
            usage: Usage::default(),
            resume_id: None,
            parked: None,
        };
        let title_workspace = meta.workspace_path.clone();

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
            transport_error_reported: None,
            transport_report: None,
            dying_sessions: BTreeMap::new(),
            #[cfg(test)]
            dying_notify: Arc::new(tokio::sync::Notify::new()),
            scheduled_limit: None,
            resume_at: None,
            limit: None,
            ttft_ms: None,
            context_window: None,
            branch: None,
            restored_parked: false,
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

        if title_eligible {
            self.inner
                .spawn_title_job(
                    thread_id.clone(),
                    title_prompt,
                    title_workspace,
                    title_harness,
                )
                .await;
        }

        self.inner.spawn_session(handle);
        Ok(CreatedThread {
            thread_id,
            turn_id,
            summary,
            seq,
        })
    }

    /// Renames a thread by hand. The name sticks; generation never replaces it.
    pub async fn rename_thread(
        &self,
        thread_id: &ThreadId,
        title: &str,
    ) -> Result<ThreadSummary, ManagerError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(ManagerError::InvalidRequest(
                "a thread title cannot be empty".into(),
            ));
        }
        if title.chars().count() > 200 {
            return Err(ManagerError::InvalidRequest(
                "a thread title is at most 200 characters".into(),
            ));
        }
        if title.contains(['\n', '\r']) {
            return Err(ManagerError::InvalidRequest(
                "a thread title must be one line".into(),
            ));
        }
        let handle = self.inner.handle(thread_id).await?;
        let mut record = handle.lock().await;
        record.meta.title = title.to_owned();
        record.meta.title_source = TitleSource::Renamed;
        let (summary, _) = self.inner.emit(&mut record, Vec::new(), Vec::new()).await?;
        Ok(summary)
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
            let stale_schedule = record.scheduled_limit.take().is_some();
            if stale_schedule {
                // The new message is the next turn, so no resume target
                // describes the thread any more.
                record.resume_at = None;
            }
            if stale_schedule && let Some(meter) = self.inner.meter.get() {
                let _ = meter.cancel_schedule(thread_id).await;
            }
            // A Send overrides a pause that was holding a requeued prompt: the
            // parked turn is delivered now and the new message queues behind
            // it. Leaving it pending here would strand it, because the schedule
            // that would have delivered it is gone. A pending turn the pause
            // does not park is not a release, and an idle thread's stale pause
            // holds no prompt to deliver at all.
            let release_pending =
                stale_schedule && record.pending_turn() && record.parks_persisted_prompt();
            record.queued.push_back(UserInput {
                text: text.clone(),
                attachments: harness_attachments,
            });
            let mut domain = vec![ThreadEvent::MessageReceived {
                source: h4rn_core::MessageSource::User,
                at: now,
            }];
            if release_pending {
                // The release is the person's action, so the thread is running
                // that prompt now: leave the paused phase and drop the limit
                // that explained it. The turn itself was already counted.
                domain.push(ThreadEvent::LimitResumeReleased { at: now });
            }
            if stale_schedule {
                // The cancelled schedule explains nothing any more, so drop the
                // pause it left behind even when it held no prompt to release.
                record.meta.parked = None;
                record.limit = None;
            }
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
            let emitted = self.inner.emit(&mut record, domain, wire).await;
            drop(record);
            if release_pending {
                // The delivery owner must exist even when the journal write
                // failed: the schedule is already cancelled, so without this
                // the parked turn would stay Pending forever.
                self.inner.spawn_advance(handle);
            }
            let (_, seq) = emitted?;
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
            #[cfg(test)]
            {
                let pause = self
                    .inner
                    .restart_pause
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                if let Some(pause) = pause {
                    pause.inspected.notify_one();
                    pause.resume.notified().await;
                }
            }
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
                // fresh session, which belongs to its own startup. A Send may
                // also have claimed a turn on it since the inspection, and that
                // turn now owns the session: the restart waits for its next
                // session start instead.
                record.detach_restart_session(generation)
            };
            let Some(session) = detached else {
                continue;
            };
            if let Err(error) = session.lock().await.shutdown().await {
                tracing::warn!(%thread_id, %error, "harness shutdown before a proxy restart failed");
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
        let mut release_pending = false;
        if let Some(record) = record.as_mut() {
            let was_paused = record.scheduled_limit.take().is_some();
            record.resume_at = None;
            release_pending = was_paused && record.pending_turn();
            if release_pending {
                // The person's cancellation releases the parked prompt: the
                // thread runs it now, so it is no longer paused and the limit
                // no longer explains a wait.
                record.meta.parked = None;
                record.limit = None;
            }
            if record.state.phase == h4rn_core::Phase::RateLimited || release_pending {
                let event = if release_pending {
                    ThreadEvent::LimitResumeReleased { at: self.now() }
                } else {
                    ThreadEvent::LimitAutoResumeSet {
                        auto_resume: false,
                        at: self.now(),
                    }
                };
                if let Err(error) = self.inner.emit(record, vec![event], Vec::new()).await {
                    // The cancellation and the release still stand in memory;
                    // the projection catches up on the next tick. Giving up
                    // here would leave the released turn with no owner.
                    tracing::error!(%error, "could not record the cancelled resume");
                }
            }
        }
        drop(record);
        if release_pending && let Some(handle) = handle {
            // The person's cancellation overrides the pause. A prompt a failed
            // write put back is waiting for the schedule that no longer exists,
            // so deliver it now: without this it stays pending with nothing to
            // start it.
            self.inner.spawn_advance(handle);
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
            let (thread_id, sessions) = {
                let mut record = handle.lock().await;
                record.closed = true;
                // A prompt parked behind an armed schedule is already durable
                // on the thread row, and the schedule that will deliver it is
                // in the ledger. Settling it here would erase `parked_json` and
                // leave the restarted daemon firing the policy's prompt instead
                // of the person's, so that one turn is left exactly as it is.
                // The same holds while the startup resync is unresolved: the
                // record restored before the ledger was attached, or its
                // ledger read failed, so it does not name the schedule yet —
                // but the schedule is still armed, and a later boot will fire
                // it.
                if !record.parks_active_turn()
                    && !record.parks_persisted_prompt()
                    && let Some(turn_id) =
                        record.active.as_ref().map(|active| active.turn_id.clone())
                {
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
                // `detach_session` is the one place that stops the pump.
                let mut sessions = Vec::new();
                if let Some(generation) = record.session.generation()
                    && let Some(session) = record.detach_session(generation)
                {
                    sessions.push(session);
                }
                // Sessions whose reader already died are out of the slot but
                // still a pump's; each needs the same teardown.
                for (_, dying) in std::mem::take(&mut record.dying_sessions) {
                    sessions.push(dying.session);
                }
                (thread_id, sessions)
            };
            for session in sessions {
                // The record lock is released first: a lock order of
                // record-then-session would deadlock with a Stop.
                let mut session = session.lock().await;
                if let Err(error) = session.shutdown().await {
                    tracing::warn!(%thread_id, %error, "harness shutdown failed");
                }
            }
        }
    }
}

impl Inner {
    /// Starts one title attempt in the background, at most one per thread.
    ///
    /// The first-prompt title is already on the row; a generated title replaces
    /// it only while the row still says the title came from the prompt, so a
    /// rename that lands first wins and a rename that lands later overwrites
    /// the generated title.
    ///
    /// Off returns before a job exists: nothing is attempted and nothing is
    /// logged per thread.
    async fn spawn_title_job(
        self: &Arc<Self>,
        thread_id: ThreadId,
        prompt: String,
        workspace: String,
        harness: HarnessKind,
    ) {
        let Some(service) = self.text_generation.get().cloned() else {
            return;
        };
        if !service.titles_enabled().await {
            return;
        }
        if !self
            .title_jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(thread_id.as_str().to_owned())
        {
            return;
        }
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            let result = service.thread_title(&prompt, &workspace, harness).await;
            match result {
                Ok(title) => inner.apply_generated_title(&thread_id, title).await,
                // Titles were turned off after the job started: still nothing
                // to report, and nothing to attempt.
                Err(TextGenerationError::Disabled) => {}
                Err(error) => {
                    // One line per failed attempt; the fallback title stays.
                    tracing::info!(
                        thread_id = thread_id.as_str(),
                        %error,
                        "kept the first-prompt thread title"
                    );
                }
            }
            inner
                .title_jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(thread_id.as_str());
            inner.title_settled.notify_waiters();
        });
    }

    /// Applies a generated title unless the person already renamed the thread.
    async fn apply_generated_title(&self, thread_id: &ThreadId, title: String) {
        let Ok(handle) = self.handle(thread_id).await else {
            return;
        };
        let mut record = handle.lock().await;
        if record.meta.title_source != TitleSource::Prompt {
            return;
        }
        record.meta.title = title;
        record.meta.title_source = TitleSource::Generated;
        if let Err(error) = self.emit(&mut record, Vec::new(), Vec::new()).await {
            tracing::warn!(%error, thread_id = thread_id.as_str(), "could not publish a generated title");
        }
    }

    /// Waits until no title attempt is in flight (tests).
    #[cfg(test)]
    async fn wait_for_titles(&self) {
        loop {
            let notified = self.title_settled.notified();
            let busy = !self
                .title_jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty();
            if !busy {
                return;
            }
            notified.await;
        }
    }

    /// The clock every lifecycle decision reads.
    fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    /// The monotonic reading retry waits are measured against.
    fn monotonic(&self) -> Instant {
        self.clock.monotonic()
    }

    fn local_host_is(&self, host: &HostId) -> bool {
        &self.local_host == host
    }

    /// Restores summary-visible schedule and limit state from the ledger.
    ///
    /// The schedule ids are restored independently of the limit history: a
    /// failed limit read only costs the reset time shown on the pause, while a
    /// lost schedule id would leave every restored record `Unnamed`, and no
    /// due attempt could ever be fired for it. Any failed part of the pass —
    /// either read, or the per-row re-check that confirms a restored schedule
    /// before binding it — marks the resync pending so [`Self::tick`] runs the
    /// whole pass again, rather than leaving the schedule unnamed until the
    /// next restart. A retry never overwrites what
    /// happened after it read the ledger: a newer limit can arm a schedule
    /// between the read and the record lock, and that pause is newer than the
    /// snapshot.
    async fn resync_meter_state(self: &Arc<Self>) {
        let Some(meter) = self.meter.get().cloned() else {
            return;
        };
        let schedules = match meter.schedules().await {
            Ok(schedules) => schedules,
            Err(error) => {
                tracing::warn!(%error, "could not re-read auto-resume schedules");
                self.note_resync_failure();
                return;
            }
        };
        let mut limits_failed = false;
        let limits = match meter.limits_report().await {
            Ok(report) => report.limits,
            Err(error) => {
                tracing::warn!(%error, "could not re-read the usage limit history");
                limits_failed = true;
                // The schedule ids are the pause's delivery owner, so restore
                // them now; the retry above fills in the reset times.
                Vec::new()
            }
        };

        let mut armed: HashMap<String, RestoredSchedule> = HashMap::new();
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
                    .or_insert(RestoredSchedule {
                        resume_at: schedule.resume_at,
                        reset_at,
                        schedule_id: schedule.schedule_id,
                    });
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

        // A newer limit can arm a schedule in the window between the ledger
        // read above and the record lock below.
        #[cfg(test)]
        {
            let pause = self
                .resync_pause
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            if let Some(pause) = pause {
                pause.read.notify_one();
                pause.resume.notified().await;
            }
        }

        let handles: Vec<ThreadHandle> = {
            let threads = self.threads.lock().await;
            threads.values().cloned().collect()
        };
        let mut release: Vec<ThreadHandle> = Vec::new();
        // A re-check that could not read the ledger leaves the record alone so a
        // later retry decides what its pause is.
        let mut recheck_failed = false;
        for handle in handles {
            let mut record = handle.lock().await;
            let key = record.meta.thread_id.as_str().to_owned();
            let before = record.signature(&self.options.settings, self.now());
            let restored_parked = std::mem::take(&mut record.restored_parked);
            let parked_turn = restored_parked
                .then(|| record.active.as_ref().map(|active| active.turn_id.clone()))
                .flatten()
                // A retry of the resync must keep the prompt the first pass
                // already bound to this pause; the journal flag was spent then.
                .or_else(|| record.parked_turn_to_keep());
            // Only a snapshot that still describes the record's pause may
            // refresh its limit details.
            let mut refresh_limit = true;
            let mut release_prompt = false;
            let mut next_resume_at = None;
            match armed.remove(&key) {
                // Every pause names its ledger id: both places that arm one
                // take the id from the row they just wrote, and a restored
                // identity is the only other way to get a pause. A pause whose
                // id is missing is therefore impossible, and an unresolved
                // pause is exactly a record with no pause at all.
                Some(restored) => match record.scheduled_limit.as_mut() {
                    None => {
                        // Cancelling takes this same record lock, so a person
                        // can cancel the snapshotted schedule after the read
                        // above and before this pass binds it. Binding the
                        // cancelled row would leave the restored prompt waiting
                        // for a fire that can never happen, so re-read that one
                        // row under the lock and release the prompt when it is
                        // no longer armed, exactly as the cancellation does.
                        match meter.schedule(&restored.schedule_id).await {
                            Ok(Some(schedule))
                                if matches!(
                                    schedule.state,
                                    ResumeState::Scheduled | ResumeState::Waiting
                                ) =>
                            {
                                next_resume_at = Some(restored.resume_at);
                                record.scheduled_limit = Some(ScheduledLimit {
                                    reset_at: restored.reset_at,
                                    auto_resume: true,
                                    generation: None,
                                    // The ledger id is the pause's identity: a
                                    // fire claimed for this attempt must match
                                    // it before it acts.
                                    schedule_id: Some(restored.schedule_id),
                                    // A prompt restored from the journal is what
                                    // this pause was holding; no other turn may
                                    // be parked by it.
                                    parked_turn,
                                });
                            }
                            Ok(_) => {
                                // The row was cancelled or finished after the
                                // read above. Only a prompt that is really
                                // parked needs a release: an idle thread, or
                                // one whose cancel already released its prompt,
                                // just loses the stale pause without a phase
                                // change.
                                release_prompt =
                                    record.pending_turn() && record.parks_persisted_prompt();
                                if !release_prompt {
                                    record.resume_at = None;
                                    record.meta.parked = None;
                                }
                                refresh_limit = false;
                            }
                            Err(error) => {
                                tracing::warn!(
                                    %error,
                                    "could not re-read the restored auto-resume schedule"
                                );
                                recheck_failed = true;
                                refresh_limit = false;
                            }
                        }
                    }
                    // The retry fills in the details the record's own attempt
                    // is still missing.
                    Some(scheduled)
                        if scheduled.schedule_id.as_deref()
                            == Some(restored.schedule_id.as_str()) =>
                    {
                        scheduled.reset_at = restored.reset_at;
                        next_resume_at = Some(restored.resume_at);
                    }
                    // A newer limit armed a different schedule after this
                    // resync read the ledger. That pause is newer than the
                    // snapshot, so restoring this one would move the thread
                    // back to an older reset and steal the parked prompt.
                    Some(_) => refresh_limit = false,
                },
                None if record.scheduled_limit.is_none()
                    && record.parks_persisted_prompt()
                    && record.pending_turn() =>
                {
                    // No armed schedule names this thread, so nothing would
                    // deliver the prompt the thread row still parks — whether
                    // the pause never survived the restart or was cancelled
                    // after an earlier pass read the ledger. An arm or a
                    // cancellation holds this record lock across its ledger
                    // write, so a pause that is about to exist is never seen
                    // here as a missing row.
                    release_prompt = true;
                }
                None => {
                    // No schedule names this thread. A record whose pause is
                    // newer than the snapshot keeps it (and its limit).
                    if record.scheduled_limit.is_some() {
                        refresh_limit = false;
                    }
                }
            }
            if release_prompt {
                // Release it exactly as a cancel, a Send or a fire does: the
                // prompt runs now, so the thread leaves the paused phase and
                // the limit no longer explains a wait.
                record.meta.parked = None;
                record.limit = None;
                record.resume_at = None;
                let released = vec![ThreadEvent::LimitResumeReleased { at: self.now() }];
                if let Err(error) = self.emit(&mut record, released, Vec::new()).await {
                    // The release still stands in memory; the projection
                    // catches up on the next tick, and the prompt below must
                    // not be left without an owner.
                    tracing::error!(%error, "could not record the released parked prompt");
                }
                release.push(handle.clone());
            }
            if let Some(at) = next_resume_at {
                record.resume_at = Some(at);
            }
            // Only show a limit on a thread the limit still explains, and only
            // from a snapshot that still describes this record's pause.
            if !limits_failed && refresh_limit {
                record.limit = if record.state.phase == h4rn_core::Phase::RateLimited
                    || record.scheduled_limit.is_some()
                {
                    latest.remove(&key)
                } else {
                    None
                };
            }
            // A retry that changes nothing must not re-append and re-broadcast
            // the summary on every tick.
            if record.signature(&self.options.settings, self.now()) != before {
                record.last_signature = None;
            }
        }
        for handle in release {
            self.spawn_advance(handle);
        }
        // Both reads landed: the ticker need not try again.
        if limits_failed || recheck_failed {
            self.note_resync_failure();
        } else {
            self.resync_pending.store(false, Ordering::Relaxed);
            self.note_resync_success();
        }
    }

    /// Whether a failed startup resync may be retried yet.
    fn resync_retry_due(&self) -> bool {
        let retry = self
            .resync_retry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        retry.at.is_none_or(|at| self.monotonic() >= at)
    }

    /// Records a failed resync and waits before the next attempt.
    ///
    /// Any partial failure leaves the resync unresolved: a pass that read the
    /// schedule list and limit history but could not re-read one schedule row
    /// has still not attached the pause that row owns, so the ticker must run
    /// the whole pass again instead of leaving the restored prompt unnamed.
    fn note_resync_failure(&self) {
        self.resync_pending.store(true, Ordering::Relaxed);
        let mut retry = self
            .resync_retry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        retry.at = Some(self.monotonic() + retry.backoff);
        retry.backoff = (retry.backoff * 2).min(Duration::from_secs(60));
    }

    /// Clears the wait after both reads landed.
    fn note_resync_success(&self) {
        *self
            .resync_retry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ResyncRetry::default();
    }

    /// Makes the next failed-resync retry due now.
    ///
    /// Test-only: the wait is time-based, and a test advances its own clock or
    /// moves the gate aside instead of sleeping.
    #[cfg(test)]
    fn clear_resync_retry(&self) {
        *self
            .resync_retry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = ResyncRetry::default();
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
        // The signature is cached only after the append succeeds. A failed
        // write leaves the release (or any other change) in memory but not in
        // the journal, so the next tick must see the difference and publish it
        // instead of trusting a cache that is already ahead of storage.

        let _guard = self.emit_lock.lock().await;
        let seqs = self
            .journal
            .append(&record.meta, &domain, &wire)
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))?;
        self.broadcast(&record.meta.thread_id, &wire, &seqs);
        record.last_signature = Some(SummarySignature::of(&summary));
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
                transport_error_reported: None,
                transport_report: None,
                dying_sessions: BTreeMap::new(),
                #[cfg(test)]
                dying_notify: Arc::new(tokio::sync::Notify::new()),
                scheduled_limit: None,
                resume_at: None,
                limit: None,
                ttft_ms: None,
                context_window: None,
                branch,
                restored_parked: false,
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
            } else if record.state.phase == h4rn_core::Phase::RateLimited
                && let Some(parked) = record.meta.parked.clone()
            {
                // A pause was holding this prompt when the daemon stopped. The
                // prompt is already in the journal and its turn is the paused
                // one, so restore the dispatch: the schedule's firing (or the
                // person's Send or cancel) then delivers it instead of the
                // policy's "continue". The ledger is attached later, which is
                // when the record learns whether the schedule survived.
                record.active = Some(ActiveTurn {
                    turn_id: parked.turn_id.clone(),
                    item_id: assistant_item_id(&parked.turn_id),
                    text: String::new(),
                    usage: Usage::default(),
                    meter: TurnMeter::new(self.now()),
                    tools: HashMap::new(),
                    last_tick: None,
                    dispatch: Arc::new(TurnDispatch::new(parked.turn_id.clone(), parked.input())),
                    applied_limit_reset: None,
                });
                record.last_turn_id = Some(parked.turn_id);
                record.restored_parked = true;
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
        // A transport death noted while the turn ran is part of the turn's one
        // row: merged into the settlement's own transport failure, published on
        // its own when the settlement carries no failure, and kept separate
        // from an unrelated failure (which must never hide it).
        let mut error = error;
        if let Some(report) = record.take_transport_report(turn_id) {
            match error.as_mut() {
                Some(failure) if failure.transport => {
                    // The exit/status report is the death's specific account,
                    // so it wins over this turn's generic "transport closed"
                    // failure even when its text is shorter.
                    failure.message = merge_transport_detail(
                        std::mem::take(&mut failure.message),
                        failure.retryable,
                        report.message,
                        report.retryable,
                    );
                    failure.retryable &= report.retryable;
                }
                Some(_) => {
                    let death = report.into_error();
                    record.note_transport_error(turn_id);
                    domain.push(ThreadEvent::FailureRaised { at: now });
                    wire.push(Event::Error {
                        thread_id: Some(record.meta.thread_id.clone()),
                        message: death.message,
                        hint: death.hint,
                        retryable: death.retryable,
                    });
                }
                None => error = Some(report.into_error()),
            }
        }
        if let Some(error) = error {
            // A dead transport can be reported twice (the bridge's row and the
            // daemon's write failure). Every other failure always gets a row, so
            // an earlier unrelated error never hides this one.
            let duplicate = error.transport && !record.note_transport_error(turn_id);
            if duplicate {
                tracing::debug!(
                    thread_id = %record.meta.thread_id,
                    %turn_id,
                    "collapsed a second report of the same transport failure"
                );
            } else {
                domain.push(ThreadEvent::FailureRaised { at: now });
                wire.push(Event::Error {
                    thread_id: Some(record.meta.thread_id.clone()),
                    message: error.message,
                    hint: error.hint,
                    retryable: error.retryable,
                });
            }
        }
        record.active = None;
        // The pause, if any, was holding that turn's prompt. A settled turn has
        // nothing left for a schedule to deliver, and the row must not restore
        // a prompt that already ended.
        record.meta.parked = None;
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
        //
        // A startup with no turn to carry starts nothing. It was spawned for a
        // turn that a Stop settled before this task reached the lock; starting
        // an agent for nobody would leave a session no turn asked for, and a
        // failure would be blamed on whichever turn arrived next. The slot stays
        // `None`, so a Send that arrives now starts its own session.
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
            let Some(turn) = record
                .active
                .as_ref()
                .map(|active| Arc::clone(&active.dispatch))
            else {
                return;
            };
            let harness_kind = record.meta.harness;
            let Some(harness) = self.harnesses.get(harness_kind) else {
                drop(record);
                self.fail_session(
                    handle,
                    None,
                    turn,
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
        let health = session.transport_health();
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
                applied_limit_reset: None,
            };
            // This session was started for the captured turn: the pump can
            // tell its death from the end of a previous turn's agent.
            turn.note_started_generation(generation);
            if context_window.is_some() {
                record.context_window = context_window;
            }
            let inner = Arc::clone(self);
            let pump_handle = handle.clone();
            let pump_session = Arc::clone(&session);
            record.pump = Some(tokio::spawn(async move {
                inner
                    .run_pump(pump_handle, generation, pump_session, health, stream)
                    .await;
            }));
        }

        // Claim the turn this startup carried: a Stop that won during startup
        // left its dispatch stopped, so its prompt is dropped and the queue
        // moves on instead.
        let still_active = {
            let record = handle.lock().await;
            record.dispatch_for(&turn.turn_id).is_some() && !record.pause_parks_turn(&turn.turn_id)
        };
        if still_active {
            self.deliver(handle.clone(), turn, Arc::clone(&session))
                .await;
        }
        // Whatever is waiting now (a queued turn, or none) is delivered on this
        // live session.
        self.advance_queue(handle).await;
    }

    /// Ends the turn a failed startup captured.
    ///
    /// A failure from a startup that no longer owns the slot (a restart, a
    /// shutdown) changes nothing: that startup's turn belongs to another path.
    /// Only the captured dispatch is ever failed here. A turn that became active
    /// after the capture (a Stop ended the captured one and promoted the next,
    /// or a Send started another) was never this startup's to settle; it gets
    /// its own startup attempt so it is not stranded behind the slot this
    /// failure clears and is not failed for a session it never asked for.
    async fn fail_session(
        self: &Arc<Self>,
        handle: ThreadHandle,
        generation: Option<u64>,
        turn: Arc<TurnDispatch>,
        message: String,
    ) {
        // Clearing the slot this startup owns and settling the turn it carried
        // are one lock scope. A second startup scheduled meanwhile (a Send, a
        // delayed queue advance) must not see the empty slot while the turn is
        // still active: it would start an agent and write that turn's prompt
        // just before this path records the failure. Under one scope it either
        // runs before this startup (and sees its `Starting` slot) or after the
        // turn has ended (and starts nothing).
        let (settled, advance) = {
            let mut record = handle.lock().await;
            if let Some(generation) = generation {
                if !record.session.is_starting(generation) {
                    // The slot moved on: this startup no longer owns anything.
                    return;
                }
                record.session = SessionSlot::None;
            }
            let captured = record
                .dispatch_for(&turn.turn_id)
                .map(|_| turn.turn_id.clone());
            let settled = match captured {
                Some(turn_id) => {
                    self.settle_turn_locked(
                        &mut record,
                        &turn_id,
                        TurnEnd::failed(
                            message,
                            Some("Install the harness or pick a different one, then retry".into()),
                            false,
                        ),
                    )
                    .await
                }
                None => false,
            };
            // A settlement spawns its own advance; only a turn this failure
            // leaves alone (or a turn the settlement promoted) needs one here.
            (settled, record.active.is_some())
        };
        if settled || advance {
            self.spawn_advance(handle);
        }
    }

    /// Records one usage limit and arms its auto-resume schedule.
    ///
    /// The pump's live limit event and the failed-write path share this: the
    /// failed write detects a limit in its merged failure detail before it
    /// settles the turn, and arming the schedule under the same record lock
    /// keeps the settlement from promoting queued work past it. Returns the
    /// row to publish when arming fails.
    ///
    /// `generation` names the session whose report carries the limit. A dying
    /// generation's late report may only arm a pause when no newer generation
    /// has armed one already: its reset can be earlier, and replacing the newer
    /// schedule would resume the thread before the provider it is waiting on.
    /// Once the thread has a pause, the reset is only ever applied to the live
    /// session that reported it: an older generation's late report must not
    /// mark the replacement's own reset as applied.
    async fn apply_limit_hit(
        &self,
        record: &mut ThreadRecord,
        generation: Option<u64>,
        reset_at: Option<DateTime<Utc>>,
        auto_resume: bool,
        now: DateTime<Utc>,
    ) -> Option<Event> {
        let thread_id = record.meta.thread_id.clone();
        if auto_resume
            && generation.is_some_and(|reporting| record.newer_generation_armed_schedule(reporting))
        {
            // A newer generation already named this thread's pause. Keep its
            // schedule (and jitter) instead of replacing it with an older
            // generation's earlier reset.
            tracing::debug!(
                thread_id = %thread_id,
                ?generation,
                "kept a newer session generation's auto-resume schedule"
            );
            return None;
        }
        record.limit = Some(LimitBrief {
            harness: Some(record.meta.harness),
            reset_at,
            message: None,
        });
        let meter = self.meter.get()?;
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
        // The ledger row itself is written on TurnCompleted; the schedule
        // survives the summary re-projection.
        if !auto_resume {
            return None;
        }
        if record.adopt_schedule_generation(generation, reset_at) {
            // A later generation confirmed the pause that is already armed.
            // It keeps the attempt and jitter it was given; only its owner
            // changed, so a delayed older report cannot replace it. Record the
            // reset on this generation too: if the person cancels the resume,
            // this generation's own exit report repeating it must not arm the
            // pause again.
            record.note_applied_reset(generation, reset_at);
            return None;
        }
        let prompt = meter.policy().prompt;
        // A replacement keeps the prompt the pause was holding: the requeued
        // turn is still the active, pending dispatch, and a pump-end advance
        // must not deliver it into the provider this newer reset named.
        let parked_turn = record.parked_turn_to_keep();
        match meter
            .arm_schedule(&thread_id, reset_at, prompt, None, now)
            .await
        {
            Ok(schedule) => {
                record.resume_at = Some(schedule.resume_at);
                record.scheduled_limit = Some(ScheduledLimit {
                    reset_at,
                    auto_resume,
                    generation,
                    schedule_id: Some(schedule.schedule_id),
                    parked_turn,
                });
                record.note_applied_reset(generation, reset_at);
                None
            }
            Err(error) => Some(Event::Error {
                thread_id: Some(thread_id),
                message: "could not schedule the automatic resume".into(),
                hint: Some(format!(
                    "The usage limit is still recorded; retry manually ({error})"
                )),
                retryable: true,
            }),
        }
    }

    /// Ends `turn_id` after its prompt could not be written, at most once.
    ///
    /// The pump can notice the same dead session, and Stop or a queued turn can
    /// end this one first, so the settlement itself decides whether this path
    /// still owns the turn. A write error that killed the transport detaches
    /// the session (by generation) and shuts it down: the next message then
    /// starts a fresh one instead of writing into a closed pipe.
    ///
    /// `limit` is the adapter's typed signal, never text parsed here. It is
    /// honored only when this write still owns both the turn and its session:
    /// a shutdown that interrupted the turn while the write was pending must
    /// not leave a schedule behind to fire after restart.
    async fn fail_prompt_write(
        self: &Arc<Self>,
        handle: ThreadHandle,
        turn_id: &TurnId,
        claimed_generation: u64,
        session: &SharedSession,
        message: String,
        limit: Option<LimitSignal>,
    ) {
        let (dead, stderr_tail) = {
            let session = session.lock().await;
            (session.is_closed(), session.stderr_tail())
        };
        // A write that fails because the agent closed its stdin can leave its
        // only diagnostic on stderr. Keep both texts in the one row: the write
        // error is what the adapter reported, the tail is what the agent said
        // before it went quiet. A typed limit says what it is instead of
        // blaming the write, because the adapter already reported it as one.
        let message = if limit.is_some() {
            format!("Rate limited: {message}")
        } else {
            format!("the harness rejected the message: {message}")
        };
        let stderr_tail = stderr_tail.trim();
        let message = if stderr_tail.is_empty() || message.contains(stderr_tail) {
            message
        } else {
            most_specific_detail(message.clone(), format!("{message}\n{stderr_tail}"))
        };
        // A dead transport also reports its own failure on the harness stream;
        // the settlement's row is deduplicated against it. If that report
        // arrives late, this is the one row, and it lands before the turn's
        // completion either way. A write that failed on a live transport is an
        // ordinary failure, never a death the bridge also reports.
        let hint = Some("Check the harness installation with `h4rnd doctor`".into());
        let mut end = if dead {
            TurnEnd::transport_failed(message, hint, false)
        } else {
            TurnEnd::failed(message, hint, false)
        };
        // The write lost the session it claimed: a restart, a shutdown or a
        // model change detached it while this prompt was on the wire. The turn
        // is still active and its dispatch is still ours, so returning would
        // leave it Working with nobody delivering it. Put the prompt back and
        // let a live session take it again, or start one; a startup already in
        // flight delivers it once it is live.
        //
        // A Stop the person already saw acknowledged is the exception: the
        // session could not take its cancel (it was already gone), so the
        // prompt must never be replayed on a fresh one. It settles interrupted
        // here, under the same lock that proves the turn is still ours.
        let (retarget, paused) = {
            let mut record = handle.lock().await;
            let Some(dispatch) = record.dispatch_for(turn_id) else {
                return;
            };
            if record.session.is_live(session) {
                (None, false)
            } else {
                // Test-only interleaving point: the old scope read the record
                // and then released the dispatch in a second step, so a Stop
                // landing in this window made the requeue fail and left the
                // turn Working. The one decision below must handle it.
                #[cfg(test)]
                dispatch.pause_recovery().await;
                // Whether the prompt returns to the queue or the stopped turn
                // ends is one dispatch-lock decision: a Stop that lands while
                // this write waits cannot be observed by a check and then
                // overwritten by a later requeue.
                match dispatch.recover_lost_session().await {
                    RecoveryOutcome::Stopped => {
                        let settled = self
                            .settle_turn_locked(
                                &mut record,
                                turn_id,
                                TurnEnd::new(TurnStatus::Interrupted, Usage::default()),
                            )
                            .await;
                        drop(record);
                        if settled {
                            // The settlement may have promoted the next queued
                            // message under the same lock.
                            self.spawn_advance(handle);
                        }
                        return;
                    }
                    RecoveryOutcome::Settled => return,
                    RecoveryOutcome::Requeue => {
                        // The failed write can still carry the adapter's typed
                        // limit even though its prompt goes back for a retry.
                        // Apply it here, under the same lock that decided the
                        // requeue, so the retry cannot be delivered straight
                        // into the paused provider: an armed schedule that
                        // names this dispatch keeps the queue (this turn
                        // included) held until it fires.
                        if let Some(signal) = &limit {
                            // The write claimed this generation before its
                            // session was detached, so the pause belongs to it
                            // even though the slot is empty now. A typed limit
                            // without a reset time still backs off: an
                            // immediate retry would run straight into the
                            // provider that just refused it.
                            let auto_resume = true;
                            let now = self.now();
                            let mut domain = Vec::new();
                            let mut wire = Vec::new();
                            self.note_limit_hit(
                                &mut record,
                                LimitObservation {
                                    generation: Some(claimed_generation),
                                    reset_at: signal.reset_at,
                                    auto_resume,
                                },
                                now,
                                &mut domain,
                                &mut wire,
                            )
                            .await;
                            // This pause exists to hold this one prompt, but
                            // only when this report armed or adopted it. Name
                            // the dispatch so the pause can never park a
                            // different turn, and record the prompt on the
                            // thread row so a restart restores it instead of
                            // firing the policy's resume prompt. A pause a
                            // newer generation armed is not this write's to
                            // redirect; `pause_parks_turn` below decides
                            // whether it holds this retry, exactly as
                            // `advance_queue` does.
                            let owns_pause =
                                record.scheduled_limit.as_ref().is_some_and(|scheduled| {
                                    scheduled.generation == Some(claimed_generation)
                                });
                            if owns_pause {
                                if let Some(scheduled) = record.scheduled_limit.as_mut() {
                                    scheduled.parked_turn = Some(turn_id.clone());
                                }
                                record.meta.parked =
                                    Some(ParkedPrompt::of(turn_id, &dispatch.input()));
                            }
                            if let Err(error) = self.emit(&mut record, domain, wire).await {
                                tracing::error!(%error, "failed to record a rate limit");
                            }
                        }
                        // One predicate decides whether a pause holds this
                        // retry, the same one `advance_queue` uses: a pause
                        // armed by another generation does not hold a prompt
                        // it does not name.
                        let paused = record.pause_parks_turn(turn_id);
                        (
                            Some(match record.session {
                                SessionSlot::None => DeliveryRetarget::Start,
                                SessionSlot::Live { .. } => DeliveryRetarget::Live,
                                SessionSlot::Starting(_) => DeliveryRetarget::Starting,
                            }),
                            paused,
                        )
                    }
                }
            }
        };
        if let Some(retarget) = retarget {
            // `advance_queue` starts a session or takes the live one; it is
            // spawned through the type-erased task so this delivery path does
            // not become recursive. A pause armed by this failed write owns
            // the requeued turn: its schedule delivers it after the reset.
            if !paused && !matches!(retarget, DeliveryRetarget::Starting) {
                self.spawn_advance(handle);
            }
            return;
        }

        // Detach and settlement are one lock scope: a delivery that saw the
        // empty slot in between would otherwise start a replacement and write
        // this prompt before the turn is marked failed.
        let (settled, detached) = {
            let mut record = handle.lock().await;
            // Validate ownership before recording a limit: a late failure
            // from a write that no longer owns the turn (shutdown settled and
            // detached it) must not arm a schedule, and there is nothing left
            // for it to settle.
            if record.dispatch_for(turn_id).is_none() || !record.session.is_live(session) {
                return;
            }
            let now = self.now();
            // A prompt that never reached the agent's stdin still reports the
            // limit the adapter parsed from its failure detail: the schedule
            // is armed here, before settlement, so the queued turn behind this
            // one cannot be promoted past a paused thread.
            let mut detached = None;
            if let Some(signal) = limit {
                let auto_resume = signal.reset_at.is_some();
                if let Some(event) = self
                    .apply_limit_hit(
                        &mut record,
                        Some(claimed_generation),
                        signal.reset_at,
                        auto_resume,
                        now,
                    )
                    .await
                {
                    end.wire.push(event);
                }
                end.wire.push(Event::UsageLimitHit {
                    thread_id: record.meta.thread_id.clone(),
                    reset_at: signal.reset_at,
                    auto_resume,
                });
                end.domain.push(ThreadEvent::UsageLimitHit {
                    reset_at: signal.reset_at,
                    auto_resume,
                    at: now,
                });
            }
            if dead
                && record.session.is_live(session)
                && let Some(generation) = record.session.generation()
            {
                // `detach_session` also stops this session's pump: an event it
                // has already read must not be applied to the turn that
                // replaces this one.
                detached = record.detach_session(generation);
            }
            let settled = self.settle_turn_locked(&mut record, turn_id, end).await;
            (settled, detached)
        };
        if let Some(detached) = detached {
            let _ = detached.lock().await.shutdown().await;
        }
        if settled {
            // The settlement may have promoted the next queued message under
            // the same lock; get it onto a session now that the lock is free.
            self.spawn_advance(handle);
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
        if self
            .attached_generation(&handle, &dispatch, &session)
            .await
            .is_none()
        {
            self.recover_delivery(&handle, &dispatch).await;
            return;
        }
        let mut guard = session.lock().await;
        let Some(generation) = self.attached_generation(&handle, &dispatch, &session).await else {
            // The turn ended, or its session was replaced, while this waited
            // for the session lock.
            drop(guard);
            self.recover_delivery(&handle, &dispatch).await;
            return;
        };
        if !dispatch.claim() {
            return;
        }
        let turn_id = dispatch.turn_id.clone();
        let sent = guard.send(dispatch.input()).await;
        drop(guard);
        if let Err(error) = sent {
            // Only the adapter's typed limit may arm a resume: an arbitrary
            // transport text that happens to mention a limit (an HTTP 413
            // payload limit, say) is an ordinary failed turn.
            let limit = error.limit().cloned();
            // The generation this write claimed is captured now: a later
            // detach empties the slot, and a limit on the failed write still
            // belongs to the session that reported it.
            self.fail_prompt_write(
                handle,
                &turn_id,
                generation,
                &session,
                error.to_string(),
                limit,
            )
            .await;
        }
    }

    /// The generation of `session` when it is still the active turn's slot.
    async fn attached_generation(
        &self,
        handle: &ThreadHandle,
        dispatch: &Arc<TurnDispatch>,
        session: &SharedSession,
    ) -> Option<u64> {
        let record = handle.lock().await;
        if record.dispatch_for(&dispatch.turn_id).is_none() || !record.session.is_live(session) {
            return None;
        }
        record.session.generation()
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
        health: Option<h4rn_harness::TransportHealth>,
        mut stream: futures::stream::BoxStream<'static, HarnessEvent>,
    ) {
        // A death between turns is published from its report, not from the end
        // of the stream: a live adapter holds the session, and with it the
        // event sender, so that stream may never end. This is the bounded
        // window the generic stdout-close notice gets for the exit report to
        // arrive and sharpen the row.
        let mut idle_deadline: Option<tokio::time::Instant> = None;
        // Events read while waiting on a dead transport's report, in the order
        // they arrived. They are applied before the completing event that
        // opened the wait, because the completion settles the turn: an event
        // behind it would land on a turn that has already ended.
        let mut pending: VecDeque<HarnessEvent> = VecDeque::new();
        // A completion that was already deferred for its exit report. It is
        // applied as-is when it reaches the front of `pending` instead of
        // waiting a second window for a report this pump has read.
        let mut completion_waited = false;
        // Set once an idle read failure or EOF detached this pump's session
        // from the slot. From then on this pump only reads on for the exit
        // report that can sharpen the one diagnostic row and carry the
        // provider's reset; every event that belongs to a turn goes to the
        // fresh session a Send started in the meantime.
        let mut dying = false;
        loop {
            // `stream.next()` is cancel-safe (the adapter's stream is a channel
            // receiver), so arming the deadline cannot lose an event that is
            // already queued: an event always wins the race.
            let event = if let Some(event) = pending.pop_front() {
                Some(event)
            } else {
                match idle_deadline {
                    Some(deadline) => {
                        match Self::wait_for_idle_report(&mut stream, deadline).await {
                            IdleWait::Event(event) => event,
                            IdleWait::Deadline => {
                                // No exit report came in the window: publish what
                                // the generic close left, drop the session and end
                                // this pump instead of waiting for the stream end
                                // the session it holds keeps from happening.
                                if let Some(dead) =
                                    self.finish_idle_death(&handle, generation, None).await
                                {
                                    let _ = dead.lock().await.shutdown().await;
                                }
                                if dying {
                                    return;
                                }
                                idle_deadline = None;
                                continue;
                            }
                        }
                    }
                    None => stream.next().await,
                }
            };
            let Some(event) = event else {
                break;
            };
            if self.closed.load(Ordering::Relaxed) {
                break;
            }
            if dying {
                // The session is out of the slot and no turn owns it any more.
                // Only the exit report and the limits on the way to it matter;
                // the helper publishes the row and shuts the dead session down.
                if self
                    .apply_dying_event(&handle, generation, event, &mut idle_deadline)
                    .await
                {
                    return;
                }
                continue;
            }
            // The transport's health is read from its lock-free flag, never
            // through the session lock: a `send` may hold that lock for the
            // length of a turn, waiting for a reply this pump's draining is what
            // unblocks. Only a completion needs it; every transport failure is
            // tagged where it happens.
            let session_closed = matches!(&event, HarnessEvent::TurnCompleted { .. })
                && health.as_ref().is_some_and(|health| health.is_closed());
            if session_closed && !completion_waited {
                // A transport that closed under the turn can still report its
                // exit (status, stderr and the provider's reset) on this
                // stream. The exit/status report belongs to this turn, and a
                // limit it carries must arm auto-resume before the completion
                // detaches the session and promotes the queued work, so both
                // are applied ahead of the completion; every other event read
                // in this window is buffered and runs before the completion
                // settles the turn, so an update behind it still reaches the
                // turn it describes.
                completion_waited = true;
                let mut reports = Vec::new();
                let mut later = Vec::new();
                self.collect_closed_completion_report(
                    &handle,
                    generation,
                    &session,
                    &mut stream,
                    &mut reports,
                    &mut later,
                )
                .await;
                // The completion settles the turn, and a pump that detached its
                // dead session ends with it. Anything buffered behind the
                // completion (a tool update that arrived before the exit report)
                // must reach the turn first, or the card it describes would stay
                // Running forever.
                for event in later.into_iter().rev() {
                    pending.push_front(event);
                }
                for event in reports.into_iter().rev() {
                    pending.push_front(event);
                }
                pending.push_back(event);
                continue;
            }
            if matches!(&event, HarnessEvent::TurnCompleted { .. }) {
                completion_waited = false;
            }
            let mut record = handle.lock().await;
            // A pump serves exactly one session generation. Once the thread has
            // detached that session (a failed write, a restart, a model change),
            // nothing this stream reports belongs to it any more: applying an
            // event here would blame the turn that replaced its own.
            if !matches!(
                &record.session,
                SessionSlot::Live {
                    generation: current,
                    session: current_session,
                    ..
                } if *current == generation && Arc::ptr_eq(current_session, &session)
            ) {
                tracing::debug!(
                    thread_id = %record.meta.thread_id,
                    generation,
                    "dropped an event from a session the thread no longer holds"
                );
                drop(record);
                break;
            }
            let now = self.now();
            let thread_id = record.meta.thread_id.clone();
            let mut domain = Vec::new();
            let mut wire = Vec::new();
            let mut detached: Option<SharedSession> = None;
            let mut settled = false;
            let mut forward_approval: Option<ForwardedRequest> = None;
            // Set when this pump has detached its own dead session and must
            // end once the event is applied: the session it holds keeps its
            // stream from ever ending.
            let mut done = false;
            // Set for an idle transport death: publish it now (the exit/status
            // report) or give the exit report its bounded window (the generic
            // close notice).
            let mut idle_death = false;
            let mut idle_fallback = false;
            // The process-exit report's own reset, when it carried one. It is
            // applied with the row, before a Send's fresh turn can settle.
            let mut idle_reset: Option<DateTime<Utc>> = None;

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
                    self.note_limit_hit(
                        &mut record,
                        LimitObservation {
                            generation: Some(generation),
                            reset_at,
                            auto_resume,
                        },
                        now,
                        &mut domain,
                        &mut wire,
                    )
                    .await;
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
                        // session instead of writing into a closed client. The
                        // pump is this task, so it must not abort itself.
                        let closed = session_closed;
                        // The adapter answers a Stop by closing the transport
                        // and only then reporting the turn failed. The person
                        // already ended this turn, so it settles interrupted
                        // with no error row, exactly as a stream end on a
                        // stopped turn does; the death is the cancel's, not a
                        // failure the person needs to see. Only a Failed
                        // completion is rewritten: a turn ACP already reported
                        // Completed did its work, and a Stop that arrives while
                        // the exit report is still on its way must not turn it
                        // into an interrupted turn.
                        let stopped = closed
                            && status == TurnStatus::Failed
                            && record.active.as_ref().is_some_and(|active| {
                                active.dispatch.state() == DispatchState::ClaimedStopped
                            });
                        let status = if stopped {
                            TurnStatus::Interrupted
                        } else {
                            status
                        };
                        if closed && record.session.is_live(&session) {
                            detached = record.clear_session(generation);
                            record.pump = None;
                            // This pump's session died under its own turn and
                            // is now detached. The session it holds keeps its
                            // stream open, so the pump ends with this turn
                            // instead of waiting for a stream end.
                            done = true;
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
                        if stopped {
                            // The death the transport reported behind the Stop
                            // is incidental to the cancel: it takes no row, or
                            // the stopped turn would still show a transport
                            // failure. A limit it carried was applied on its
                            // own event and keeps its schedule.
                            record.take_transport_report(&turn_id);
                        }
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
                                transport: true,
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
                    // Every harness error is its own row. Only the tagged
                    // writer death below shares a row with the daemon's failed
                    // prompt write, so a buffered diagnostic can never take
                    // that slot and suppress the failure it followed.
                    if !record.session.is_live(&session) {
                        // This pump's session was already detached, so whoever
                        // detached it reported the failure. A late error from a
                        // session nobody attaches to any more is a duplicate and
                        // must not be blamed on the turn that replaced it.
                        tracing::debug!(
                            thread_id = %thread_id,
                            "dropped a late error from a detached session"
                        );
                    } else {
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
                HarnessEvent::TransportClosed {
                    message,
                    retryable,
                    kind,
                    reset_at,
                } => {
                    // The tagged writer death. A death is reported more than
                    // once (the bridge's generic stdout-close notice first, then
                    // the prompt failure or exit row that carries the agent's
                    // stderr). All of them are one row, so the first report is
                    // held for the turn's settlement and each later one only
                    // sharpens it; publishing the generic one immediately would
                    // take the turn's one row and leave the stderr out.
                    let turn_id = record.turn_id_for_event();
                    if !record.session.is_live(&session) {
                        tracing::debug!(
                            thread_id = %thread_id,
                            "dropped a late transport failure from a detached session"
                        );
                    } else if record.active.is_none() {
                        // Nothing to merge with yet: a death between turns is
                        // its own row. The adapter reports that death twice,
                        // the generic stdout close first and then the exit
                        // status with the stderr it drained, so both are held
                        // for this session generation. The exit report
                        // publishes the merged row as soon as it arrives; an
                        // EOF or a read error only opens the bounded window for
                        // it, because either can precede the exit report that
                        // carries the provider's reset.
                        //
                        // The session leaves the slot now, not when the report
                        // arrives: its reader is already gone, so a Send in the
                        // window must start a fresh agent instead of claiming
                        // this one and failing on a dead write. The pump keeps
                        // reading this session's stream for the exit report.
                        record.mark_session_dying(generation);
                        record.hold_idle_transport_report(generation, &turn_id, message, retryable);
                        dying = true;
                        match kind {
                            // The process-exit report is the specific account
                            // of the death, and a writer failure is final: the
                            // client killed the agent with it.
                            TransportCloseKind::Exit | TransportCloseKind::Write => {
                                idle_death = true;
                                idle_reset = reset_at;
                            }
                            // The generic stdout close and a read error can be
                            // followed by the exit report inside one window.
                            TransportCloseKind::Eof | TransportCloseKind::Read => {
                                idle_fallback = true;
                            }
                        }
                    } else if !record.note_transport_report(&turn_id, message, retryable) {
                        tracing::debug!(
                            thread_id = %thread_id,
                            "collapsed a second report of the same transport failure"
                        );
                    }
                    // Otherwise the report is held: the settlement publishes it
                    // merged with whichever report carries the most detail.
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
            if done {
                // The pump detached its own dead session: end here, with the
                // successor (if any) already scheduled, rather than wait for a
                // stream end the session it still holds keeps from happening.
                return;
            }
            if idle_fallback {
                idle_deadline = Some(tokio::time::Instant::now() + IDLE_DEATH_DETAIL_WAIT);
            }
            if idle_death {
                if let Some(dead) = self
                    .finish_idle_death(&handle, generation, idle_reset)
                    .await
                {
                    // The row is published and the dead session is out of the
                    // slot, so the next Send starts fresh. Nothing else will
                    // arrive on this pump's stream that belongs to it.
                    let _ = dead.lock().await.shutdown().await;
                }
                return;
            }
        }

        // An idle death whose stream ended before its report arrived: the row
        // is still this pump's to publish, and its session is already out of
        // the slot.
        if dying {
            if !self.closed.load(Ordering::Relaxed)
                && let Some(dead) = self.finish_idle_death(&handle, generation, None).await
            {
                let _ = dead.lock().await.shutdown().await;
            }
            return;
        }

        // The stream ended. Only the pump that still owns the attached session
        // may clear it: a failed write or a restart may already have detached
        // it and started another one.
        let (settled, advance) = {
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
            let hint = if stderr_tail.trim().is_empty() {
                "Send the message again to resume".to_owned()
            } else {
                format!("Send the message again to resume. Harness stderr: {stderr_tail}")
            };
            // A turn this pump wrote to ends here: the agent's stream is gone
            // before it reported the turn's completion.
            //
            // A turn it never wrote to has two cases. If this session was the
            // one started *for* the turn (its own startup), the agent exited
            // before it accepted the prompt: settle the turn and let the queue
            // advance, because replacing the session would just repeat the same
            // death forever. If the session belonged to an earlier turn, this
            // one is still waiting for a session and gets one fresh startup.
            let turn = record.active.as_ref().and_then(|active| {
                let (message, interrupted) = match active.dispatch.state() {
                    DispatchState::Claimed => {
                        ("the harness session ended before the turn completed", false)
                    }
                    // The person stopped this turn after its prompt was
                    // claimed, and the session that was cancelling it is gone:
                    // it ends interrupted, not as a transport failure.
                    DispatchState::ClaimedStopped => ("the turn was stopped", true),
                    DispatchState::Pending
                        if active.dispatch.started_generation() == Some(generation) =>
                    {
                        ("the agent exited before it accepted the prompt", false)
                    }
                    DispatchState::Pending | DispatchState::Stopped | DispatchState::Settled => {
                        return None;
                    }
                };
                Some((active.turn_id.clone(), message, hint.clone(), interrupted))
            });
            // A death reported between turns has no settlement to wait for, so
            // its held report is published here as the session's one row. Only
            // this pump's own generation is finalized: another generation's
            // death may still be waiting for its own exit report.
            let idle = record
                .dying_sessions
                .remove(&generation)
                .and_then(|dying| dying.report);
            // The pump is this task, so it must not abort itself.
            record.clear_session(generation);
            record.pump = None;
            let has_turn = turn.is_some();
            let settled = match turn {
                Some((turn_id, message, hint, interrupted)) => {
                    // Detach and settlement are one lock scope. A delivery that
                    // saw the empty slot in between would otherwise start a
                    // replacement and write the prompt before this turn is
                    // marked failed, running a turn that never can.
                    let end = if interrupted {
                        // A Stop owns this turn: it ends interrupted, not as a
                        // failure. The death the transport reported behind the
                        // Stop is incidental (the agent went away while its
                        // cancel was in flight), so its held report is dropped
                        // rather than taking the turn's one error row. A limit
                        // the report carried was applied on its own event and
                        // keeps its schedule.
                        record.take_transport_report(&turn_id);
                        TurnEnd::new(TurnStatus::Interrupted, Usage::default())
                    } else {
                        TurnEnd::transport_failed(message.into(), Some(hint), true)
                    };
                    self.settle_turn_locked(&mut record, &turn_id, end).await
                }
                None => {
                    // A promoted turn this session never wrote to is retried on
                    // a fresh startup. The transport report this session's death
                    // left for it describes the old transport, so it must not
                    // become the retry's error row.
                    let pending = record.active.as_ref().map(|active| active.turn_id.clone());
                    if let Some(turn_id) = pending {
                        record.take_transport_report(&turn_id);
                    }
                    false
                }
            };
            if !has_turn && let Some(report) = idle {
                // The session's stream ended with no turn to blame, so this is
                // the row for the idle transport death: the generic close
                // merged with the exit status and stderr that followed it. It
                // is a diagnostic, not a failed turn: raising a failure here
                // would flip a thread whose auto-resume just armed from
                // Scheduled to Error and ask for attention (main published the
                // row without a lifecycle transition too).
                let thread_id = record.meta.thread_id.clone();
                if let Err(error) = self
                    .emit(
                        &mut record,
                        Vec::new(),
                        vec![Event::Error {
                            thread_id: Some(thread_id),
                            message: report.message,
                            hint: Some("Inspect the thread and retry when ready".into()),
                            retryable: report.retryable,
                        }],
                    )
                    .await
                {
                    tracing::error!(%error, "failed to record a transport failure");
                }
            }
            (settled, record.active.is_some())
        };
        if settled || advance {
            // A settled turn may have promoted the next queued message under
            // the same lock, and a pending turn this session never reached
            // needs a fresh startup; get either onto a session now.
            self.spawn_advance(handle);
        }
    }

    /// Applies one typed limit signal and projects its rows.
    ///
    /// Shared by the pump's own `LimitHit` arm and the bounded wait that
    /// catches a limit only a dead transport's exit report carries, so both
    /// arm the same schedule and publish the same events.
    async fn note_limit_hit(
        &self,
        record: &mut ThreadRecord,
        limit: LimitObservation,
        now: DateTime<Utc>,
        domain: &mut Vec<ThreadEvent>,
        wire: &mut Vec<Event>,
    ) {
        let LimitObservation {
            generation,
            reset_at,
            auto_resume,
        } = limit;
        if let Some(event) = self
            .apply_limit_hit(record, generation, reset_at, auto_resume, now)
            .await
        {
            let _ = self.emit(record, Vec::new(), vec![event]).await;
        }
        domain.push(ThreadEvent::UsageLimitHit {
            reset_at,
            auto_resume,
            at: now,
        });
        wire.push(Event::UsageLimitHit {
            thread_id: record.meta.thread_id.clone(),
            reset_at,
            auto_resume,
        });
    }

    /// The idle fallback's one poll: the session's stream, or its deadline.
    ///
    /// A report that is already queued always beats the fallback, so a death
    /// whose exit report and deadline are ready at the same poll publishes the
    /// report instead of the generic close notice. Kept as its own function so
    /// a test can repeat that ready-together poll without a whole pump.
    async fn wait_for_idle_report(
        stream: &mut futures::stream::BoxStream<'static, HarnessEvent>,
        deadline: tokio::time::Instant,
    ) -> IdleWait {
        tokio::select! {
            biased;
            event = stream.next() => IdleWait::Event(event),
            _ = tokio::time::sleep_until(deadline) => IdleWait::Deadline,
        }
    }

    /// Waits, bounded, for a closed transport's exit report after a completed
    /// turn, applying the rate limit it may carry.
    ///
    /// ACP can answer `end_turn` and only then exit, with the provider's reset
    /// on stderr. The completion sees the closed transport, and without this
    /// window the pump would detach the session and promote the queued work
    /// before that report arrives: auto-resume would never be armed and the
    /// queued message would start at once on a fresh agent. A `LimitHit` (the
    /// report's typed signal) is collected while the turn still owns the record
    /// and applied once after the wait, so one provider pause arms one
    /// schedule, but only the exit report ends the wait: an ordinary limit
    /// without a reset can arrive first, and the reset that pause belongs to
    /// travels on the exit report that follows it.
    /// The exit/status report goes to `reports` because the turn's settlement
    /// must merge it into its one row; every other event goes to `later` and is
    /// applied before the completion, in the order it arrived, because the
    /// completion settles the turn and an update behind it would never reach
    /// the turn it describes.
    async fn collect_closed_completion_report(
        self: &Arc<Self>,
        handle: &ThreadHandle,
        generation: u64,
        session: &SharedSession,
        stream: &mut futures::stream::BoxStream<'static, HarnessEvent>,
        reports: &mut Vec<HarnessEvent>,
        later: &mut Vec<HarnessEvent>,
    ) {
        let deadline = tokio::time::Instant::now() + IDLE_DEATH_DETAIL_WAIT;
        // The limits observed while waiting, in the order they arrived. They are
        // applied once after the wait instead of where they land: the bridge
        // reports an exit's reset both as a typed limit and on the exit report,
        // and a notification that arrived before the completion may already
        // have armed the same reset. Applying each event would replace the
        // schedule and bump its attempt counter, changing the jitter of one
        // provider pause.
        let mut limits: Vec<(Option<DateTime<Utc>>, bool)> = Vec::new();
        // The exit report's own reset, when it carried one. It is the most
        // specific account of the pause and wins over a notification.
        let mut exit_reset: Option<Option<DateTime<Utc>>> = None;
        loop {
            tokio::select! {
                // A report that is already queued always beats the deadline,
                // exactly as in the idle fallback: when both are ready at the
                // same poll, the exit report must name the turn's row instead
                // of the window expiring first.
                biased;
                event = stream.next() => match event {
                    None => break,
                    Some(HarnessEvent::LimitHit {
                        reset_at,
                        auto_resume,
                    }) => {
                        // Collected, never applied here: the wait only ends on
                        // the exit report (or its deadline), and one schedule is
                        // armed for the whole window.
                        limits.push((reset_at, auto_resume));
                    }
                    Some(event) => {
                        // The process-exit report is the last report of the
                        // death, so nothing else can sharpen the turn: stop
                        // waiting once it arrives. A read error is also
                        // non-retryable, but the exit report that may carry the
                        // provider's reset can still follow it.
                        let report_reset = match &event {
                            HarnessEvent::TransportClosed {
                                kind: TransportCloseKind::Exit,
                                reset_at,
                                ..
                            } => Some(*reset_at),
                            _ => None,
                        };
                        if let Some(reset_at) = report_reset {
                            exit_reset = Some(reset_at);
                            reports.push(event);
                            break;
                        }
                        if matches!(&event, HarnessEvent::ApprovalRequested { .. }) {
                            // An approval buffered behind a completion cannot be
                            // answered: the completion settles the turn and the
                            // pump detaches this closed transport, so no session
                            // would ever receive the answer. It was never
                            // published either, so it expires silently here
                            // instead of becoming a live cue nothing can
                            // resolve.
                            tracing::debug!(
                                "dropped an approval buffered behind a closed completion"
                            );
                            continue;
                        }
                        later.push(event);
                    }
                },
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        // Arm once, before the completion settles the turn.
        let signal = if let Some(Some(reset)) = exit_reset {
            // The exit report names the pause itself. That is the most specific
            // account, so wait for its reset even when a notification read
            // before it named another.
            Some((Some(reset), true))
        } else {
            // A reset-less exit report (or no exit report before the deadline)
            // falls back to the notifications collected in the window: the last
            // one that named a reset, otherwise the last one, which may be a
            // reset-less limit that only needs its row.
            limits
                .iter()
                .rev()
                .find(|(reset, _)| reset.is_some())
                .copied()
                .or_else(|| limits.last().copied())
        };
        if let Some((reset_at, auto_resume)) = signal {
            self.apply_completion_limit(handle, generation, session, reset_at, auto_resume)
                .await;
        }
    }

    /// Arms the schedule a dead transport's exit report carries.
    ///
    /// The turn is still active here, so this is the pump's ordinary limit
    /// handling. A session that was detached while this waited no longer owns
    /// the limit, exactly as for any other event from it. A limit this turn
    /// already armed for the same reset is not replaced: the notification may
    /// have arrived before the completion, and re-arming would bump the
    /// schedule's attempt and change the jitter of one provider pause.
    async fn apply_completion_limit(
        self: &Arc<Self>,
        handle: &ThreadHandle,
        generation: u64,
        session: &SharedSession,
        reset_at: Option<DateTime<Utc>>,
        auto_resume: bool,
    ) {
        let now = self.now();
        let mut record = handle.lock().await;
        if !record.session.is_live(session) || record.session.generation() != Some(generation) {
            return;
        }
        if auto_resume
            && record
                .active
                .as_ref()
                .is_some_and(|active| active.applied_limit_reset == Some((generation, reset_at)))
        {
            // This exact generation already applied this exact reset (the
            // typed limit that arrived before its completion). The person may
            // have cancelled the resume while this report was still on its
            // way: arming the same reset again would undo that cancellation.
            // The turn's record knows what it applied; the live schedule does
            // not survive a cancellation, so it cannot be the guard here.
            record.adopt_schedule_generation(Some(generation), reset_at);
            return;
        }
        if record.adopt_schedule_generation(Some(generation), reset_at) {
            // A later generation confirmed the pause that is already armed.
            // It keeps its attempt and jitter, and now owns the schedule. The
            // reset belongs to this generation too, so its own exit-only
            // repeat after a cancellation cannot arm the pause again.
            record.note_applied_reset(Some(generation), reset_at);
            return;
        }
        let mut domain = Vec::new();
        let mut wire = Vec::new();
        self.note_limit_hit(
            &mut record,
            LimitObservation {
                generation: Some(generation),
                reset_at,
                auto_resume,
            },
            now,
            &mut domain,
            &mut wire,
        )
        .await;
        if let Err(error) = self.emit(&mut record, domain, wire).await {
            tracing::error!(%error, "failed to record a rate limit");
        }
    }

    /// Applies one event read on the stream of a session the pump detached as
    /// dying, and reports whether the death is fully published.
    ///
    /// The slot no longer holds this session, so nothing here is allowed to
    /// touch a turn: the session's death belongs to the generation, not to
    /// whatever the thread is running now (a Send in the window started a fresh
    /// session and its own turn). Only the exit report ends the wait; a limit
    /// notification on the way to it still arms its schedule, exactly as it
    /// would have before the detach.
    async fn apply_dying_event(
        self: &Arc<Self>,
        handle: &ThreadHandle,
        generation: u64,
        event: HarnessEvent,
        idle_deadline: &mut Option<tokio::time::Instant>,
    ) -> bool {
        match event {
            HarnessEvent::TransportClosed {
                message,
                retryable,
                kind,
                reset_at,
            } => {
                let mut record = handle.lock().await;
                if !record.is_dying_session(generation) {
                    // This death was already published (or the session was
                    // discarded): nothing left to sharpen.
                    return true;
                }
                let turn_id = record.turn_id_for_event();
                record.hold_idle_transport_report(generation, &turn_id, message, retryable);
                drop(record);
                // Only the process-exit report (and a writer failure, which
                // kills the agent) is the death's final account; any other
                // report leaves the exit report its window.
                if matches!(kind, TransportCloseKind::Exit | TransportCloseKind::Write) {
                    if let Some(dead) = self.finish_idle_death(handle, generation, reset_at).await {
                        let _ = dead.lock().await.shutdown().await;
                    }
                    return true;
                }
                *idle_deadline = Some(tokio::time::Instant::now() + IDLE_DEATH_DETAIL_WAIT);
            }
            HarnessEvent::LimitHit {
                reset_at,
                auto_resume,
            } => {
                // The report's own typed signal. A schedule this death already
                // armed for the same reset is left alone, so one pause never
                // arms twice — even if the person cancelled the resume in
                // between and the exit report is yet to repeat the reset.
                let mut record = handle.lock().await;
                if !record.is_dying_session(generation) {
                    return true;
                }
                if record.newer_generation_armed_schedule(generation) {
                    // The newer generation's pause is the thread's; this
                    // report's reset must not replace it. The death still
                    // publishes its own row when its exit report arrives.
                    return false;
                }
                if let (true, Some(reset_at)) = (auto_resume, reset_at)
                    && !record.note_idle_reset_applied(generation, reset_at)
                {
                    return false;
                }
                let now = self.now();
                let mut domain = Vec::new();
                let mut wire = Vec::new();
                self.note_limit_hit(
                    &mut record,
                    LimitObservation {
                        generation: Some(generation),
                        reset_at,
                        auto_resume,
                    },
                    now,
                    &mut domain,
                    &mut wire,
                )
                .await;
                if let Err(error) = self.emit(&mut record, domain, wire).await {
                    tracing::error!(%error, "failed to record a rate limit");
                }
            }
            event => {
                // Nothing else can belong to a session whose reader is gone:
                // a completion or an update here describes a turn that ended
                // before the death, or one that the fresh session now owns.
                tracing::debug!(
                    generation,
                    ?event,
                    "dropped an event from a session whose transport died between turns"
                );
            }
        }
        false
    }

    /// Publishes the one row for an idle death the pump already detached, and
    /// arms the reset its exit report carried.
    ///
    /// The pump calls this as soon as the exit/status report arrives, or after
    /// [`IDLE_DEATH_DETAIL_WAIT`] when only a generic close came. The session
    /// left the slot when the death was known, so a Send in the window already
    /// started a fresh agent: this only takes the report and the session it
    /// belongs to, and never touches whatever the thread is running now.
    ///
    /// `None` when this generation's death was already published; otherwise the
    /// dead session, which the caller shuts down outside the record lock.
    async fn finish_idle_death(
        self: &Arc<Self>,
        handle: &ThreadHandle,
        generation: u64,
        reset_at: Option<DateTime<Utc>>,
    ) -> Option<SharedSession> {
        let mut record = handle.lock().await;
        if !record.is_dying_session(generation) {
            return None;
        }
        if let Some(reset_at) = reset_at {
            self.arm_idle_reset(&mut record, generation, reset_at).await;
        }
        let dying = record.dying_sessions.remove(&generation)?;
        let Some(report) = dying.report else {
            // Nothing to publish (a report the thread already accounted for),
            // but the session is still this pump's to drop.
            return Some(dying.session);
        };
        let thread_id = record.meta.thread_id.clone();
        // The row is the idle death's diagnostic, not a failed turn. Raising a
        // failure would flip the thread to Error — a death that came with a
        // limit has just armed its auto-resume, and the thread must stay
        // Scheduled instead of asking for attention.
        if let Err(error) = self
            .emit(
                &mut record,
                Vec::new(),
                vec![Event::Error {
                    thread_id: Some(thread_id),
                    message: report.message,
                    hint: Some("Inspect the thread and retry when ready".into()),
                    retryable: report.retryable,
                }],
            )
            .await
        {
            tracing::error!(%error, "failed to record a transport failure");
        }
        Some(dying.session)
    }

    /// Arms the reset an idle death's exit report carries.
    ///
    /// There is no turn to settle, so this is the thread's own limit handling.
    /// A reset this death already applied (the adapter's typed limit usually
    /// lands first) is not replaced: one provider pause keeps the attempt and
    /// jitter it was given. The death's own record is the guard, not the live
    /// schedule: the person can cancel the resume while the exit report is
    /// still arriving, and that report repeats the same reset.
    async fn arm_idle_reset(
        &self,
        record: &mut ThreadRecord,
        generation: u64,
        reset_at: DateTime<Utc>,
    ) {
        if record.newer_generation_armed_schedule(generation) {
            // A newer generation's pause already names this thread's wait.
            // This death keeps its own row, but its earlier reset must not
            // replace that schedule.
            return;
        }
        if !record.note_idle_reset_applied(generation, reset_at) {
            return;
        }
        if record.adopt_schedule_generation(Some(generation), Some(reset_at)) {
            // A newer generation confirmed the pause that is already armed.
            // It keeps the attempt and jitter it was given, but it now owns
            // the schedule so this older death cannot replace it later.
            return;
        }
        let now = self.now();
        let mut domain = Vec::new();
        let mut wire = Vec::new();
        self.note_limit_hit(
            record,
            LimitObservation {
                generation: Some(generation),
                reset_at: Some(reset_at),
                auto_resume: true,
            },
            now,
            &mut domain,
            &mut wire,
        )
        .await;
        if let Err(error) = self.emit(record, domain, wire).await {
            tracing::error!(%error, "failed to record a rate limit");
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
            let parks_active = record
                .active
                .as_ref()
                .is_some_and(|active| record.pause_parks_turn(&active.turn_id));
            if parks_active {
                // An armed auto-resume owns this turn's next attempt: the
                // requeued prompt waits for the schedule instead of running
                // into the paused provider. The schedule delivers it when it
                // fires. A pause armed for a different dispatch does not park
                // this one (a Send's own replacement turn still starts).
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
        let (harness, model, thread_id, closed, active, parks_active) = {
            let record = handle.lock().await;
            (
                record.meta.harness,
                record.meta.model.clone(),
                record.meta.thread_id.clone(),
                record.closed,
                record.active.is_some(),
                record.parks_active_turn(),
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
        if active && !parks_active {
            // A turn is already running (a manual resume, or the limited turn
            // itself has not finished). Wait quietly: the schedule stays armed
            // and is re-checked on the next tick, and `send` cancels it when a
            // new turn supersedes it. Emitting here would repeat every second.
            //
            // A turn whose prompt is still pending is the exception, but only
            // when this pause parks that exact turn: a failed write that lost
            // its session put it back for a retry and a typed limit paused the
            // thread. A Send's own pending startup is not this schedule's to
            // release, so it is left alone without claiming the attempt.
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

        // Claim the attempt first, under the thread lock and before any model
        // change: `cancel_resume` takes the same lock before it writes
        // `Cancelled`, so exactly one of the two wins. A cancellation that
        // landed while the proxy was thinking makes the claim fail here and no
        // turn runs.
        enum Claim {
            Closed,
            Active,
            Unnamed,
            Stale,
            Claimed,
        }
        let claim = {
            let record = handle.lock().await;
            if record.closed {
                Claim::Closed
            } else if record.active.is_some() && !record.parks_active_turn() {
                // The only active turn a fire may release is the pending one
                // this pause parks. Anything else (a manual Send's startup, or
                // a running turn) is left alone, and the attempt stays
                // unclaimed for the tick that can act on it.
                Claim::Active
            } else if !record.names_schedule(&schedule.schedule_id) {
                // The record has to name the attempt before the ledger claim:
                // a fire that consumed an attempt the thread never showed is
                // an attempt the schedule can never deliver. A restarted
                // daemon knows its paused prompts from the journal, but only
                // `resync_meter_state` binds them to the ledger schedule.
                Claim::Unnamed
            } else if !meter
                .claim_schedule(&schedule)
                .await
                .map_err(|error| error.to_string())?
            {
                Claim::Stale
            } else {
                Claim::Claimed
            }
        };
        match claim {
            Claim::Closed => {
                let _ = meter
                    .finish_schedule(
                        schedule,
                        ResumeState::Cancelled,
                        Some("the thread is closed".into()),
                    )
                    .await;
                return Ok(None);
            }
            Claim::Active => {
                tracing::debug!(
                    thread_id = %thread_id,
                    "auto-resume due while the thread is active; waiting"
                );
                return Ok(None);
            }
            Claim::Unnamed => {
                tracing::debug!(
                    thread_id = %thread_id,
                    schedule = %schedule.schedule_id,
                    "the record does not name this schedule; not claiming it"
                );
                return Ok(None);
            }
            Claim::Stale => {
                tracing::debug!(
                    thread_id = %thread_id,
                    schedule = %schedule.schedule_id,
                    "auto-resume was cancelled or superseded before it claimed; not starting it"
                );
                return Ok(None);
            }
            Claim::Claimed => {}
        }

        // A test can hold the fire here, after its attempt was claimed, while
        // another path replaces the schedule.
        #[cfg(test)]
        {
            let pause = self
                .fire_pause
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            if let Some(pause) = pause {
                pause.claimed.notify_one();
                pause.resume.notified().await;
            }
        }

        // Re-take the lock after the unlocked I/O and check that nothing
        // replaced this schedule meanwhile: `send` may have started a turn
        // (which cancels the schedule), a newer limit may have replaced it, or
        // the daemon may have closed the thread.
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
        // The schedule this fire claimed may have been replaced by a newer
        // limit while the record lock was free. Clearing or dispatching on its
        // behalf would drop the newer pause and hand off a delivery it never
        // owned. The record must name this exact schedule: a missing pause or
        // an unknown identity is not a match, because neither proves the
        // attempt this fire claimed is the one the thread is waiting on. The
        // same holds for the turn being released: only the pending prompt this
        // pause parks may be delivered.
        let owns_schedule = record.names_schedule(&schedule.schedule_id);
        let parks_active = record.parks_active_turn();
        if !owns_schedule || (record.active.is_some() && !parks_active) {
            drop(record);
            tracing::debug!(
                thread_id = %thread_id,
                schedule = %schedule.schedule_id,
                "the record no longer names this schedule; not acting on it"
            );
            // The attempt was already claimed, and the ledger marks it Fired.
            // Record that it will not deliver; `finish_schedule` is a no-op
            // when a newer row replaced this attempt, so the newer pause is
            // never cancelled on its behalf.
            let _ = meter
                .finish_schedule(
                    schedule,
                    ResumeState::Cancelled,
                    Some("superseded before delivery".to_owned()),
                )
                .await;
            return Ok(None);
        }
        let pending_turn = parks_active;
        // Whoever chose it (the policy at reset time, or a person through
        // `FailOver`), a changed model needs a fresh session: the live one was
        // started on the limited model. This happens only after the schedule
        // identity matched, so a fire that lost its pause never changes the
        // model or drops a session.
        let mut detached: Option<SharedSession> = None;
        if let Some(failover) = &schedule.failover
            && record.meta.model != failover.model
        {
            record.meta.model = failover.model.clone();
            // A startup already booting the limited model is superseded too:
            // clearing its generation makes it discard the session it makes
            // (Invariant 1), and the resume below starts a new one on the new
            // model. A live session is detached by the generation this
            // schedule inspected; a newer generation is never this schedule's
            // to drop.
            detached = if let SessionSlot::Starting(_) = record.session {
                record.session = SessionSlot::None;
                None
            } else {
                // `detach_session` stops this session's pump itself.
                record
                    .session
                    .generation()
                    .and_then(|generation| record.detach_session(generation))
            };
        }
        if pending_turn {
            // The pause was armed by a failed write that put its prompt back.
            // That requeued turn is the attempt this reset was waiting for, so
            // deliver it instead of starting the policy's resume prompt. The
            // queue was held while the schedule was armed, so nothing else can
            // own the slot's delivery.
            let now = self.now();
            record.clear_pause();
            record.limit = None;
            // The thread is running that prompt again; leave the paused phase
            // and drop the limit that explained it. The turn itself was
            // already recorded and counted when the person sent it.
            let domain = vec![ThreadEvent::LimitResumeReleased { at: now }];
            if let Err(error) = self.emit(&mut record, domain, Vec::new()).await {
                // The pause is cleared and the prompt is about to be written;
                // a failed projection write must not strand it. The next tick
                // rewrites the summary.
                tracing::error!(%error, "could not record the released turn");
            }
            let needs_session = matches!(record.session, SessionSlot::None);
            drop(record);
            // The attempt is already claimed, so the ledger cannot fire it
            // again: whether or not the outcome write lands, the prompt has one
            // delivery owner now.
            let fired = match meter
                .finish_schedule(schedule, ResumeState::Fired, None)
                .await
            {
                Ok(fired) => fired,
                Err(error) => {
                    tracing::error!(%error, "could not mark the resume schedule fired");
                    None
                }
            };
            if let Some(session) = detached {
                let mut session = session.lock().await;
                if let Err(error) = session.shutdown().await {
                    tracing::warn!(%thread_id, %error, "harness shutdown before failover failed");
                }
            }
            if needs_session {
                self.spawn_session(handle);
            } else {
                self.schedule_active(&handle).await;
            }
            return Ok(fired);
        }

        let prompt = schedule.prompt.clone();
        let now = self.now();
        record.clear_pause();
        record.limit = None;
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
            // The turn is begun and its pause is gone; the prompt still needs
            // an owner. The ledger records the failure so the schedule can be
            // re-checked, but the delivery below is not conditional on it.
            tracing::error!(%error, "could not record the auto-resume turn");
        }
        let needs_session = matches!(record.session, SessionSlot::None);
        drop(record);

        let fired = match meter
            .finish_schedule(schedule, ResumeState::Fired, None)
            .await
        {
            Ok(fired) => fired,
            Err(error) => {
                tracing::error!(%error, "could not mark the resume schedule fired");
                None
            }
        };
        if let Some(session) = detached {
            let mut session = session.lock().await;
            if let Err(error) = session.shutdown().await {
                tracing::warn!(%thread_id, %error, "harness shutdown before failover failed");
            }
        }
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
        if self.resync_pending.load(Ordering::Relaxed) && self.resync_retry_due() {
            // The startup read failed; retry before firing anything that would
            // depend on the ledger ids it could not attach. The wait grows
            // while the ledger stays unreadable, so a broken read does not
            // rewrite every thread's summary on every tick.
            self.resync_meter_state().await;
        }
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

            let wire = vec![Event::ThreadUpserted(summary)];
            let _guard = self.emit_lock.lock().await;
            // The signature is cached only once the projection is durable and
            // published: a failed append must be retried on the next tick
            // instead of being hidden by a cache that already moved on.
            match self
                .journal
                .append_wire(&record.meta.thread_id, &wire)
                .await
            {
                Ok(seqs) => {
                    self.broadcast(&record.meta.thread_id, &wire, &seqs);
                    record.last_signature = Some(signature);
                }
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

impl ParkedPrompt {
    /// The durable form of a prompt a pause is holding.
    fn of(turn_id: &TurnId, input: &UserInput) -> Self {
        Self {
            turn_id: turn_id.clone(),
            text: input.text.clone(),
            attachments: input
                .attachments
                .iter()
                .map(|attachment| ParkedAttachment {
                    id: attachment.id.clone(),
                    path: attachment.path.display().to_string(),
                    media_type: attachment.media_type.clone(),
                })
                .collect(),
        }
    }

    /// The delivery form of the restored prompt.
    fn input(&self) -> UserInput {
        UserInput {
            text: self.text.clone(),
            attachments: self
                .attachments
                .iter()
                .map(|attachment| AttachmentRef {
                    id: attachment.id.clone(),
                    path: std::path::PathBuf::from(&attachment.path),
                    media_type: attachment.media_type.clone(),
                })
                .collect(),
        }
    }
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
    use crate::text_generation::{TextGeneration, TextGenerationError};

    /// Spawns `work` and returns when it is parked on the thread's record lock.
    ///
    /// `work` signals on the channel immediately before it takes the lock, and
    /// the caller holds that lock until every such task has signalled. On the
    /// test runtime the task then reaches its first `lock().await` in the same
    /// poll, so the signal proves the task is queued behind the caller's guard.
    /// Tokio's mutex is fair, so dropping the guard resumes the tasks in that
    /// order: tests build an interleaving instead of guessing at it with
    /// `yield_now` loops.
    fn spawn_parked<T, F, Fut>(
        work: F,
    ) -> (
        tokio::task::JoinHandle<T>,
        tokio::sync::oneshot::Receiver<()>,
    )
    where
        F: FnOnce(tokio::sync::oneshot::Sender<()>) -> Fut,
        Fut: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (parked, arrived) = tokio::sync::oneshot::channel();
        (tokio::spawn(work(parked)), arrived)
    }

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

    /// Holds tokio's mocked clock still while a test delivers events by hand.
    ///
    /// A paused clock auto-advances whenever the runtime has nothing to run,
    /// which would let a fallback deadline fire before the test has delivered
    /// the exit report that must win over it. A blocking task inhibits that
    /// auto-advance until [`Self::release`] lets time run again.
    struct HeldClock {
        release: Option<std::sync::mpsc::Sender<()>>,
    }

    impl HeldClock {
        /// Pauses time and keeps it from auto-advancing.
        fn hold() -> Self {
            tokio::time::pause();
            let (release, held) = std::sync::mpsc::channel::<()>();
            tokio::task::spawn_blocking(move || {
                // Closing the channel is the release.
                let _ = held.recv();
            });
            Self {
                release: Some(release),
            }
        }

        /// Lets the clock run again.
        fn release(&mut self) {
            self.release.take();
            tokio::time::resume();
        }
    }

    /// The next error row, when one is already queued.
    ///
    /// One poll, so a test can prove no row was published without a sleep.
    fn queued_error(events: &mut broadcast::Receiver<WireEvent>) -> Option<String> {
        futures::FutureExt::now_or_never(next_error(events))
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

    async fn next_error(events: &mut broadcast::Receiver<WireEvent>) -> String {
        loop {
            let wire = next_event(events).await;
            if let Event::Error { message, .. } = wire.event {
                return message;
            }
        }
    }

    async fn open_test_manager(harness: FakeHarness, host: HostId) -> ThreadManager {
        let journal: Arc<dyn Journal> =
            Arc::new(SqliteJournal::in_memory().await.expect("journal"));
        open_test_manager_with_journal(harness, host, journal).await
    }

    async fn open_test_manager_with_journal(
        harness: FakeHarness,
        host: HostId,
        journal: Arc<dyn Journal>,
    ) -> ThreadManager {
        open_test_manager_with_clock(harness, host, journal, None).await
    }

    /// [`open_test_manager_with_journal`] with a clock the test moves by hand.
    async fn open_test_manager_with_clock(
        harness: FakeHarness,
        host: HostId,
        journal: Arc<dyn Journal>,
        clock: Option<Arc<dyn Clock>>,
    ) -> ThreadManager {
        ThreadManager::open(
            journal,
            Arc::new(StaticHarnessCatalog::with(Arc::new(harness))),
            host,
            ThreadManagerOptions {
                // The manager only reacts to this test's own calls.
                tick_interval: Duration::from_secs(3600),
                clock,
                ..ThreadManagerOptions::default()
            },
            None,
        )
        .await
        .expect("manager")
    }

    /// A clock that only moves when a test says so.
    ///
    /// Wall time and monotonic time advance together, but a test can move one
    /// without the other: retry waits follow the monotonic reading, so a
    /// wall-clock jump must not move them.
    #[derive(Debug)]
    struct ManualClock {
        wall: std::sync::Mutex<DateTime<Utc>>,
        /// Time elapsed since `base`, moved only by [`Self::advance`] and
        /// [`Self::advance_monotonic`].
        mono: std::sync::Mutex<Duration>,
        base: Instant,
    }

    impl ManualClock {
        fn new() -> Self {
            Self {
                wall: std::sync::Mutex::new(Utc::now()),
                mono: std::sync::Mutex::new(Duration::ZERO),
                base: Instant::now(),
            }
        }

        /// Moves both readings forward by `by`.
        fn advance(&self, by: chrono::Duration) {
            self.shift_wall(by);
            if let Ok(by) = by.to_std() {
                self.advance_monotonic(by);
            }
        }

        /// Moves only the wall clock, as an NTP correction or a manual change
        /// would.
        fn shift_wall(&self, by: chrono::Duration) {
            let mut now = self
                .wall
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *now += by;
        }

        /// Moves only the monotonic reading.
        fn advance_monotonic(&self, by: Duration) {
            let mut elapsed = self
                .mono
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *elapsed += by;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> DateTime<Utc> {
            *self
                .wall
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn monotonic(&self) -> Instant {
            let elapsed = self
                .mono
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.base + *elapsed
        }
    }

    /// A journal whose next `append` calls fail on demand.
    ///
    /// A delivery that gives up when the journal cannot record its release
    /// leaves the released prompt with no owner, so the failure path needs a
    /// deterministic test. Everything except `append` is delegated.
    struct FlakyJournal {
        inner: Arc<dyn Journal>,
        fail_next_appends: std::sync::atomic::AtomicUsize,
    }

    impl FlakyJournal {
        async fn in_memory() -> Arc<Self> {
            Arc::new(Self {
                inner: Arc::new(SqliteJournal::in_memory().await.expect("journal")),
                fail_next_appends: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        /// Makes the next `count` journal appends fail.
        fn fail_next_appends(&self, count: usize) {
            self.fail_next_appends.store(count, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl Journal for FlakyJournal {
        async fn upsert_thread(
            &self,
            meta: &ThreadMeta,
        ) -> Result<(), crate::journal::JournalError> {
            self.inner.upsert_thread(meta).await
        }

        async fn list_threads(&self) -> Result<Vec<ThreadMeta>, crate::journal::JournalError> {
            self.inner.list_threads().await
        }

        async fn append(
            &self,
            meta: &ThreadMeta,
            domain: &[ThreadEvent],
            wire: &[Event],
        ) -> Result<Vec<u64>, crate::journal::JournalError> {
            if self.fail_next_appends.load(Ordering::SeqCst) > 0 {
                self.fail_next_appends.fetch_sub(1, Ordering::SeqCst);
                return Err(crate::journal::JournalError::Io(std::io::Error::other(
                    "injected journal write failure",
                )));
            }
            self.inner.append(meta, domain, wire).await
        }

        async fn append_wire(
            &self,
            thread_id: &ThreadId,
            wire: &[Event],
        ) -> Result<Vec<u64>, crate::journal::JournalError> {
            self.inner.append_wire(thread_id, wire).await
        }

        async fn read_wire_since(
            &self,
            thread_id: &ThreadId,
            after: u64,
        ) -> Result<Vec<WireEvent>, crate::journal::JournalError> {
            self.inner.read_wire_since(thread_id, after).await
        }

        async fn read_wire_since_global(
            &self,
            after: u64,
        ) -> Result<Vec<WireEvent>, crate::journal::JournalError> {
            self.inner.read_wire_since_global(after).await
        }

        async fn head(&self) -> Result<u64, crate::journal::JournalError> {
            self.inner.head().await
        }

        async fn read_domain(
            &self,
            thread_id: &ThreadId,
        ) -> Result<Vec<ThreadEvent>, crate::journal::JournalError> {
            self.inner.read_domain(thread_id).await
        }
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

    /// A thread whose second prompt a failed write put back behind a typed
    /// limit, with no session and the schedule armed.
    struct ParkedRequeue {
        /// Keeps the workspace alive for the whole test.
        _workspace: tempfile::TempDir,
        manager: ThreadManager,
        meter: Arc<h4rn_meter::MeterHub>,
        events: broadcast::Receiver<WireEvent>,
        handle: ThreadHandle,
        thread_id: ThreadId,
        second: AcceptedMessage,
        sessions: h4rn_harness::SessionCount,
        sends: h4rn_harness::SendCount,
        /// Holds startups from the index given to the helper on; `None` when the
        /// setup gates none.
        start_gate: Option<h4rn_harness::Gate>,
    }

    /// Parks a requeued turn behind a typed limit on a lost write.
    ///
    /// The write claims its prompt, a restart detaches its session, and the
    /// adapter's typed limit arrives on the failure. `reset_at` is `None` for a
    /// limit that never named its reset. The helper returns once the limit row
    /// is published and the pause owns the Pending turn.
    async fn park_requeued_turn(reset_at: Option<DateTime<Utc>>) -> ParkedRequeue {
        let journal: Arc<dyn Journal> =
            Arc::new(SqliteJournal::in_memory().await.expect("journal"));
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        park_requeued_turn_on(reset_at, journal, meter, None).await
    }

    /// [`park_requeued_turn`] whose startups from `start_gate_after` on are held
    /// by a gate the caller releases.
    async fn park_requeued_turn_gated(
        reset_at: Option<DateTime<Utc>>,
        start_gate_after: usize,
    ) -> ParkedRequeue {
        let journal: Arc<dyn Journal> =
            Arc::new(SqliteJournal::in_memory().await.expect("journal"));
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        park_requeued_turn_on(reset_at, journal, meter, Some(start_gate_after)).await
    }

    /// [`park_requeued_turn`] with the caller's durable journal and ledger, so a
    /// test can restart the daemon on them.
    async fn park_requeued_turn_on(
        reset_at: Option<DateTime<Utc>>,
        journal: Arc<dyn Journal>,
        meter: Arc<h4rn_meter::MeterHub>,
        start_gate_after: Option<usize>,
    ) -> ParkedRequeue {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_send_gate();
        let harness = harness.with_limit_failure_after(1, "provider usage limit reached", reset_at);
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let (harness, start_gate) = match start_gate_after {
            Some(after) => {
                let (harness, gate) = harness.with_start_gate_after(after);
                (harness, Some(gate))
            }
            None => (harness, None),
        };
        let host = HostId::new("host_test");
        let manager = open_test_manager_with_journal(harness, host.clone(), journal).await;
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        // The second prompt is claimed and blocks in the write. While it is on
        // the wire, a restart detaches its session.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let detached = {
            let mut record = handle.lock().await;
            let generation = record.session.generation().expect("a live generation");
            record.detach_session(generation)
        };
        assert!(
            detached.is_some(),
            "the writing session was detached under the claimed turn"
        );

        // The parked write now fails with the typed limit. The prompt goes back
        // and the limit pauses the thread before anything delivers it.
        gate.release();
        loop {
            let wire = tokio::time::timeout(Duration::from_secs(10), next_event(&mut events))
                .await
                .expect("a limit row arrived");
            if matches!(wire.event, Event::UsageLimitHit { .. }) {
                break;
            }
        }
        {
            let record = handle.lock().await;
            assert!(
                record.pending_turn(),
                "the failed write put its prompt back"
            );
            assert!(
                record.scheduled_limit.is_some(),
                "the typed limit paused the thread"
            );
            assert_eq!(
                record
                    .scheduled_limit
                    .as_ref()
                    .and_then(|scheduled| scheduled.generation),
                Some(1),
                "the pause belongs to the session that claimed the write"
            );
            assert_eq!(
                record
                    .scheduled_limit
                    .as_ref()
                    .and_then(|scheduled| scheduled.parked_turn.clone()),
                Some(second.turn_id.clone()),
                "the pause names the one dispatch it parks"
            );
            assert_eq!(
                record
                    .meta
                    .parked
                    .as_ref()
                    .map(|parked| parked.turn_id.clone()),
                Some(second.turn_id.clone()),
                "the parked prompt is recorded on the thread row"
            );
            assert!(
                matches!(record.session, SessionSlot::None),
                "the requeued turn has no session"
            );
        }
        assert_eq!(
            sends.get(),
            2,
            "the parked prompt has not been written again"
        );
        ParkedRequeue {
            _workspace: workspace,
            manager,
            meter,
            events,
            handle,
            thread_id: created.thread_id,
            second,
            sessions,
            sends,
            start_gate,
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

    /// A cancel the harness rejected without closing the session is not a
    /// stop: the next Stop must try again instead of reporting the turn as
    /// already stopped while it keeps running.
    #[tokio::test]
    async fn a_second_stop_retries_a_failed_cancel() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, completion) = FakeHarness::new().with_completion_gate();
        let (harness, sends) = harness.with_send_count();
        let (harness, cancels) = harness.with_interrupt_count();
        let harness = harness.with_interrupt_failure_once();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        sends.wait_for(1).await;
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");

        // The first cancel fails with the session still open, so the turn is
        // still live: the failure reaches the caller and nothing ended.
        let error = manager
            .interrupt(&created.thread_id)
            .await
            .expect_err("a failed cancel is reported, not acknowledged");
        assert!(matches!(error, ManagerError::Internal(_)), "{error:?}");
        assert_eq!(cancels.get(), 1, "the first Stop tried the cancel");
        assert!(
            handle.lock().await.active.is_some(),
            "the turn is still running after a cancel that never arrived"
        );

        // The second Stop retries the cancel on the same claimed turn.
        manager
            .interrupt(&created.thread_id)
            .await
            .expect("the second Stop retries the cancel");
        assert_eq!(cancels.get(), 2, "the second Stop sent the cancel again");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the turn completed");
        assert_eq!(turn, created.turn_id);
        assert_eq!(
            status,
            TurnStatus::Interrupted,
            "the retried cancel ended the turn"
        );
        assert!(
            handle.lock().await.active.is_none(),
            "the stopped turn is no longer active"
        );
        completion.release();
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

    /// A startup failure while the captured turn was already stopped still
    /// advances the turn that was queued behind it.
    #[tokio::test]
    async fn a_failed_startup_does_not_strand_the_turn_queued_behind_a_stop() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_start_gate();
        let harness = harness.with_start_failure_once("the harness could not start");
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
            "the startup is in flight, holding the start gate"
        );

        // B waits behind A while A still owns the thread.
        let queued = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
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

        // The startup A outlived now fails. B is already the active turn, so
        // the failure must start a fresh session for it instead of leaving it
        // Working with no session.
        gate.release();
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_ne!(turn, created.turn_id);
        assert_eq!(status, TurnStatus::Completed, "the queued turn still runs");
        assert_eq!(sessions.get(), 2, "the queued turn started a fresh session");
    }

    /// A message sent after the only turn was stopped still runs when the
    /// startup that the stop outlived fails.
    #[tokio::test]
    async fn a_failed_startup_runs_the_message_sent_after_the_stop() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_start_gate();
        let harness = harness.with_start_failure_once("the harness could not start");
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
            .expect("stop the first turn");
        let (stopped, status) = next_turn_completion(&mut events).await;
        assert_eq!(stopped, created.turn_id);
        assert_eq!(status, TurnStatus::Interrupted);

        // C arrives while the superseded startup still holds the slot.
        let c = manager
            .send_message(&created.thread_id, "third".into(), Vec::new())
            .await
            .expect("send");
        assert_ne!(c.turn_id, created.turn_id);

        gate.release();
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_eq!(turn, c.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert_eq!(sessions.get(), 2, "C ran on a fresh session");
    }

    /// A pump whose session the thread no longer holds drops its events: a late
    /// completion from it can never settle the turn that replaced its own.
    #[tokio::test]
    async fn a_stale_pump_cannot_settle_the_turn_that_replaced_its_own() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, sessions) = FakeHarness::new().with_session_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);
        assert_eq!(sessions.get(), 1);

        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (session, generation) = {
            let record = handle.lock().await;
            (
                record.live_session().cloned().expect("a live session"),
                record.session.generation().expect("a generation"),
            )
        };
        // The session is detached (what a failed write does) and B becomes the
        // active, still-pending turn.
        let b = {
            let mut record = handle.lock().await;
            record.detach_session(generation);
            record.begin_turn(
                manager.now(),
                UserInput {
                    text: "B".into(),
                    attachments: Vec::new(),
                },
            )
        };

        // The detached pump's stream now reports the failure of its own attempt.
        let stream = futures::stream::iter(vec![HarnessEvent::TurnCompleted {
            status: TurnStatus::Failed,
            usage: Usage::default(),
        }])
        .boxed();
        Arc::clone(&manager.inner)
            .run_pump(handle.clone(), generation, session, None, stream)
            .await;

        {
            let record = handle.lock().await;
            assert_eq!(
                record.active.as_ref().map(|active| &active.turn_id),
                Some(&b.turn_id),
                "the stale pump did not touch the turn that replaced it"
            );
        }
        manager.inner.spawn_advance(handle.clone());
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_eq!(turn, b.turn_id);
        assert_eq!(
            status,
            TurnStatus::Completed,
            "B runs on a fresh session instead of failing"
        );
        assert_eq!(sessions.get(), 2);
    }

    /// A transport death reported while a promoted turn is still waiting for a
    /// session belongs to the session that died, not to the retry: B completes
    /// on a fresh agent with no error row carried over from A's transport.
    #[tokio::test]
    async fn a_dead_sessions_report_does_not_follow_the_turn_it_never_wrote() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, sessions) = FakeHarness::new().with_session_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);
        assert_eq!(sessions.get(), 1);

        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (session, generation) = {
            let record = handle.lock().await;
            (
                record.live_session().cloned().expect("a live session"),
                record.session.generation().expect("a generation"),
            )
        };
        // B is the turn promoted behind A; the agent that ran A dies before any
        // writer claims B's prompt on it.
        let b = {
            let mut record = handle.lock().await;
            record.begin_turn(
                manager.now(),
                UserInput {
                    text: "B".into(),
                    attachments: Vec::new(),
                },
            )
        };

        // The dying session reports its own death first, then its stream ends.
        let stream = futures::stream::iter(vec![HarnessEvent::TransportClosed {
            message: "the harness transport closed".into(),
            retryable: false,
            kind: TransportCloseKind::Write,
            reset_at: None,
        }])
        .boxed();
        Arc::clone(&manager.inner)
            .run_pump(handle.clone(), generation, session, None, stream)
            .await;

        let mut errors = Vec::new();
        let (turn, status) = loop {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::Error { message, .. } => errors.push(message),
                Event::TurnCompleted {
                    turn_id, status, ..
                } => break (turn_id, status),
                _ => {}
            }
        };
        assert_eq!(turn, b.turn_id);
        assert_eq!(status, TurnStatus::Completed, "{errors:?}");
        assert!(
            errors.is_empty(),
            "the retry inherited the dead session's report: {errors:?}"
        );
        assert_eq!(sessions.get(), 2, "B ran on a fresh agent");
    }

    /// The trailing `TurnCompleted(Failed)` the ACP adapter reports after a
    /// writer error must not fail the message queued behind the failed write.
    #[tokio::test]
    async fn a_trailing_completion_from_a_dead_transport_does_not_fail_the_queued_turn() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, trailing) = FakeHarness::new().with_trailing_failed_completion();
        let (harness, sessions) = harness.with_session_count();
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

        // The second prompt's write fails as a dead transport; the third waits
        // behind it.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        let third = manager
            .send_message(&created.thread_id, "third".into(), Vec::new())
            .await
            .expect("send");
        assert_eq!(third.turn_id, second.turn_id);

        let mut errors = 0;
        loop {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::Error { .. } => errors += 1,
                Event::TurnCompleted {
                    turn_id, status, ..
                } => {
                    assert_eq!(turn_id, second.turn_id);
                    assert_eq!(status, TurnStatus::Failed);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(errors, 1, "one row for the writer failure");

        // The adapter now reports the same death again as a failed completion.
        trailing.release();
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_ne!(turn, second.turn_id);
        assert_eq!(
            status,
            TurnStatus::Completed,
            "the promoted turn is not failed by the stale pump"
        );
        assert_eq!(
            sessions.get(),
            2,
            "the promoted turn started a fresh session"
        );
    }

    /// A stream that ends while the active turn was never written leaves that
    /// turn for a fresh session instead of failing it.
    #[tokio::test]
    async fn a_pump_end_leaves_a_pending_turn_for_a_fresh_session() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, sessions) = FakeHarness::new().with_session_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);
        assert_eq!(sessions.get(), 1);

        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (session, generation) = {
            let record = handle.lock().await;
            (
                record.live_session().cloned().expect("a live session"),
                record.session.generation().expect("a generation"),
            )
        };
        // The agent finishes A with `end_turn` and exits while B is queued: B
        // is the active turn but this pump never wrote its prompt.
        let b = {
            let mut record = handle.lock().await;
            record.begin_turn(
                manager.now(),
                UserInput {
                    text: "B".into(),
                    attachments: Vec::new(),
                },
            )
        };
        let stream = futures::stream::empty().boxed();
        Arc::clone(&manager.inner)
            .run_pump(handle.clone(), generation, session, None, stream)
            .await;

        {
            let record = handle.lock().await;
            assert_eq!(
                record.active.as_ref().map(|active| &active.turn_id),
                Some(&b.turn_id),
                "the pending turn is left for a fresh session"
            );
        }
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_eq!(turn, b.turn_id);
        assert_eq!(status, TurnStatus::Completed, "B runs on a fresh session");
        assert_eq!(sessions.get(), 2, "B started a fresh agent");
    }

    /// A turn whose own startup's session dies before its prompt is claimed can
    /// never run: the turn fails once and the queue moves on, instead of the
    /// pump replacing the session forever.
    #[tokio::test]
    async fn a_turn_whose_own_session_dies_before_delivery_is_failed_once() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, sessions) = FakeHarness::new()
            .with_stderr_tail("boom: the agent crashed")
            .with_session_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);
        assert_eq!(sessions.get(), 1);

        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (session, generation) = {
            let record = handle.lock().await;
            (
                record.live_session().cloned().expect("a live session"),
                record.session.generation().expect("a generation"),
            )
        };
        // B is the turn this session's startup carried, and the agent exits
        // before any writer claimed B's prompt.
        let b = {
            let mut record = handle.lock().await;
            record.begin_turn(
                manager.now(),
                UserInput {
                    text: "B".into(),
                    attachments: Vec::new(),
                },
            )
        };
        b.note_started_generation(generation);

        Arc::clone(&manager.inner)
            .run_pump(
                handle.clone(),
                generation,
                session,
                None,
                futures::stream::empty().boxed(),
            )
            .await;

        let mut errors = 0;
        let (turn, status) = loop {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::Error { message, hint, .. } => {
                    errors += 1;
                    assert!(
                        message.contains("before it accepted the prompt"),
                        "the row names the startup exit: {message}"
                    );
                    assert!(
                        hint.unwrap_or_default().contains("boom: the agent crashed"),
                        "the row keeps the stderr tail"
                    );
                }
                Event::TurnCompleted {
                    turn_id, status, ..
                } => break (turn_id, status),
                _ => {}
            }
        };
        assert_eq!(turn, b.turn_id);
        assert_eq!(status, TurnStatus::Failed);
        assert_eq!(errors, 1, "one row for the turn that never ran");
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sessions.get(),
            1,
            "a session that dies at start is not replaced"
        );
        assert!(
            handle.lock().await.active.is_none(),
            "the turn ended instead of staying active"
        );

        // The queue moves on: the next message gets its own startup.
        let next = manager
            .send_message(&created.thread_id, "C".into(), Vec::new())
            .await
            .expect("send");
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_eq!(turn, next.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert_eq!(sessions.get(), 2, "the next message started its own agent");
    }

    /// A startup that lands exactly where the pump used to clear a dead session
    /// and then fail its turn cannot start an agent: the clear and the
    /// settlement are one record-lock scope, so the startup runs before it
    /// (seeing the live slot) or after the turn has ended (starting nothing).
    ///
    /// The competing startup is parked on the record lock before the guard is
    /// dropped, so the interleaving is the lock's fair queue order rather than a
    /// timing guess.
    #[tokio::test]
    async fn a_startup_in_the_pump_end_gap_cannot_run_the_failed_turn() {
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
        let (session, generation) = {
            let record = handle.lock().await;
            (
                record.live_session().cloned().expect("a live session"),
                record.session.generation().expect("a generation"),
            )
        };
        // B is the turn this session's startup carried, and the agent's stream
        // ended before any writer claimed B's prompt.
        let b = {
            let mut record = handle.lock().await;
            record.begin_turn(
                manager.now(),
                UserInput {
                    text: "B".into(),
                    attachments: Vec::new(),
                },
            )
        };
        b.note_started_generation(generation);

        // The pump's stream end and a startup that was scheduled for B are
        // queued on the record lock in that order, with the pump first.
        let guard = handle.lock().await;
        let (pump, pump_parked) = spawn_parked(|parked| {
            let inner = Arc::clone(&manager.inner);
            let handle = handle.clone();
            let session = Arc::clone(&session);
            async move {
                parked.send(()).expect("the test is still waiting");
                inner
                    .run_pump(
                        handle,
                        generation,
                        session,
                        None,
                        futures::stream::empty().boxed(),
                    )
                    .await;
            }
        });
        pump_parked
            .await
            .expect("the pump is parked on the record lock");
        let (startup, startup_parked) = spawn_parked(|parked| {
            let inner = Arc::clone(&manager.inner);
            let handle = handle.clone();
            async move {
                parked.send(()).expect("the test is still waiting");
                inner.start_session(handle).await;
            }
        });
        startup_parked
            .await
            .expect("the competing startup is parked on the record lock");
        drop(guard);
        pump.await.expect("pump task");
        startup.await.expect("startup task");

        let mut errors = 0;
        let (turn, status) = loop {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::Error { message, .. } => {
                    errors += 1;
                    assert!(
                        message.contains("before it accepted the prompt"),
                        "the row names the startup exit: {message}"
                    );
                }
                Event::TurnCompleted {
                    turn_id, status, ..
                } => break (turn_id, status),
                _ => {}
            }
        };
        assert_eq!(turn, b.turn_id);
        assert_eq!(status, TurnStatus::Failed);
        assert_eq!(errors, 1, "the turn is failed exactly once");
        assert_eq!(
            sessions.get(),
            1,
            "the startup that arrived after the clear started no agent"
        );
        assert_eq!(
            sends.get(),
            1,
            "the failed turn's prompt never reached an agent"
        );
        let record = handle.lock().await;
        assert!(record.active.is_none(), "the turn ended");
        assert!(
            matches!(record.session, SessionSlot::None),
            "the slot is left free for the next turn's own startup"
        );
    }

    /// A startup that fails clears its own slot and settles the turn it carried
    /// in one record-lock scope, so a second startup scheduled for the same
    /// turn cannot take the empty slot, start an agent and write the prompt
    /// just before the turn is recorded as failed.
    ///
    /// Both scheduled actors are parked on the record lock before the guard is
    /// dropped: the failure first, the competing startup second.
    #[tokio::test]
    async fn a_second_startup_cannot_take_the_slot_a_failing_startup_is_clearing() {
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
        // The state the first startup leaves: no session, its turn active, and
        // the slot marked as starting for the generation it captured. The
        // live session from the first turn is detached, as a restart would.
        let (a, generation) = {
            let mut record = handle.lock().await;
            if let Some(previous) = record
                .session
                .generation()
                .and_then(|generation| record.detach_session(generation))
            {
                drop(previous);
            }
            let generation = record.next_session_gen();
            record.session = SessionSlot::Starting(generation);
            let a = record.begin_turn(
                manager.now(),
                UserInput {
                    text: "A".into(),
                    attachments: Vec::new(),
                },
            );
            (a, generation)
        };

        let guard = handle.lock().await;
        let (failing, failing_parked) = spawn_parked(|parked| {
            let inner = Arc::clone(&manager.inner);
            let handle = handle.clone();
            let a = Arc::clone(&a);
            async move {
                parked.send(()).expect("the test is still waiting");
                inner
                    .fail_session(
                        handle,
                        Some(generation),
                        a,
                        "the harness could not start".into(),
                    )
                    .await;
            }
        });
        failing_parked
            .await
            .expect("the failing startup is parked on the record lock");
        let (second, second_parked) = spawn_parked(|parked| {
            let inner = Arc::clone(&manager.inner);
            let handle = handle.clone();
            async move {
                parked.send(()).expect("the test is still waiting");
                inner.start_session(handle).await;
            }
        });
        second_parked
            .await
            .expect("the competing startup is parked on the record lock");
        drop(guard);
        failing.await.expect("failing startup task");
        second.await.expect("second startup task");

        let mut errors = 0;
        let (turn, status) = loop {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::Error { message, .. } => {
                    errors += 1;
                    assert_eq!(message, "the harness could not start");
                }
                Event::TurnCompleted {
                    turn_id, status, ..
                } => break (turn_id, status),
                _ => {}
            }
        };
        assert_eq!(turn, a.turn_id);
        assert_eq!(status, TurnStatus::Failed);
        assert_eq!(errors, 1, "the turn is failed exactly once");
        assert_eq!(
            sessions.get(),
            1,
            "the competing startup started no agent for the failed turn"
        );
        assert_eq!(
            sends.get(),
            1,
            "the competing startup never wrote the failed turn's prompt"
        );
        let record = handle.lock().await;
        assert!(record.active.is_none(), "the turn ended");
        assert!(
            matches!(record.session, SessionSlot::None),
            "the slot stays free for the next turn"
        );
    }

    /// A harness whose sessions all exit right after the handshake fails each
    /// turn on the one session it started, and moves the queue on instead of
    /// starting replacements forever.
    #[tokio::test]
    async fn sessions_that_exit_at_startup_end_their_turn_without_a_respawn_loop() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, sessions) = FakeHarness::new()
            .with_immediate_stream_end()
            .with_session_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");

        let (turn, status) = next_turn_completion(&mut events).await;
        assert_eq!(turn, created.turn_id);
        assert_eq!(status, TurnStatus::Failed);
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sessions.get(),
            1,
            "the dead session was not replaced in a loop"
        );

        // Two messages queue behind each other; each gets exactly one session
        // and fails on it.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        let third = manager
            .send_message(&created.thread_id, "third".into(), Vec::new())
            .await
            .expect("send");
        assert_eq!(third.turn_id, second.turn_id);

        let (turn, status) = next_turn_completion(&mut events).await;
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Failed);
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_ne!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Failed, "the queued turn advanced");
        assert_eq!(sessions.get(), 3, "one session per turn, none replaced");
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert_eq!(sessions.get(), 3, "no replacement sessions after the last");
    }

    /// An unrelated harness error earlier in the turn does not hide the stderr
    /// row the pump publishes when the stream ends.
    #[tokio::test]
    async fn an_earlier_error_does_not_hide_the_pump_end_stderr_row() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, end_gate) = FakeHarness::with_events(vec![HarnessEvent::Error {
            message: "an unrelated harness warning".into(),
            retryable: false,
        }])
        .with_stderr_tail("boom: the agent crashed")
        .with_stream_end_gate();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");

        let first = loop {
            let wire = next_event(&mut events).await;
            if matches!(wire.event, Event::Error { .. }) {
                break wire;
            }
        };
        match first.event {
            Event::Error { message, .. } => {
                assert_eq!(message, "an unrelated harness warning")
            }
            other => panic!("expected the harness error row, got {other:?}"),
        }
        assert!(
            wait_until(|| sends.get() == 1).await,
            "the prompt was written before the stream ended"
        );

        // The agent's stream ends. The turn is settled with the stderr hint,
        // which must still get its own row after the unrelated error.
        end_gate.release();
        let wire = loop {
            let wire = next_event(&mut events).await;
            if matches!(wire.event, Event::Error { .. }) {
                break wire;
            }
        };
        match wire.event {
            Event::Error { message, hint, .. } => {
                assert!(
                    message.contains("ended before the turn completed"),
                    "the pump-end row is published: {message}"
                );
                let hint = hint.unwrap_or_default();
                assert!(
                    hint.contains("boom: the agent crashed"),
                    "the row keeps the stderr tail: {hint}"
                );
            }
            other => panic!("expected the pump-end error row, got {other:?}"),
        }
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_eq!(turn, created.turn_id);
        assert_eq!(status, TurnStatus::Failed);
    }

    /// A model change supersedes a startup that is still booting the old model.
    #[tokio::test]
    async fn a_model_change_supersedes_a_startup_in_flight() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_start_gate();
        let (harness, sessions) = harness.with_session_count();
        let (harness, shutdowns) = harness.with_shutdown_count();
        let (harness, models) = harness.with_session_models();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert!(
            wait_until(|| sessions.get() == 1).await,
            "the old model's startup is in flight"
        );
        manager
            .interrupt(&created.thread_id)
            .await
            .expect("stop the first turn");
        let (stopped, status) = next_turn_completion(&mut events).await;
        assert_eq!(stopped, created.turn_id);
        assert_eq!(status, TurnStatus::Interrupted);

        let schedule = meter
            .arm_schedule(
                &created.thread_id,
                None,
                "resume".into(),
                Some(ResumeFailover {
                    model: "other-model".into(),
                    account: None,
                    account_label: None,
                    reason: "the old model hit its limit".into(),
                }),
                manager.now(),
            )
            .await
            .expect("arm schedule");
        // The record learns the schedule from the ledger (as it does after a
        // restart), which is also what gives the fire its identity to match.
        manager.resync_meter_state().await;
        manager
            .inner
            .fire_schedule(schedule)
            .await
            .expect("fire the failover");

        gate.release();
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_ne!(turn, created.turn_id);
        assert_eq!(status, TurnStatus::Completed, "the resume runs");
        assert_eq!(
            models.get(),
            vec!["fake-model".to_owned(), "other-model".to_owned()],
            "the resume started a session on the new model"
        );
        assert_eq!(
            shutdowns.get(),
            1,
            "the startup that was booting the old model was discarded"
        );
        assert_eq!(sessions.get(), 2);
        assert_eq!(
            manager
                .inner
                .handle(&created.thread_id)
                .await
                .expect("handle")
                .lock()
                .await
                .meta
                .model,
            "other-model"
        );
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

    /// A restart inspects a thread while it is idle, then a Send claims its
    /// session before the restart detaches it. The detach must see the active
    /// turn under the same lock and leave the session with the turn that is
    /// already writing to it.
    #[tokio::test]
    async fn a_restart_does_not_detach_a_session_a_send_just_claimed() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, completion) = FakeHarness::new().with_later_completion_gate();
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let (harness, shutdowns) = harness.with_shutdown_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), next_turn_status(&mut events))
                .await
                .expect("the turn completed"),
            TurnStatus::Completed
        );
        assert_eq!(sessions.get(), 1);

        // Hold the restart between its idle inspection and its detach.
        let pause = Arc::new(RestartPause::default());
        *manager
            .inner
            .restart_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&pause));
        let restarting = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.restart_sessions(&[HarnessKind::ClaudeCode]).await })
        };
        pause.inspected.notified().await;

        // A Send now claims the live session and writes its prompt to it.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;

        // The restart resumes and must leave that session alone.
        pause.resume.notify_one();
        let restarted = restarting.await.expect("restart task");
        assert!(
            restarted.restarted.is_empty(),
            "the restart skipped the thread whose session a turn had claimed: {restarted:?}"
        );
        assert_eq!(
            shutdowns.get(),
            0,
            "the claimed session was not shut down under its turn"
        );
        assert_eq!(
            sessions.get(),
            1,
            "no replacement session was started for the claimed turn"
        );

        // The turn still runs to completion on the session it claimed.
        completion.release();
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the turn completed");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert_eq!(sessions.get(), 1);
    }

    /// A write that fails after its session was detached (a restart, a
    /// shutdown) still owns an active turn. It must put the prompt back and
    /// deliver it again instead of returning and leaving the turn Working.
    #[tokio::test]
    async fn a_failed_write_after_its_session_was_detached_is_delivered_again() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_send_gate();
        let harness = harness.with_dead_transport_after(1);
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), next_turn_status(&mut events))
                .await
                .expect("the turn completed"),
            TurnStatus::Completed
        );

        // The second prompt is claimed and blocks in the write. While it is on
        // the wire, a restart detaches its session.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let detached = {
            let mut record = handle.lock().await;
            let generation = record.session.generation().expect("a live generation");
            record.detach_session(generation)
        };
        assert!(
            detached.is_some(),
            "the writing session was detached under the claimed turn"
        );

        // The write now fails on the dead session. The turn must not be left
        // active with no delivery: it is written again on a fresh session.
        gate.release();
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the turn completed");
        assert_eq!(turn, second.turn_id);
        assert_eq!(
            status,
            TurnStatus::Completed,
            "the prompt was delivered on a fresh session"
        );
        assert_eq!(sessions.get(), 2, "a fresh session carried the retry");
        assert!(
            handle.lock().await.active.is_none(),
            "the turn ended instead of staying Working"
        );
    }

    /// A Stop the person already saw acknowledged must never replay.
    ///
    /// The session detaches while the prompt write is in flight, so the Stop's
    /// cancel has nowhere to go. When that write later fails, the recovery path
    /// used to put the prompt back and write it on a fresh session: a turn the
    /// person had stopped would run anyway. It now ends interrupted.
    #[tokio::test]
    async fn a_stopped_write_that_lost_its_session_is_not_replayed() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_send_gate();
        let harness = harness.with_dead_transport_after(1);
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let (harness, cancels) = harness.with_interrupt_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), next_turn_status(&mut events))
                .await
                .expect("the turn completed"),
            TurnStatus::Completed
        );

        // The second prompt is claimed and blocks in its write. Its session
        // detaches under it, so the Stop that follows has no session to cancel.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let detached = {
            let mut record = handle.lock().await;
            let generation = record.session.generation().expect("a live generation");
            record.detach_session(generation)
        };
        assert!(
            detached.is_some(),
            "the writing session was detached under the claimed turn"
        );
        manager
            .interrupt(&created.thread_id)
            .await
            .expect("a stop after the prompt is claimed is accepted");
        assert_eq!(cancels.get(), 0, "there was no session left to cancel");

        // The write now fails on the detached session. The prompt must not be
        // replayed: the stopped turn ends interrupted, on no new session.
        gate.release();
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the turn completed");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Interrupted);
        assert_eq!(sends.get(), 2, "the stopped prompt was never written again");
        assert_eq!(
            sessions.get(),
            1,
            "no fresh session carried a stopped prompt"
        );
        assert!(
            handle.lock().await.active.is_none(),
            "the stopped turn ended instead of staying Working"
        );
    }

    /// A Stop that lands exactly in the failed write's inspect-to-requeue
    /// window must still settle the turn.
    ///
    /// The recovery used to check the dispatch for a Stop and then requeue it
    /// in two steps; a Stop between them made the requeue fail and the path
    /// return, leaving the turn Working with nobody delivering it. The one
    /// decision now either puts the prompt back or ends the turn interrupted.
    #[tokio::test]
    async fn a_stop_inside_the_failed_writes_recovery_window_settles_the_turn() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_send_gate();
        let harness = harness.with_dead_transport_after(1);
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), next_turn_status(&mut events))
                .await
                .expect("the turn completed"),
            TurnStatus::Completed
        );

        // The second prompt is claimed and blocks in its write; its session
        // detaches under it, so the write failure takes the recovery path.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let dispatch = {
            let record = handle.lock().await;
            record
                .dispatch_for(&second.turn_id)
                .expect("the second turn's dispatch")
        };
        let detached = {
            let mut record = handle.lock().await;
            let generation = record.session.generation().expect("a live generation");
            record.detach_session(generation)
        };
        assert!(detached.is_some(), "the writing session was detached");

        // Hold the write failure between inspecting the dispatch and acting on
        // it, and land the Stop in that window.
        let pause = Arc::new(RecoverPause::default());
        dispatch.pause_recovery_at(Arc::clone(&pause));
        gate.release();
        pause.inspected.notified().await;
        assert!(
            matches!(dispatch.stop(), StopOutcome::AlreadySent),
            "the Stop lands on the claimed turn"
        );
        pause.resume.notify_one();

        // The turn must end, not stay active with nobody delivering it.
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the turn completed");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Interrupted);
        assert_eq!(sends.get(), 2, "the stopped prompt was never written again");
        assert_eq!(
            sessions.get(),
            1,
            "no fresh session carried a stopped prompt"
        );
        assert!(
            handle.lock().await.active.is_none(),
            "the stopped turn ended instead of staying Working"
        );
    }

    /// A Stop followed by the session's death ends the turn interrupted, not as
    /// a transport failure: the person already ended the turn, so the death
    /// that followed the cancel must not raise an error row.
    #[tokio::test]
    async fn a_session_death_after_a_stop_raises_no_error_row() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, sends) = FakeHarness::new().with_send_count();
        // The fake agent's own completion is held so the Stop lands on a turn
        // that is still claimed; the session's death is reported below.
        let (harness, completion) = harness.with_later_completion_gate();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), next_turn_status(&mut events))
                .await
                .expect("the turn completed"),
            TurnStatus::Completed
        );

        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (session, generation, dispatch) = {
            let record = handle.lock().await;
            (
                record.live_session().cloned().expect("a live session"),
                record.session.generation().expect("a generation"),
                record
                    .dispatch_for(&second.turn_id)
                    .expect("the turn's dispatch"),
            )
        };
        assert_eq!(
            dispatch.stop(),
            StopOutcome::AlreadySent,
            "the Stop lands on the claimed turn"
        );

        // The session reports its death on the way out, and the adapter's
        // prompt task then reports the turn failed, as ACP does after a
        // cancel closes the transport.
        let health = h4rn_harness::TransportHealth::new();
        health.close_with(TransportCloseKind::Eof);
        let stream = futures::stream::iter(vec![
            HarnessEvent::TransportClosed {
                message: "the harness transport closed".into(),
                retryable: true,
                kind: TransportCloseKind::Eof,
                reset_at: None,
            },
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Failed,
                usage: Usage::default(),
            },
        ])
        .boxed();
        tokio::time::timeout(
            Duration::from_secs(5),
            Arc::clone(&manager.inner).run_pump(
                handle.clone(),
                generation,
                session,
                Some(health),
                stream,
            ),
        )
        .await
        .expect("the pump ends with the stopped turn");

        let mut errors = Vec::new();
        let (turn, status) = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let wire = next_event(&mut events).await;
                match wire.event {
                    Event::Error { message, .. } => errors.push(message),
                    Event::TurnCompleted {
                        turn_id, status, ..
                    } => break (turn_id, status),
                    _ => {}
                }
            }
        })
        .await
        .expect("the stopped turn settled");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Interrupted);
        assert!(
            errors.is_empty(),
            "the stopped turn published a transport error row: {errors:?}"
        );
        completion.release();
    }

    /// A read error is non-retryable but it is not the exit report: the pump
    /// must keep waiting for the exit kind, so a reset that only that report
    /// carries still arms the schedule before the queue is promoted.
    #[tokio::test]
    async fn a_read_error_waits_for_the_exit_reports_reset() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(5);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, completion) = harness.with_later_completion_gate();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), next_turn_status(&mut events))
                .await
                .expect("the turn completed"),
            TurnStatus::Completed
        );

        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        manager
            .send_message(&created.thread_id, "third".into(), Vec::new())
            .await
            .expect("queue third");
        sends.wait_for(2).await;

        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (session, pump) = {
            let mut record = handle.lock().await;
            (
                record.live_session().cloned().expect("a live session"),
                record.pump.take().expect("this session's pump"),
            )
        };
        session
            .lock()
            .await
            .shutdown()
            .await
            .expect("close the fake session");
        drop(session);

        // `end_turn` arrives on a dead transport, then a stdout read error
        // (also non-retryable), and only then the exit report carrying the
        // only reset.
        harness_events.emit(HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Usage::default(),
        });
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "failed to read the ACP agent stdout: broken pipe".into(),
            retryable: false,
            kind: TransportCloseKind::Read,
            reset_at: None,
        });
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "agy exited (exit status: 7): agy: the session ended".into(),
            retryable: false,
            kind: TransportCloseKind::Exit,
            reset_at: Some(reset_at),
        });

        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("the pump ends with the completion")
            .expect("the pump task did not panic");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the turn completed");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);

        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(
            schedules.len(),
            1,
            "the read error did not end the wait before the exit report's reset"
        );
        assert!(
            schedules[0].reason.contains("provider reset at"),
            "the schedule is armed from the exit report's reset, not a backoff: {:?}",
            schedules[0].reason
        );
        assert!(
            schedules[0].resume_at >= reset_at,
            "the schedule waits for the reset the exit report carried: {:?}",
            schedules[0].resume_at
        );
        let summary = manager
            .thread_summary(&created.thread_id)
            .await
            .expect("summary");
        assert_eq!(
            summary.agent_state,
            AgentState::Scheduled,
            "the armed resume keeps the thread scheduled"
        );
        // The reset is minutes away; a paused clock makes the "no queued work
        // started yet" window pass at once instead of waiting it in real time.
        tokio::time::pause();
        let started = tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                let wire = next_event(&mut events).await;
                if let Event::TurnStarted { turn_id, .. } = wire.event {
                    return turn_id;
                }
            }
        })
        .await;
        tokio::time::resume();
        assert!(
            started.is_err(),
            "queued work did not start past the exit report's reset: {started:?}"
        );
        completion.release();
    }

    /// A limit notification without a reset is not the exit report, so it must
    /// not end the closed-completion wait.
    ///
    /// The bridge reports an ordinary limit first and only then the exit that
    /// carries the provider's reset. Ending the wait on the first one left the
    /// thread paused on nothing and promoted the queued work.
    #[tokio::test]
    async fn a_reset_less_limit_waits_for_the_exit_reports_reset() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(47);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, completion) = harness.with_later_completion_gate();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), next_turn_status(&mut events))
                .await
                .expect("the turn completed"),
            TurnStatus::Completed
        );

        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        manager
            .send_message(&created.thread_id, "third".into(), Vec::new())
            .await
            .expect("queue third");
        sends.wait_for(2).await;

        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (session, pump) = {
            let mut record = handle.lock().await;
            (
                record.live_session().cloned().expect("a live session"),
                record.pump.take().expect("this session's pump"),
            )
        };
        session
            .lock()
            .await
            .shutdown()
            .await
            .expect("close the fake session");
        drop(session);

        // The completion lands on a closed transport. The limit notification
        // that follows carries no reset; only the exit report does.
        harness_events.emit(HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Usage::default(),
        });
        harness_events.emit(HarnessEvent::LimitHit {
            reset_at: None,
            auto_resume: false,
        });
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "agy exited (exit status: 7): agy: the session ended".into(),
            retryable: false,
            kind: TransportCloseKind::Exit,
            reset_at: Some(reset_at),
        });

        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("the pump ends with the completion")
            .expect("the pump task did not panic");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the turn completed");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);

        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(
            schedules.len(),
            1,
            "the reset-less limit did not end the wait before the exit report's reset"
        );
        assert!(
            schedules[0].reason.contains("provider reset at"),
            "the schedule is armed from the exit report's reset, not a backoff: {:?}",
            schedules[0].reason
        );
        assert!(
            schedules[0].resume_at >= reset_at,
            "the schedule waits for the reset the exit report carried: {:?}",
            schedules[0].resume_at
        );
        let summary = manager
            .thread_summary(&created.thread_id)
            .await
            .expect("summary");
        assert_eq!(
            summary.agent_state,
            AgentState::Scheduled,
            "the armed resume keeps the thread scheduled"
        );
        // The reset is minutes away; a paused clock makes the "no queued work
        // started yet" window pass at once instead of waiting it in real time.
        tokio::time::pause();
        let started = tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                let wire = next_event(&mut events).await;
                if let Event::TurnStarted { turn_id, .. } = wire.event {
                    return turn_id;
                }
            }
        })
        .await;
        tokio::time::resume();
        assert!(
            started.is_err(),
            "queued work did not start past the exit report's reset: {started:?}"
        );
        completion.release();
    }

    /// One provider limit arms one schedule.
    ///
    /// The bridge reports one pause twice: the prompt path's typed limit arrives
    /// before the completion, and the exit report after it carries the same
    /// reset. Arming each event replaced the schedule and bumped its attempt
    /// counter, changing the jitter of the one pause they describe.
    #[tokio::test]
    async fn one_provider_limit_arms_one_schedule() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(12);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, completion) = harness.with_later_completion_gate();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), next_turn_status(&mut events))
                .await
                .expect("the turn completed"),
            TurnStatus::Completed
        );

        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        manager
            .send_message(&created.thread_id, "third".into(), Vec::new())
            .await
            .expect("queue third");
        sends.wait_for(2).await;

        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (session, pump) = {
            let mut record = handle.lock().await;
            (
                record.live_session().cloned().expect("a live session"),
                record.pump.take().expect("this session's pump"),
            )
        };
        session
            .lock()
            .await
            .shutdown()
            .await
            .expect("close the fake session");
        drop(session);

        // The typed limit lands first, while the turn is still active; the
        // completion then waits for the exit report, which carries the reset
        // again.
        harness_events.emit(HarnessEvent::LimitHit {
            reset_at: Some(reset_at),
            auto_resume: true,
        });
        harness_events.emit(HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Usage::default(),
        });
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "agy exited (exit status: 7): agy: the session ended".into(),
            retryable: false,
            kind: TransportCloseKind::Exit,
            reset_at: Some(reset_at),
        });

        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("the pump ends with the completion")
            .expect("the pump task did not panic");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the turn completed");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);

        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(schedules.len(), 1, "one provider limit is one schedule");
        assert_eq!(
            schedules[0].attempt, 0,
            "the exit report did not replace the schedule the notification armed"
        );
        assert!(
            schedules[0].reason.contains("provider reset at"),
            "the schedule keeps the provider's reset: {:?}",
            schedules[0].reason
        );
        assert!(
            schedules[0].resume_at >= reset_at,
            "the schedule waits for the provider's reset: {:?}",
            schedules[0].resume_at
        );
        let summary = manager
            .thread_summary(&created.thread_id)
            .await
            .expect("summary");
        assert_eq!(
            summary.agent_state,
            AgentState::Scheduled,
            "the armed resume keeps the thread scheduled"
        );
        // The reset is minutes away; a paused clock makes the "no queued work
        // started yet" window pass at once instead of waiting it in real time.
        tokio::time::pause();
        let started = tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                let wire = next_event(&mut events).await;
                if let Event::TurnStarted { turn_id, .. } = wire.event {
                    return turn_id;
                }
            }
        })
        .await;
        tokio::time::resume();
        assert!(
            started.is_err(),
            "queued work did not start past the provider's reset: {started:?}"
        );
        completion.release();
    }

    /// A duplicate reset on the exit report must not undo a cancelled resume.
    ///
    /// The bridge can report one provider pause twice: the typed limit before
    /// the closed completion, then the same reset on the exit report after it.
    /// The person can cancel the resume while the pump waits for that exit
    /// report; applying the duplicated reset again would arm a new schedule
    /// behind the cancellation.
    #[tokio::test]
    async fn a_duplicate_exit_reset_does_not_rearm_a_cancelled_resume() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(12);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, completion) = harness.with_later_completion_gate();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), next_turn_status(&mut events))
                .await
                .expect("the turn completed"),
            TurnStatus::Completed
        );

        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        manager
            .send_message(&created.thread_id, "third".into(), Vec::new())
            .await
            .expect("queue third");
        sends.wait_for(2).await;

        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (session, pump) = {
            let mut record = handle.lock().await;
            (
                record.live_session().cloned().expect("a live session"),
                record.pump.take().expect("this session's pump"),
            )
        };
        session
            .lock()
            .await
            .shutdown()
            .await
            .expect("close the fake session");
        drop(session);

        // The typed limit lands first, while the turn is still active, and arms
        // its schedule before the completion is even reported.
        harness_events.emit(HarnessEvent::LimitHit {
            reset_at: Some(reset_at),
            auto_resume: true,
        });
        loop {
            let wire = tokio::time::timeout(Duration::from_secs(10), next_event(&mut events))
                .await
                .expect("the event arrived");
            if matches!(wire.event, Event::UsageLimitHit { .. }) {
                break;
            }
        }
        manager
            .cancel_resume(&created.thread_id)
            .await
            .expect("cancel the armed resume");
        assert!(
            !meter
                .schedules()
                .await
                .expect("read schedules")
                .iter()
                .any(|schedule| matches!(
                    schedule.state,
                    ResumeState::Scheduled | ResumeState::Waiting
                )),
            "the cancellation removed the schedule the limit armed"
        );

        // The completion waits for the exit report, which repeats the same
        // reset. It must not arm a new schedule behind the cancellation.
        harness_events.emit(HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Usage::default(),
        });
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "agy exited (exit status: 7): agy: the session ended".into(),
            retryable: false,
            kind: TransportCloseKind::Exit,
            reset_at: Some(reset_at),
        });

        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("the pump ends with the completion")
            .expect("the pump task did not panic");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the turn completed");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert!(
            !meter
                .schedules()
                .await
                .expect("read schedules")
                .iter()
                .any(|schedule| matches!(
                    schedule.state,
                    ResumeState::Scheduled | ResumeState::Waiting
                )),
            "the duplicated exit reset re-armed a resume the person cancelled"
        );
        let summary = manager
            .thread_summary(&created.thread_id)
            .await
            .expect("summary");
        assert_ne!(
            summary.agent_state,
            AgentState::Scheduled,
            "the cancelled resume left the thread scheduled"
        );
        completion.release();
    }

    /// A tool update that arrived behind a closed transport's completion is
    /// part of that turn: applying it before the settlement keeps the tool card
    /// from staying Running after the turn ends.
    #[tokio::test]
    async fn a_tool_update_buffered_behind_a_closed_completion_reaches_the_turn() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, completion) = harness.with_later_completion_gate();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), next_turn_status(&mut events))
                .await
                .expect("the turn completed"),
            TurnStatus::Completed
        );

        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (session, pump) = {
            let mut record = handle.lock().await;
            (
                record.live_session().cloned().expect("a live session"),
                record.pump.take().expect("this session's pump"),
            )
        };
        session
            .lock()
            .await
            .shutdown()
            .await
            .expect("close the fake session");
        drop(session);

        let started_at = chrono::Utc::now();
        let tool = |status| h4rn_protocol::ToolCall {
            tool_call_id: "tool-1".into(),
            name: "Edit".into(),
            status,
            summary: "src/lib.rs".into(),
            input: None,
            output: None,
            started_at,
            completed_at: None,
            stats: None,
        };
        harness_events.emit(HarnessEvent::ToolCall {
            tool_call: tool(ToolCallStatus::Running),
        });
        harness_events.emit(HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Usage::default(),
        });
        // Both arrive behind the completion, before the terminal exit report.
        harness_events.emit(HarnessEvent::ToolCall {
            tool_call: tool(ToolCallStatus::Completed),
        });
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "agy exited (exit status: 0)".into(),
            retryable: false,
            kind: TransportCloseKind::Exit,
            reset_at: None,
        });

        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("the pump ends with the completion")
            .expect("the pump task did not panic");

        let mut tool_completed = false;
        let (turn, status) = loop {
            let wire = tokio::time::timeout(Duration::from_secs(10), next_event(&mut events))
                .await
                .expect("the event arrived");
            match wire.event {
                Event::ToolCall { tool_call, .. }
                    if tool_call.tool_call_id == "tool-1"
                        && tool_call.status == ToolCallStatus::Completed =>
                {
                    tool_completed = true;
                }
                Event::TurnCompleted {
                    turn_id, status, ..
                } => break (turn_id, status),
                _ => {}
            }
        };
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert!(
            tool_completed,
            "the buffered tool completion reached the turn before it settled"
        );
        completion.release();
    }

    /// An approval buffered behind a closed completion can never be answered:
    /// the completion settles the turn and the pump detaches the transport, so
    /// no session would receive the answer.
    ///
    /// It must expire with the turn instead of becoming a live pending cue.
    #[tokio::test]
    async fn an_approval_buffered_behind_a_closed_completion_expires() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, completion) = harness.with_later_completion_gate();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), next_turn_status(&mut events))
                .await
                .expect("the turn completed"),
            TurnStatus::Completed
        );

        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (session, pump) = {
            let mut record = handle.lock().await;
            (
                record.live_session().cloned().expect("a live session"),
                record.pump.take().expect("this session's pump"),
            )
        };
        session
            .lock()
            .await
            .shutdown()
            .await
            .expect("close the fake session");
        drop(session);

        harness_events.emit(HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Usage::default(),
        });
        // The approval arrives behind the completion, before the exit report.
        harness_events.emit(HarnessEvent::ApprovalRequested {
            approval_id: "approval-1".into(),
            kind: ApprovalKind::Command,
            prompt: "run the tests?".into(),
            options: Vec::new(),
        });
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "agy exited (exit status: 0)".into(),
            retryable: false,
            kind: TransportCloseKind::Exit,
            reset_at: None,
        });

        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("the pump ends with the completion")
            .expect("the pump task did not panic");

        let mut approval_published = false;
        let (turn, status) = loop {
            let wire = tokio::time::timeout(Duration::from_secs(10), next_event(&mut events))
                .await
                .expect("the event arrived");
            match wire.event {
                Event::ApprovalRequested { .. } => approval_published = true,
                Event::TurnCompleted {
                    turn_id, status, ..
                } => break (turn_id, status),
                _ => {}
            }
        };
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert!(
            !approval_published,
            "an approval buffered behind a closed completion was never published"
        );
        assert!(
            handle.lock().await.pending_approvals.is_empty(),
            "no live pending approval remains after the turn settled"
        );
        completion.release();
    }

    /// When the fallback deadline and an exit report are ready together, the
    /// report wins: the dead session's specific row is published instead of the
    /// generic close notice.
    ///
    /// The stream is a channel and reports when it is parked, so the test can
    /// queue the exit report and expire the deadline before the wait polls
    /// again. Both branches are ready at that one poll, which is what makes the
    /// test depend on the `biased;` order: without it the select would pick the
    /// expired deadline half the time. The rounds repeat the same
    /// ready-together poll, so a select without `biased;` loses reliably
    /// instead of one time in two.
    #[tokio::test]
    async fn an_idle_report_wins_over_an_expired_fallback_deadline() {
        for round in 0..64 {
            let (reports, queued) = tokio::sync::mpsc::unbounded_channel();
            let parked = Arc::new(tokio::sync::Notify::new());
            let mut stream: futures::stream::BoxStream<'static, HarnessEvent> =
                Box::pin(ParkedStream {
                    events: queued,
                    parked: Arc::clone(&parked),
                });
            let waiting = tokio::spawn(async move {
                let deadline = tokio::time::Instant::now() + IDLE_DEATH_DETAIL_WAIT;
                Inner::wait_for_idle_report(&mut stream, deadline).await
            });
            // The wait is parked with the deadline not yet expired.
            parked.notified().await;
            tokio::time::pause();

            // Queue the exit report and expire the deadline before the wait
            // polls again. The clock's advance alone does not wake it, so
            // nothing is decided yet; queuing the report wakes the parked
            // select with both branches ready at one poll.
            tokio::time::advance(IDLE_DEATH_DETAIL_WAIT + Duration::from_millis(100)).await;
            reports
                .send(HarnessEvent::TransportClosed {
                    message: "agy exited (exit status: 7): agy: the session ended".into(),
                    retryable: false,
                    kind: TransportCloseKind::Exit,
                    reset_at: None,
                })
                .expect("queue the exit report");
            tokio::time::resume();

            let outcome = tokio::time::timeout(Duration::from_secs(3), waiting)
                .await
                .expect("the wait ends")
                .expect("the wait task did not panic");
            assert!(
                matches!(
                    outcome,
                    IdleWait::Event(Some(HarnessEvent::TransportClosed {
                        kind: TransportCloseKind::Exit,
                        ..
                    }))
                ),
                "round {round}: the ready exit report beats the expired deadline: {outcome:?}"
            );
        }
    }

    /// A harness stream that reports when its reader has parked.
    ///
    /// A test queues events only after that signal, so a report and an expired
    /// deadline can be made ready at the same poll of a waiting select.
    struct ParkedStream {
        events: tokio::sync::mpsc::UnboundedReceiver<HarnessEvent>,
        parked: Arc<tokio::sync::Notify>,
    }

    impl futures::Stream for ParkedStream {
        type Item = HarnessEvent;

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<HarnessEvent>> {
            let this = self.get_mut();
            match this.events.poll_recv(cx) {
                std::task::Poll::Pending => {
                    // Everything queued was read; only the test can make the
                    // stream ready from here.
                    this.parked.notify_one();
                    std::task::Poll::Pending
                }
                ready => ready,
            }
        }
    }

    /// A closed-completion wait prefers a queued exit report over its deadline.
    ///
    /// The wait's select must poll the stream first when both are ready at the
    /// same poll; otherwise the window expires without the exit report's status
    /// and stderr, and the turn's one row keeps the generic close notice. The
    /// rounds repeat the same ready-together poll, so a select without
    /// `biased;` loses the race reliably instead of one time in two.
    #[tokio::test]
    async fn a_closed_completion_report_wins_over_an_expired_deadline() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let host = HostId::new("host_test");
        let manager = open_test_manager(FakeHarness::new(), host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(10), next_turn_status(&mut events))
                .await
                .expect("the turn completed"),
            TurnStatus::Completed
        );

        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (session, generation, old_pump) = {
            let mut record = handle.lock().await;
            (
                record.live_session().cloned().expect("a live session"),
                record.session.generation().expect("a generation"),
                record.pump.take().expect("this session's pump"),
            )
        };
        // The session's own pump must not read this stream first.
        old_pump.abort();
        let _ = old_pump.await;

        for round in 0..64 {
            let (reports, queued) = tokio::sync::mpsc::unbounded_channel();
            let parked = Arc::new(tokio::sync::Notify::new());
            let mut stream: futures::stream::BoxStream<'static, HarnessEvent> =
                Box::pin(ParkedStream {
                    events: queued,
                    parked: Arc::clone(&parked),
                });
            let inner = Arc::clone(&manager.inner);
            let handle = handle.clone();
            let session = session.clone();
            let waiting = tokio::spawn(async move {
                let mut reports_out = Vec::new();
                let mut later = Vec::new();
                inner
                    .collect_closed_completion_report(
                        &handle,
                        generation,
                        &session,
                        &mut stream,
                        &mut reports_out,
                        &mut later,
                    )
                    .await;
                (reports_out, later)
            });
            // The wait is parked with the deadline not yet expired.
            parked.notified().await;
            tokio::time::pause();
            tokio::time::advance(IDLE_DEATH_DETAIL_WAIT + Duration::from_millis(100)).await;
            // One harmless event makes the wait poll again, re-arming the
            // expired deadline; the exit report is queued before that poll.
            reports
                .send(HarnessEvent::Usage {
                    usage: Usage::default(),
                })
                .expect("queue an event that makes the wait poll again");
            reports
                .send(HarnessEvent::TransportClosed {
                    message: "agy exited (exit status: 7): agy: the session ended".into(),
                    retryable: false,
                    kind: TransportCloseKind::Exit,
                    reset_at: None,
                })
                .expect("queue the exit report");
            tokio::time::resume();

            let (reports, later) = tokio::time::timeout(Duration::from_secs(3), waiting)
                .await
                .expect("the wait ends")
                .expect("the wait task did not panic");
            assert!(
                matches!(
                    reports.first(),
                    Some(HarnessEvent::TransportClosed {
                        kind: TransportCloseKind::Exit,
                        ..
                    })
                ),
                "round {round}: the ready exit report beats the expired deadline: {reports:?}"
            );
            assert_eq!(
                later.len(),
                1,
                "round {round}: the buffered event waits for the completion"
            );
        }
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

    /// A text generation backend whose answer the test controls.
    struct TestTitles {
        result: Result<String, TextGenerationError>,
        started: Option<Arc<tokio::sync::Notify>>,
        release: Option<Arc<tokio::sync::Notify>>,
        enabled: bool,
        calls: Option<Arc<std::sync::atomic::AtomicUsize>>,
    }

    impl TestTitles {
        fn fixed(title: &str) -> Self {
            Self {
                result: Ok(title.to_owned()),
                started: None,
                release: None,
                enabled: true,
                calls: None,
            }
        }

        fn failed(error: TextGenerationError) -> Self {
            Self {
                result: Err(error),
                started: None,
                release: None,
                enabled: true,
                calls: None,
            }
        }

        /// A backend whose mode is Off, counting any call it should never get.
        fn off() -> (Self, Arc<std::sync::atomic::AtomicUsize>) {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                Self {
                    result: Err(TextGenerationError::Disabled),
                    started: None,
                    release: None,
                    enabled: false,
                    calls: Some(Arc::clone(&calls)),
                },
                calls,
            )
        }

        fn gated(title: &str) -> (Self, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
            let started = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            (
                Self {
                    result: Ok(title.to_owned()),
                    started: Some(Arc::clone(&started)),
                    release: Some(Arc::clone(&release)),
                    enabled: true,
                    calls: None,
                },
                started,
                release,
            )
        }
    }

    #[async_trait::async_trait]
    impl TextGeneration for TestTitles {
        async fn titles_enabled(&self) -> bool {
            self.enabled
        }

        async fn thread_title(
            &self,
            _first_prompt: &str,
            _workspace: &str,
            _harness: HarnessKind,
        ) -> Result<String, TextGenerationError> {
            if let Some(calls) = &self.calls {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            if let Some(started) = &self.started {
                started.notify_one();
            }
            if let Some(release) = &self.release {
                release.notified().await;
            }
            self.result.clone()
        }
    }

    #[tokio::test]
    async fn off_starts_no_title_job_and_never_calls_the_backend() {
        let workspace = tempfile::tempdir().expect("workspace");
        let manager = open_test_manager(FakeHarness::new(), HostId::new("host_title")).await;
        let (backend, calls) = TestTitles::off();
        manager.set_text_generation(Arc::new(backend));
        let created = manager
            .create_thread(new_thread(
                HostId::new("host_title"),
                workspace.path(),
                "Keep the first line",
            ))
            .await
            .expect("create");
        manager.inner.wait_for_titles().await;
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "Off must not even attempt a title"
        );
        assert!(
            manager
                .inner
                .title_jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty(),
            "Off registers no job"
        );
        let meta = manager
            .thread_meta(&created.thread_id)
            .await
            .expect("thread");
        assert_eq!(meta.title, "Keep the first line");
        assert_eq!(meta.title_source, TitleSource::Prompt);
    }

    #[tokio::test]
    async fn a_generated_title_replaces_the_first_prompt_title() {
        let workspace = tempfile::tempdir().expect("workspace");
        let manager = open_test_manager(FakeHarness::new(), HostId::new("host_title")).await;
        manager.set_text_generation(Arc::new(TestTitles::fixed("Generated title")));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(
                HostId::new("host_title"),
                workspace.path(),
                "Fix the login form",
            ))
            .await
            .expect("create");
        assert_eq!(created.summary.title, "Fix the login form");
        manager.inner.wait_for_titles().await;

        let meta = manager
            .thread_meta(&created.thread_id)
            .await
            .expect("thread");
        assert_eq!(meta.title, "Generated title");
        assert_eq!(meta.title_source, TitleSource::Generated);

        let mut saw_generated = false;
        while let Ok(wire) = events.try_recv() {
            if let Event::ThreadUpserted(summary) = wire.event
                && summary.thread_id == created.thread_id
                && summary.title == "Generated title"
            {
                saw_generated = true;
            }
        }
        assert!(saw_generated, "the generated title is a normal upsert");
    }

    #[tokio::test]
    async fn a_failed_or_timed_out_title_keeps_the_fallback() {
        let workspace = tempfile::tempdir().expect("workspace");
        let manager = open_test_manager(FakeHarness::new(), HostId::new("host_title")).await;
        manager.set_text_generation(Arc::new(TestTitles::failed(TextGenerationError::Timeout)));
        let created = manager
            .create_thread(new_thread(
                HostId::new("host_title"),
                workspace.path(),
                "Keep this title",
            ))
            .await
            .expect("create");
        manager.inner.wait_for_titles().await;
        let meta = manager
            .thread_meta(&created.thread_id)
            .await
            .expect("thread");
        assert_eq!(meta.title, "Keep this title");
        assert_eq!(meta.title_source, TitleSource::Prompt);
    }

    #[tokio::test]
    async fn a_rename_during_generation_wins() {
        let workspace = tempfile::tempdir().expect("workspace");
        let manager = open_test_manager(FakeHarness::new(), HostId::new("host_title")).await;
        let (backend, started, release) = TestTitles::gated("Generated title");
        manager.set_text_generation(Arc::new(backend));
        let created = manager
            .create_thread(new_thread(
                HostId::new("host_title"),
                workspace.path(),
                "Fallback title",
            ))
            .await
            .expect("create");

        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .expect("the title job started");
        let renamed = manager
            .rename_thread(&created.thread_id, "My own name")
            .await
            .expect("rename");
        assert_eq!(renamed.title, "My own name");
        release.notify_one();
        manager.inner.wait_for_titles().await;

        let meta = manager
            .thread_meta(&created.thread_id)
            .await
            .expect("thread");
        assert_eq!(meta.title, "My own name");
        assert_eq!(meta.title_source, TitleSource::Renamed);
    }

    #[tokio::test]
    async fn a_rename_must_be_one_non_empty_line() {
        let workspace = tempfile::tempdir().expect("workspace");
        let manager = open_test_manager(FakeHarness::new(), HostId::new("host_title")).await;
        let created = manager
            .create_thread(new_thread(
                HostId::new("host_title"),
                workspace.path(),
                "Fallback title",
            ))
            .await
            .expect("create");
        assert!(
            manager
                .rename_thread(&created.thread_id, "  ")
                .await
                .is_err()
        );
        assert!(
            manager
                .rename_thread(&created.thread_id, "two\nlines")
                .await
                .is_err()
        );
        assert_eq!(
            manager
                .rename_thread(&created.thread_id, "  One line  ")
                .await
                .expect("rename")
                .title,
            "One line"
        );
    }

    #[tokio::test]
    async fn a_subagent_child_keeps_the_title_its_task_gave_it() {
        let workspace = tempfile::tempdir().expect("workspace");
        let manager = open_test_manager(FakeHarness::new(), HostId::new("host_title")).await;
        manager.set_text_generation(Arc::new(TestTitles::fixed("Should not appear")));
        let mut request = new_thread(
            HostId::new("host_title"),
            workspace.path(),
            "Review the diff",
        );
        request.parent_thread_id = Some(ThreadId::new("th_parent"));
        request.subagent_id = Some(SubagentId::new("sub_1"));
        let created = manager.create_thread(request).await.expect("create");
        // No job was started, so there is nothing to wait for; the prompt is
        // the title its task gave it.
        let meta = manager
            .thread_meta(&created.thread_id)
            .await
            .expect("thread");
        assert_eq!(meta.title, "Review the diff");
        assert_eq!(meta.title_source, TitleSource::Prompt);
    }

    #[test]
    fn item_ids_are_stable_for_a_turn() {
        let turn = TurnId::new("turn_abc");
        assert_eq!(user_item_id(&turn), "item_user_abc");
        assert_eq!(assistant_item_id(&turn), "item_assistant_abc");
        assert!(fresh_item_id("user").starts_with("item_user_"));
    }

    /// The pump must apply events without the session lock.
    ///
    /// `send` holds that lock while the harness streams; the session's event
    /// queue is bounded like a real adapter's, so a pump that stopped draining
    /// (for example to wait for that lock) would leave the queue full and the
    /// write would never return.
    #[tokio::test]
    async fn the_pump_drains_events_while_a_send_holds_the_session() {
        /// Far more events than the session's queue holds.
        const DELTAS: usize = 1_100;
        /// The back-pressure a production transport applies.
        const EVENT_QUEUE: usize = 64;
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, send_done) = FakeHarness::new().with_streaming_send(DELTAS);
        let harness = harness.with_bounded_event_queue(EVENT_QUEUE);
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");

        // The fake's send has not returned (the gate is closed): a pump that
        // waited for the session lock could publish nothing here.
        let mut assistant = String::new();
        let completed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let wire = next_event(&mut events).await;
                match wire.event {
                    Event::ItemCompleted {
                        item:
                            Item::Message {
                                role: MessageRole::Assistant,
                                text,
                                ..
                            },
                        ..
                    } => assistant = text,
                    Event::TurnCompleted {
                        turn_id, status, ..
                    } => return (turn_id, status),
                    _ => {}
                }
            }
        })
        .await
        .expect("the pump applied the whole stream while send held the session");
        assert_eq!(completed.0, created.turn_id);
        assert_eq!(completed.1, TurnStatus::Completed);
        assert_eq!(
            assistant.chars().count(),
            DELTAS,
            "every streamed delta reached the turn"
        );

        send_done.release();
        let _ = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await;
    }

    /// A startup that reached the lock after its turn was stopped starts no
    /// session at all: it has nothing to carry, and a failure of it must never
    /// be blamed on the turn that arrived next.
    #[tokio::test]
    async fn a_startup_with_no_turn_starts_no_session_and_leaves_the_next_turn_its_own() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_start_gate();
        let harness = harness.with_start_failure_once("the harness could not start");
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
            "the thread's own startup is in flight"
        );

        // Stop the only turn while that startup is still booting, then let the
        // startup fail: its captured turn is already gone.
        manager
            .interrupt(&created.thread_id)
            .await
            .expect("stop the first turn");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Interrupted);
        gate.release();
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        {
            let handle = manager
                .inner
                .handle(&created.thread_id)
                .await
                .expect("thread handle");
            let record = handle.lock().await;
            assert!(
                record.active.is_none(),
                "the failed startup did not resurrect the stopped turn"
            );
        }
        assert_eq!(
            sessions.get(),
            1,
            "the failed startup never created a session"
        );

        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        // The state a Stop between spawn and capture leaves: no turn to carry
        // and no session. The startup that was spawned then must start nothing.
        manager.inner.spawn_session(handle.clone());
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sessions.get(),
            1,
            "a startup with no captured turn starts no session"
        );
        {
            let record = handle.lock().await;
            assert!(
                matches!(record.session, SessionSlot::None),
                "the slot is left free for the next turn's own startup"
            );
        }

        // The next message gets its own startup, and that one succeeds.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_eq!(turn, second.turn_id, "the new message runs its own turn");
        assert_eq!(
            status,
            TurnStatus::Completed,
            "a startup with no turn cannot fail the next one"
        );
        assert_eq!(sessions.get(), 2, "the next turn started its own session");
    }

    /// A diagnostic the bridge buffered before the writer died is its own row,
    /// and it must not stand in for the transport failure that follows it.
    #[tokio::test]
    async fn a_buffered_diagnostic_does_not_hide_the_transport_failure() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, death) = FakeHarness::new()
            .with_dead_transport_after(1)
            .with_buffered_diagnostic("an unrelated ACP diagnostic");
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        // The second write parks its diagnostic and then holds the session
        // lock until the test has seen that row.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        let mut errors = Vec::new();
        loop {
            let wire = next_event(&mut events).await;
            if let Event::Error { message, .. } = wire.event {
                errors.push(message);
                break;
            }
        }
        assert_eq!(errors, ["an unrelated ACP diagnostic"]);

        // Now let the writer die. Its tagged row is the failure; the write
        // failure's row for the same death is the one that collapses.
        death.release();
        loop {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::Error { message, .. } => errors.push(message),
                Event::TurnCompleted {
                    turn_id, status, ..
                } => {
                    assert_eq!(turn_id, second.turn_id);
                    assert_eq!(status, TurnStatus::Failed);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(
            errors.len(),
            2,
            "the diagnostic and the real failure are both rows: {errors:?}"
        );
        assert!(
            errors[1].contains("transport"),
            "the real transport failure was not hidden: {errors:?}"
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
            "the turn ended once"
        );
    }

    /// The bridge's generic stdout-close notice reaches the pump before the
    /// prompt failure that carries the agent's stderr. They are one row, and it
    /// keeps the stderr rather than the generic notice's bare text.
    #[tokio::test]
    async fn the_generic_stdout_close_keeps_the_stderr_failure_detail() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, failure) = FakeHarness::new()
            .with_dead_transport_after(1)
            .with_stderr_tail("agy: the session died mid-turn")
            .with_generic_death_gate();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        // The generic notice is consumed first and held for the turn: no row is
        // published for it on its own.
        assert!(
            wait_until(|| handle
                .try_lock()
                .is_ok_and(|record| record.transport_report.is_some()))
            .await,
            "the pump consumed the generic stdout-close notice"
        );

        // Now the failed write reports the same death with the stderr detail.
        failure.release();
        let mut errors = Vec::new();
        loop {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::Error { message, .. } => errors.push(message),
                Event::TurnCompleted {
                    turn_id, status, ..
                } => {
                    assert_eq!(turn_id, second.turn_id);
                    assert_eq!(status, TurnStatus::Failed);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(
            errors.len(),
            1,
            "one row for one dead transport: {errors:?}"
        );
        assert!(
            errors[0].contains("agy: the session died mid-turn"),
            "the row keeps the stderr the generic notice lacks: {errors:?}"
        );
    }

    /// An agent that closes its stdout between turns and then exits reports the
    /// same idle death twice: the generic close notice first, the exit status
    /// and its stderr second. They are one row, and it keeps the stderr.
    ///
    /// The session (and so its event sender) stays alive, exactly like a real
    /// adapter's: the stream never ends, so the row must come from the exit
    /// report itself.
    #[tokio::test]
    async fn an_idle_close_and_exit_merge_into_one_row_with_the_stderr() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        // Between turns: the agent's stdout closes, then its exit report lands
        // with the stderr the close notice could not carry.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the harness transport closed".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "agy exited (exited: 1): agy: the session ended".into(),
            retryable: false,
            kind: TransportCloseKind::Exit,
            reset_at: None,
        });

        let row = next_error(&mut events).await;
        assert!(
            row.contains("agy exited (exited: 1): agy: the session ended"),
            "the row is the exit report with the stderr: {row:?}"
        );
        assert!(
            row.contains("agy: the session ended"),
            "the row keeps the stderr the generic close lacked: {row:?}"
        );
        let duplicate = tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                let wire = next_event(&mut events).await;
                if let Event::Error { message, .. } = wire.event {
                    return message;
                }
            }
        })
        .await;
        assert!(
            duplicate.is_err(),
            "one idle death is one row, not two: {duplicate:?}"
        );
    }

    /// A death between turns is published from the exit report itself: a live
    /// adapter holds its session, and with it the event sender, so the stream
    /// does not end while the daemon owns it. The row must arrive on its own (no
    /// Send) and the dead session must be detached, so the next Send starts
    /// fresh.
    #[tokio::test]
    async fn a_between_turns_exit_report_publishes_without_a_stream_end() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, shutdowns) = harness.with_shutdown_count();
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
        let (session, weak) = {
            let record = handle.lock().await;
            let session = record.live_session().cloned().expect("a live session");
            let weak = Arc::downgrade(&session);
            (session, weak)
        };
        drop(session);

        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the harness transport closed".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "agy exited (exited: 1): agy: the session ended".into(),
            retryable: false,
            kind: TransportCloseKind::Exit,
            reset_at: None,
        });

        let row = tokio::time::timeout(Duration::from_secs(3), next_error(&mut events))
            .await
            .expect("the exit report publishes the row by itself");
        assert!(
            row.contains("agy exited (exited: 1): agy: the session ended"),
            "the row is the exit report with the stderr: {row:?}"
        );
        assert!(
            wait_until(|| handle
                .try_lock()
                .is_ok_and(
                    |record| matches!(record.session, SessionSlot::None) && record.pump.is_none()
                ))
            .await,
            "the dead session is detached and its pump is gone"
        );
        assert!(
            wait_until(|| shutdowns.get() == 1).await,
            "the dead session was shut down"
        );
        assert!(
            wait_until(|| weak.upgrade().is_none()).await,
            "the pump dropped the dead session"
        );
    }

    /// A generic stdout-close notice is all the adapter reports when it never
    /// reaps the agent: the row waits for the bounded window the exit report
    /// gets, then publishes the notice alone instead of waiting for a stream end
    /// that never comes.
    #[tokio::test]
    async fn a_generic_between_turns_close_publishes_after_the_fallback() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the harness transport closed".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        // The exit report still has its window: publishing the generic notice
        // at once would take the row the exit status and stderr belong in.
        assert!(
            tokio::time::timeout(Duration::from_millis(150), next_error(&mut events))
                .await
                .is_err(),
            "the generic close waits for the exit report"
        );
        let row = tokio::time::timeout(Duration::from_secs(3), next_error(&mut events))
            .await
            .expect("the bounded window publishes the generic close");
        assert_eq!(row, "the harness transport closed");

        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        assert!(
            wait_until(|| handle
                .try_lock()
                .is_ok_and(
                    |record| matches!(record.session, SessionSlot::None) && record.pump.is_none()
                ))
            .await,
            "the dead session is detached and its pump is gone"
        );
    }

    /// A completion that ends a turn on a closed transport also ends the pump:
    /// the session it holds keeps its stream open, so the pump settles the turn,
    /// drops the session and returns instead of waiting for that stream to end.
    #[tokio::test]
    async fn a_closed_transport_completion_ends_the_pump_and_drops_its_session() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, completion) = harness.with_later_completion_gate();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        // The second turn is running on the live session; its completion is
        // withheld until the gate opens.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (session, pump) = {
            let mut record = handle.lock().await;
            assert!(
                record.live_session().is_some(),
                "the second turn is on the live session"
            );
            (
                record.live_session().cloned().expect("a live session"),
                record.pump.take().expect("this session's pump"),
            )
        };
        let weak = Arc::downgrade(&session);

        // The agent's transport closes under the turn (a write deadline killed
        // it) and the adapter reports the turn's failed completion last.
        session
            .lock()
            .await
            .shutdown()
            .await
            .expect("close the fake session");
        drop(session);
        harness_events.emit(HarnessEvent::TurnCompleted {
            status: TurnStatus::Failed,
            usage: Usage::default(),
        });

        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("the pump ends instead of waiting for a stream end")
            .expect("the pump task did not panic");
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Failed);
        assert!(
            wait_until(|| weak.upgrade().is_none()).await,
            "the pump dropped the dead session"
        );
        let record = handle.lock().await;
        assert!(
            record.session.live().is_none(),
            "the dead session is detached"
        );
        assert!(record.pump.is_none(), "the record holds no pump");
        // The withheld completion must not arrive as a second turn end.
        completion.release();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(150),
                next_turn_completion(&mut events)
            )
            .await
            .is_err(),
            "the turn ended exactly once"
        );
    }

    /// A turn that ends with `end_turn` on a closed transport still takes the
    /// rate limit its exit report carries: the schedule is armed before the
    /// completion settles, so the message queued behind the turn waits for it
    /// instead of starting at once on a fresh agent.
    #[tokio::test]
    async fn a_closed_completion_arms_the_exit_reports_limit_before_promoting() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(5);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, completion) = harness.with_later_completion_gate();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        manager
            .send_message(&created.thread_id, "third".into(), Vec::new())
            .await
            .expect("queue third");

        // The transport dies under the running turn; its pump is taken so the
        // test can await the end the completion causes.
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let (session, pump) = {
            let mut record = handle.lock().await;
            (
                record.live_session().cloned().expect("a live session"),
                record.pump.take().expect("this session's pump"),
            )
        };
        session
            .lock()
            .await
            .shutdown()
            .await
            .expect("close the fake session");
        drop(session);

        // The agent answered `end_turn` and only then exited; the reset is on
        // the exit report, which follows the completion on the same stream.
        harness_events.emit(HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Usage::default(),
        });
        harness_events.emit(HarnessEvent::LimitHit {
            reset_at: Some(reset_at),
            auto_resume: true,
        });

        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("the pump ends with the completion")
            .expect("the pump task did not panic");
        let (turn, status) = next_turn_completion(&mut events).await;
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed, "the agent ended the turn");

        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(
            schedules.len(),
            1,
            "the exit report's limit armed one schedule"
        );
        assert_eq!(schedules[0].thread_id, created.thread_id);
        let summary = manager
            .thread_summary(&created.thread_id)
            .await
            .expect("summary");
        assert_eq!(
            summary.agent_state,
            AgentState::Scheduled,
            "the thread waits for its armed resume"
        );

        // The limit arrived while the turn still owned the record, so the
        // completion could not promote the queued message.
        let started = tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                let wire = next_event(&mut events).await;
                if let Event::TurnStarted { turn_id, .. } = wire.event {
                    return turn_id;
                }
            }
        })
        .await;
        assert!(
            started.is_err(),
            "queued work does not start past the exit report's limit: {started:?}"
        );
        // A withheld completion from the dead session must not arrive as a
        // second turn end either.
        completion.release();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(150),
                next_turn_completion(&mut events)
            )
            .await
            .is_err(),
            "the turn ended exactly once"
        );
    }

    /// An idle death whose exit report carries a rate limit keeps the thread
    /// Scheduled: the diagnostic row is published without raising a failure,
    /// exactly as main published a between-turns row without a lifecycle
    /// transition, so the armed resume is not turned into an Error asking for
    /// attention.
    #[tokio::test]
    async fn an_idle_limit_exit_keeps_the_thread_scheduled() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(5);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        // Between turns: stdout closes, and the only report that follows is
        // the exit's rate limit, while the session (and its sender) stays
        // alive.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        harness_events.emit(HarnessEvent::LimitHit {
            reset_at: Some(reset_at),
            auto_resume: true,
        });

        let row = tokio::time::timeout(Duration::from_secs(3), next_error(&mut events))
            .await
            .expect("the fallback publishes the idle row");
        assert_eq!(row, "the ACP agent stdout closed");

        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(
            schedules.len(),
            1,
            "the exit report's limit armed one schedule"
        );
        assert_eq!(schedules[0].thread_id, created.thread_id);
        let summary = manager
            .thread_summary(&created.thread_id)
            .await
            .expect("summary");
        assert_eq!(
            summary.agent_state,
            AgentState::Scheduled,
            "the idle diagnostic did not turn the armed resume into an Error"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(250), next_error(&mut events))
                .await
                .is_err(),
            "one idle death is one diagnostic row"
        );
    }

    /// A Send that lands while an idle session's death is still waiting for its
    /// exit report must start a fresh agent instead of claiming the dead one.
    ///
    /// The read failure detaches the session from the slot at once, so the
    /// prompt reaches a new session and completes. The old pump keeps reading
    /// its own stream: the exit report still publishes the death's one row and
    /// arms the reset it carries.
    #[tokio::test]
    async fn a_send_during_an_idle_read_failure_starts_a_fresh_session() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(9);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let (harness, shutdowns) = harness.with_shutdown_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        let status = tokio::time::timeout(Duration::from_secs(10), next_turn_status(&mut events))
            .await
            .expect("the first turn completed");
        assert_eq!(status, TurnStatus::Completed);

        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        // The fallback deadline is a timer, so hold the clock still: the read
        // failure must publish nothing until its exit report arrives, whatever
        // the wall clock does, and the test releases the clock to let the rest
        // of the window run.
        let mut clock = HeldClock::hold();
        let dying_notify = handle.lock().await.dying_notify.clone();
        // The reader dies between turns. Its session is out of the slot at
        // once; the pump keeps it for the exit report that can sharpen the row.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "failed to read the ACP agent stdout: broken pipe".into(),
            retryable: false,
            kind: TransportCloseKind::Read,
            reset_at: None,
        });
        dying_notify.notified().await;
        {
            let record = handle.lock().await;
            assert!(
                matches!(record.session, SessionSlot::None),
                "the dead session left the slot before its exit report"
            );
            assert!(
                record.dying_sessions.contains_key(&record.session_gen),
                "the pump keeps the death for its exit report"
            );
        }
        assert!(
            queued_error(&mut events).is_none(),
            "the read failure waits for the exit report before it publishes a row"
        );

        // A Send in that window must start a fresh agent, not write to the dead
        // one. Its turn completes on the new session.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("the send is accepted");
        sends.wait_for(2).await;
        assert_eq!(
            sessions.get(),
            2,
            "the Send started a fresh session instead of reusing the dead one"
        );

        // The old session's exit report now arrives on its own stream, while
        // the fresh turn is still running. It is still the dead generation's
        // row and reset, not the fresh turn's.
        harness_events.emit_to(
            0,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 7): agy: the session ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(reset_at),
            },
        );
        clock.release();

        let mut completion = None;
        let mut row = None;
        while completion.is_none() || row.is_none() {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::Error { message, .. } => row = Some(message),
                Event::TurnCompleted {
                    turn_id, status, ..
                } if turn_id == second.turn_id => completion = Some(status),
                _ => {}
            }
        }
        assert_eq!(
            completion,
            Some(TurnStatus::Completed),
            "the prompt was written to a live session"
        );
        let row = row.expect("the exit report publishes the idle death's row");
        assert!(
            row.contains("agy exited (exit status: 7): agy: the session ended"),
            "the row is the exit report with its stderr: {row:?}"
        );
        assert!(
            queued_error(&mut events).is_none(),
            "the death is one diagnostic row"
        );
        shutdowns.wait_for(1).await;

        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(
            schedules.len(),
            1,
            "the exit report armed exactly one reset"
        );
        assert!(
            schedules[0].reason.contains("provider reset at"),
            "the schedule is armed from the exit report's reset: {:?}",
            schedules[0].reason
        );
        assert!(
            schedules[0].resume_at >= reset_at,
            "the schedule waits for the exit report's reset: {:?}",
            schedules[0].resume_at
        );
        let summary = manager
            .thread_summary(&created.thread_id)
            .await
            .expect("summary");
        assert_eq!(
            summary.agent_state,
            AgentState::Scheduled,
            "the armed reset keeps the thread scheduled"
        );
    }

    /// Two idle deaths in a row keep their own rows and resets.
    ///
    /// The first dead session can be replaced by a Send before its exit report
    /// arrives, and the replacement can die too before it. One pending slot let
    /// the second death overwrite the first, losing that death's row and reset;
    /// every death now belongs to its own generation.
    #[tokio::test]
    async fn two_idle_deaths_keep_their_own_rows_and_resets() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let first_reset = chrono::Utc::now() + chrono::Duration::minutes(6);
        let second_reset = chrono::Utc::now() + chrono::Duration::minutes(13);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
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
        let dying_notify = handle.lock().await.dying_notify.clone();
        // Both exit reports arrive by hand. Hold the mocked clock for the whole
        // test: a fallback deadline that auto-advanced would publish the first
        // death before the second one's report, which is exactly the ordering
        // this test must not depend on.
        let mut clock = HeldClock::hold();

        // G dies idle. Nothing is published until its exit report arrives.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        dying_notify.notified().await;

        // A Send starts G+1, and its turn completes.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;
        assert_eq!(sessions.get(), 2, "the Send replaced the dead session");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the fresh session ran the turn");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);

        // G+1 dies idle too, before G's exit report has arrived.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed again".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        dying_notify.notified().await;

        // G's exit report arrives now. It is still G's row and G's reset.
        harness_events.emit_to(
            0,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 7): the first agent ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(first_reset),
            },
        );
        let row = tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the first death's exit report publishes its row");
        assert!(
            row.contains("the first agent ended"),
            "the row is the first death's exit report: {row:?}"
        );
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(schedules.len(), 1, "the first death armed its reset");
        assert!(
            schedules[0].resume_at >= first_reset,
            "the schedule waits for the first death's reset: {:?}",
            schedules[0].resume_at
        );

        // G+1's exit report is a second, separate row and reset.
        harness_events.emit_to(
            1,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 9): the second agent ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(second_reset),
            },
        );
        let row = tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the second death's exit report publishes its row");
        assert!(
            row.contains("the second agent ended"),
            "the second death has its own row: {row:?}"
        );
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(schedules.len(), 1, "one thread keeps one schedule");
        assert!(
            schedules[0].resume_at >= second_reset,
            "the newer reset replaced the older pause: {:?}",
            schedules[0].resume_at
        );
        assert_eq!(
            schedules[0].attempt, 1,
            "the second pause is a new attempt, not the first one re-armed"
        );
        clock.release();
    }

    /// An older dead generation's exit report must not replace a newer one's
    /// schedule when the two arrive out of order.
    ///
    /// G+1 can report a later reset (13 minutes) before G's delayed report
    /// delivers an earlier one (6 minutes). Both deaths publish their own row,
    /// but the thread must keep waiting on G+1's reset; G's earlier report may
    /// not pull the resume forward.
    #[tokio::test]
    async fn an_older_death_does_not_replace_a_newer_generations_schedule() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let first_reset = chrono::Utc::now() + chrono::Duration::minutes(6);
        let second_reset = chrono::Utc::now() + chrono::Duration::minutes(13);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
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
        let dying_notify = handle.lock().await.dying_notify.clone();
        // Both exit reports are delivered by hand, older generation last.
        let mut clock = HeldClock::hold();

        // G dies idle and waits for its exit report.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        dying_notify.notified().await;

        // A Send starts G+1; its turn completes and G+1 dies idle too.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;
        assert_eq!(sessions.get(), 2, "the Send replaced the dead session");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the fresh session ran the turn");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed again".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        dying_notify.notified().await;

        // The newer generation's report arrives first and arms its reset.
        harness_events.emit_to(
            1,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 9): the second agent ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(second_reset),
            },
        );
        let row = tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the newer death's exit report publishes its row");
        assert!(
            row.contains("the second agent ended"),
            "the newer death has its own row: {row:?}"
        );

        // The older generation's delayed report now arrives with an earlier
        // reset. It is still its own row, but not the thread's pause.
        harness_events.emit_to(
            0,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 7): the first agent ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(first_reset),
            },
        );
        let row = tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the older death's exit report still publishes its row");
        assert!(
            row.contains("the first agent ended"),
            "the older death keeps its own row: {row:?}"
        );
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(schedules.len(), 1, "one thread keeps one schedule");
        assert!(
            schedules[0].resume_at >= second_reset,
            "the older report pulled the resume before the newer reset: {:?}",
            schedules[0].resume_at
        );
        assert_eq!(
            schedules[0].attempt, 0,
            "the older report re-armed the newer generation's pause"
        );
        clock.release();
    }

    /// A duplicate reset on an idle exit report must not undo a cancelled
    /// resume.
    ///
    /// Between turns the bridge can report one provider pause twice: the typed
    /// limit before the exit report, then the same reset on it. The person can
    /// cancel the resume in between; arming the repeated reset would put the
    /// pause back. The death's own record knows the reset was already applied,
    /// even though the live schedule is gone.
    #[tokio::test]
    async fn a_duplicate_idle_exit_reset_does_not_rearm_a_cancelled_resume() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(15);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
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
        let dying_notify = handle.lock().await.dying_notify.clone();
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        dying_notify.notified().await;

        // The typed limit arrives while the death waits for its exit report,
        // and arms its schedule.
        harness_events.emit(HarnessEvent::LimitHit {
            reset_at: Some(reset_at),
            auto_resume: true,
        });
        loop {
            let wire = tokio::time::timeout(Duration::from_secs(10), next_event(&mut events))
                .await
                .expect("the limit row arrived");
            if matches!(wire.event, Event::UsageLimitHit { .. }) {
                break;
            }
        }
        assert!(
            meter
                .schedules()
                .await
                .expect("read schedules")
                .iter()
                .any(|schedule| matches!(
                    schedule.state,
                    ResumeState::Scheduled | ResumeState::Waiting
                )),
            "the typed limit armed the resume"
        );
        manager
            .cancel_resume(&created.thread_id)
            .await
            .expect("cancel the armed resume");

        // The exit report repeats the same reset. It must not arm it again.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "agy exited (exit status: 7): agy: the session ended".into(),
            retryable: false,
            kind: TransportCloseKind::Exit,
            reset_at: Some(reset_at),
        });
        let row = tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the exit report publishes the idle death's row");
        assert!(
            row.contains("agy exited (exit status: 7): agy: the session ended"),
            "the row is the exit report with its stderr: {row:?}"
        );
        assert!(
            !meter
                .schedules()
                .await
                .expect("read schedules")
                .iter()
                .any(|schedule| matches!(
                    schedule.state,
                    ResumeState::Scheduled | ResumeState::Waiting
                )),
            "the duplicated exit reset re-armed a resume the person cancelled"
        );
        let summary = manager
            .thread_summary(&created.thread_id)
            .await
            .expect("summary");
        assert_ne!(
            summary.agent_state,
            AgentState::Scheduled,
            "the cancelled resume left the thread scheduled"
        );
    }

    /// A cancelled resume stays cancelled even when the idle exit report is the
    /// first death report after the limit.
    ///
    /// The bridge sends its typed limit and then the exit with the same reset,
    /// with no EOF or read error before them. The session is still live when
    /// the limit arms, so only the live slot can remember what that generation
    /// applied; the exit then opens the dying entry and must inherit it instead
    /// of arming the pause the person cancelled in the window.
    #[tokio::test]
    async fn a_cancelled_resume_stays_cancelled_when_only_the_exit_repeats_the_reset() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(15);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        // The typed limit arms while the session is still live and idle.
        harness_events.emit(HarnessEvent::LimitHit {
            reset_at: Some(reset_at),
            auto_resume: true,
        });
        loop {
            let wire = tokio::time::timeout(Duration::from_secs(10), next_event(&mut events))
                .await
                .expect("the limit row arrived");
            if matches!(wire.event, Event::UsageLimitHit { .. }) {
                break;
            }
        }
        assert!(
            meter
                .schedules()
                .await
                .expect("read schedules")
                .iter()
                .any(|schedule| matches!(
                    schedule.state,
                    ResumeState::Scheduled | ResumeState::Waiting
                )),
            "the typed limit armed the resume"
        );
        manager
            .cancel_resume(&created.thread_id)
            .await
            .expect("cancel the armed resume");

        // The exit report repeats the same reset with no EOF before it. The
        // dying entry must carry the live slot's applied reset.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "agy exited (exit status: 7): agy: the session ended".into(),
            retryable: false,
            kind: TransportCloseKind::Exit,
            reset_at: Some(reset_at),
        });
        let row = tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the exit report publishes the idle death's row");
        assert!(
            row.contains("agy exited (exit status: 7): agy: the session ended"),
            "the row is the exit report with its stderr: {row:?}"
        );
        assert!(
            !meter
                .schedules()
                .await
                .expect("read schedules")
                .iter()
                .any(|schedule| matches!(
                    schedule.state,
                    ResumeState::Scheduled | ResumeState::Waiting
                )),
            "the exit-only report re-armed a resume the person cancelled"
        );
        let summary = manager
            .thread_summary(&created.thread_id)
            .await
            .expect("summary");
        assert_ne!(
            summary.agent_state,
            AgentState::Scheduled,
            "the cancelled resume left the thread scheduled"
        );
    }

    /// A Stop during a closed transport's exit wait keeps a reported completion.
    ///
    /// ACP can report the turn completed and only then exit. If the person
    /// stops the turn while the pump waits for that exit report, the success
    /// already reported must stay a completion instead of being rewritten as
    /// an interrupted turn.
    #[tokio::test]
    async fn a_stop_during_the_exit_wait_keeps_a_reported_completion() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, completion) = harness.with_later_completion_gate();
        let (harness, sends) = harness.with_send_count();
        // The cancel cannot reach the closed transport, so the Stop stays
        // recorded without ending the turn itself.
        let harness = harness.with_interrupt_failure_once();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let session = handle
            .lock()
            .await
            .live_session()
            .cloned()
            .expect("a live session");
        session
            .lock()
            .await
            .shutdown()
            .await
            .expect("close the fake session");
        drop(session);

        // The agent reports the turn completed, and the pump starts waiting for
        // the exit report that follows it.
        harness_events.emit(HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage: Usage::default(),
        });
        // The person stops the turn during that wait. The session is closed, so
        // the cancel is recorded as undeliverable and the turn stays claimed.
        manager
            .interrupt(&created.thread_id)
            .await
            .expect("the stop is accepted");
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "agy exited (exit status: 0)".into(),
            retryable: false,
            kind: TransportCloseKind::Exit,
            reset_at: None,
        });

        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the turn settled");
        assert_eq!(turn, second.turn_id);
        assert_eq!(
            status,
            TurnStatus::Completed,
            "a Stop during the exit wait rewrote a completed turn as interrupted"
        );
        completion.release();
    }

    /// A failed write whose text merely mentions a limit is an ordinary failed
    /// turn: the daemon arms a resume only for the adapter's typed signal, so
    /// another harness's "payload size limit exceeded" cannot pause a thread.
    #[tokio::test]
    async fn an_untyped_limit_word_in_a_write_error_arms_no_schedule() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let harness =
            FakeHarness::new().with_send_failure_after(1, "HTTP 413: payload size limit exceeded");
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        let mut errors = Vec::new();
        let mut limit_rows = 0;
        loop {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::Error { message, .. } => errors.push(message),
                Event::UsageLimitHit { .. } => limit_rows += 1,
                Event::TurnCompleted {
                    turn_id, status, ..
                } => {
                    assert_eq!(turn_id, second.turn_id);
                    assert_eq!(status, TurnStatus::Failed);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(
            limit_rows, 0,
            "no limit row is published before the turn completes"
        );
        assert!(
            errors.iter().any(|row| row.contains("payload size limit")),
            "the write error is the turn's ordinary row: {errors:?}"
        );
        assert!(
            meter.schedules().await.expect("read schedules").is_empty(),
            "untyped limit text armed no schedule"
        );
        manager.shutdown().await;
    }

    /// A write that lost its turn while it was parked must not arm a schedule
    /// when its typed limit arrives: shutdown already interrupted the turn, so
    /// a schedule persisted here would fire after a restart.
    #[tokio::test]
    async fn a_write_that_lost_its_turn_does_not_arm_a_schedule() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (harness, gate) = FakeHarness::new().with_send_gate();
        let (harness, sends) = harness.with_send_count();
        let harness = harness.with_limit_failure_after(
            1,
            "usage limit reached",
            Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
        );
        let host = HostId::new("host_test");
        let manager = Arc::new(open_test_manager(harness, host.clone()).await);
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        assert!(
            wait_until(|| sends.get() == 2).await,
            "the write reached the gated send"
        );

        // Shutdown settles and detaches the turn while the write is parked.
        let shutdown = tokio::spawn({
            let manager = Arc::clone(&manager);
            async move { manager.shutdown().await }
        });
        loop {
            let (turn, status) = next_turn_completion(&mut events).await;
            if turn == second.turn_id {
                assert_eq!(status, TurnStatus::Interrupted);
                break;
            }
        }

        // The parked write now fails with the adapter's typed limit. Its turn
        // is no longer active, so the limit is dropped with no schedule.
        gate.release();
        shutdown.await.expect("shutdown task");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            meter.schedules().await.expect("read schedules").is_empty(),
            "a write that lost its turn armed no schedule"
        );
    }

    /// A typed limit from a failed write is the row the user reads and the
    /// signal that pauses the thread: the message alone under a limit prefix,
    /// the schedule persisted, and the message queued behind it not started.
    #[tokio::test]
    async fn a_typed_limit_from_a_failed_write_holds_the_queue_and_names_the_limit() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(5);
        let harness = FakeHarness::new().with_limit_failure_after(
            1,
            "provider usage limit reached",
            Some(reset_at),
        );
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        let third = manager
            .send_message(&created.thread_id, "third".into(), Vec::new())
            .await
            .expect("queue third");

        let mut errors = Vec::new();
        let mut limit_rows = 0;
        loop {
            let wire = next_event(&mut events).await;
            match wire.event {
                Event::Error { message, .. } => errors.push(message),
                Event::UsageLimitHit {
                    reset_at: hit_at, ..
                } => {
                    limit_rows += 1;
                    assert_eq!(hit_at, Some(reset_at));
                }
                Event::TurnCompleted {
                    turn_id, status, ..
                } if turn_id == second.turn_id => {
                    assert_eq!(status, TurnStatus::Failed);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(limit_rows, 1, "the typed limit is one row");
        assert!(
            errors
                .iter()
                .any(|row| row == "Rate limited: provider usage limit reached"),
            "the row names the limit instead of blaming the write: {errors:?}"
        );
        assert!(
            !errors
                .iter()
                .any(|row| row.starts_with("the harness rejected the message:")),
            "a typed limit does not read like an ordinary rejected write: {errors:?}"
        );
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(schedules.len(), 1, "the reset time is persisted");
        assert_eq!(schedules[0].thread_id, created.thread_id);
        assert_eq!(schedules[0].state, h4rn_protocol::ResumeState::Scheduled);

        // The queued message waits for the schedule instead of running into
        // the limit immediately.
        let started = tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                let wire = next_event(&mut events).await;
                if let Event::TurnStarted { turn_id, .. } = wire.event {
                    return turn_id;
                }
            }
        })
        .await;
        assert!(
            started.is_err(),
            "queued work does not start behind a typed limit: {started:?}"
        );
        let _ = third;
        manager.shutdown().await;
    }

    /// A typed limit on a failed write whose session was lost pauses the retry.
    ///
    /// The write claims its prompt, then a restart detaches its session; the
    /// adapter's typed limit arrives on the failure. The prompt goes back for a
    /// retry, and the limit must arm its schedule under the same lock: without
    /// it the retry starts at once on a fresh session, straight into the paused
    /// provider.
    #[tokio::test]
    async fn a_typed_limit_on_a_requeued_write_waits_for_the_schedule() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(5);
        let (harness, gate) = FakeHarness::new().with_send_gate();
        let harness =
            harness.with_limit_failure_after(1, "provider usage limit reached", Some(reset_at));
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        // The second prompt is claimed and blocks in the write. While it is on
        // the wire, a restart detaches its session.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;
        let handle = manager
            .inner
            .handle(&created.thread_id)
            .await
            .expect("thread handle");
        let detached = {
            let mut record = handle.lock().await;
            let generation = record.session.generation().expect("a live generation");
            record.detach_session(generation)
        };
        assert!(
            detached.is_some(),
            "the writing session was detached under the claimed turn"
        );

        // The parked write now fails with the typed limit. The prompt is put
        // back, but the limit must pause the thread instead of letting the
        // retry run on a fresh session.
        gate.release();
        loop {
            let wire = tokio::time::timeout(Duration::from_secs(10), next_event(&mut events))
                .await
                .expect("a limit row arrived");
            match wire.event {
                Event::UsageLimitHit {
                    reset_at: hit_at, ..
                } => {
                    assert_eq!(hit_at, Some(reset_at));
                    break;
                }
                Event::TurnCompleted { turn_id, .. } if turn_id == second.turn_id => {
                    panic!("the retry ran into the paused provider");
                }
                _ => {}
            }
        }
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(schedules.len(), 1, "the typed limit is persisted");
        assert_eq!(schedules[0].state, h4rn_protocol::ResumeState::Scheduled);
        let summary = manager
            .thread_summary(&created.thread_id)
            .await
            .expect("summary");
        assert_eq!(
            summary.agent_state,
            AgentState::Scheduled,
            "the failed write paused the thread"
        );
        assert_eq!(
            sessions.get(),
            1,
            "the retry did not start behind the armed schedule"
        );

        // Nothing starts while the pause is armed.
        tokio::time::pause();
        let started = tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                let wire = next_event(&mut events).await;
                if let Event::TurnStarted { turn_id, .. } = wire.event {
                    return turn_id;
                }
            }
        })
        .await;
        tokio::time::resume();
        assert!(
            started.is_err(),
            "the requeued prompt started behind the armed schedule: {started:?}"
        );

        // The schedule's firing delivers the requeued turn itself, not the
        // policy's resume prompt.
        manager
            .resume_now(&created.thread_id)
            .await
            .expect("the armed schedule fires");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the requeued turn completed");
        assert_eq!(turn, second.turn_id);
        assert_eq!(
            status,
            TurnStatus::Completed,
            "the requeued prompt ran on a fresh session"
        );
        assert_eq!(sessions.get(), 2, "a fresh session carried the retry");
        assert!(
            handle.lock().await.active.is_none(),
            "the turn ended instead of staying Working"
        );
    }

    /// A cancelled pause delivers the prompt the failed write put back.
    ///
    /// The requeued turn is Pending behind the schedule, so clearing the
    /// schedule is the person's decision to run it now. Nothing else would
    /// deliver it and the turn would stay Working forever. The replacement
    /// startup is held by a gate so the test can see the release before (and
    /// then after) its prompt is written, instead of polling a counter.
    #[tokio::test]
    async fn cancelling_a_pause_delivers_the_requeued_turn() {
        let parked =
            park_requeued_turn_gated(Some(chrono::Utc::now() + chrono::Duration::minutes(5)), 1)
                .await;
        let ParkedRequeue {
            _workspace,
            manager,
            meter,
            mut events,
            handle,
            thread_id,
            second,
            sessions,
            sends,
            start_gate,
        } = parked;
        let start_gate = start_gate.expect("the setup gates the replacement startup");

        manager
            .cancel_resume(&thread_id)
            .await
            .expect("cancel the armed resume");

        // The release is the person's action: the thread is running the parked
        // prompt again, not left rate-limited or waiting for attention, and no
        // resume time is shown.
        let summary = manager.thread_summary(&thread_id).await.expect("summary");
        assert_eq!(
            summary.tab,
            ThreadTab::Working,
            "the released turn is not Working"
        );
        assert_eq!(summary.agent_state, AgentState::Thinking);
        assert!(!summary.needs_attention, "the release asked for attention");
        assert_eq!(
            summary.resume_at, None,
            "the released thread shows a resume time"
        );
        {
            let record = handle.lock().await;
            assert!(record.scheduled_limit.is_none());
            assert!(record.meta.parked.is_none());
            assert!(
                record.pending_turn(),
                "the parked turn is still the next turn"
            );
        }
        assert_eq!(
            sends.get(),
            2,
            "the gated release wrote the parked prompt before the handoff"
        );

        start_gate.release();
        tokio::time::timeout(Duration::from_secs(10), sessions.wait_for(2))
            .await
            .expect("the release never started a session");
        tokio::time::timeout(Duration::from_secs(10), sends.wait_for(3))
            .await
            .expect("the released prompt was never written");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the released requeued turn completed");
        assert_eq!(turn, second.turn_id);
        assert_eq!(
            status,
            TurnStatus::Completed,
            "the cancelled pause's requeued prompt ran on a fresh session"
        );
        assert!(
            !meter
                .schedules()
                .await
                .expect("read schedules")
                .iter()
                .any(|schedule| matches!(
                    schedule.state,
                    ResumeState::Scheduled | ResumeState::Waiting
                )),
            "the cancellation left a schedule behind"
        );
    }

    /// A Send over a pause runs the parked turn first, then the new message.
    ///
    /// The new message is the next turn, not a replacement for the one the
    /// failed write put back. Cancelling the schedule to send it must release
    /// that parked turn instead of stranding it Pending.
    #[tokio::test]
    async fn a_send_during_a_pause_runs_the_parked_turn_before_the_new_message() {
        let parked =
            park_requeued_turn(Some(chrono::Utc::now() + chrono::Duration::minutes(5))).await;
        let ParkedRequeue {
            _workspace,
            manager,
            meter,
            mut events,
            thread_id,
            second,
            ..
        } = parked;

        let third = manager
            .send_message(&thread_id, "third".into(), Vec::new())
            .await
            .expect("send");
        assert_eq!(
            third.turn_id, second.turn_id,
            "the new message queues behind the parked turn instead of replacing it"
        );
        let summary = manager.thread_summary(&thread_id).await.expect("summary");
        assert_eq!(
            summary.tab,
            ThreadTab::Working,
            "the released turn is not Working"
        );
        assert_eq!(summary.agent_state, AgentState::Thinking);
        assert!(!summary.needs_attention);
        assert_eq!(
            summary.resume_at, None,
            "the released thread shows a resume time"
        );
        assert!(
            !meter
                .schedules()
                .await
                .expect("read schedules")
                .iter()
                .any(|schedule| matches!(
                    schedule.state,
                    ResumeState::Scheduled | ResumeState::Waiting
                )),
            "the Send left a schedule behind"
        );

        // The parked turn is delivered first.
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the parked turn was left with nothing to deliver it");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);

        // The message that arrived during the pause is the next turn.
        let next = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let wire = next_event(&mut events).await;
                if let Event::TurnStarted { turn_id, .. } = wire.event
                    && turn_id != second.turn_id
                {
                    return turn_id;
                }
            }
        })
        .await
        .expect("the queued message ran after the parked turn");
        assert_ne!(next, second.turn_id);
    }

    /// An older death's late report must not mark the live replacement's reset
    /// as applied.
    ///
    /// G's exit report arms a pause while G+1 is running. That says nothing
    /// about G+1, so when the person cancels and G+1 later reports the same
    /// reset on its first exit, that report must arm the pause again.
    #[tokio::test]
    async fn an_older_deaths_late_report_does_not_consume_the_newer_generations_reset() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(15);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
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
        let dying_notify = handle.lock().await.dying_notify.clone();
        let mut clock = HeldClock::hold();

        // G dies idle and waits for its exit report.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        dying_notify.notified().await;

        // A Send starts G+1 and its turn completes.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;
        assert_eq!(sessions.get(), 2, "the Send replaced the dead session");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the fresh session ran the turn");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);

        // G's delayed exit arms a pause while G+1 is live. It must not record
        // that reset on G+1.
        harness_events.emit_to(
            0,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 7): the first agent ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(reset_at),
            },
        );
        let row = tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the older death's exit report publishes its row");
        assert!(
            row.contains("the first agent ended"),
            "the row is the older death's exit report: {row:?}"
        );
        assert!(
            meter
                .schedules()
                .await
                .expect("read schedules")
                .iter()
                .any(|schedule| matches!(
                    schedule.state,
                    ResumeState::Scheduled | ResumeState::Waiting
                )),
            "the older death armed its pause"
        );

        // The person cancels it. G+1's own first report of the same reset is a
        // different generation's pause and must still arm.
        manager
            .cancel_resume(&created.thread_id)
            .await
            .expect("cancel the armed resume");
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed again".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        dying_notify.notified().await;
        harness_events.emit_to(
            1,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 9): the second agent ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(reset_at),
            },
        );
        let row = tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the newer death's exit report publishes its row");
        assert!(
            row.contains("the second agent ended"),
            "the row is the newer death's exit report: {row:?}"
        );
        let schedules = meter.schedules().await.expect("read schedules");
        assert!(
            schedules.iter().any(|schedule| {
                matches!(
                    schedule.state,
                    ResumeState::Scheduled | ResumeState::Waiting
                ) && schedule.resume_at >= reset_at
            }),
            "the older death had marked the newer generation's reset as applied: {schedules:?}"
        );
        clock.release();
    }

    /// A newer generation's confirmation of the same reset owns the pause.
    ///
    /// G1 arms 13 minutes and G3 confirms the same 13 minutes, keeping the
    /// attempt and jitter. G2's delayed 6-minute report must not replace it;
    /// without the ownership change G3's confirmation left the older generation
    /// in charge, and G2's earlier reset would win.
    #[tokio::test]
    async fn an_equal_reset_transfers_the_pause_to_the_newer_generation() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let first_reset = chrono::Utc::now() + chrono::Duration::minutes(13);
        let earlier_reset = chrono::Utc::now() + chrono::Duration::minutes(6);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
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
        let dying_notify = handle.lock().await.dying_notify.clone();
        let mut clock = HeldClock::hold();

        // G1, G2 and G3 each die idle and wait for their own exit report.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        dying_notify.notified().await;
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the second generation ran the turn");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed again".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        dying_notify.notified().await;
        let third = manager
            .send_message(&created.thread_id, "third".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(3).await;
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the third generation ran the turn");
        assert_eq!(turn, third.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed once more".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        dying_notify.notified().await;

        // G1's report arms the 13-minute pause.
        harness_events.emit_to(
            0,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 7): the first agent ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(first_reset),
            },
        );
        tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the first death's exit report publishes its row");
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].attempt, 0, "the first report armed the pause");

        // G3 confirms the same reset and must become its owner, keeping the
        // attempt.
        harness_events.emit_to(
            2,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 11): the third agent ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(first_reset),
            },
        );
        tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the third death's exit report publishes its row");
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(
            schedules[0].attempt, 0,
            "the confirmation replaced the pause instead of adopting it"
        );

        // G2's delayed earlier reset must not replace the pause G3 now owns.
        harness_events.emit_to(
            1,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 9): the second agent ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(earlier_reset),
            },
        );
        tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the second death's exit report publishes its row");
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(schedules.len(), 1, "one thread keeps one schedule");
        assert!(
            schedules[0].resume_at >= first_reset,
            "an older generation's earlier reset replaced the newer pause: {schedules:?}"
        );
        assert_eq!(
            schedules[0].attempt, 0,
            "the older report re-armed the newer generation's pause"
        );
        let record = handle.lock().await;
        assert_eq!(
            record
                .scheduled_limit
                .as_ref()
                .and_then(|scheduled| scheduled.generation),
            Some(3),
            "the confirmed pause is owned by the newest generation"
        );
        drop(record);
        clock.release();
    }

    /// A fire that lost its schedule to a newer limit must not clear the newer
    /// pause or hand off its delivery.
    ///
    /// The fire claims its attempt, then a newer limit replaces the schedule
    /// while the record lock is free. Acting on the old attempt at that point
    /// would drop the newer pause and start the parked turn it never owned.
    #[tokio::test]
    async fn a_fire_that_lost_its_schedule_leaves_the_newer_pause_alone() {
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(5);
        let newer_reset = chrono::Utc::now() + chrono::Duration::minutes(20);
        let parked = park_requeued_turn(Some(reset_at)).await;
        let ParkedRequeue {
            _workspace,
            manager,
            meter,
            events: _events,
            handle,
            thread_id,
            sessions,
            ..
        } = parked;
        let armed = meter.schedules().await.expect("read schedules");
        assert_eq!(armed.len(), 1);
        let armed_id = armed[0].schedule_id.clone();

        // Hold the fire after it claimed its attempt.
        let pause = Arc::new(FirePause::default());
        *manager
            .inner
            .fire_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&pause));
        let firing = {
            let manager = manager.clone();
            let thread_id = thread_id.clone();
            tokio::spawn(async move { manager.resume_now(&thread_id).await })
        };
        pause.claimed.notified().await;

        // A newer limit replaces the schedule while the fire waits.
        {
            let mut record = handle.lock().await;
            let event = manager
                .inner
                .apply_limit_hit(&mut record, Some(2), Some(newer_reset), true, manager.now())
                .await;
            assert!(event.is_none(), "the newer pause armed");
        }
        let replaced = meter.schedules().await.expect("read schedules");
        assert_eq!(replaced.len(), 1);
        assert_ne!(
            replaced[0].schedule_id, armed_id,
            "the newer limit replaced the schedule"
        );

        pause.resume.notify_one();
        let fired = firing
            .await
            .expect("the fire task")
            .expect("the fire did not fail");
        assert!(
            fired.is_none(),
            "the superseded fire acted on its old attempt: {fired:?}"
        );

        // The newer pause is intact and nothing was delivered for the old fire.
        let record = handle.lock().await;
        assert!(record.pending_turn(), "the parked turn is still waiting");
        assert!(
            matches!(record.session, SessionSlot::None),
            "the superseded fire handed off a delivery"
        );
        let scheduled = record
            .scheduled_limit
            .as_ref()
            .expect("the newer pause is still on the record");
        assert_eq!(scheduled.reset_at, Some(newer_reset));
        assert_eq!(
            scheduled.schedule_id.as_ref(),
            Some(&replaced[0].schedule_id),
            "the record still describes the newer schedule"
        );
        drop(record);
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].schedule_id, replaced[0].schedule_id);
        assert_eq!(
            schedules[0].state,
            ResumeState::Scheduled,
            "the superseded fire finished the newer pause"
        );
        assert_eq!(
            sessions.get(),
            1,
            "nothing started on the old fire's behalf"
        );
    }

    /// A typed limit without a reset time still backs off on a requeued write.
    ///
    /// The adapter said "rate limited" but named no reset. The retry must wait
    /// the meter's unknown-reset backoff instead of running straight back into
    /// the paused provider.
    #[tokio::test]
    async fn a_typed_limit_without_a_reset_on_a_requeued_write_backs_off() {
        let parked = park_requeued_turn(None).await;
        let ParkedRequeue {
            _workspace,
            manager,
            meter,
            mut events,
            thread_id,
            second,
            sessions,
            ..
        } = parked;

        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(schedules.len(), 1, "the reset-less limit is persisted");
        assert_eq!(
            schedules[0].state,
            ResumeState::Waiting,
            "an unknown reset waits its backoff instead of retrying at once"
        );
        assert!(
            schedules[0].reason.contains("did not report a reset time"),
            "the backoff says the provider named no reset: {:?}",
            schedules[0].reason
        );
        assert!(
            schedules[0].resume_at >= schedules[0].scheduled_at + chrono::Duration::seconds(30),
            "the first unknown-reset retry is not the initial backoff: {:?}",
            schedules[0]
        );
        assert_eq!(
            sessions.get(),
            1,
            "the retry ran immediately instead of waiting the backoff"
        );

        // The backoff's firing delivers the parked turn itself.
        manager
            .resume_now(&thread_id)
            .await
            .expect("the armed backoff fires");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the requeued turn completed");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
    }

    /// A firing pause projects the parked turn as running.
    ///
    /// The schedule's attempt is spent and the prompt runs now, so the thread
    /// must leave the paused phase and drop its resume time and limit without
    /// counting a new turn.
    #[tokio::test]
    async fn firing_a_paused_turn_projects_it_as_running() {
        let parked =
            park_requeued_turn_gated(Some(chrono::Utc::now() + chrono::Duration::minutes(5)), 1)
                .await;
        let ParkedRequeue {
            _workspace,
            manager,
            mut events,
            handle,
            thread_id,
            second,
            sends,
            start_gate,
            ..
        } = parked;
        let start_gate = start_gate.expect("the setup gates the replacement startup");

        manager
            .resume_now(&thread_id)
            .await
            .expect("the armed schedule fires");

        let summary = manager.thread_summary(&thread_id).await.expect("summary");
        assert_eq!(
            summary.tab,
            ThreadTab::Working,
            "the fired turn is not Working"
        );
        assert_eq!(
            summary.agent_state,
            AgentState::Thinking,
            "the thread is still projected as paused"
        );
        assert!(!summary.needs_attention);
        assert_eq!(
            summary.resume_at, None,
            "the fired thread shows a resume time"
        );
        assert_eq!(
            summary.limit, None,
            "the fired thread still shows its limit"
        );
        {
            let record = handle.lock().await;
            assert!(record.scheduled_limit.is_none());
            assert!(record.meta.parked.is_none());
            assert!(
                record.pending_turn(),
                "the parked turn is still the next turn"
            );
        }
        assert_eq!(sends.get(), 2, "the gated handoff wrote the prompt early");

        start_gate.release();
        tokio::time::timeout(Duration::from_secs(10), sends.wait_for(3))
            .await
            .expect("the fired prompt was never written");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the fired turn completed");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
    }

    /// The fire's parked-turn delivery survives a failing ledger write.
    ///
    /// The attempt is claimed before the pause is cleared, so a failure to
    /// record the outcome cannot leave the released prompt without an owner:
    /// the delivery is spawned regardless and the storage error is logged.
    #[tokio::test]
    async fn a_parked_turn_is_delivered_when_the_ledger_write_fails() {
        let parked =
            park_requeued_turn(Some(chrono::Utc::now() + chrono::Duration::minutes(5))).await;
        let ParkedRequeue {
            _workspace,
            manager,
            meter,
            mut events,
            thread_id,
            second,
            sessions,
            sends,
            ..
        } = parked;

        meter.store().fail_schedule_finish_for_tests();
        manager
            .resume_now(&thread_id)
            .await
            .expect("the fire runs even though its outcome write fails");

        tokio::time::timeout(Duration::from_secs(10), sessions.wait_for(2))
            .await
            .expect("the release never started a session");
        tokio::time::timeout(Duration::from_secs(10), sends.wait_for(3))
            .await
            .expect("the released prompt was never written");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the released turn completed instead of being stranded");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
    }

    /// A release the journal could not record is published on the next tick.
    ///
    /// The paused thread leaves the paused phase in memory before its release
    /// is appended. Caching that summary as already published would hide it
    /// from the client forever, so a failed append must leave the previous
    /// signature in place and the next tick writes the Running state.
    #[tokio::test]
    async fn a_release_the_journal_cannot_record_is_published_on_the_next_tick() {
        let journal = FlakyJournal::in_memory().await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        let parked = park_requeued_turn_on(
            Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
            journal.clone(),
            meter,
            Some(1),
        )
        .await;
        let ParkedRequeue {
            _workspace,
            manager,
            mut events,
            thread_id,
            start_gate,
            ..
        } = parked;
        let start_gate = start_gate.expect("the setup gates the replacement startup");

        journal.fail_next_appends(1);
        manager
            .resume_now(&thread_id)
            .await
            .expect("the armed schedule fires");
        // The release's own projection never reached the journal or the wire.
        while futures::FutureExt::now_or_never(events.recv()).is_some() {}
        assert_eq!(
            manager
                .thread_summary(&thread_id)
                .await
                .expect("summary")
                .agent_state,
            AgentState::Thinking,
            "the fire did not release the parked turn"
        );

        // The next tick publishes the Running state the failed append lost.
        manager.inner.tick().await;
        let summary = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let wire = next_event(&mut events).await;
                if let Event::ThreadUpserted(summary) = wire.event
                    && summary.tab == ThreadTab::Working
                    && summary.agent_state == AgentState::Thinking
                {
                    return summary;
                }
            }
        })
        .await
        .expect("the failed release was never published");
        assert!(!summary.needs_attention);
        start_gate.release();
    }

    /// A cancelled pause still releases the parked prompt when the journal
    /// cannot record the release.
    ///
    /// The schedule is cleared first, so bailing out on the failed state write
    /// would leave the Pending turn with nothing that could ever deliver it.
    #[tokio::test]
    async fn a_cancel_release_survives_a_failing_journal_write() {
        let journal = FlakyJournal::in_memory().await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        let parked = park_requeued_turn_on(
            Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
            journal.clone(),
            meter,
            None,
        )
        .await;
        let ParkedRequeue {
            _workspace,
            manager,
            mut events,
            thread_id,
            second,
            sessions,
            sends,
            ..
        } = parked;

        journal.fail_next_appends(1);
        manager
            .cancel_resume(&thread_id)
            .await
            .expect("the cancellation is accepted");

        tokio::time::timeout(Duration::from_secs(10), sessions.wait_for(2))
            .await
            .expect("the release never started a session");
        tokio::time::timeout(Duration::from_secs(10), sends.wait_for(3))
            .await
            .expect("the released prompt was never written");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the released turn completed instead of being stranded");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
    }

    /// A Send over a pause still releases the parked prompt when the journal
    /// cannot record the new message.
    #[tokio::test]
    async fn a_send_release_survives_a_failing_journal_write() {
        let journal = FlakyJournal::in_memory().await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        let parked = park_requeued_turn_on(
            Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
            journal.clone(),
            meter,
            None,
        )
        .await;
        let ParkedRequeue {
            _workspace,
            manager,
            thread_id,
            sessions,
            sends,
            ..
        } = parked;

        journal.fail_next_appends(1);
        let refused = manager
            .send_message(&thread_id, "third".into(), Vec::new())
            .await;
        assert!(refused.is_err(), "the journal rejected the new message");

        // The new message was not recorded, but the parked turn was released:
        // the schedule is gone, so it has no other delivery owner.
        tokio::time::timeout(Duration::from_secs(10), sessions.wait_for(2))
            .await
            .expect("the release never started a session");
        tokio::time::timeout(Duration::from_secs(10), sends.wait_for(3))
            .await
            .expect("the released prompt was never written");
    }

    /// A fire only acts when the record still names the schedule it claimed.
    ///
    /// A missing pause is not a match: it proves nothing about which attempt
    /// the thread is waiting on, so neither delivery branch may run.
    #[tokio::test]
    async fn a_fire_requires_the_schedule_the_record_names() {
        let parked =
            park_requeued_turn(Some(chrono::Utc::now() + chrono::Duration::minutes(5))).await;
        let ParkedRequeue {
            _workspace,
            manager,
            meter,
            events: _events,
            handle,
            thread_id,
            sessions,
            sends,
            ..
        } = parked;

        let pause = Arc::new(FirePause::default());
        *manager
            .inner
            .fire_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&pause));
        let firing = {
            let manager = manager.clone();
            let thread_id = thread_id.clone();
            tokio::spawn(async move { manager.resume_now(&thread_id).await })
        };
        pause.claimed.notified().await;

        // The record loses the pause while the claimed fire waits (a record
        // rebuilt from a ledger without an identity looks like this).
        handle.lock().await.scheduled_limit = None;

        pause.resume.notify_one();
        let fired = firing
            .await
            .expect("the fire task")
            .expect("the fire did not fail");
        assert!(
            fired.is_none(),
            "a fire without a matching schedule delivered anyway: {fired:?}"
        );
        let record = handle.lock().await;
        assert!(record.pending_turn(), "the parked turn is still waiting");
        assert!(
            matches!(record.session, SessionSlot::None),
            "the unmatched fire handed off a delivery"
        );
        drop(record);
        assert_eq!(sessions.get(), 1, "the unmatched fire started a session");
        assert_eq!(sends.get(), 2, "the unmatched fire wrote the parked prompt");
        // The attempt was already claimed; the fire records that it did not
        // deliver instead of leaving the row looking fired.
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(
            schedules[0].state,
            ResumeState::Cancelled,
            "the claimed fire was left as delivered: {:?}",
            schedules[0]
        );
        assert_eq!(
            schedules[0].last_error.as_deref(),
            Some("superseded before delivery")
        );
    }

    /// A pause only parks the prompt it was armed for.
    ///
    /// An old agent's exit report can arm a pause while a Send is starting a
    /// replacement turn. That pause says nothing about the new prompt, so the
    /// startup delivers it exactly as it does with no pause at all.
    #[tokio::test]
    async fn a_pause_does_not_park_a_replacement_turn() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(6);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let (harness, start_gate) = harness.with_start_gate_after(1);
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
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
        let dying_notify = handle.lock().await.dying_notify.clone();
        let mut clock = HeldClock::hold();

        // The first agent dies between turns; its exit report is still to come.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        dying_notify.notified().await;

        // A Send starts the replacement, whose startup this test holds.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        assert!(
            handle.lock().await.pending_turn(),
            "the replacement prompt waits for its startup"
        );

        // The dead agent's exit report arms a pause the replacement did not ask
        // for.
        harness_events.emit_to(
            0,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 7): the first agent ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(reset_at),
            },
        );
        tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the death's exit report publishes its row");
        {
            let record = handle.lock().await;
            assert!(
                record.scheduled_limit.is_some(),
                "the death's reset armed a pause"
            );
            assert!(
                !record.pause_parks_turn(&second.turn_id),
                "the pause named a prompt it was not armed for"
            );
        }

        // The startup delivers the replacement prompt as on main. The death's
        // row is already published, so the clock can run: the waits below are
        // then bounded instead of hanging on a regression.
        clock.release();
        start_gate.release();
        tokio::time::timeout(Duration::from_secs(10), sessions.wait_for(2))
            .await
            .expect("the replacement startup never began");
        tokio::time::timeout(Duration::from_secs(10), sends.wait_for(2))
            .await
            .expect("the replacement prompt was never written");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the replacement turn was parked by another turn's pause");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
    }

    /// A Send that cancels a stale pause releases no prompt the pause never
    /// parked.
    ///
    /// An old agent's exit arms a pause while a Send's replacement turn is
    /// pending. A second Send makes that pause stale, and cancelling it must
    /// not emit a release for the replacement turn: the pause never held it,
    /// and a release would move the thread's phase and spend an advance on a
    /// prompt that already has an owner.
    #[tokio::test]
    async fn a_send_cancelling_a_stale_pause_releases_no_prompt() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(6);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let (harness, start_gate) = harness.with_start_gate_after(1);
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
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
        let dying_notify = handle.lock().await.dying_notify.clone();
        let mut clock = HeldClock::hold();

        // The first agent dies between turns; its exit report is still to come.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        dying_notify.notified().await;

        // A Send starts the replacement, whose startup this test holds.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        assert!(
            handle.lock().await.pending_turn(),
            "the replacement prompt waits for its startup"
        );

        // The dead agent's exit report arms a pause the replacement did not ask
        // for.
        harness_events.emit_to(
            0,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 7): the first agent ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(reset_at),
            },
        );
        tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the death's exit report publishes its row");
        {
            let record = handle.lock().await;
            assert!(record.scheduled_limit.is_some(), "the death armed a pause");
            assert!(
                !record.pause_parks_turn(&second.turn_id),
                "the pause named a prompt it was not armed for"
            );
        }

        // A second Send makes the pause stale and cancels it. The replacement
        // turn was never parked by that pause, so nothing is released.
        let third = manager
            .send_message(&created.thread_id, "third".into(), Vec::new())
            .await
            .expect("send while paused");
        assert_eq!(
            third.turn_id, second.turn_id,
            "a Send while a turn is running queues behind that turn"
        );
        let domain = manager
            .inner
            .journal
            .read_domain(&created.thread_id)
            .await
            .expect("read the domain log");
        assert!(
            !domain
                .iter()
                .any(|event| matches!(event, ThreadEvent::LimitResumeReleased { .. })),
            "the Send released a prompt the stale pause never parked"
        );
        {
            let record = handle.lock().await;
            assert!(record.scheduled_limit.is_none());
            assert!(record.meta.parked.is_none());
            assert!(record.limit.is_none());
        }

        // The replacement prompt still runs and the new message queues behind
        // it; both complete on the startup this test held.
        clock.release();
        start_gate.release();
        tokio::time::timeout(Duration::from_secs(10), sends.wait_for(3))
            .await
            .expect("the queued messages were never written");
        let mut completed = Vec::new();
        for _ in 0..2 {
            let (turn, status) =
                tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                    .await
                    .expect("a queued message never completed");
            assert_eq!(status, TurnStatus::Completed);
            completed.push(turn);
        }
        assert!(completed.contains(&second.turn_id));
        assert!(
            tokio::time::timeout(Duration::from_secs(1), sessions.wait_for(2))
                .await
                .is_ok(),
            "the replacement startup never began"
        );
    }

    /// A fire never releases a manual Send's pending startup.
    ///
    /// An old agent's exit arms a pause while a Send is starting a replacement
    /// turn. That turn is pending, but the pause does not park it, so a due
    /// fire must leave it alone and spend no attempt on it.
    #[tokio::test]
    async fn a_fire_leaves_a_pending_turn_the_pause_does_not_park() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(6);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let (harness, start_gate) = harness.with_start_gate_after(1);
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
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
        let dying_notify = handle.lock().await.dying_notify.clone();
        let mut clock = HeldClock::hold();

        // The first agent dies between turns; its exit report is still to come.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        dying_notify.notified().await;

        // A Send starts the replacement, whose startup this test holds.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        assert!(
            handle.lock().await.pending_turn(),
            "the replacement prompt waits for its startup"
        );

        // The dead agent's exit report arms a pause the replacement did not ask
        // for: it names no parked turn.
        harness_events.emit_to(
            0,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 7): the first agent ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(reset_at),
            },
        );
        tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the death's exit report publishes its row");
        {
            let record = handle.lock().await;
            assert!(record.scheduled_limit.is_some(), "the death armed a pause");
            assert!(
                !record.pause_parks_turn(&second.turn_id),
                "the pause named a prompt it was not armed for"
            );
        }

        // The due fire leaves the Send's turn alone and spends no attempt.
        let fired = manager
            .resume_now(&created.thread_id)
            .await
            .expect("the due fire waits instead of failing");
        assert!(
            fired.is_none(),
            "the fire released a prompt the pause did not park: {fired:?}"
        );
        {
            let record = handle.lock().await;
            assert!(record.pending_turn(), "the fire released the Send's prompt");
            assert!(
                matches!(record.session, SessionSlot::Starting(_)),
                "the fire disturbed the Send's startup"
            );
            assert_eq!(
                record
                    .scheduled_limit
                    .as_ref()
                    .and_then(|scheduled| scheduled.parked_turn.clone()),
                None,
                "the fire attached the Send's turn to the pause"
            );
        }
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(schedules.len(), 1, "the fire left the pause armed");
        assert_eq!(
            schedules[0].state,
            ResumeState::Scheduled,
            "the fire spent the attempt on a turn it did not own"
        );
        assert_eq!(
            schedules[0].attempt, 0,
            "the fire spent the attempt on a turn it did not own"
        );
        assert_eq!(sends.get(), 1, "the fire wrote the Send's prompt early");

        // The startup still delivers the Send's own prompt.
        clock.release();
        start_gate.release();
        tokio::time::timeout(Duration::from_secs(10), sessions.wait_for(2))
            .await
            .expect("the replacement startup never began");
        tokio::time::timeout(Duration::from_secs(10), sends.wait_for(2))
            .await
            .expect("the replacement prompt was never written");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the replacement turn was stranded");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
    }

    /// A live session that adopts an equal reset records it for its own
    /// generation, so a later exit-only repeat cannot re-arm a cancelled pause.
    ///
    /// G1's exit arms the pause; G2 confirms the same reset on its own stream
    /// and takes ownership. If that transfer did not mark G2's generation, G2's
    /// own exit report would arm the pause again after the person cancelled it.
    #[tokio::test]
    async fn a_live_session_adopting_the_pause_records_the_reset_for_its_generation() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(13);
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
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
        let dying_notify = handle.lock().await.dying_notify.clone();
        let mut clock = HeldClock::hold();

        // G1 dies idle and waits for its exit report.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        dying_notify.notified().await;

        // G2 runs the next turn and stays live.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sessions.wait_for(2).await;
        sends.wait_for(2).await;
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("G2 completed its turn");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);

        // G1's exit report arms the pause for generation 1.
        harness_events.emit_to(
            0,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 7): the first agent ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(reset_at),
            },
        );
        tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the first death's exit report publishes its row");
        assert_eq!(
            handle
                .lock()
                .await
                .scheduled_limit
                .as_ref()
                .and_then(|scheduled| scheduled.generation),
            Some(1),
            "G1's exit armed the pause"
        );

        // G2 confirms the same reset and takes ownership of it.
        harness_events.emit_to(
            1,
            HarnessEvent::LimitHit {
                reset_at: Some(reset_at),
                auto_resume: true,
            },
        );
        loop {
            let wire = next_event(&mut events).await;
            if matches!(wire.event, Event::UsageLimitHit { .. }) {
                break;
            }
        }
        {
            let record = handle.lock().await;
            let scheduled = record
                .scheduled_limit
                .as_ref()
                .expect("the pause is still armed");
            assert_eq!(scheduled.generation, Some(2), "G2 owns the pause");
            assert_eq!(
                meter.schedules().await.expect("read schedules")[0].attempt,
                0,
                "the confirmation re-armed the pause"
            );
        }

        // The person cancels; nothing is pending, so only the state clears.
        manager
            .cancel_resume(&created.thread_id)
            .await
            .expect("cancel the resume");

        // G2 dies and its exit report repeats the reset. It is the same reset
        // G2 itself confirmed, so it must not arm the cancelled pause again.
        harness_events.emit_to(
            1,
            HarnessEvent::TransportClosed {
                message: "the ACP agent stdout closed".into(),
                retryable: true,
                kind: TransportCloseKind::Eof,
                reset_at: None,
            },
        );
        dying_notify.notified().await;
        harness_events.emit_to(
            1,
            HarnessEvent::TransportClosed {
                message: "agy exited (exit status: 9): the second agent ended".into(),
                retryable: false,
                kind: TransportCloseKind::Exit,
                reset_at: Some(reset_at),
            },
        );
        tokio::time::timeout(Duration::from_secs(5), next_error(&mut events))
            .await
            .expect("the second death's exit report publishes its row");
        assert!(
            !meter
                .schedules()
                .await
                .expect("read schedules")
                .iter()
                .any(|schedule| matches!(
                    schedule.state,
                    ResumeState::Scheduled | ResumeState::Waiting
                )),
            "the exit repeat re-armed the pause the person cancelled"
        );
        clock.release();
    }

    /// A parked prompt survives a daemon restart.
    ///
    /// The prompt and its turn are already in the journal when the failed write
    /// arms the pause, so the restarted daemon restores the dispatch and the
    /// schedule delivers that prompt, not the policy's "continue".
    #[tokio::test]
    async fn a_parked_prompt_survives_a_daemon_restart() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let journal_path = workspace.path().join("journal.sqlite");
        let meter_path = workspace.path().join("meter.sqlite");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(20);
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal"),
        );
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter"),
        );
        let parked = park_requeued_turn_on(Some(reset_at), journal, meter, None).await;
        let ParkedRequeue {
            _workspace,
            manager,
            thread_id,
            second,
            ..
        } = parked;

        // A graceful shutdown keeps the parked prompt and its schedule, so the
        // restarted daemon restores the dispatch and delivers that prompt.
        manager.shutdown().await;
        drop(manager);
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal after restart"),
        );
        let (harness, sends) = FakeHarness::new().with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager_with_journal(harness, host, journal).await;
        manager.set_meter(Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter after restart"),
        ));
        manager.resync_meter_state().await;
        let mut events = manager.subscribe();

        // The restored thread shows its schedule and still owns the prompt.
        let summary = manager.thread_summary(&thread_id).await.expect("summary");
        assert_eq!(summary.tab, ThreadTab::Working);
        assert_eq!(summary.agent_state, AgentState::Scheduled);
        let ledger = manager
            .meter()
            .expect("meter")
            .schedules()
            .await
            .expect("read schedules");
        assert_eq!(summary.resume_at, Some(ledger[0].resume_at));
        let meta = manager.thread_meta(&thread_id).await.expect("thread meta");
        assert_eq!(
            meta.parked.as_ref().map(|parked| parked.turn_id.clone()),
            Some(second.turn_id.clone()),
            "the thread row does not remember the parked prompt"
        );
        {
            let handle = manager
                .inner
                .handle(&thread_id)
                .await
                .expect("thread handle");
            let record = handle.lock().await;
            assert_eq!(
                record.active.as_ref().map(|active| active.turn_id.clone()),
                Some(second.turn_id.clone()),
                "the restart did not restore the parked dispatch"
            );
            assert!(record.pending_turn());
            assert_eq!(
                record
                    .scheduled_limit
                    .as_ref()
                    .and_then(|scheduled| scheduled.parked_turn.clone()),
                Some(second.turn_id.clone()),
                "the restored pause does not name the restored prompt"
            );
        }

        // The schedule fires and delivers the restored prompt.
        manager
            .resume_now(&thread_id)
            .await
            .expect("the restored schedule fires");
        let mut echoed = String::new();
        let (turn, status) = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let wire = next_event(&mut events).await;
                match wire.event {
                    Event::MessageDelta { text, .. } => echoed.push_str(&text),
                    Event::TurnCompleted {
                        turn_id, status, ..
                    } => return (turn_id, status),
                    _ => {}
                }
            }
        })
        .await
        .expect("the restored prompt completed instead of being lost");
        assert_eq!(
            turn, second.turn_id,
            "the resume started a new turn instead of the parked one"
        );
        assert_eq!(status, TurnStatus::Completed);
        assert!(
            echoed.contains("echo: second"),
            "the policy's prompt ran instead of the parked one: {echoed:?}"
        );
        assert_eq!(sends.get(), 1);
    }

    /// A shutdown while the startup resync is unresolved keeps the parked
    /// prompt.
    ///
    /// The schedule read can fail on the boot after a parked prompt was
    /// restored, so the record does not know the schedule that will deliver
    /// it. Settling the restored turn in that window would clear `parked_json`
    /// while the ledger stays armed, and the next boot would fire the policy's
    /// prompt instead of the person's.
    #[tokio::test]
    async fn a_shutdown_during_a_failed_resync_keeps_the_parked_prompt() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let journal_path = workspace.path().join("journal.sqlite");
        let meter_path = workspace.path().join("meter.sqlite");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(20);
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal"),
        );
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter"),
        );
        let parked = park_requeued_turn_on(Some(reset_at), journal, meter, None).await;
        let ParkedRequeue {
            _workspace,
            manager,
            thread_id,
            second,
            ..
        } = parked;
        // The first daemon stops without a chance to settle anything.
        drop(manager);

        // The next boot cannot read the schedule ledger and stops before the
        // retry succeeds.
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal after restart"),
        );
        let (harness, _) = FakeHarness::new().with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager_with_journal(harness, host, journal).await;
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter after restart"),
        );
        meter.store().fail_next_schedule_reads_for_tests(1);
        manager.set_meter(Arc::clone(&meter));
        manager.resync_meter_state().await;
        assert!(
            manager.inner.resync_pending.load(Ordering::Relaxed),
            "the failed schedule read did not leave the resync unresolved"
        );
        {
            let handle = manager
                .inner
                .handle(&thread_id)
                .await
                .expect("thread handle");
            let record = handle.lock().await;
            assert!(
                !record.parks_active_turn(),
                "precondition: the unresolved record names no schedule"
            );
            assert!(
                record.parks_persisted_prompt(),
                "the restored prompt is not the durable parked one"
            );
        }
        manager.shutdown().await;
        drop(manager);

        // A working ledger now delivers the schedule. It must fire the prompt
        // the person sent, not the policy's resume prompt.
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal after shutdown"),
        );
        let (harness, sends) = FakeHarness::new().with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager_with_journal(harness, host, journal).await;
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter after shutdown"),
        );
        manager.set_meter(Arc::clone(&meter));
        manager.resync_meter_state().await;
        let mut events = manager.subscribe();
        let meta = manager.thread_meta(&thread_id).await.expect("thread meta");
        assert_eq!(
            meta.parked.as_ref().map(|parked| parked.turn_id.clone()),
            Some(second.turn_id.clone()),
            "the shutdown erased the parked prompt"
        );
        manager
            .resume_now(&thread_id)
            .await
            .expect("the surviving schedule fires");

        let mut echoed = String::new();
        let (turn, status) = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let wire = next_event(&mut events).await;
                match wire.event {
                    Event::MessageDelta { text, .. } => echoed.push_str(&text),
                    Event::TurnCompleted {
                        turn_id, status, ..
                    } => return (turn_id, status),
                    _ => {}
                }
            }
        })
        .await
        .expect("the surviving schedule delivered no turn");
        assert_eq!(
            turn, second.turn_id,
            "the resume started a new turn instead of the parked one"
        );
        assert_eq!(status, TurnStatus::Completed);
        assert!(
            echoed.contains("echo: second"),
            "the policy's prompt ran instead of the parked one: {echoed:?}"
        );
        assert_eq!(sends.get(), 1);
    }

    /// A resync retry never restores a schedule armed after its ledger read.
    ///
    /// The retry reads the ledger, and a newer limit can arm schedule B before
    /// the retry reaches the record. Writing the older snapshot A into the
    /// record would move the thread back to A's earlier reset and leave the
    /// parked prompt waiting for a schedule the record no longer names.
    #[tokio::test]
    async fn a_resync_retry_does_not_replace_a_newer_schedule() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let journal_path = workspace.path().join("journal.sqlite");
        let meter_path = workspace.path().join("meter.sqlite");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(20);
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal"),
        );
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter"),
        );
        let parked = park_requeued_turn_on(Some(reset_at), journal, meter, None).await;
        let ParkedRequeue {
            _workspace,
            manager,
            thread_id,
            second,
            ..
        } = parked;
        drop(manager);

        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal after restart"),
        );
        let (harness, sends) = FakeHarness::new().with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager_with_journal(harness, host, journal).await;
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter after restart"),
        );
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let first = meter.schedules().await.expect("read schedules");
        assert_eq!(first.len(), 1, "the paused schedule survived the restart");
        let first_id = first[0].schedule_id.clone();

        // Hold the resync between its ledger read and the record lock.
        let pause = Arc::new(ResyncPause::default());
        *manager
            .inner
            .resync_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&pause));
        let resyncing = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.resync_meter_state().await })
        };
        tokio::time::timeout(Duration::from_secs(10), pause.read.notified())
            .await
            .expect("the resync never read the ledger");

        // While it is held, a newer limit arms schedule B on the record.
        let newer_reset = chrono::Utc::now() + chrono::Duration::minutes(40);
        let handle = manager
            .inner
            .handle(&thread_id)
            .await
            .expect("thread handle");
        let newer_id = {
            let mut record = handle.lock().await;
            let mut domain = Vec::new();
            let mut wire = Vec::new();
            manager
                .inner
                .note_limit_hit(
                    &mut record,
                    LimitObservation {
                        generation: Some(99),
                        reset_at: Some(newer_reset),
                        auto_resume: true,
                    },
                    manager.now(),
                    &mut domain,
                    &mut wire,
                )
                .await;
            let id = record
                .scheduled_limit
                .as_ref()
                .and_then(|scheduled| scheduled.schedule_id.clone())
                .expect("the newer limit armed a schedule");
            assert_ne!(id, first_id, "the newer limit replaced the schedule");
            id
        };
        pause.resume.notify_one();
        resyncing.await.expect("the resync task");

        {
            let record = handle.lock().await;
            let scheduled = record
                .scheduled_limit
                .as_ref()
                .expect("the retry dropped every pause");
            assert_eq!(
                scheduled.schedule_id.as_deref(),
                Some(newer_id.as_str()),
                "the retry restored the older schedule over the newer one"
            );
            assert_eq!(
                scheduled.reset_at,
                Some(newer_reset),
                "the retry moved the pause back to the older reset"
            );
            assert_eq!(
                scheduled.parked_turn.as_ref(),
                Some(&second.turn_id),
                "the newer pause lost the prompt it was holding"
            );
        }

        // The newer schedule fires and delivers the parked prompt.
        manager
            .resume_now(&thread_id)
            .await
            .expect("the newer schedule fires");
        let mut echoed = String::new();
        let (turn, status) = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let wire = next_event(&mut events).await;
                match wire.event {
                    Event::MessageDelta { text, .. } => echoed.push_str(&text),
                    Event::TurnCompleted {
                        turn_id, status, ..
                    } => return (turn_id, status),
                    _ => {}
                }
            }
        })
        .await
        .expect("the newer schedule delivered no turn");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert!(
            echoed.contains("echo: second"),
            "the parked prompt was not delivered: {echoed:?}"
        );
        assert_eq!(sends.get(), 1);
    }

    /// A resync retry never binds a schedule the person cancelled after its
    /// ledger read.
    ///
    /// The startup read fails, so the restored prompt has no pause that will
    /// deliver it. The retry then snapshots schedule A while it is still armed,
    /// the person cancels A before the retry reaches the record, and binding
    /// that stale snapshot would leave the prompt waiting for a fire that can
    /// never happen. The retry re-reads A under the lock instead and releases
    /// the prompt, so it runs now.
    #[tokio::test]
    async fn a_resync_retry_releases_a_prompt_cancelled_after_its_read() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let journal_path = workspace.path().join("journal.sqlite");
        let meter_path = workspace.path().join("meter.sqlite");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(20);
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal"),
        );
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter"),
        );
        let parked = park_requeued_turn_on(Some(reset_at), journal, meter, None).await;
        let ParkedRequeue {
            _workspace,
            manager,
            thread_id,
            second,
            ..
        } = parked;
        drop(manager);

        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal after restart"),
        );
        let (harness, sends) = FakeHarness::new().with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager_with_journal(harness, host, journal).await;
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter after restart"),
        );
        meter.store().fail_next_schedule_reads_for_tests(1);
        manager.set_meter(Arc::clone(&meter));

        // The first read fails: the restored prompt is still unresolved.
        manager.resync_meter_state().await;
        assert!(
            manager.inner.resync_pending.load(Ordering::Relaxed),
            "the failed schedule read left no retry pending"
        );

        // Hold the retry between its ledger read and the record lock.
        let pause = Arc::new(ResyncPause::default());
        *manager
            .inner
            .resync_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&pause));
        let mut events = manager.subscribe();
        let resyncing = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.resync_meter_state().await })
        };
        tokio::time::timeout(Duration::from_secs(10), pause.read.notified())
            .await
            .expect("the retry never read the ledger");

        // The person cancels A while the retry holds its snapshot.
        let cancelled = manager
            .cancel_resume(&thread_id)
            .await
            .expect("cancel the armed resume");
        assert!(
            cancelled.is_some(),
            "the parked prompt had no schedule to cancel"
        );
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(
            schedules[0].state,
            ResumeState::Cancelled,
            "the test did not cancel the armed schedule"
        );

        pause.resume.notify_one();
        resyncing.await.expect("the resync task");

        {
            let handle = manager
                .inner
                .handle(&thread_id)
                .await
                .expect("thread handle");
            let record = handle.lock().await;
            assert!(
                record.scheduled_limit.is_none(),
                "the retry bound a schedule that was already cancelled"
            );
            assert!(
                record.meta.parked.is_none(),
                "the cancelled schedule kept the prompt parked"
            );
        }

        // The release delivers the person's prompt instead of leaving it
        // waiting for the cancelled schedule.
        let mut echoed = String::new();
        let (turn, status) = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let wire = next_event(&mut events).await;
                match wire.event {
                    Event::MessageDelta { text, .. } => echoed.push_str(&text),
                    Event::TurnCompleted {
                        turn_id, status, ..
                    } => return (turn_id, status),
                    _ => {}
                }
            }
        })
        .await
        .expect("the released prompt never ran");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert!(
            echoed.contains("echo: second"),
            "the parked prompt was not delivered: {echoed:?}"
        );
        assert_eq!(sends.get(), 1, "the released prompt was not written once");
    }

    /// A retry does not release a prompt the cancel already released.
    ///
    /// The first resync bound the pause to the restored prompt, so the person's
    /// cancellation releases it and records one release. A later retry then
    /// snapshots the now-cancelled row, and its re-check must not release the
    /// same prompt a second time.
    #[tokio::test]
    async fn a_retry_does_not_release_a_prompt_the_cancel_already_released() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let journal_path = workspace.path().join("journal.sqlite");
        let meter_path = workspace.path().join("meter.sqlite");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(20);
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal"),
        );
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter"),
        );
        let parked = park_requeued_turn_on(Some(reset_at), journal, meter, None).await;
        let ParkedRequeue {
            _workspace,
            manager,
            thread_id,
            second,
            ..
        } = parked;
        drop(manager);

        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal after restart"),
        );
        let (harness, sends) = FakeHarness::new().with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager_with_journal(harness, host, journal).await;
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter after restart"),
        );
        manager.set_meter(Arc::clone(&meter));

        // The first resync binds the restored pause to its prompt.
        manager.resync_meter_state().await;
        {
            let handle = manager
                .inner
                .handle(&thread_id)
                .await
                .expect("thread handle");
            let record = handle.lock().await;
            assert!(
                record.parks_active_turn(),
                "precondition: the pause holds the restored prompt"
            );
        }

        // Hold a later retry between its ledger read and the record lock.
        let pause = Arc::new(ResyncPause::default());
        *manager
            .inner
            .resync_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&pause));
        let mut events = manager.subscribe();
        let resyncing = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.resync_meter_state().await })
        };
        tokio::time::timeout(Duration::from_secs(10), pause.read.notified())
            .await
            .expect("the retry never read the ledger");

        // The cancel releases the prompt and records that release while the
        // retry still holds its snapshot.
        let cancelled = manager
            .cancel_resume(&thread_id)
            .await
            .expect("cancel the armed resume");
        assert!(cancelled.is_some(), "the parked prompt lost its schedule");
        let domain = manager
            .inner
            .journal
            .read_domain(&thread_id)
            .await
            .expect("read the domain log");
        assert_eq!(
            domain
                .iter()
                .filter(|event| matches!(event, ThreadEvent::LimitResumeReleased { .. }))
                .count(),
            1,
            "precondition: the cancel released the parked prompt once"
        );

        pause.resume.notify_one();
        resyncing.await.expect("the resync task");

        // The retry saw a cancelled row, but the prompt it held is already
        // released: no second release and no phase change for it.
        let domain = manager
            .inner
            .journal
            .read_domain(&thread_id)
            .await
            .expect("read the domain log");
        assert_eq!(
            domain
                .iter()
                .filter(|event| matches!(event, ThreadEvent::LimitResumeReleased { .. }))
                .count(),
            1,
            "the retry released a prompt the cancel already released"
        );
        {
            let handle = manager
                .inner
                .handle(&thread_id)
                .await
                .expect("thread handle");
            let record = handle.lock().await;
            assert!(record.scheduled_limit.is_none());
            assert!(record.meta.parked.is_none());
        }

        // The one release delivers the person's prompt exactly once.
        let mut echoed = String::new();
        let (turn, status) = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let wire = next_event(&mut events).await;
                match wire.event {
                    Event::MessageDelta { text, .. } => echoed.push_str(&text),
                    Event::TurnCompleted {
                        turn_id, status, ..
                    } => return (turn_id, status),
                    _ => {}
                }
            }
        })
        .await
        .expect("the cancelled schedule's prompt never ran");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert!(
            echoed.contains("echo: second"),
            "the parked prompt was not delivered: {echoed:?}"
        );
        assert_eq!(sends.get(), 1, "the released prompt was not written once");
    }

    /// A cancelled idle schedule snapshot releases no prompt.
    ///
    /// The thread has no pending turn: an exit report's limit armed its
    /// schedule between turns, and nothing is parked behind it. If the person
    /// cancels that schedule after a retry snapshotted it, the retry must drop
    /// the stale pause instead of emitting a release that would flip the idle
    /// thread to Thinking.
    #[tokio::test]
    async fn a_cancelled_idle_schedule_snapshot_releases_no_prompt() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let journal_path = workspace.path().join("journal.sqlite");
        let meter_path = workspace.path().join("meter.sqlite");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(15);
        let journal: Arc<dyn Journal> = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal"),
        );
        let (harness, harness_events) = FakeHarness::new().with_event_injector();
        let host = HostId::new("host_test");
        let manager = open_test_manager_with_journal(harness, host.clone(), journal).await;
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter"),
        );
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();
        let created = manager
            .create_thread(new_thread(host, workspace.path(), "first"))
            .await
            .expect("create");
        assert_eq!(next_turn_status(&mut events).await, TurnStatus::Completed);

        // Between turns, the exit's limit arms a schedule while the thread is
        // idle: no pending turn and no parked prompt.
        harness_events.emit(HarnessEvent::TransportClosed {
            message: "the ACP agent stdout closed".into(),
            retryable: true,
            kind: TransportCloseKind::Eof,
            reset_at: None,
        });
        harness_events.emit(HarnessEvent::LimitHit {
            reset_at: Some(reset_at),
            auto_resume: true,
        });
        let row = tokio::time::timeout(Duration::from_secs(3), next_error(&mut events))
            .await
            .expect("the fallback publishes the idle row");
        assert_eq!(row, "the ACP agent stdout closed");
        {
            let handle = manager
                .inner
                .handle(&created.thread_id)
                .await
                .expect("thread handle");
            let record = handle.lock().await;
            assert!(
                record.scheduled_limit.is_some(),
                "the idle limit armed no pause"
            );
            assert!(!record.pending_turn(), "precondition: the thread is idle");
            assert!(
                !record.parks_persisted_prompt(),
                "precondition: an idle pause parks no prompt"
            );
            assert_eq!(record.state.phase, h4rn_core::Phase::RateLimited);
        }
        manager.shutdown().await;
        drop(manager);

        // The next boot cannot read the schedule list, so the first resync is
        // unresolved and the retry is the pass that snapshots the row.
        let journal: Arc<dyn Journal> = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal after restart"),
        );
        let harness = FakeHarness::new();
        let host = HostId::new("host_test");
        let manager = open_test_manager_with_journal(harness, host, journal).await;
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter after restart"),
        );
        meter.store().fail_next_schedule_reads_for_tests(1);
        manager.set_meter(Arc::clone(&meter));
        manager.resync_meter_state().await;
        assert!(
            manager.inner.resync_pending.load(Ordering::Relaxed),
            "the failed schedule read left no retry pending"
        );

        // Hold the retry between its ledger read and the record lock.
        let pause = Arc::new(ResyncPause::default());
        *manager
            .inner
            .resync_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&pause));
        let resyncing = {
            let manager = manager.clone();
            tokio::spawn(async move { manager.resync_meter_state().await })
        };
        tokio::time::timeout(Duration::from_secs(10), pause.read.notified())
            .await
            .expect("the retry never read the ledger");

        // The person cancels the schedule while the retry holds its snapshot.
        let cancelled = manager
            .cancel_resume(&created.thread_id)
            .await
            .expect("cancel the armed resume");
        assert!(
            cancelled.is_some(),
            "the idle thread had no schedule to cancel"
        );

        pause.resume.notify_one();
        resyncing.await.expect("the resync task");

        // Nothing was parked, so the cancelled snapshot must not release
        // anything or move the thread out of its paused phase.
        let domain = manager
            .inner
            .journal
            .read_domain(&created.thread_id)
            .await
            .expect("read the domain log");
        assert!(
            !domain
                .iter()
                .any(|event| matches!(event, ThreadEvent::LimitResumeReleased { .. })),
            "the idle thread's cancelled snapshot released a prompt it never parked"
        );
        {
            let handle = manager
                .inner
                .handle(&created.thread_id)
                .await
                .expect("thread handle");
            let record = handle.lock().await;
            assert_eq!(
                record.state.phase,
                h4rn_core::Phase::RateLimited,
                "the cancelled idle snapshot moved the thread out of its pause"
            );
            assert!(record.active.is_none());
            assert!(record.meta.parked.is_none());
            assert!(record.scheduled_limit.is_none());
        }
        let summary = manager
            .thread_summary(&created.thread_id)
            .await
            .expect("summary");
        assert_eq!(
            summary.agent_state,
            AgentState::RateLimited,
            "the cancelled idle snapshot made the thread look like it is thinking"
        );
    }

    /// The resync retry deadline is monotonic.
    ///
    /// A wall clock that jumps backwards (NTP, a manual change) must not stretch
    /// the wait: the manager's monotonic reading alone decides when the retry
    /// runs.
    #[tokio::test]
    async fn a_backward_wall_clock_jump_does_not_stretch_the_resync_retry() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let journal_path = workspace.path().join("journal.sqlite");
        let meter_path = workspace.path().join("meter.sqlite");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(20);
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal"),
        );
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter"),
        );
        let parked = park_requeued_turn_on(Some(reset_at), journal, meter, None).await;
        let ParkedRequeue {
            _workspace,
            manager,
            ..
        } = parked;
        drop(manager);

        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal after restart"),
        );
        let (harness, _) = FakeHarness::new().with_send_count();
        let host = HostId::new("host_test");
        let clock = Arc::new(ManualClock::new());
        let manager = open_test_manager_with_clock(
            harness,
            host,
            journal,
            Some(Arc::clone(&clock) as Arc<dyn Clock>),
        )
        .await;
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter after restart"),
        );
        meter.store().fail_next_limit_reads_for_tests(1);
        manager.set_meter(Arc::clone(&meter));

        manager.resync_meter_state().await;
        assert_eq!(
            meter.store().limit_reads_for_tests(),
            1,
            "the first resync did not try the limit history"
        );
        assert!(
            manager.inner.resync_pending.load(Ordering::Relaxed),
            "the failed read left no retry pending"
        );

        // The wall clock jumps an hour back. A deadline read from the wall
        // clock would now be an hour away.
        clock.shift_wall(chrono::Duration::hours(-1));
        manager.inner.tick().await;
        assert_eq!(
            meter.store().limit_reads_for_tests(),
            1,
            "the backward wall-clock jump moved the retry deadline"
        );

        // Only the monotonic wait decides when the retry runs.
        clock.advance_monotonic(Duration::from_secs(1));
        manager.inner.tick().await;
        assert_eq!(
            meter.store().limit_reads_for_tests(),
            2,
            "the retry missed its monotonic deadline"
        );
        assert!(
            !manager.inner.resync_pending.load(Ordering::Relaxed),
            "the successful retry left a retry pending"
        );
    }

    /// A failed resync waits its backoff, and each wait doubles to a minute.
    ///
    /// The wait is what keeps an unreadable ledger from rewriting every summary
    /// on every tick, so it needs its own deterministic test: a tick that does
    /// not advance the clock must not read the ledger again, the first wait is
    /// one second, and each later wait doubles up to the 60 s cap.
    #[tokio::test]
    async fn a_failed_resync_waits_its_backoff_before_retrying() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let journal_path = workspace.path().join("journal.sqlite");
        let meter_path = workspace.path().join("meter.sqlite");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(20);
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal"),
        );
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter"),
        );
        let parked = park_requeued_turn_on(Some(reset_at), journal, meter, None).await;
        let ParkedRequeue {
            _workspace,
            manager,
            ..
        } = parked;
        drop(manager);

        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal after restart"),
        );
        let (harness, _) = FakeHarness::new().with_send_count();
        let host = HostId::new("host_test");
        let clock = Arc::new(ManualClock::new());
        let manager = open_test_manager_with_clock(
            harness,
            host,
            journal,
            Some(Arc::clone(&clock) as Arc<dyn Clock>),
        )
        .await;
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter after restart"),
        );
        // Every attempt keeps failing, so only the wait decides a retry.
        meter.store().fail_next_limit_reads_for_tests(100);
        manager.set_meter(Arc::clone(&meter));

        manager.resync_meter_state().await;
        let reads = || meter.store().limit_reads_for_tests();
        assert_eq!(reads(), 1, "the first resync did not try the limit history");

        // A tick without advancing the clock does not retry.
        manager.inner.tick().await;
        assert_eq!(reads(), 1, "a tick retried before the backoff elapsed");

        // The first wait is one second.
        clock.advance_monotonic(Duration::from_secs(1));
        manager.inner.tick().await;
        assert_eq!(reads(), 2, "the retry did not run at the first backoff");

        // Each later wait doubles, capped at a minute.
        for wait in [2u64, 4, 8, 16, 32, 60, 60] {
            let before = reads();
            clock.advance_monotonic(Duration::from_secs(wait - 1));
            manager.inner.tick().await;
            assert_eq!(
                reads(),
                before,
                "a tick retried before the {wait}s wait elapsed"
            );
            clock.advance_monotonic(Duration::from_secs(1));
            manager.inner.tick().await;
            assert_eq!(reads(), before + 1, "the retry missed the {wait}s wait");
        }
        assert!(
            manager.inner.resync_pending.load(Ordering::Relaxed),
            "the test's ledger unexpectedly recovered"
        );
    }

    /// A failing ledger read does not rewrite every summary on every tick.
    ///
    /// Each retry used to clear every thread's cached signature, so a ledger
    /// that stayed unreadable re-appended and broadcast the whole inbox once a
    /// tick. Only a record whose restored state changed may be re-published,
    /// and the retry itself backs off.
    #[tokio::test]
    async fn a_failing_ledger_read_does_not_rebroadcast_every_summary() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let journal_path = workspace.path().join("journal.sqlite");
        let meter_path = workspace.path().join("meter.sqlite");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(20);
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal"),
        );
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter"),
        );
        let parked = park_requeued_turn_on(Some(reset_at), journal, meter, None).await;
        let ParkedRequeue {
            _workspace,
            manager,
            thread_id,
            ..
        } = parked;
        drop(manager);

        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal after restart"),
        );
        let (harness, _) = FakeHarness::new().with_send_count();
        let host = HostId::new("host_test");
        let clock = Arc::new(ManualClock::new());
        let manager = open_test_manager_with_clock(
            harness,
            host,
            journal,
            Some(Arc::clone(&clock) as Arc<dyn Clock>),
        )
        .await;
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter after restart"),
        );
        meter.store().fail_next_limit_reads_for_tests(16);
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();

        // The startup read fails its limit history but still binds the
        // schedule, so this one change is published.
        manager.resync_meter_state().await;
        assert!(
            manager.inner.resync_pending.load(Ordering::Relaxed),
            "the failed read did not leave a retry pending"
        );
        {
            let handle = manager
                .inner
                .handle(&thread_id)
                .await
                .expect("thread handle");
            let record = handle.lock().await;
            assert!(
                record.scheduled_limit.is_some(),
                "the failed limit read blocked the schedule-id restore"
            );
        }

        // Each tick retries after the backoff has passed and reads the same
        // ledger. Only the first tick publishes the one summary that changed.
        let mut upserts = 0;
        for wait in [1, 2, 4, 8] {
            clock.advance(chrono::Duration::seconds(wait));
            manager.inner.tick().await;
            while let Ok(wire) = events.try_recv() {
                if matches!(wire.event, Event::ThreadUpserted(_)) {
                    upserts += 1;
                }
            }
        }
        assert_eq!(
            upserts, 1,
            "a failing ledger read republished the inbox on every tick"
        );
        assert!(
            manager.inner.resync_pending.load(Ordering::Relaxed),
            "the test's ledger unexpectedly recovered"
        );
    }

    /// A fire only claims an attempt the thread's record names.
    ///
    /// A restarted daemon restores the parked prompt from the journal before
    /// the ledger is attached. A due schedule in that window must not be
    /// claimed: the attempt would be consumed and its resumed prompt could
    /// never run. Once the resync binds the schedule to the record, the same
    /// due attempt fires and delivers the parked prompt.
    #[tokio::test]
    async fn a_fire_before_the_resync_leaves_the_due_schedule_unclaimed() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let journal_path = workspace.path().join("journal.sqlite");
        let meter_path = workspace.path().join("meter.sqlite");
        // The provider reset has already passed, so the armed schedule is due
        // as soon as the ledger row exists.
        let reset_at = chrono::Utc::now() - chrono::Duration::minutes(1);
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal"),
        );
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter"),
        );
        let parked = park_requeued_turn_on(Some(reset_at), journal, meter, None).await;
        let ParkedRequeue {
            _workspace,
            manager,
            thread_id: _,
            second,
            ..
        } = parked;

        // The crash leaves the journal and the due ledger schedule behind.
        drop(manager);
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal after restart"),
        );
        let (harness, sends) = FakeHarness::new().with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager_with_journal(harness, host, journal).await;
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter after restart"),
        );
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();

        // A fire in the window before the resync: the record knows the parked
        // prompt but not the schedule that names it.
        *manager
            .inner
            .last_schedule_check
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        manager.inner.fire_due_schedules().await;
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(schedules.len(), 1, "the arm is the only ledger row");
        assert_eq!(
            schedules[0].state,
            ResumeState::Scheduled,
            "the fire claimed an attempt the record did not name: {:?}",
            schedules[0]
        );
        assert_eq!(
            schedules[0].attempt, 0,
            "the unmatched fire consumed an attempt"
        );
        assert_eq!(
            sends.get(),
            0,
            "the unmatched fire delivered a prompt it never owned"
        );

        // The resync binds the schedule to the restored record, so the same
        // due attempt now fires and delivers the parked prompt.
        manager.resync_meter_state().await;
        *manager
            .inner
            .last_schedule_check
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        manager.inner.fire_due_schedules().await;
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the resynced schedule delivered the restored prompt");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert_eq!(sends.get(), 1);
    }

    /// A failed startup ledger read is retried, and the due schedule still
    /// fires.
    ///
    /// The schedule ids are restored even when the limit history cannot be
    /// read, and the ticker retries the resync so the pause learns its reset
    /// and the due attempt is delivered instead of staying `Unnamed` forever.
    #[tokio::test]
    async fn a_failed_startup_ledger_read_is_retried_and_the_due_schedule_fires() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let journal_path = workspace.path().join("journal.sqlite");
        let meter_path = workspace.path().join("meter.sqlite");
        // Already due: the schedule fires as soon as a tick sees it.
        let reset_at = chrono::Utc::now() - chrono::Duration::minutes(1);
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal"),
        );
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter"),
        );
        let parked = park_requeued_turn_on(Some(reset_at), journal, meter, None).await;
        let ParkedRequeue {
            _workspace,
            manager,
            thread_id,
            second,
            ..
        } = parked;
        drop(manager);

        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal after restart"),
        );
        let (harness, sends) = FakeHarness::new().with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager_with_journal(harness, host, journal).await;
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter after restart"),
        );
        meter.store().fail_next_limit_reads_for_tests(1);
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();

        // The first resync cannot read the limit history, but the schedule id
        // still binds to the restored prompt.
        manager.resync_meter_state().await;
        let armed = meter.schedules().await.expect("read schedules");
        assert_eq!(armed.len(), 1, "the paused schedule survived the restart");
        let schedule_id = armed[0].schedule_id.clone();
        {
            let handle = manager
                .inner
                .handle(&thread_id)
                .await
                .expect("thread handle");
            let record = handle.lock().await;
            let scheduled = record
                .scheduled_limit
                .as_ref()
                .expect("the failed limit read still restored the pause");
            assert_eq!(
                scheduled.schedule_id.as_ref(),
                Some(&schedule_id),
                "the failed limit read blocked the schedule-id restore"
            );
            assert_eq!(
                scheduled.parked_turn.as_ref(),
                Some(&second.turn_id),
                "the restored pause lost the prompt it was holding"
            );
            assert_eq!(
                scheduled.reset_at, None,
                "the failed limit read somehow knew the reset"
            );
        }
        assert_eq!(
            meter.store().limit_reads_for_tests(),
            1,
            "the first resync read the limit history"
        );
        assert!(
            manager.inner.resync_pending.load(Ordering::Relaxed),
            "the failed read did not schedule a retry"
        );

        // The next tick retries the resync (which now succeeds) and fires the
        // due attempt for the restored prompt. The retry wait is time-based;
        // the test moves it aside rather than waiting.
        manager.inner.clear_resync_retry();
        *manager
            .inner
            .last_schedule_check
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        manager.inner.tick().await;
        assert_eq!(
            meter.store().limit_reads_for_tests(),
            2,
            "the tick did not retry the failed ledger read"
        );
        assert!(
            !manager.inner.resync_pending.load(Ordering::Relaxed),
            "the successful retry left a retry pending"
        );
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the due schedule never delivered the restored prompt");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert_eq!(sends.get(), 1);
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(
            schedules[0].state,
            ResumeState::Fired,
            "the retried resync left the due schedule unfired"
        );
    }

    /// A resync that cannot re-read one schedule row is retried.
    ///
    /// The startup pass reads the schedule list and the limit history, then
    /// fails on the per-row re-check that confirms the restored schedule before
    /// binding it. That failure leaves the restored prompt unnamed, so the
    /// ticker must treat the resync as unresolved and run the whole pass again
    /// instead of leaving the prompt waiting forever.
    #[tokio::test]
    async fn a_failed_per_schedule_reread_is_retried_and_the_due_schedule_fires() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let journal_path = workspace.path().join("journal.sqlite");
        let meter_path = workspace.path().join("meter.sqlite");
        // Already due: the schedule fires as soon as a tick sees it.
        let reset_at = chrono::Utc::now() - chrono::Duration::minutes(1);
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal"),
        );
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter"),
        );
        let parked = park_requeued_turn_on(Some(reset_at), journal, meter, None).await;
        let ParkedRequeue {
            _workspace,
            manager,
            thread_id,
            second,
            ..
        } = parked;
        drop(manager);

        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal after restart"),
        );
        let (harness, sends) = FakeHarness::new().with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager_with_journal(harness, host, journal).await;
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter after restart"),
        );
        // The list and the limit history read fine; only the per-row re-check
        // that confirms the restored schedule fails.
        meter.store().fail_next_schedule_queries_for_tests(1);
        manager.set_meter(Arc::clone(&meter));
        let mut events = manager.subscribe();

        manager.resync_meter_state().await;
        {
            let handle = manager
                .inner
                .handle(&thread_id)
                .await
                .expect("thread handle");
            let record = handle.lock().await;
            assert!(
                record.scheduled_limit.is_none(),
                "the failed re-check still bound the restored schedule"
            );
            assert!(
                record.parks_persisted_prompt(),
                "the restored prompt is not the durable parked one"
            );
        }
        assert!(
            manager.inner.resync_pending.load(Ordering::Relaxed),
            "the failed per-schedule re-check left no retry pending"
        );

        // The next tick retries the resync (which now succeeds) and fires the
        // due attempt for the restored prompt. The retry wait is time-based;
        // the test moves it aside rather than waiting.
        manager.inner.clear_resync_retry();
        *manager
            .inner
            .last_schedule_check
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        manager.inner.tick().await;
        assert!(
            !manager.inner.resync_pending.load(Ordering::Relaxed),
            "the successful retry left a retry pending"
        );
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the due schedule never delivered the restored prompt");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert_eq!(sends.get(), 1);
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(
            schedules[0].state,
            ResumeState::Fired,
            "the retried resync left the due schedule unfired"
        );
    }

    /// An older generation's report never overwrites a newer turn's marker.
    ///
    /// G+1 arms a pause for a reset, the person cancels it, G's delayed exit
    /// report repeats the reset and is cancelled too, and G+1's own completion
    /// repeats the reset one last time. The turn's marker still names G+1, so
    /// the completion must not arm the pause the person cancelled twice.
    #[tokio::test]
    async fn an_older_generations_report_does_not_consume_a_newer_turns_marker() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(10);
        let (harness, gate) = FakeHarness::new().with_later_completion_gate();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
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

        // A restart starts generation 2, which runs the second turn: its
        // completion is not gated because it is the session's first send.
        {
            let mut record = handle.lock().await;
            let generation = record.session.generation().expect("a live generation");
            record.detach_session(generation);
        }
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("generation 2 ran the second turn");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);

        // The third turn runs on generation 2 as well, and its completion is
        // held by the gate, so the turn stays active while the reports land.
        manager
            .send_message(&created.thread_id, "third".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(3).await;
        let session = {
            let record = handle.lock().await;
            assert_eq!(
                record.session.generation(),
                Some(2),
                "the active turn runs on generation 2"
            );
            assert!(
                record.pending_turn() || record.active.is_some(),
                "the third turn is still the active one"
            );
            assert_eq!(
                record.active_turn_generation(),
                Some(2),
                "the active turn belongs to generation 2"
            );
            record.live_session().cloned().expect("a live generation 2")
        };

        // G+1 arms its pause and the turn records the reset as applied.
        manager
            .inner
            .apply_completion_limit(&handle, 2, &session, Some(reset_at), true)
            .await;
        {
            let record = handle.lock().await;
            assert_eq!(
                record
                    .active
                    .as_ref()
                    .and_then(|active| active.applied_limit_reset),
                Some((2, Some(reset_at))),
                "the completion did not record the applied reset on its own turn"
            );
        }
        manager
            .cancel_resume(&created.thread_id)
            .await
            .expect("cancel the first pause");

        // G's delayed exit report repeats the same reset while G+1's turn is
        // still the active one, and is cancelled as well.
        {
            let mut record = handle.lock().await;
            let event = manager
                .inner
                .apply_limit_hit(&mut record, Some(1), Some(reset_at), true, manager.now())
                .await;
            assert!(event.is_none(), "the older generation armed a pause");
            assert_eq!(
                record
                    .active
                    .as_ref()
                    .and_then(|active| active.applied_limit_reset),
                Some((2, Some(reset_at))),
                "the older generation overwrote the newer turn's marker"
            );
        }
        manager
            .cancel_resume(&created.thread_id)
            .await
            .expect("cancel the older generation's pause");

        // G+1's completion repeats the reset: the turn already applied it, so
        // the cancelled pause must stay cancelled.
        manager
            .inner
            .apply_completion_limit(&handle, 2, &session, Some(reset_at), true)
            .await;
        let schedules = meter.schedules().await.expect("read schedules");
        assert!(
            !schedules.iter().any(|schedule| matches!(
                schedule.state,
                ResumeState::Scheduled | ResumeState::Waiting
            )),
            "the completion re-armed a pause the person cancelled: {schedules:?}"
        );
        assert!(
            handle.lock().await.scheduled_limit.is_none(),
            "the completion left a pause on the record"
        );
        gate.release();
    }

    /// A pause armed by another generation does not hold this retry.
    ///
    /// The failed write requeues its prompt, and a pause already names the
    /// thread — but not this dispatch. `advance_queue` would not park the turn
    /// for that pause, so the retry path must not skip the advance either:
    /// otherwise the prompt waits for a schedule that never delivers it.
    #[tokio::test]
    async fn a_pause_armed_by_another_generation_does_not_hold_the_retried_write() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(9);
        let (harness, gate) = FakeHarness::new().with_send_gate();
        let harness = harness.with_send_failure_after(1, "the harness rejected the message");
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
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

        // The second prompt's write is claimed on the live session and waits
        // on the gate; the adapter rejects it once the gate opens.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;

        // Another generation's pause names the thread but no dispatch.
        {
            let mut record = handle.lock().await;
            let event = manager
                .inner
                .apply_limit_hit(&mut record, Some(9), Some(reset_at), true, manager.now())
                .await;
            assert!(event.is_none(), "the other generation armed its pause");
            assert!(
                !record.pause_parks_turn(&second.turn_id),
                "the pause named a dispatch it was not armed for"
            );
        }
        {
            let mut record = handle.lock().await;
            let generation = record.session.generation().expect("the writing generation");
            record.detach_session(generation);
        }

        // The write fails and requeues its prompt. The pause does not name it,
        // so the retry must be delivered instead of waiting forever.
        gate.release();
        let (turn, _status) = tokio::time::timeout(Duration::from_secs(10), async {
            next_turn_completion(&mut events).await
        })
        .await
        .expect("the retried prompt waited on a pause that does not park it");
        assert_eq!(turn, second.turn_id);
        assert_eq!(sends.get(), 3, "the retry was never written");
        assert_eq!(
            sessions.get(),
            2,
            "the retry never started a replacement session"
        );
    }

    /// A typed limit on a retried write does not attach to a newer pause.
    ///
    /// The write fails with the adapter's typed limit while a pause a newer
    /// generation armed already names the thread. That pause is not this
    /// write's to redirect, so the retry must run without becoming its parked
    /// turn.
    #[tokio::test]
    async fn a_typed_limit_on_a_retried_write_does_not_attach_to_a_newer_pause() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(9);
        let newer_reset = chrono::Utc::now() + chrono::Duration::minutes(19);
        let (harness, gate) = FakeHarness::new().with_send_gate();
        let harness =
            harness.with_limit_failure_after(1, "provider usage limit reached", Some(reset_at));
        let (harness, sessions) = harness.with_session_count();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager(harness, host.clone()).await;
        let meter = Arc::new(h4rn_meter::MeterHub::in_memory().await.expect("meter"));
        manager.set_meter(Arc::clone(&meter));
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

        // The second prompt's write is claimed on the live session and waits
        // on the gate; the adapter reports a typed limit once it opens.
        let second = manager
            .send_message(&created.thread_id, "second".into(), Vec::new())
            .await
            .expect("send");
        sends.wait_for(2).await;

        // Another generation's pause names the thread before the write fails.
        {
            let mut record = handle.lock().await;
            let event = manager
                .inner
                .apply_limit_hit(&mut record, Some(9), Some(newer_reset), true, manager.now())
                .await;
            assert!(event.is_none(), "the other generation armed its pause");
        }
        let armed = meter.schedules().await.expect("read schedules");
        assert_eq!(armed.len(), 1);
        {
            let mut record = handle.lock().await;
            let generation = record.session.generation().expect("the writing generation");
            record.detach_session(generation);
        }

        // The write fails with its own typed limit. The newer generation's
        // pause is not this write's, so the retry runs instead of waiting.
        gate.release();
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the retried prompt waited on a pause that does not park it");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert_eq!(sends.get(), 3, "the retry was never written");
        assert_eq!(
            sessions.get(),
            2,
            "the retry never started a replacement session"
        );
        let schedules = meter.schedules().await.expect("read schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(
            schedules[0].schedule_id, armed[0].schedule_id,
            "the retry replaced the newer generation's pause"
        );
        assert_eq!(
            schedules[0].state,
            ResumeState::Scheduled,
            "the retry spent the newer generation's attempt"
        );
        {
            let record = handle.lock().await;
            assert_eq!(
                record
                    .scheduled_limit
                    .as_ref()
                    .and_then(|scheduled| scheduled.parked_turn.clone()),
                None,
                "the retry attached its prompt to the newer pause"
            );
        }
    }

    /// A replacement schedule keeps the prompt the pause was holding.
    ///
    /// A newer limit replaces the pause a failed write armed while its prompt
    /// is still the active, pending dispatch. The replacement must name that
    /// prompt too, or a pump-end advance would deliver it into the paused
    /// provider.
    #[tokio::test]
    async fn a_replacement_schedule_keeps_the_parked_turn() {
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(4);
        let newer_reset = chrono::Utc::now() + chrono::Duration::minutes(14);
        let parked = park_requeued_turn(Some(reset_at)).await;
        let ParkedRequeue {
            _workspace,
            manager,
            meter,
            mut events,
            handle,
            thread_id,
            second,
            sessions,
            sends,
            ..
        } = parked;

        // A newer generation's reset replaces the armed schedule.
        {
            let mut record = handle.lock().await;
            let event = manager
                .inner
                .apply_limit_hit(&mut record, Some(2), Some(newer_reset), true, manager.now())
                .await;
            assert!(event.is_none(), "the newer pause armed");
            let scheduled = record
                .scheduled_limit
                .as_ref()
                .expect("the newer pause replaced the schedule");
            assert_eq!(scheduled.reset_at, Some(newer_reset));
            assert_eq!(
                scheduled.parked_turn.as_ref(),
                Some(&second.turn_id),
                "the replacement stopped parking the requeued prompt"
            );
            assert!(
                record.pause_parks_turn(&second.turn_id),
                "the replacement pause no longer parks the prompt it replaced"
            );
        }

        // A pump-end advance must not deliver the prompt into the pause.
        manager.inner.advance_queue(handle.clone()).await;
        assert_eq!(
            sessions.get(),
            1,
            "the advance started a session into the paused provider"
        );
        assert_eq!(sends.get(), 2, "the advance wrote the parked prompt");

        // The replacement still delivers the prompt when it fires.
        manager
            .resume_now(&thread_id)
            .await
            .expect("the replacement fires");
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the replacement delivered the parked prompt");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert_eq!(sends.get(), 3);
        assert_eq!(meter.schedules().await.expect("read schedules").len(), 1);
    }

    /// A parked prompt released by a restart that lost its schedule runs.
    ///
    /// The journal still has the paused row but the ledger has no armed
    /// schedule. Nothing would deliver the restored prompt, so the resync
    /// releases it exactly as a cancel does: the release is recorded, the
    /// thread leaves the paused phase, and the prompt is delivered.
    #[tokio::test]
    async fn a_restart_without_the_schedule_releases_the_parked_prompt() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let journal_path = workspace.path().join("journal.sqlite");
        let meter_path = workspace.path().join("meter.sqlite");
        let reset_at = chrono::Utc::now() + chrono::Duration::minutes(15);
        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal"),
        );
        let meter = Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter"),
        );
        let parked = park_requeued_turn_on(Some(reset_at), journal, meter.clone(), None).await;
        let ParkedRequeue {
            _workspace,
            manager,
            thread_id,
            second,
            ..
        } = parked;
        drop(manager);

        // The schedule is gone before the restart: only the journal's parked
        // row survives.
        meter
            .cancel_schedule(&thread_id)
            .await
            .expect("drop the schedule");
        drop(meter);

        let journal = Arc::new(
            SqliteJournal::connect(&journal_path)
                .await
                .expect("journal after restart"),
        );
        let (harness, sessions) = FakeHarness::new().with_session_count();
        let (harness, sends) = harness.with_send_count();
        let host = HostId::new("host_test");
        let manager = open_test_manager_with_journal(harness, host, journal).await;
        manager.set_meter(Arc::new(
            h4rn_meter::MeterHub::open(&meter_path)
                .await
                .expect("meter after restart"),
        ));
        let mut events = manager.subscribe();
        manager.resync_meter_state().await;

        // The release is the same one a cancel records, so the journal holds
        // it and the prompt is no longer explained by a paused phase.
        let domain = manager
            .inner
            .journal
            .read_domain(&thread_id)
            .await
            .expect("read the domain log");
        assert!(
            domain
                .iter()
                .any(|event| matches!(event, ThreadEvent::LimitResumeReleased { .. })),
            "the restart released the prompt without recording the release"
        );
        {
            let handle = manager
                .inner
                .handle(&thread_id)
                .await
                .expect("thread handle");
            let record = handle.lock().await;
            assert_ne!(
                record.state.phase,
                h4rn_core::Phase::RateLimited,
                "the released thread is still shown as paused"
            );
            assert!(
                record.limit.is_none(),
                "the released thread keeps the limit that explained the wait"
            );
            assert!(
                record.meta.parked.is_none(),
                "the parked row was not cleared"
            );
        }

        // The released prompt is delivered instead of stranded.
        let (turn, status) =
            tokio::time::timeout(Duration::from_secs(10), next_turn_completion(&mut events))
                .await
                .expect("the released prompt was never delivered");
        assert_eq!(turn, second.turn_id);
        assert_eq!(status, TurnStatus::Completed);
        assert_eq!(sends.get(), 1);
        assert_eq!(sessions.get(), 1);
    }
}
