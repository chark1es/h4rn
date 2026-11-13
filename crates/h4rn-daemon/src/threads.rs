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
use h4rn_harness::{HarnessEvent, HarnessInstall, HarnessSession, SessionSpec, UserInput};
use h4rn_protocol::{
    AgentState, ApprovalDecision, ApprovalKind, Event, HarnessKind, HostId, Item, MessageRole,
    ProtoError, ProtoErrorCode, ThreadFilter, ThreadId, ThreadSummary, ThreadTab, ToolCallStatus,
    TurnId, TurnStatus, Usage,
};
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

use crate::journal::{Journal, ThreadMeta, WireEvent};

/// Capacity of the live event broadcast. A subscriber that falls further behind
/// than this is dropped with a resume hint rather than slowing other clients.
pub const BROADCAST_CAPACITY: usize = 512;

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
    local_host: HostId,
    options: ThreadManagerOptions,
    events: broadcast::Sender<WireEvent>,
    emit_lock: Mutex<()>,
    threads: Mutex<HashMap<String, ThreadHandle>>,
    ticker: std::sync::Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
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
    closed: bool,
}

struct ActiveTurn {
    turn_id: TurnId,
    item_id: String,
    text: String,
    usage: Usage,
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
    ) -> Result<Self, ManagerError> {
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let manager = Self {
            inner: Arc::new(Inner {
                journal,
                harnesses,
                local_host,
                options,
                events,
                emit_lock: Mutex::new(()),
                threads: Mutex::new(HashMap::new()),
                ticker: std::sync::Mutex::new(None),
                closed: AtomicBool::new(false),
            }),
        };
        manager.inner.restore().await?;
        Ok(manager)
    }

    /// Subscribes to the live projected event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<WireEvent> {
        self.inner.events.subscribe()
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
    pub async fn create_thread(&self, request: NewThread) -> Result<ThreadSummary, ManagerError> {
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

        let now = Utc::now();
        let thread_id = ThreadId::new_uuid_v7();
        let meta = ThreadMeta {
            thread_id: thread_id.clone(),
            host: request.host,
            harness: request.harness,
            title: title_from_prompt(&request.prompt, &request.workspace_path),
            workspace_path: request.workspace_path,
            model: request.model,
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
            Event::ItemCompleted(Item::Message {
                item_id: user_item_id(&turn_id),
                role: MessageRole::User,
                text: request.prompt.clone(),
            }),
            Event::TurnStarted {
                thread_id: thread_id.clone(),
                turn_id,
                started_at: now,
            },
        ];

        let handle: ThreadHandle = Arc::new(Mutex::new(record));
        let summary = {
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
                attachments: Vec::new(),
            }),
        );
        Ok(summary)
    }

    /// Appends a user message and starts or queues its turn.
    pub async fn send_message(
        &self,
        thread_id: &ThreadId,
        text: String,
    ) -> Result<(), ManagerError> {
        let handle = self.inner.handle(thread_id).await?;
        let mut record = handle.lock().await;
        if record.closed {
            return Err(ManagerError::NotFound(format!(
                "thread {thread_id} is closed"
            )));
        }

        let now = Utc::now();
        if record.active.is_some() {
            // A turn is running: the message waits its turn, and the thread
            // stays Working. `MessageReceived` records the user activity.
            record.queued.push_back(UserInput {
                text: text.clone(),
                attachments: Vec::new(),
            });
            let domain = vec![ThreadEvent::MessageReceived {
                source: h4rn_core::MessageSource::User,
                at: now,
            }];
            let wire = vec![Event::ItemCompleted(Item::Message {
                item_id: fresh_item_id("user"),
                role: MessageRole::User,
                text,
            })];
            self.inner.emit(&mut record, domain, wire).await?;
            return Ok(());
        }

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
            Event::ItemCompleted(Item::Message {
                item_id: user_item_id(&turn_id),
                role: MessageRole::User,
                text: text.clone(),
            }),
            Event::TurnStarted {
                thread_id: thread_id.clone(),
                turn_id,
                started_at: now,
            },
        ];
        self.inner.emit(&mut record, domain, wire).await?;

        let input = UserInput {
            text,
            attachments: Vec::new(),
        };
        if record.session.is_some() {
            drop(record);
            self.inner.deliver(handle, input).await;
        } else {
            drop(record);
            self.inner.spawn_session(handle, Some(input));
        }
        Ok(())
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
            Some(session) => Some(session.respond_approval(approval_id, decision).await),
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
                self.inner.emit(&mut record, domain, Vec::new()).await?;
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
        self.inner.emit(&mut record, domain, Vec::new()).await
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
        self.inner.emit(&mut record, domain, Vec::new()).await
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
    ) -> Result<ThreadSummary, ManagerError> {
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
        Ok(summary)
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
            let spec = SessionSpec {
                workspace: std::path::PathBuf::from(&record.meta.workspace_path),
                model: record.meta.model.clone(),
                resume_id: record.meta.resume_id.clone(),
                mcp_endpoint: None,
                env_overrides: std::collections::BTreeMap::new(),
                attachments: Vec::new(),
            };
            (harness, spec)
        };

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

            match event {
                HarnessEvent::SessionStarted { session_id } => {
                    record.meta.resume_id = Some(session_id);
                }
                HarnessEvent::TextDelta { text } => {
                    if let Some(active) = record.active.as_mut() {
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
                    let turn_id = record.turn_id_for_event();
                    wire.push(Event::ToolCall {
                        thread_id: thread_id.clone(),
                        turn_id,
                        tool_call: tool_call.clone(),
                    });
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
                    }
                }
                HarnessEvent::LimitHit {
                    reset_at,
                    auto_resume,
                } => {
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
                    let active = record.active.take();
                    let (turn_id, item_id, text, turn_usage) = match active {
                        Some(active) => (active.turn_id, active.item_id, active.text, active.usage),
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
                        turn_usage
                    } else {
                        usage
                    };
                    record.meta.usage.merge(&usage);
                    domain.push(ThreadEvent::TurnCompleted { status, at: now });
                    if !text.is_empty() {
                        wire.push(Event::ItemCompleted(Item::Message {
                            item_id,
                            role: MessageRole::Assistant,
                            text,
                        }));
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
        record.session = None;
        record.pump = None;
        if record.active.is_some() {
            let now = Utc::now();
            record.active = None;
            let turn_id = record.turn_id_for_event();
            let wire = vec![
                Event::Error {
                    thread_id: Some(record.meta.thread_id.clone()),
                    message: "the harness session ended before the turn completed".into(),
                    hint: Some("Send the message again to resume".into()),
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

    async fn tick(&self) {
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
    }
}

fn user_item_id(turn_id: &TurnId) -> String {
    format!("item_user_{}", turn_suffix(turn_id))
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
