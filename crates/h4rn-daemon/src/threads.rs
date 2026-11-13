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
    HarnessKind, HostId, Item, LimitSource, MessageRole, ProtoError, ProtoErrorCode,
    ResumeFailover, ResumeSchedule, ResumeState, RuntimeMode, ThreadFilter, ThreadId,
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

struct ThreadRecord {
    meta: ThreadMeta,
    state: ThreadState,
    active: Option<ActiveTurn>,
    last_turn_id: Option<TurnId>,
    session: Option<Box<dyn HarnessSession>>,
    pump: Option<JoinHandle<()>>,
    pending_approvals: BTreeMap<String, PendingApproval>,
    queued: VecDeque<UserInput>,
    last_signature: Option<SummarySignature>,
    /// A limit hit whose auto-resume schedule is armed for this thread.
    scheduled_limit: Option<ScheduledLimit>,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SummarySignature {
    tab: ThreadTab,
    pinned: bool,
    needs_attention: bool,
    unread: bool,
    agent_state: AgentState,
    snoozed_until: Option<DateTime<Utc>>,
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
        }
    }
}

impl ThreadRecord {
    fn summary(&self, settings: &LifecycleSettings) -> ThreadSummary {
        let now = Utc::now();
        let tab = tab_for(&self.state, now, settings);
        ThreadSummary {
            thread_id: self.meta.thread_id.clone(),
            host: self.meta.host.clone(),
            harness: self.meta.harness,
            title: self.meta.title.clone(),
            workspace_path: self.meta.workspace_path.clone(),
            model: self.meta.model.clone(),
            tab,
            pinned: self.state.pinned(),
            snoozed_until: is_snoozed(&self.state, now)
                .then_some(self.state.snoozed_until)
                .flatten(),
            last_activity: self.state.last_activity,
            agent_state: self.state.agent_state,
            usage: self.meta.usage.clone(),
            needs_attention: needs_attention(&self.state, now, settings),
            unread: tab == ThreadTab::Done && !self.state.acknowledged,
        }
    }

    fn signature(&self, settings: &LifecycleSettings) -> SummarySignature {
        SummarySignature::of(&self.summary(settings))
    }

    fn begin_turn(&mut self, _at: DateTime<Utc>) -> (TurnId, String) {
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
        let thread_id = ThreadId::new_uuid_v7();
        let resolved_attachments = self
            .inner
            .resolve_attachments(&thread_id, &request.attachments)
            .await?;
        let harness_attachments = harness_refs(&resolved_attachments);
        let meta = ThreadMeta {
            thread_id: thread_id.clone(),
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
            self.inner.deliver(handle, input).await;
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
        let mut record = handle.lock().await;
        if record.active.is_none() {
            return Err(ManagerError::Conflict(format!(
                "thread {thread_id} has no running turn"
            )));
        }
        let Some(session) = record.session.as_mut() else {
            return Err(ManagerError::Conflict(format!(
                "thread {thread_id} has no live harness session"
            )));
        };
        session
            .interrupt()
            .await
            .map_err(|error| ManagerError::Internal(error.to_string()))
    }

    /// Answers a pending approval or question.
    pub async fn respond_approval(
        &self,
        thread_id: &ThreadId,
        approval_id: &str,
        decision: ApprovalDecision,
        option_id: Option<String>,
        by_device: Option<DeviceId>,
    ) -> Result<(), ManagerError> {
        let handle = self.inner.handle(thread_id).await?;
        let mut record = handle.lock().await;
        let Some(pending) = record.pending_approvals.remove(approval_id) else {
            return Err(ManagerError::NotFound(format!(
                "approval {approval_id} is not pending on thread {thread_id}"
            )));
        };
        let kind = pending.kind;

        let response = match record.session.as_mut() {
            Some(session) => Some(
                session
                    .respond_approval(approval_id, decision, option_id.as_deref())
                    .await,
            ),
            None => None,
        };

        match response {
            None => {
                record
                    .pending_approvals
                    .insert(approval_id.to_owned(), pending);
                Err(ManagerError::Conflict(format!(
                    "thread {thread_id} lost its harness session"
                )))
            }
            Some(Err(error)) => {
                record
                    .pending_approvals
                    .insert(approval_id.to_owned(), pending);
                Err(ManagerError::Internal(error.to_string()))
            }
            Some(Ok(())) => {
                let now = Utc::now();
                let domain = if kind == ApprovalKind::Question {
                    vec![ThreadEvent::QuestionAnswered { at: now }]
                } else {
                    vec![ThreadEvent::ApprovalResolved { at: now }]
                };
                let wire = vec![Event::ApprovalResolved {
                    thread_id: thread_id.clone(),
                    approval_id: approval_id.to_owned(),
                    option_id,
                    by_device,
                }];
                self.inner.emit(&mut record, domain, wire).await?;
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
                }];
                let _ = self.inner.emit(&mut record, domain, wire).await;
            }
            if let Some(mut session) = record.session.take() {
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
        let mut record = handle.lock().await;
        if record.closed || self.closed.load(Ordering::Relaxed) {
            let _ = session.shutdown().await;
            return;
        }
        record.session = Some(session);
        let inner = Arc::clone(self);
        let pump_handle = handle.clone();
        record.pump = Some(tokio::spawn(async move {
            inner.run_pump(pump_handle, stream).await;
        }));

        if let Some(input) = first_input
            && let Some(session) = record.session.as_mut()
            && let Err(error) = session.send(input).await
        {
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
        let mut record = handle.lock().await;
        let Some(session) = record.session.as_mut() else {
            drop(record);
            self.spawn_session(handle, Some(input));
            return;
        };
        if let Err(error) = session.send(input).await {
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

            // `SessionStarted` normally carries the native id, but keep the
            // defaulted accessor in sync too so an adapter that only reports
            // it through `session_id` is still resumable after a restart.
            if record.meta.resume_id.is_none()
                && let Some(session_id) = record
                    .session
                    .as_ref()
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
                        if let Some(session) = record.session.as_ref() {
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
                                Ok(count) => tracing::debug!(
                                    thread_id = %thread_id,
                                    count,
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
                    record
                        .pending_approvals
                        .insert(approval_id.clone(), PendingApproval { kind });
                    if kind == ApprovalKind::Question {
                        domain.push(ThreadEvent::QuestionAsked { at: now });
                    } else {
                        domain.push(ThreadEvent::ApprovalRequested { at: now });
                    }
                    wire.push(Event::ApprovalRequested {
                        thread_id: thread_id.clone(),
                        turn_id,
                        approval_id,
                        kind,
                        prompt,
                        options,
                    });
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
                                Ok(_) => {
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
                    wire.push(Event::TurnCompleted {
                        thread_id: thread_id.clone(),
                        turn_id,
                        usage,
                        status,
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

            if completed {
                self.advance_queue(handle.clone()).await;
            }
        }

        let mut record = handle.lock().await;
        let stderr_tail = record
            .session
            .as_ref()
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
            self.deliver(handle, input).await;
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
            record.meta.model = choice.model.clone();
            schedule.failover = Some(ResumeFailover {
                model: choice.model,
                account: choice.account,
                reason: choice.reason,
            });
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
            self.deliver(handle, input).await;
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
            if record.last_signature == Some(signature) {
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
