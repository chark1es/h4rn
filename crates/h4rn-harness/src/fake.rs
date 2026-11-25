//! A scriptable harness used by tests in this crate and downstream.
//!
//! `FakeHarness` does not pretend to speak a vendor protocol. It emits the
//! normalized [`HarnessEvent`] values directly, which is exactly what a
//! projection or lifecycle test needs, and lets a test assert the daemon's
//! response to any event sequence without a child process.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::{self, UnboundedSender, unbounded};
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use h4rn_protocol::{
    ApprovalDecision, HarnessKind, RewindMethod, SessionRewind, ToolCall, ToolCallStatus,
    TurnStatus, Usage,
};

use crate::{
    Harness, HarnessError, HarnessEvent, HarnessInstall, HarnessSession, LimitSignal, SessionSpec,
    TransportCloseKind, TransportHealth, UserInput,
};

/// The event producer for one fake session.
///
/// Production sessions read their transport into a bounded queue; a test that
/// wants the same back-pressure opts in with
/// [`FakeHarness::with_bounded_event_queue`]. Then a pump that stops draining
/// blocks the session's own `send`, exactly like a real adapter whose queue
/// filled up.
#[derive(Clone, Debug)]
enum EventSink {
    Unbounded(UnboundedSender<HarnessEvent>),
    Bounded(mpsc::Sender<HarnessEvent>),
}

impl EventSink {
    /// Queues an event without waiting; a full bounded queue drops it.
    ///
    /// Used from paths that cannot await (a sync trait method, a detached
    /// task). Tests that count events use the unbounded sink.
    fn try_send(&self, event: HarnessEvent) {
        match self {
            Self::Unbounded(sender) => {
                let _ = sender.unbounded_send(event);
            }
            Self::Bounded(sender) => {
                let _ = sender.clone().try_send(event);
            }
        }
    }

    /// Queues an event, waiting for room in a bounded queue.
    async fn send(&self, event: HarnessEvent) {
        match self {
            Self::Unbounded(sender) => {
                let _ = sender.unbounded_send(event);
            }
            Self::Bounded(sender) => {
                let _ = sender.clone().send(event).await;
            }
        }
    }
}

/// A test-only failure a fake session reports instead of answering a prompt.
#[derive(Clone, Debug)]
enum SendFailure {
    /// An ordinary transport failure. Its text is never parsed for a limit.
    Transport(String),
    /// A provider limit the adapter parsed itself, with its reset when known.
    RateLimited(LimitSignal),
}

/// A test's handle on the live fake session's event stream.
///
/// A real adapter keeps its event sender alive for as long as the daemon holds
/// the session, so the stream does not end when the agent dies. A test that
/// needs to report a death the way the adapter does (the generic stdout close,
/// then the exit status with the stderr it drained) pushes it through this
/// handle instead of ending the stream. It also carries the session's transport
/// health, so a test can model an agent that exited while its stream stays
/// open.
#[derive(Clone, Debug, Default)]
pub struct EventInjector(Arc<Mutex<Vec<InjectedSession>>>);

#[derive(Clone, Debug)]
struct InjectedSession {
    sink: EventSink,
    health: crate::TransportHealth,
}

impl EventInjector {
    /// Queues one event on the most recently started session's stream.
    pub fn emit(&self, event: HarnessEvent) {
        let sender = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last()
            .map(|session| session.sink.clone());
        if let Some(sender) = sender {
            sender.try_send(event);
        }
    }

    /// Queues one event on the most recently started session's stream, waiting
    /// for a bounded queue to have room.
    ///
    /// With a one-slot queue this returns only once the pump has taken the
    /// previous event, which is how a test orders an injected update before a
    /// later step without a wall-clock wait.
    pub async fn send(&self, event: HarnessEvent) {
        let sender = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last()
            .map(|session| session.sink.clone());
        if let Some(sender) = sender {
            sender.send(event).await;
        }
    }

    /// Queues one event on the stream of the session started at `session`
    /// (oldest first).
    ///
    /// A test uses this to report the exit of a session whose replacement has
    /// already been started: [`Self::emit`] would target the new one.
    pub fn emit_to(&self, session: usize, event: HarnessEvent) {
        let sender = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session)
            .map(|session| session.sink.clone());
        if let Some(sender) = sender {
            sender.try_send(event);
        }
    }

    /// Marks the most recently started session's transport closed, the way an
    /// agent that exited does, without ending its event stream.
    ///
    /// A test uses this so the daemon classifies the next completion as one on
    /// a closed transport, exactly as a real peer's exit does.
    pub fn close_transport(&self) {
        let health = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last()
            .map(|session| session.health.clone());
        if let Some(health) = health {
            health.close_with(crate::TransportCloseKind::Exit);
        }
    }
}

/// A fake harness adapter.
#[derive(Clone, Debug)]
pub struct FakeHarness {
    kind: HarnessKind,
    install: Option<HarnessInstall>,
    script: Option<Vec<HarnessEvent>>,
    /// Per-session scripts, consumed in order; the last one repeats.
    session_scripts: Option<Arc<SessionScripts>>,
    /// A test-only pause before a turn's input is answered.
    send_delay: Option<Duration>,
    /// A test-only latch before a turn's input is answered.
    send_gate: Option<Gate>,
    /// Which send [`Self::send_gate`] starts holding, counting from one.
    send_gate_after: usize,
    /// A test-only latch before a write directive's file appears.
    write_gate: Option<Gate>,
    /// A test-only count of prompts the session was asked to write.
    send_count: Option<SendCount>,
    /// A test-only count of messages steered into a running turn.
    steer_count: Option<SteerCount>,
    /// A test-only failure a steer reports instead of being accepted.
    steer_failure: Option<String>,
    /// A test-only refusal a steer reports before touching the transport.
    steer_rejection: Option<String>,
    /// A test-only definite pre-write failure a steer reports.
    steer_not_sent: Option<String>,
    /// A test-only count of interrupts the session received.
    interrupt_count: Option<InterruptCount>,
    /// Cancels still due to fail with a transient error (the session stays
    /// open), so a test can prove a second Stop retries the cancel.
    interrupt_failures: Option<Arc<AtomicUsize>>,
    /// A test-only latch before a session is returned from `start_session`.
    start_gate: Option<Gate>,
    /// When set with [`Self::start_gate`], only startups from this index on
    /// wait on it (0-based). `None` gates every startup.
    start_gate_after: Option<usize>,
    /// Startups seen so far, so [`Self::start_gate_after`] can count.
    starts_seen: Arc<AtomicUsize>,
    /// A test-only message: after the start gate, `start_session` fails.
    start_failure: Option<String>,
    /// A test-only message: a startup asked to resume fails with it, while a
    /// fresh startup succeeds. Models a harness that cannot find the session.
    resume_failure: Option<String>,
    /// A test-only message: a startup asked to resume starts and then dies
    /// before it reports a session, the way a real adapter's bad `--resume`
    /// fails after `start_session` returned `Ok`.
    resume_death: Option<String>,
    /// How many startups fail before one succeeds; `None` fails every one.
    start_failure_budget: Option<usize>,
    /// Startups attempted so far, so a budget can count down.
    start_attempts: Option<Arc<AtomicUsize>>,
    /// The model of every session this harness actually started.
    session_models: Option<SessionModels>,
    /// A test-only latch between a `send`'s prompt and its completion.
    completion_gate: Option<Gate>,
    /// Whether the completion gate also holds the session's first send. Off
    /// means only later turns are gated, so thread creation still completes.
    completion_gate_first_send: bool,
    /// A test-only count of sessions this harness has started.
    session_count: Option<SessionCount>,
    /// The settings each session started with, and every live switch applied.
    session_settings: Option<SessionSettings>,
    /// The MCP endpoint and bearer token each session was started with.
    session_mcp: Option<SessionMcp>,
    /// Model ids the fake session advertises, when a test asks for a catalogue.
    advertised_models: Option<Vec<String>>,
    /// The model a session reports it is running, when a test models an agent
    /// that restored its own model on resume.
    restored_model: Option<String>,
    /// Model ids that mean the agent's own default to the fake session.
    default_models: Vec<String>,
    /// Effects each model offers, so a model switch changes the catalogue.
    model_efforts: Option<BTreeMap<String, Vec<String>>>,
    /// A configured ACP agent id this fake harness accepts, when a test creates
    /// an ACP thread.
    instance: Option<String>,
    /// Whether a live session accepts in-place setting changes.
    switchable: bool,
    /// A test-only count of sessions this harness shut down.
    shutdown_count: Option<ShutdownCount>,
    /// A test-only switch: prompts after the first `n` fail as a dead transport.
    dead_sends_after: Option<usize>,
    /// Whether a typed limit failure also reports a closed transport, the way
    /// a write into a session whose agent just exited does.
    close_on_limit_failure: bool,
    /// A test-only switch: prompts after the first `n` fail with this
    /// (possibly untyped) error instead of being answered.
    send_failure_after: Option<(usize, SendFailure)>,
    /// A test-only latch before a session's event stream ends.
    stream_end_gate: Option<Gate>,
    /// A test-only latch before a dead send's trailing `TurnCompleted`.
    trailing_completion_gate: Option<Gate>,
    /// A test-only stderr tail the session reports with its failures.
    stderr: Option<String>,
    /// A test-only count of events a send streams before holding the session
    /// lock until its gate opens.
    streaming_send: Option<(usize, Gate)>,
    /// A test-only diagnostic the session parks on its stream just before a
    /// dead send's transport failure, and the gate that holds that failure.
    diagnostic_before_death: Option<(String, Gate)>,
    /// A test-only capacity for the session's event queue, so a pump that stops
    /// draining blocks the session's own `send`.
    event_queue_capacity: Option<usize>,
    /// Ends every session's event stream right after its handshake.
    immediate_stream_end: bool,
    /// Holds a dead send's failure until the returned gate opens, after the
    /// session has reported the transport death on its stream.
    generic_death_gate: Option<Gate>,
    /// A test's handle on the live session's event stream.
    event_injector: Option<EventInjector>,
    /// How this fake harness says it rewinds provider history.
    rewind_method: RewindMethod,
    /// Every rewind boundary a session was started with, when a test asked.
    rewind_specs: Option<Arc<Mutex<Vec<Option<SessionRewind>>>>>,
    /// The prompt id every tagged send carried, when a test asked.
    tagged_prompts: Option<TaggedPrompts>,
}

/// A shared count of prompts a fake session was asked to write.
///
/// A test reads it to prove the daemon did (or did not) dispatch a prompt to
/// the harness, without depending on event timing. It also notifies waiters, so
/// a test can wait for the arrival itself instead of spinning on the count.
#[derive(Clone, Debug, Default)]
pub struct SendCount {
    count: Arc<AtomicUsize>,
    notify: Arc<tokio::sync::Notify>,
}

impl SendCount {
    /// How many prompts have reached the fake session so far.
    pub fn get(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// Waits until at least `count` prompts have been written.
    pub async fn wait_for(&self, count: usize) {
        loop {
            if self.get() >= count {
                return;
            }
            // Register before re-checking: `note` wakes only waiters that are
            // already registered, so a prompt counted between the load above
            // and this await would otherwise be lost.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.get() >= count {
                return;
            }
            notified.await;
        }
    }

    /// Counts one more prompt and wakes the waiters.
    fn note(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// A shared count of steers a fake session was asked to deliver.
///
/// Same shape as [`SendCount`], kept separate so a test can say whether a
/// message was written into the running turn or queued for the next one.
#[derive(Clone, Debug, Default)]
pub struct SteerCount {
    count: Arc<AtomicUsize>,
    notify: Arc<tokio::sync::Notify>,
    texts: Arc<std::sync::Mutex<Vec<String>>>,
}

impl SteerCount {
    /// How many messages have been steered so far.
    pub fn get(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// The text of every steer, in the order the harness received it.
    pub fn texts(&self) -> Vec<String> {
        self.texts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Waits until at least `count` messages have been steered.
    pub async fn wait_for(&self, count: usize) {
        loop {
            if self.get() >= count {
                return;
            }
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.get() >= count {
                return;
            }
            notified.await;
        }
    }

    /// Counts one more steer and wakes the waiters.
    fn note(&self, text: &str) {
        self.texts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(text.to_owned());
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// A shared count of sessions a fake harness has started.
///
/// A test reads it to prove a dead session was replaced by a fresh one. It also
/// notifies waiters, so a test can wait for the start itself instead of spinning
/// on the count.
#[derive(Clone, Debug, Default)]
pub struct SessionCount {
    count: Arc<AtomicUsize>,
    notify: Arc<tokio::sync::Notify>,
}

impl SessionCount {
    /// How many sessions the fake harness has started so far.
    pub fn get(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// Waits until at least `count` sessions have been started.
    pub async fn wait_for(&self, count: usize) {
        loop {
            if self.get() >= count {
                return;
            }
            // Register before re-checking: `note` wakes only waiters that are
            // already registered, so a session counted between the load above
            // and this await would otherwise be lost.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.get() >= count {
                return;
            }
            notified.await;
        }
    }

    /// Counts one more session and wakes the waiters.
    fn note(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// A shared count of sessions a fake harness shut down.
///
/// A test reads it to prove a startup that lost its slot discarded the session
/// it made instead of attaching it. It also notifies waiters, so a test can wait
/// for the shutdown itself instead of spinning on the count.
#[derive(Clone, Debug, Default)]
pub struct ShutdownCount {
    count: Arc<AtomicUsize>,
    notify: Arc<tokio::sync::Notify>,
}

impl ShutdownCount {
    /// How many sessions the fake harness has shut down so far.
    pub fn get(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// Waits until at least `count` sessions have been shut down.
    pub async fn wait_for(&self, count: usize) {
        loop {
            if self.get() >= count {
                return;
            }
            // Register before re-checking: `note` wakes only waiters that are
            // already registered, so a shutdown counted between the load above
            // and this await would otherwise be lost.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.get() >= count {
                return;
            }
            notified.await;
        }
    }

    /// Counts one more shutdown and wakes the waiters.
    fn note(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// The model every session a fake harness started was asked to run.
///
/// A test reads it to prove a model change superseded an in-flight startup
/// instead of attaching the session that was booting the old model.
#[derive(Clone, Debug, Default)]
pub struct SessionModels(Arc<Mutex<Vec<String>>>);

impl SessionModels {
    /// The models of the sessions started so far, in order.
    pub fn get(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// One setting change a fake thread went through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingChange {
    /// A session started with these thread settings.
    Started {
        /// Model the session was started on.
        model: String,
        /// Permission mode the session was started on.
        runtime_mode: h4rn_protocol::RuntimeMode,
        /// Reasoning effort the session was started on.
        reasoning_effort: Option<String>,
        /// Whether the session was asked for the fast tier.
        fast: bool,
    },
    /// The live session was switched to a permission mode.
    Mode(h4rn_protocol::RuntimeMode),
    /// The live session was switched to a model.
    Model(String),
    /// The live session was switched to an effort (`None` is the default).
    Effort(Option<String>),
    /// The live session was switched on (or off) the fast tier.
    Fast(bool),
}

/// Every setting a fake thread started with, and every live switch it applied.
///
/// A test reads this to prove whether a change reached the live session or was
/// kept for the next start.
#[derive(Clone, Debug, Default)]
pub struct SessionSettings(Arc<Mutex<Vec<SettingChange>>>);

impl SessionSettings {
    /// Every recorded change, in the order the fake saw it.
    pub fn get(&self) -> Vec<SettingChange> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn note(&self, change: SettingChange) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(change);
    }
}

/// One session's MCP injection, as the adapter received it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpInjection {
    /// The endpoint handed to the harness.
    pub endpoint: Option<String>,
    /// The bearer token, when the daemon minted one.
    pub token: Option<String>,
}

/// A log of every session's MCP injection.
#[derive(Clone, Debug, Default)]
pub struct SessionMcp(Arc<Mutex<Vec<McpInjection>>>);

impl SessionMcp {
    /// Every injection, in startup order.
    pub fn get(&self) -> Vec<McpInjection> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn note(&self, injection: McpInjection) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(injection);
    }
}

/// A shared count of cancels a fake session was asked to send.
///
/// A test reads it to prove a Stop either dropped the prompt or cancelled one
/// that was already on the wire, never silently both or neither.
#[derive(Clone, Debug, Default)]
pub struct InterruptCount(Arc<AtomicUsize>);

impl InterruptCount {
    /// How many times the fake session's turn was interrupted so far.
    pub fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

/// A latch a test releases when the daemon should be allowed to proceed.
///
/// The fake harness uses it instead of a sleep, so a test proves an ordering
/// (for example "the request is answered before the adapter accepts input")
/// by the ordering itself rather than by a wall-clock guess.
#[derive(Clone, Debug, Default)]
pub struct Gate {
    released: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl Gate {
    /// A closed gate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens the gate for every waiter, now and later.
    pub fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Whether the gate is open.
    pub fn is_released(&self) -> bool {
        self.released.load(Ordering::SeqCst)
    }

    /// Waits until the gate opens.
    pub async fn wait(&self) {
        loop {
            if self.is_released() {
                return;
            }
            // Register before re-checking: `release` wakes only waiters that
            // are already registered, so a release between the load above and
            // this await would otherwise be lost.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_released() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Debug)]
struct SessionScripts {
    next: AtomicUsize,
    scripts: Vec<Vec<HarnessEvent>>,
}

impl SessionScripts {
    /// The next script, repeating the last one once the list is exhausted.
    fn next_script(&self) -> Vec<HarnessEvent> {
        if self.scripts.is_empty() {
            return Vec::new();
        }
        let index = self
            .next
            .fetch_add(1, Ordering::SeqCst)
            .min(self.scripts.len() - 1);
        self.scripts[index].clone()
    }
}

impl Default for FakeHarness {
    fn default() -> Self {
        Self {
            kind: HarnessKind::ClaudeCode,
            install: Some(HarnessInstall {
                path: PathBuf::from("/usr/local/bin/fake-harness"),
                version: "0.0.0-fake".into(),
            }),
            script: None,
            session_scripts: None,
            send_delay: None,
            send_gate: None,
            send_gate_after: 1,
            steer_count: None,
            steer_failure: None,
            steer_rejection: None,
            steer_not_sent: None,
            write_gate: None,
            send_count: None,
            interrupt_count: None,
            interrupt_failures: None,
            start_gate: None,
            start_gate_after: None,
            starts_seen: Arc::new(AtomicUsize::new(0)),
            start_failure: None,
            resume_failure: None,
            resume_death: None,
            start_failure_budget: None,
            start_attempts: None,
            session_models: None,
            session_settings: None,
            session_mcp: None,
            advertised_models: None,
            restored_model: None,
            default_models: Vec::new(),
            model_efforts: None,
            instance: None,
            switchable: true,
            completion_gate: None,
            completion_gate_first_send: true,
            session_count: None,
            shutdown_count: None,
            dead_sends_after: None,
            close_on_limit_failure: false,
            send_failure_after: None,
            stream_end_gate: None,
            trailing_completion_gate: None,
            stderr: None,
            streaming_send: None,
            diagnostic_before_death: None,
            event_queue_capacity: None,
            immediate_stream_end: false,
            generic_death_gate: None,
            event_injector: None,
            rewind_method: RewindMethod::Handoff,
            rewind_specs: None,
            tagged_prompts: None,
        }
    }
}

impl FakeHarness {
    /// A detected harness that echoes each message and completes the turn.
    pub fn new() -> Self {
        Self::default()
    }

    /// A harness that is not installed.
    pub fn undetected() -> Self {
        Self {
            install: None,
            ..Self::default()
        }
    }

    /// A harness that emits exactly [`events`] after `SessionStarted`.
    ///
    /// The script is replayed once per session.
    pub fn with_events(events: Vec<HarnessEvent>) -> Self {
        Self {
            script: Some(events),
            ..Self::default()
        }
    }

    /// A harness that emits a different script for each new turn.
    ///
    /// The first script runs when the session starts; later scripts run when
    /// the daemon sends another input, so a test can model "first turn hits a
    /// limit, the resumed turn succeeds". The last script repeats.
    pub fn with_session_scripts(scripts: Vec<Vec<HarnessEvent>>) -> Self {
        Self {
            session_scripts: Some(Arc::new(SessionScripts {
                next: AtomicUsize::new(0),
                scripts,
            })),
            ..Self::default()
        }
    }

    /// Overrides the adapter kind.
    pub fn with_kind(mut self, kind: HarnessKind) -> Self {
        self.kind = kind;
        self
    }

    /// Pauses before answering a turn's input (but not the priming send).
    ///
    /// A test uses this to model a slow harness whose first token arrives long
    /// after the request that started the turn.
    pub fn with_send_delay(mut self, delay: Duration) -> Self {
        self.send_delay = Some(delay);
        self
    }

    /// Holds every non-priming `send` until the returned [`Gate`] is released.
    ///
    /// A test uses this to prove that a request is answered before the adapter
    /// accepts the input: the request cannot have waited for a send that the
    /// test has not unblocked yet.
    pub fn with_send_gate(mut self) -> (Self, Gate) {
        let gate = Gate::new();
        self.send_gate = Some(gate.clone());
        self.send_gate_after = 1;
        (self, gate)
    }

    /// Holds every `send` after the first `sends` until the gate is released.
    ///
    /// A test uses this to block one later prompt in its write while the turns
    /// before it run normally: the daemon's first `send` primes the script, so
    /// a gate after `2` holds the third prompt written to a session.
    pub fn with_send_gate_after(mut self, sends: usize) -> (Self, Gate) {
        let gate = Gate::new();
        self.send_gate = Some(gate.clone());
        self.send_gate_after = sends;
        (self, gate)
    }

    /// Counts every prompt the daemon sends to the returned [`SendCount`].
    ///
    /// A test uses this to prove a turn was stopped before its prompt reached
    /// the harness, independently of event timing.
    pub fn with_send_count(mut self) -> (Self, SendCount) {
        let count = SendCount::default();
        self.send_count = Some(count.clone());
        (self, count)
    }

    /// Counts every message steered into a running turn.
    pub fn with_steer_count(mut self) -> (Self, SteerCount) {
        let count = SteerCount::default();
        self.steer_count = Some(count.clone());
        (self, count)
    }

    /// Makes every steer fail with this test-only transport error.
    ///
    /// A transport failure is unclear: the message may already be on the wire,
    /// so it is the shape a timeout has.
    pub fn with_steer_failure(mut self, message: impl Into<String>) -> Self {
        self.steer_failure = Some(message.into());
        self
    }

    /// Makes every steer refused with this test-only protocol error.
    ///
    /// A refusal is definite: the harness answered without taking the message,
    /// so a queued message may safely stay queued.
    pub fn with_steer_rejection(mut self, message: impl Into<String>) -> Self {
        self.steer_rejection = Some(message.into());
        self
    }

    /// Makes every steer fail as a definite pre-write failure.
    ///
    /// The adapter knows the frame never reached the transport, so the daemon
    /// must put the message back rather than reporting an unknown delivery.
    pub fn with_steer_not_sent(mut self, message: impl Into<String>) -> Self {
        self.steer_not_sent = Some(message.into());
        self
    }

    /// Holds a write directive's file write until the returned [`Gate`] is
    /// released, after the harness has announced the edit.
    ///
    /// A test releases it once it has seen the announced tool call, which the
    /// daemon journals only after it captured the pre-turn snapshot, so the
    /// change set is computed against the file's pre-turn bytes.
    pub fn with_write_gate(mut self) -> (Self, Gate) {
        let gate = Gate::new();
        self.write_gate = Some(gate.clone());
        (self, gate)
    }

    /// Holds every `start_session` until the returned [`Gate`] is released.
    ///
    /// A test uses this to model a harness that is slow to start, so Stop
    /// arrives while the thread has a turn but no session yet.
    pub fn with_start_gate(mut self) -> (Self, Gate) {
        let gate = Gate::new();
        self.start_gate = Some(gate.clone());
        (self, gate)
    }

    /// Holds every startup after the first `starts` until the gate is released.
    ///
    /// A test uses this to keep a replacement session from booting while it
    /// arranges the state that startup will find, without also holding the
    /// session the thread already has.
    pub fn with_start_gate_after(mut self, starts: usize) -> (Self, Gate) {
        let gate = Gate::new();
        self.start_gate = Some(gate.clone());
        self.start_gate_after = Some(starts);
        (self, gate)
    }

    /// Counts every session the daemon starts in the returned [`SessionCount`].
    pub fn with_session_count(mut self) -> (Self, SessionCount) {
        let count = SessionCount::default();
        self.session_count = Some(count.clone());
        (self, count)
    }

    /// Counts every session shutdown in the returned [`ShutdownCount`].
    pub fn with_shutdown_count(mut self) -> (Self, ShutdownCount) {
        let count = ShutdownCount::default();
        self.shutdown_count = Some(count.clone());
        (self, count)
    }

    /// Fails `start_session` with `message` after the start gate opens.
    ///
    /// The gate comes first so a test can order a Stop, a restart or a
    /// shutdown before the failure, which is the race this models.
    pub fn with_start_failure(mut self, message: impl Into<String>) -> Self {
        self.start_failure = Some(message.into());
        self
    }

    /// Fails only the first `start_session` with `message`; later ones succeed.
    ///
    /// A test uses this to prove a failed startup does not strand the turn that
    /// was queued behind it: the retry has to reach a working session.
    pub fn with_start_failure_once(mut self, message: impl Into<String>) -> Self {
        self.start_failure = Some(message.into());
        self.start_failure_budget = Some(1);
        self.start_attempts = Some(Arc::new(AtomicUsize::new(0)));
        self
    }

    /// Fails a startup that asks to resume, while a fresh startup succeeds.
    ///
    /// Models a harness that cannot find the saved session, so the daemon's
    /// fallback to a handoff is what the test observes.
    pub fn with_resume_failure(mut self, message: impl Into<String>) -> Self {
        self.resume_failure = Some(message.into());
        self
    }

    /// Starts a resume, then kills it before it reports a session.
    ///
    /// Models the real Claude Code adapter, whose `start_session` returns `Ok`
    /// once the child is spawned: a bad `--resume` exits afterwards, so the
    /// only report the daemon sees is the transport death.
    pub fn with_resume_death(mut self, message: impl Into<String>) -> Self {
        self.resume_death = Some(message.into());
        self
    }

    /// Records the model of every session this harness starts.
    pub fn with_session_models(mut self) -> (Self, SessionModels) {
        let models = SessionModels::default();
        self.session_models = Some(models.clone());
        (self, models)
    }

    /// Records each session's starting settings and every live switch it
    /// applies.
    pub fn with_session_settings(mut self) -> (Self, SessionSettings) {
        let settings = SessionSettings::default();
        self.session_settings = Some(settings.clone());
        (self, settings)
    }

    /// Records the MCP endpoint and token every session is started with.
    ///
    /// The daemon mints a token per harness session and passes it through the
    /// adapter; a test reads this to prove the injection happened and that the
    /// token is bound to the thread (ADR-044).
    pub fn with_mcp_injection(mut self) -> (Self, SessionMcp) {
        let mcp = SessionMcp::default();
        self.session_mcp = Some(mcp.clone());
        (self, mcp)
    }

    /// A live session that refuses in-place setting changes.
    ///
    /// The daemon must then keep the change for the next session start, which
    /// the settings log proves.
    pub fn with_unswitchable_settings(mut self) -> Self {
        self.switchable = false;
        self
    }

    /// Advertises a model catalogue from every fake session.
    ///
    /// A test uses this to prove the daemon validates a model change against
    /// what the live session actually offers.
    pub fn with_advertised_models(mut self, models: impl IntoIterator<Item = String>) -> Self {
        self.advertised_models = Some(models.into_iter().collect());
        self
    }

    /// Treats `alias` as the agent's own default model.
    ///
    /// A test uses this to prove a non-model change on a thread created on the
    /// alias is not refused as an unknown model. An ACP or Antigravity fake
    /// also answers an in-place switch to the alias with
    /// [`HarnessError::Deferred`], the way the real adapters do: only a fresh
    /// session can start on the agent's default.
    pub fn with_default_model(mut self, alias: impl Into<String>) -> Self {
        self.default_models.push(alias.into());
        self
    }

    /// Reports `model` as the one every session is running.
    ///
    /// A resumed agent keeps the model the conversation was using, which may
    /// not be the id the thread asked for. A test uses this to prove the daemon
    /// shows what the session really runs instead of the stale request.
    pub fn with_restored_model(mut self, model: impl Into<String>) -> Self {
        self.restored_model = Some(model.into());
        self
    }

    /// Gives each model its own reasoning-effort catalogue.
    ///
    /// The fake advertises these model ids, and a live model switch changes the
    /// efforts the session reports, so a test can prove the summary
    /// re-advertises them.
    pub fn with_model_efforts(
        mut self,
        efforts: impl IntoIterator<Item = (String, Vec<String>)>,
    ) -> Self {
        self.model_efforts = Some(efforts.into_iter().collect());
        self
    }

    /// Accepts `id` as a configured ACP agent.
    ///
    /// The daemon refuses an ACP thread that names no configured instance, so a
    /// test that creates one against this fake asks for this id.
    pub fn with_instance(mut self, id: impl Into<String>) -> Self {
        self.instance = Some(id.into());
        self
    }

    /// Ends the session's event stream once the returned gate is released.
    ///
    /// A test uses this to order "the agent exits after this turn" against
    /// something else it does first.
    pub fn with_stream_end_gate(mut self) -> (Self, Gate) {
        let gate = Gate::new();
        self.stream_end_gate = Some(gate.clone());
        (self, gate)
    }

    /// Emits a trailing `TurnCompleted{Failed}` on a dead send, once the
    /// returned gate is released.
    ///
    /// The real ACP adapter reports both the writer error and, after it, a
    /// failed completion from its stream. A test uses the gate to deliver that
    /// second report after the daemon has already settled the turn.
    pub fn with_trailing_failed_completion(mut self) -> (Self, Gate) {
        let gate = Gate::new();
        self.trailing_completion_gate = Some(gate.clone());
        (self, gate)
    }

    /// Reports this stderr tail from the session's failures.
    pub fn with_stderr_tail(mut self, tail: impl Into<String>) -> Self {
        self.stderr = Some(tail.into());
        self
    }

    /// Holds a turn's completion until the returned [`Gate`] is released.
    ///
    /// `send` still returns and its prompt still counts, but the synthetic
    /// `TurnCompleted` waits, so a Stop can be ordered after the prompt reached
    /// the harness without racing the completion.
    pub fn with_completion_gate(mut self) -> (Self, Gate) {
        let gate = Gate::new();
        self.completion_gate = Some(gate.clone());
        self.completion_gate_first_send = true;
        (self, gate)
    }

    /// Like [`Self::with_completion_gate`], but the first send of a session is
    /// not gated, so creating the thread still completes.
    pub fn with_later_completion_gate(mut self) -> (Self, Gate) {
        let gate = Gate::new();
        self.completion_gate = Some(gate.clone());
        self.completion_gate_first_send = false;
        (self, gate)
    }

    /// Counts every interrupt the daemon sends in the returned [`InterruptCount`].
    pub fn with_interrupt_count(mut self) -> (Self, InterruptCount) {
        let count = InterruptCount::default();
        self.interrupt_count = Some(count.clone());
        (self, count)
    }

    /// Makes the next cancel fail with a transient error, leaving the session
    /// open the way a slow or briefly unavailable harness does.
    ///
    /// A test uses this to prove a Stop the harness did not take is retried by
    /// the next Stop instead of being acknowledged as already stopped.
    pub fn with_interrupt_failure_once(mut self) -> Self {
        self.interrupt_failures = Some(Arc::new(AtomicUsize::new(1)));
        self
    }

    /// Lets the first `ok_sends` prompts succeed, then makes every later send
    /// fail as a dead transport whose session reports itself closed.
    ///
    /// A test uses this to model a prompt write that timed out and killed the
    /// transport: the daemon must detach the session and start a fresh one on
    /// the next message.
    pub fn with_dead_transport_after(mut self, ok_sends: usize) -> Self {
        self.dead_sends_after = Some(ok_sends);
        self
    }

    /// Lets the first `ok_sends` prompts succeed, then fails later sends with
    /// the typed limit *and* a transport that reports itself closed.
    ///
    /// A test uses this for a promoted prompt written into a session whose
    /// agent already exited with the reset on stderr: the prompt never reaches
    /// the provider, the write fails on the dead transport, and the limit on
    /// the failure is what must hold the queued turn.
    pub fn with_closed_limit_after(
        mut self,
        ok_sends: usize,
        message: impl Into<String>,
        reset_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Self {
        self.close_on_limit_failure = true;
        self.send_failure_after = Some((
            ok_sends,
            SendFailure::RateLimited(LimitSignal {
                message: message.into(),
                reset_at,
            }),
        ));
        self
    }

    /// Lets the first `ok_sends` prompts succeed, then fails every later send
    /// with an ordinary transport error whose text is `message`.
    ///
    /// A test uses this to prove a write error is not treated as a rate limit
    /// just because its text mentions one.
    pub fn with_send_failure_after(mut self, ok_sends: usize, message: impl Into<String>) -> Self {
        self.send_failure_after = Some((ok_sends, SendFailure::Transport(message.into())));
        self
    }

    /// Lets the first `ok_sends` prompts succeed, then fails every later send
    /// with the typed limit signal an adapter parses for itself.
    ///
    /// A test uses this to prove the daemon arms the resume only from the
    /// adapter's parsed limit, under the same ownership check as any failure.
    pub fn with_limit_failure_after(
        mut self,
        ok_sends: usize,
        message: impl Into<String>,
        reset_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Self {
        let message = message.into();
        self.send_failure_after = Some((
            ok_sends,
            SendFailure::RateLimited(LimitSignal { reset_at, message }),
        ));
        self
    }

    /// Streams `count` text deltas and a completed turn from `send`, then holds
    /// the send until the returned [`Gate`] is released.
    ///
    /// A test uses this to model a harness whose write does not return until
    /// its own event queue has been drained (Codex's `turn/start` reply comes
    /// after the events it may have queued): the daemon's pump has to apply
    /// every event, including the completion, while `send` still holds the
    /// session lock.
    pub fn with_streaming_send(mut self, count: usize) -> (Self, Gate) {
        let gate = Gate::new();
        self.streaming_send = Some((count, gate.clone()));
        (self, gate)
    }

    /// Parks `message` on the stream before a dead send's transport failure and
    /// holds the send until the returned [`Gate`] is released.
    ///
    /// A test releases the gate once it has seen the diagnostic's row, so the
    /// diagnostic is consumed before the death it precedes. A pump that waited
    /// for the session lock could never publish it while this send holds it.
    pub fn with_buffered_diagnostic(mut self, message: impl Into<String>) -> (Self, Gate) {
        let gate = Gate::new();
        self.diagnostic_before_death = Some((message.into(), gate.clone()));
        (self, gate)
    }

    /// Gives each session a bounded event queue of `capacity` events.
    ///
    /// The daemon's pump must keep draining while a write holds the session
    /// lock: with a bounded queue, a pump that waited for that lock would leave
    /// the queue full and block the write forever.
    pub fn with_bounded_event_queue(mut self, capacity: usize) -> Self {
        self.event_queue_capacity = Some(capacity.max(1));
        self
    }

    /// Ends every session's event stream right after its handshake.
    ///
    /// A test uses this to model an agent that exits immediately: no session
    /// ever accepts a prompt.
    pub fn with_immediate_stream_end(mut self) -> Self {
        self.immediate_stream_end = true;
        self
    }

    /// Reports a dead send's transport death on the stream and holds the send
    /// itself until the returned [`Gate`] is released.
    ///
    /// A test uses this to order the bridge's generic stdout-close notice
    /// before the failure that carries the agent's stderr.
    pub fn with_generic_death_gate(mut self) -> (Self, Gate) {
        let gate = Gate::new();
        self.generic_death_gate = Some(gate.clone());
        (self, gate)
    }

    /// Returns a handle that queues events on the live session's stream.
    ///
    /// A test uses this to report a transport death between turns the way a
    /// real adapter does, while the session (and so its event sender) stays
    /// alive: the stream never ends, so the daemon's row cannot wait for it.
    /// Advertises a native provider rewind.
    pub fn with_native_rewind(mut self) -> Self {
        self.rewind_method = RewindMethod::Native;
        self
    }

    /// Records the rewind boundary every session start was given.
    pub fn with_rewind_specs(mut self) -> (Self, RewindSpecs) {
        let specs = RewindSpecs::default();
        self.rewind_specs = Some(Arc::clone(&specs.0));
        (self, specs)
    }

    /// Records the prompt id every tagged send carried.
    pub fn with_tagged_prompts(mut self) -> (Self, TaggedPrompts) {
        let prompts = TaggedPrompts::default();
        self.tagged_prompts = Some(prompts.clone());
        (self, prompts)
    }

    /// Hands the test a handle on a live session's event stream.
    pub fn with_event_injector(mut self) -> (Self, EventInjector) {
        let injector = EventInjector::default();
        self.event_injector = Some(injector.clone());
        (self, injector)
    }
}

/// The rewind boundaries a fake session was started with, in start order.
#[derive(Clone, Debug, Default)]
pub struct RewindSpecs(Arc<Mutex<Vec<Option<SessionRewind>>>>);

impl RewindSpecs {
    /// Every boundary, oldest start first.
    pub fn get(&self) -> Vec<Option<SessionRewind>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// The prompt ids tagged sends carried, in send order.
#[derive(Clone, Debug, Default)]
pub struct TaggedPrompts(Arc<Mutex<Vec<Option<String>>>>);

impl TaggedPrompts {
    /// Every prompt id, oldest send first.
    pub fn get(&self) -> Vec<Option<String>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[async_trait]
impl Harness for FakeHarness {
    fn kind(&self) -> HarnessKind {
        self.kind
    }

    fn detect(&self) -> Option<HarnessInstall> {
        self.install.clone()
    }

    fn rewind_method(&self) -> RewindMethod {
        self.rewind_method
    }

    fn has_instance(&self, instance: &str) -> bool {
        self.instance.as_deref() == Some(instance)
    }

    async fn start_session(
        &self,
        spec: SessionSpec,
    ) -> Result<Box<dyn HarnessSession>, HarnessError> {
        if self.install.is_none() {
            return Err(HarnessError::NotDetected { kind: self.kind });
        }
        if let Some(count) = &self.session_count {
            count.note();
        }
        if let Some(specs) = &self.rewind_specs {
            specs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(spec.rewind.clone());
        }
        if let Some(models) = &self.session_models {
            models
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(spec.model.clone());
        }
        if let Some(mcp) = &self.session_mcp {
            mcp.note(McpInjection {
                endpoint: spec.mcp_endpoint.clone(),
                token: spec.env_overrides.get(crate::ENV_MCP_AUTH_TOKEN).cloned(),
            });
        }
        if let Some(settings) = &self.session_settings {
            settings.note(SettingChange::Started {
                model: spec.model.clone(),
                runtime_mode: spec.runtime_mode,
                reasoning_effort: spec.reasoning_effort.clone(),
                fast: spec.fast,
            });
        }
        let starts_seen = self.starts_seen.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.start_gate
            && self
                .start_gate_after
                .is_none_or(|after| starts_seen >= after)
        {
            gate.wait().await;
        }
        if let Some(message) = &self.start_failure {
            let fail = match (self.start_attempts.as_ref(), self.start_failure_budget) {
                (Some(attempts), Some(budget)) => attempts.fetch_add(1, Ordering::SeqCst) < budget,
                _ => true,
            };
            if fail {
                return Err(HarnessError::Transport {
                    harness: self.kind,
                    message: message.clone(),
                });
            }
        }
        if let Some(message) = &self.resume_failure
            && spec.resume_id.is_some()
        {
            return Err(HarnessError::Transport {
                harness: self.kind,
                message: message.clone(),
            });
        }
        let (sender, receiver): (EventSink, BoxStream<'static, HarnessEvent>) =
            match self.event_queue_capacity {
                Some(capacity) => {
                    let (sender, receiver) = mpsc::channel(capacity);
                    (EventSink::Bounded(sender), receiver.boxed())
                }
                None => {
                    let (sender, receiver) = unbounded();
                    (EventSink::Unbounded(sender), receiver.boxed())
                }
            };
        let session_id = spec
            .resume_id
            .clone()
            .unwrap_or_else(|| format!("fake-{}", uuid_like(&spec.workspace)));
        let dies_on_start = self
            .resume_death
            .as_ref()
            .filter(|_| spec.resume_id.is_some())
            .cloned();
        if let Some(message) = &dies_on_start {
            // The adapter returned `Ok` and the agent then exited before it
            // reported a session. Its stream carries only that death and then
            // ends, exactly like a child that failed to resume.
            sender
                .send(HarnessEvent::TransportClosed {
                    message: message.clone(),
                    retryable: true,
                    kind: TransportCloseKind::Exit,
                    reset_at: None,
                })
                .await;
        } else {
            sender
                .send(HarnessEvent::SessionStarted {
                    session_id: session_id.clone(),
                })
                .await;
        }
        let health = TransportHealth::new();
        if let Some(injector) = &self.event_injector {
            injector
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(InjectedSession {
                    sink: sender.clone(),
                    health: health.clone(),
                });
        }

        let session_scripts = self.session_scripts.clone();
        let primed = session_scripts.is_some();
        let initial = match &session_scripts {
            Some(scripts) => scripts.next_script(),
            None => self.script.clone().unwrap_or_default(),
        };
        let script = session_scripts.is_some() || self.script.is_some();
        if script && dies_on_start.is_none() {
            for event in &initial {
                sender.send(event.clone()).await;
            }
        }

        // A test can slow the echo turn down so a UI e2e has time to cancel a
        // live child. Normal daemon builds never set this.
        let env_delay = std::env::var("H4RN_FAKE_HARNESS_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_default();
        let delay = self.send_delay.unwrap_or(env_delay);

        Ok(Box::new(FakeSession {
            kind: self.kind,
            workspace: spec.workspace,
            session_id,
            sender: if dies_on_start.is_some() {
                None
            } else {
                Some(sender)
            },
            receiver: Some(receiver),
            script: self.script.clone(),
            session_scripts,
            settings: self.session_settings.clone(),
            advertised_models: self.advertised_models.clone(),
            restored_model: self.restored_model.clone(),
            default_models: self.default_models.clone(),
            model_efforts: self.model_efforts.clone(),
            current_model: spec.model.clone(),
            switchable: self.switchable,
            primed,
            delay,
            send_gate: self
                .send_gate
                .clone()
                .map(|gate| (self.send_gate_after, gate)),
            send_count: self.send_count.clone(),
            steer_count: self.steer_count.clone(),
            steer_failure: self.steer_failure.clone(),
            steer_rejection: self.steer_rejection.clone(),
            steer_not_sent: self.steer_not_sent.clone(),
            interrupt_count: self.interrupt_count.clone(),
            interrupt_failures: self.interrupt_failures.clone(),
            dead_sends_after: self.dead_sends_after,
            close_on_limit_failure: self.close_on_limit_failure,
            send_failure_after: self.send_failure_after.clone(),
            sends: 0,
            has_answered: false,
            write_gate: self.write_gate.clone(),
            completion_gate: self.completion_gate.clone(),
            completion_gate_first_send: self.completion_gate_first_send,
            interrupted: Arc::new(AtomicBool::new(false)),
            shutdown_count: self.shutdown_count.clone(),
            stream_end_gate: self.stream_end_gate.clone(),
            trailing_completion_gate: self.trailing_completion_gate.clone(),
            stderr: self.stderr.clone().unwrap_or_default(),
            closed: false,
            health,
            streaming_send: self.streaming_send.clone(),
            diagnostic_before_death: self.diagnostic_before_death.clone(),
            immediate_stream_end: self.immediate_stream_end,
            generic_death_gate: self.generic_death_gate.clone(),
            tagged_prompts: self.tagged_prompts.clone(),
        }))
    }
}

/// The session returned by [`FakeHarness`].
pub struct FakeSession {
    kind: HarnessKind,
    workspace: PathBuf,
    /// The id this session reported, so the daemon can tag provider boundaries
    /// with the session that recorded them.
    session_id: String,
    /// The stream's producer. `None` (or dropped) ends the stream after the
    /// events already queued.
    sender: Option<EventSink>,
    receiver: Option<BoxStream<'static, HarnessEvent>>,
    script: Option<Vec<HarnessEvent>>,
    session_scripts: Option<Arc<SessionScripts>>,
    /// Where setting changes are recorded, when a test asked for them.
    settings: Option<SessionSettings>,
    /// Model ids this session advertises, when a test asked for a catalogue.
    advertised_models: Option<Vec<String>>,
    /// The model the agent restored on resume, when a test asked for one.
    restored_model: Option<String>,
    /// Model ids that mean the agent's own default to this session.
    default_models: Vec<String>,
    /// Efforts each model offers, when a test asked for per-model catalogues.
    model_efforts: Option<BTreeMap<String, Vec<String>>>,
    /// The model this session runs, so its effort catalogue can follow it.
    current_model: String,
    /// Whether this session accepts in-place setting changes.
    switchable: bool,
    /// The daemon's first `send` is the input that started the scripted turn.
    primed: bool,
    /// Env-gated pause before an echo turn, for cancel-flow e2e tests.
    delay: Duration,
    /// A test-controlled latch on a turn's input, from this send on.
    send_gate: Option<(usize, Gate)>,
    /// A shared count of prompts this session was asked to write.
    send_count: Option<SendCount>,
    /// A shared count of messages steered into the running turn.
    steer_count: Option<SteerCount>,
    /// A test-only failure a steer reports.
    steer_failure: Option<String>,
    /// A test-only refusal a steer reports.
    steer_rejection: Option<String>,
    /// A test-only definite pre-write failure a steer reports.
    steer_not_sent: Option<String>,
    /// A shared count of cancels this session received.
    interrupt_count: Option<InterruptCount>,
    /// Cancels still due to fail with a transient error.
    interrupt_failures: Option<Arc<AtomicUsize>>,
    /// Sends after this many fail as a dead transport and close the session.
    dead_sends_after: Option<usize>,
    /// Whether a typed limit failure also reports a closed transport.
    close_on_limit_failure: bool,
    /// Sends after this many fail with this test-only failure.
    send_failure_after: Option<(usize, SendFailure)>,
    /// Prompts this session has been asked to write.
    sends: usize,
    /// Whether this session has already answered one send, so the gate only
    /// holds later turns and not the one that creates the thread.
    has_answered: bool,
    /// A test-controlled latch on a write directive's file write.
    write_gate: Option<Gate>,
    /// A test-controlled latch between the prompt and its completion.
    completion_gate: Option<Gate>,
    /// Whether that gate also holds the session's first send.
    completion_gate_first_send: bool,
    /// Whether a Stop already ended the current turn, so a gated completion
    /// must not publish a second one.
    interrupted: Arc<AtomicBool>,
    /// A shared count of shutdowns this session reported.
    shutdown_count: Option<ShutdownCount>,
    /// A test-controlled latch before the stream ends.
    stream_end_gate: Option<Gate>,
    /// A test-controlled latch before a dead send's trailing completion.
    trailing_completion_gate: Option<Gate>,
    /// The stderr tail this session reports.
    stderr: String,
    closed: bool,
    /// The lock-free death flag the daemon's pump reads without this session's
    /// lock.
    health: TransportHealth,
    /// A test-only count of events a send streams before waiting on its gate.
    streaming_send: Option<(usize, Gate)>,
    /// A test-only diagnostic parked on the stream before a dead send's
    /// transport failure, and the gate that holds that failure.
    diagnostic_before_death: Option<(String, Gate)>,
    /// Ends this session's event stream right after its handshake.
    immediate_stream_end: bool,
    /// Holds a dead send's failure until this gate opens.
    generic_death_gate: Option<Gate>,
    /// Where tagged prompt ids are recorded, when a test asked for them.
    tagged_prompts: Option<TaggedPrompts>,
}

#[async_trait]
impl HarnessSession for FakeSession {
    fn kind(&self) -> HarnessKind {
        self.kind
    }

    fn session_id(&self) -> Option<String> {
        Some(self.session_id.clone())
    }

    fn controls(&self) -> Option<crate::SessionControls> {
        // The fake advertises its harness kind's catalogue, so a test can ask
        // for the same modes, efforts and fast tier the real adapter offers.
        // It switches all of them in place unless the test asked otherwise.
        let base = crate::thread_controls(self.kind);
        let models = self.advertised_models.clone().or_else(|| {
            self.model_efforts
                .as_ref()
                .map(|efforts| efforts.keys().cloned().collect())
        });
        let efforts = self
            .model_efforts
            .as_ref()
            .and_then(|efforts| efforts.get(&self.current_model).cloned())
            .unwrap_or(base.efforts);
        Some(crate::SessionControls {
            modes: base.modes,
            models,
            // The fake runs the model it was started with; a test can ask for
            // an agent-restored one to model a resumed session.
            current_model: self.restored_model.clone(),
            default_models: self.default_models.clone(),
            efforts,
            fast: base.fast,
            switch_mode: self.switchable,
            switch_model: self.switchable,
            switch_effort: self.switchable,
            switch_fast: self.switchable,
        })
    }

    async fn steer(&mut self, input: UserInput) -> Result<(), HarnessError> {
        if self.closed {
            return Err(HarnessError::NotSent {
                harness: self.kind,
                message: "session is shut down".into(),
            });
        }
        if let Some(message) = &self.steer_failure {
            return Err(HarnessError::Transport {
                harness: self.kind,
                message: message.clone(),
            });
        }
        if let Some(message) = &self.steer_rejection {
            return Err(HarnessError::Protocol {
                harness: self.kind,
                message: message.clone(),
            });
        }
        if let Some(message) = &self.steer_not_sent {
            return Err(HarnessError::NotSent {
                harness: self.kind,
                message: message.clone(),
            });
        }
        if let Some(count) = &self.steer_count {
            count.note(&input.text);
        }
        Ok(())
    }

    async fn send(&mut self, input: UserInput) -> Result<(), HarnessError> {
        if self.closed {
            return Err(HarnessError::Transport {
                harness: self.kind,
                message: "session is shut down".into(),
            });
        }
        self.sends += 1;
        if let Some(count) = &self.send_count {
            count.note();
        }

        // The first send starts the thread; a test gates later turns so a
        // request that must not wait for the adapter stays observable. The gate
        // comes before the dead-transport switch, so a test can queue a
        // follow-up behind a write that then fails.
        let first_send = !self.has_answered;
        self.has_answered = true;
        let gate_completion =
            self.completion_gate.is_some() && (self.completion_gate_first_send || !first_send);
        if let Some((count, gate)) = self.streaming_send.clone() {
            // The write does not return until the daemon's pump has applied
            // every event it streamed, so a pump that waits for this session's
            // lock can never see the completion that ends the turn. With a
            // bounded event queue the streamed events also back-pressure: a
            // pump that stopped draining would leave this write blocked.
            let sender = self.sender.clone();
            if let Some(sender) = sender {
                for _ in 0..count {
                    sender
                        .send(HarnessEvent::TextDelta { text: "x".into() })
                        .await;
                }
                sender
                    .send(HarnessEvent::TurnCompleted {
                        status: TurnStatus::Completed,
                        usage: usage_for(&input.text),
                    })
                    .await;
            }
            gate.wait().await;
            return Ok(());
        }
        if let Some((after, gate)) = &self.send_gate
            && self.sends > *after
        {
            gate.wait().await;
        }

        if let Some((after, failure)) = self.send_failure_after.clone()
            && self.sends > after
        {
            if self.close_on_limit_failure && matches!(failure, SendFailure::RateLimited(_)) {
                self.closed = true;
                self.health.close();
            }
            return Err(match failure {
                SendFailure::Transport(message) => HarnessError::Transport {
                    harness: self.kind,
                    message,
                },
                SendFailure::RateLimited(limit) => {
                    let message = limit.message.clone();
                    HarnessError::RateLimited {
                        harness: self.kind,
                        message,
                        limit,
                    }
                }
            });
        }

        if let Some(limit) = self.dead_sends_after
            && self.sends > limit
        {
            // A write that failed mid-frame kills the transport; report it as
            // closed so the daemon detaches this session. A real adapter reports
            // the transport failure on its own stream, so the fake does too,
            // tagged as the transport death it is.
            self.closed = true;
            self.health.close();
            if let Some((message, gate)) = self.diagnostic_before_death.clone() {
                // The writer is already dead, and this diagnostic was buffered
                // before that: the daemon consumes it after the transport is
                // closed, exactly the order that used to let a diagnostic take
                // the transport failure's row. The send holds the session until
                // the test has seen this row.
                self.emit(HarnessEvent::Error {
                    message,
                    retryable: false,
                });
                gate.wait().await;
            }
            self.emit(HarnessEvent::TransportClosed {
                message: "the harness transport closed".into(),
                retryable: false,
                kind: TransportCloseKind::Write,
                reset_at: None,
            });
            if let Some(gate) = self.generic_death_gate.clone() {
                // The bridge's generic stdout-close notice is already on the
                // stream; the failure that carries the stderr waits until the
                // test has seen that notice consumed.
                gate.wait().await;
            }
            if let Some(gate) = self.trailing_completion_gate.clone() {
                let sender = self.sender.clone();
                tokio::spawn(async move {
                    gate.wait().await;
                    send_opt(
                        &sender,
                        HarnessEvent::TurnCompleted {
                            status: TurnStatus::Failed,
                            usage: Usage::default(),
                        },
                    );
                });
            }
            // The failure a dead write reports carries the agent's stderr, the
            // way the ACP prompt task appends its client's stderr tail.
            let message = if self.stderr.trim().is_empty() {
                "the harness transport closed".to_owned()
            } else {
                format!("the harness transport closed\n{}", self.stderr.trim())
            };
            return Err(HarnessError::Transport {
                harness: self.kind,
                message,
            });
        }

        // The priming send only announces the first script; it is not a turn
        // whose latency a test is modelling.
        if !self.primed && !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }

        if let Some(scripts) = &self.session_scripts {
            if self.primed {
                self.primed = false;
                return Ok(());
            }
            for event in scripts.next_script() {
                self.emit(event);
            }
            return Ok(());
        }

        if self.script.is_some() {
            // A scripted session is a complete fixture; `send` is accepted
            // (the harness contract allows it) but does not replay the script.
            return Ok(());
        }

        // Test convention: a user message may carry one write directive,
        // `[[write:relative/path]]<content>`, so daemon end-to-end tests can
        // produce a real working-tree change without a vendor harness.
        if let Some((path, _content)) = parse_write_directive(&input.text) {
            // Announce the edit first. The write itself runs on its own task
            // after a short pause: `send` returns immediately, which lets the
            // daemon's pump observe the running tool call and snapshot the
            // pre-turn bytes before the file exists. Writing inline here would
            // race that snapshot because the daemon holds the thread lock for
            // the whole `send` call.
            let now = chrono::Utc::now();
            let tool_call_id = format!("fake-edit-{now}");
            self.emit(HarnessEvent::ToolCall {
                tool_call: ToolCall {
                    tool_call_id: tool_call_id.clone(),
                    name: "Edit".into(),
                    status: ToolCallStatus::Running,
                    summary: path.to_owned(),
                    input: Some(serde_json::json!({ "path": path })),
                    output: None,
                    started_at: now,
                    completed_at: None,
                    stats: None,
                },
            });
            let sender = self.sender.clone();
            let workspace = self.workspace.clone();
            let usage = usage_for(&input.text);
            let text = input.text;
            let write_gate = self.write_gate.clone();
            tokio::spawn(async move {
                match write_gate {
                    // The test releases this once the announced edit is
                    // journaled, which is after the pre-turn snapshot.
                    Some(gate) => gate.wait().await,
                    None => tokio::time::sleep(std::time::Duration::from_millis(400)).await,
                }
                if let Some((written, stats)) = apply_write_directive(&workspace, &text) {
                    send_opt(
                        &sender,
                        HarnessEvent::ToolCall {
                            tool_call: ToolCall {
                                tool_call_id,
                                name: "Edit".into(),
                                status: ToolCallStatus::Completed,
                                summary: written.clone(),
                                input: None,
                                output: Some(format!("wrote {written}")),
                                started_at: now,
                                completed_at: Some(chrono::Utc::now()),
                                stats: Some(stats),
                            },
                        },
                    );
                    send_opt(
                        &sender,
                        HarnessEvent::TextDelta {
                            text: format!("wrote {written}"),
                        },
                    );
                }
                send_opt(
                    &sender,
                    HarnessEvent::TextDelta {
                        text: format!("echo: {text}"),
                    },
                );
                send_opt(
                    &sender,
                    HarnessEvent::Usage {
                        usage: usage.clone(),
                    },
                );
                send_opt(
                    &sender,
                    HarnessEvent::TurnCompleted {
                        status: TurnStatus::Completed,
                        usage,
                    },
                );
            });
            return Ok(());
        }

        self.emit(HarnessEvent::TextDelta {
            text: format!("echo: {}", input.text),
        });
        // Deterministic, plausible numbers so the UI's token, cost and
        // throughput surfaces have something real to render in e2e tests.
        let usage = usage_for(&input.text);
        self.emit(HarnessEvent::Usage {
            usage: usage.clone(),
        });
        // Tests opt into a limit with a prompt marker; the reset delay comes
        // from the environment so a test can keep the countdown on screen.
        if input.text.contains("[h4rn-limit]") {
            let reset_ms = std::env::var("H4RN_FAKE_LIMIT_MS")
                .ok()
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(60_000);
            self.emit(HarnessEvent::LimitHit {
                reset_at: Some(
                    chrono::Utc::now() + chrono::Duration::milliseconds(reset_ms.max(1)),
                ),
                auto_resume: true,
            });
        }
        self.complete(
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage,
            },
            gate_completion,
        );
        Ok(())
    }

    async fn send_tagged(
        &mut self,
        input: UserInput,
        prompt_id: Option<&str>,
    ) -> Result<(), HarnessError> {
        if let Some(prompts) = &self.tagged_prompts {
            prompts
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(prompt_id.map(str::to_owned));
        }
        self.send(input).await
    }

    fn provider_turn(&self) -> crate::ProviderTurn {
        match self.kind {
            HarnessKind::Codex => crate::ProviderTurn {
                turn_id: Some(format!("provider-turn-{}", self.sends)),
                last_message_id: None,
            },
            _ => crate::ProviderTurn {
                turn_id: None,
                last_message_id: (self.sends > 1)
                    .then(|| format!("provider-message-{}", self.sends - 1)),
            },
        }
    }

    async fn interrupt(&mut self) -> Result<(), HarnessError> {
        if let Some(count) = &self.interrupt_count {
            count.0.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(remaining) = &self.interrupt_failures {
            // A `&mut self` session sends one cancel at a time, so the
            // read-then-store cannot race another attempt.
            let left = remaining.load(Ordering::SeqCst);
            if left > 0 {
                remaining.store(left - 1, Ordering::SeqCst);
                // A transient cancel failure: the session (and with it the
                // transport) stays open, so the caller may try the cancel
                // again.
                return Err(HarnessError::Transport {
                    harness: self.kind,
                    message: "the cancel request timed out".into(),
                });
            }
        }
        self.interrupted.store(true, Ordering::SeqCst);
        self.emit(HarnessEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            usage: Usage::default(),
        });
        Ok(())
    }

    async fn respond_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
        option_id: Option<&str>,
    ) -> Result<(), HarnessError> {
        self.emit(HarnessEvent::TextDelta {
            text: match option_id {
                Some(option) => {
                    format!("approval {approval_id}: {decision:?} ({option})")
                }
                None => format!("approval {approval_id}: {decision:?}"),
            },
        });
        Ok(())
    }

    fn events(&mut self) -> BoxStream<'static, HarnessEvent> {
        let Some(receiver) = self.receiver.take() else {
            return futures::stream::empty().boxed();
        };
        // An agent that exits at startup delivers its handshake and nothing
        // else, so the daemon sees the stream close before it writes a prompt.
        let receiver: BoxStream<'static, HarnessEvent> = if self.immediate_stream_end {
            receiver.take(1).boxed()
        } else {
            receiver
        };
        // A test can hold the end of the stream open, so "the agent exited
        // after this turn" is ordered against everything else it does.
        match self.stream_end_gate.clone() {
            Some(gate) => receiver
                .take_until(async move { gate.wait().await })
                .boxed(),
            None => receiver,
        }
    }

    fn is_closed(&self) -> bool {
        self.closed
    }

    fn transport_health(&self) -> Option<TransportHealth> {
        Some(self.health.clone())
    }

    async fn shutdown(&mut self) -> Result<(), HarnessError> {
        self.closed = true;
        self.health.close();
        if let Some(count) = &self.shutdown_count {
            count.note();
        }
        // Dropping the sender would end the stream; keep it alive until the
        // session is dropped so an in-flight event is not lost.
        Ok(())
    }

    async fn set_runtime_mode(
        &mut self,
        mode: h4rn_protocol::RuntimeMode,
    ) -> Result<(), HarnessError> {
        if !self.switchable {
            return Err(HarnessError::Unsupported {
                harness: self.kind,
                feature: "switching the permission mode on a live session".into(),
            });
        }
        if let Some(settings) = &self.settings {
            settings.note(SettingChange::Mode(mode));
        }
        Ok(())
    }

    async fn set_model(&mut self, model: &str) -> Result<(), HarnessError> {
        if !self.switchable {
            return Err(HarnessError::Unsupported {
                harness: self.kind,
                feature: "switching the model on a live session".into(),
            });
        }
        // An ACP agent cannot go back to its own default alias in place (there
        // is no "reset to default" method), but a fresh session starts on that
        // default. The fake answers the same way so a daemon test exercises the
        // deferral the real adapter produces.
        if matches!(self.kind, HarnessKind::Acp | HarnessKind::Antigravity)
            && self
                .default_models
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(model))
        {
            return Err(HarnessError::Deferred {
                harness: self.kind,
                feature: "the agent's default model".into(),
            });
        }
        self.current_model = model.to_string();
        if let Some(settings) = &self.settings {
            settings.note(SettingChange::Model(model.to_string()));
        }
        Ok(())
    }

    async fn set_reasoning_effort(&mut self, effort: Option<&str>) -> Result<(), HarnessError> {
        if !self.switchable {
            return Err(HarnessError::Unsupported {
                harness: self.kind,
                feature: "switching reasoning effort on a live session".into(),
            });
        }
        if let Some(settings) = &self.settings {
            settings.note(SettingChange::Effort(effort.map(str::to_string)));
        }
        Ok(())
    }

    async fn set_fast(&mut self, fast: bool) -> Result<(), HarnessError> {
        if !self.switchable {
            return Err(HarnessError::Unsupported {
                harness: self.kind,
                feature: "switching the fast tier on a live session".into(),
            });
        }
        if let Some(settings) = &self.settings {
            settings.note(SettingChange::Fast(fast));
        }
        Ok(())
    }

    fn stderr_tail(&self) -> String {
        self.stderr.clone()
    }
}

impl FakeSession {
    /// Queues one event on this session's stream.
    fn emit(&self, event: HarnessEvent) {
        send_opt(&self.sender, event);
    }

    /// Sends a turn's completion, optionally behind the completion gate.
    ///
    /// A gated completion waits until the test releases it, and is dropped when
    /// a Stop already ended the turn, so the fake never reports a second end.
    fn complete(&self, event: HarnessEvent, gated: bool) {
        match &self.completion_gate {
            Some(gate) if gated => {
                let gate = gate.clone();
                let sender = self.sender.clone();
                let interrupted = Arc::clone(&self.interrupted);
                tokio::spawn(async move {
                    gate.wait().await;
                    if !interrupted.load(Ordering::SeqCst) {
                        send_opt(&sender, event);
                    }
                });
            }
            _ => self.emit(event),
        }
    }
}

fn send_opt(sender: &Option<EventSink>, event: HarnessEvent) {
    // A closed receiver means the consumer went away; that is not an error for
    // the fake session, and the daemon handles the same case by shutting down.
    if let Some(sender) = sender {
        sender.try_send(event);
    }
}

fn uuid_like(path: &std::path::Path) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!(
        "{}-{nanos}",
        path.file_name().unwrap_or_default().to_string_lossy()
    )
}

/// A minimal spec for tests.
pub fn test_spec() -> SessionSpec {
    SessionSpec {
        workspace: PathBuf::from("/tmp/h4rn-workspace"),
        model: "test-model".into(),
        runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
        reasoning_effort: None,
        fast: false,
        resume_id: None,
        mcp_endpoint: Some("http://127.0.0.1:9100/mcp".into()),
        env_overrides: BTreeMap::new(),
        attachments: Vec::new(),
        agent_id: None,
        rewind: None,
    }
}

/// Deterministic usage so the token, cost and throughput surfaces have
/// something real to render in e2e tests.
fn usage_for(text: &str) -> Usage {
    let output_tokens = (text.chars().count() as u64 / 4).max(8);
    Usage {
        input_tokens: 120 + text.chars().count() as u64 / 2,
        output_tokens,
        cached_input_tokens: 64,
        cost_micros: 0,
        duration_ms: 0,
        ..Usage::default()
    }
}

/// Applies the first `[[write:path]]content` directive in `text`.
///
/// Returns the relative path written. Traversal and absolute paths are
/// ignored: the fake harness must never write outside the session workspace.
fn parse_write_directive(text: &str) -> Option<(&str, &str)> {
    let start = text.find("[[write:")?;
    let rest = &text[start + "[[write:".len()..];
    let end = rest.find("]]")?;
    let relative = rest[..end].trim();
    if relative.is_empty()
        || relative.starts_with('/')
        || relative
            .split(['/', '\\'])
            .any(|component| component == ".." || component.is_empty())
    {
        return None;
    }
    let content = rest[end + 2..]
        .strip_prefix('\n')
        .unwrap_or(&rest[end + 2..]);
    Some((relative, content))
}

/// Writes the directive's file and returns its relative path with the real
/// diffstat against whatever was there before.
fn apply_write_directive(
    workspace: &Path,
    text: &str,
) -> Option<(String, h4rn_protocol::ToolStats)> {
    let (relative, content) = parse_write_directive(text)?;
    let path = workspace.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let before = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, content).ok()?;
    let (added, removed) = crate::stats::line_diff(&before, content);
    Some((
        relative.to_owned(),
        h4rn_protocol::ToolStats::Diff { added, removed },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Harness, HarnessSession, UserInput};

    async fn next(session: &mut dyn HarnessSession) -> HarnessEvent {
        session.events().next().await.expect("event")
    }

    #[tokio::test]
    async fn detects_and_starts_a_session() {
        let harness = FakeHarness::new();
        assert!(harness.detect().is_some());
        assert_eq!(harness.kind(), HarnessKind::ClaudeCode);

        let mut session = harness.start_session(test_spec()).await.unwrap();
        assert!(matches!(
            next(session.as_mut()).await,
            HarnessEvent::SessionStarted { .. }
        ));
    }

    #[tokio::test]
    async fn write_directive_edits_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = test_spec();
        spec.workspace = dir.path().to_path_buf();
        let mut session = FakeHarness::new().start_session(spec).await.unwrap();
        let mut events = session.events();
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::SessionStarted { .. })
        ));
        session
            .send(UserInput {
                text: "edit it [[write:src/lib.rs]]fn main() {}\n".into(),
                attachments: Vec::new(),
            })
            .await
            .unwrap();
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::ToolCall { .. })
        ));
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::ToolCall { .. })
        ));
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::TextDelta { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
            "fn main() {}\n"
        );
        assert!(
            parse_write_directive("[[write:../escape]]x").is_none(),
            "traversal is refused"
        );
    }

    #[tokio::test]
    async fn undetected_harness_refuses_to_start() {
        let result = FakeHarness::undetected().start_session(test_spec()).await;
        assert!(matches!(result, Err(HarnessError::NotDetected { .. })));
    }

    #[tokio::test]
    async fn send_echoes_and_completes_the_turn() {
        let mut session = FakeHarness::new().start_session(test_spec()).await.unwrap();
        let mut events = session.events();
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::SessionStarted { .. })
        ));

        session
            .send(UserInput {
                text: "hello".into(),
                attachments: Vec::new(),
            })
            .await
            .unwrap();

        assert_eq!(
            events.next().await,
            Some(HarnessEvent::TextDelta {
                text: "echo: hello".into()
            })
        );
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::Usage { .. })
        ));
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn interrupt_reports_an_interrupted_turn() {
        let mut session = FakeHarness::new().start_session(test_spec()).await.unwrap();
        let mut events = session.events();
        let _ = events.next().await;

        session.interrupt().await.unwrap();
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn approvals_are_answered_through_the_event_stream() {
        let mut session = FakeHarness::new().start_session(test_spec()).await.unwrap();
        let mut events = session.events();
        let _ = events.next().await;

        session
            .respond_approval("approval-1", ApprovalDecision::Approve, Some("opt_allow"))
            .await
            .unwrap();
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::TextDelta { text }) if text.contains("approval-1")
        ));
    }

    #[tokio::test]
    async fn scripted_events_are_replayed_in_order() {
        let script = vec![
            HarnessEvent::TextDelta { text: "one".into() },
            HarnessEvent::TextDelta { text: "two".into() },
        ];
        let mut session = FakeHarness::with_events(script)
            .start_session(test_spec())
            .await
            .unwrap();
        let mut events = session.events();
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::SessionStarted { .. })
        ));
        assert_eq!(
            events.next().await,
            Some(HarnessEvent::TextDelta { text: "one".into() })
        );
        assert_eq!(
            events.next().await,
            Some(HarnessEvent::TextDelta { text: "two".into() })
        );
    }

    #[tokio::test]
    async fn limit_marker_emits_a_limit_before_completion() {
        let mut session = FakeHarness::new().start_session(test_spec()).await.unwrap();
        let mut events = session.events();
        assert!(matches!(
            events.next().await,
            Some(HarnessEvent::SessionStarted { .. })
        ));

        session
            .send(UserInput {
                text: "[h4rn-limit]".into(),
                attachments: Vec::new(),
            })
            .await
            .unwrap();

        let mut saw_usage = false;
        let mut reset_at = None;
        loop {
            match events.next().await.expect("event") {
                HarnessEvent::Usage { .. } => saw_usage = true,
                HarnessEvent::LimitHit {
                    reset_at: at,
                    auto_resume,
                } => {
                    assert!(auto_resume);
                    reset_at = at;
                }
                HarnessEvent::TurnCompleted { status, .. } => {
                    assert_eq!(status, TurnStatus::Completed);
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_usage, "limit marker still reports usage");
        assert!(reset_at.is_some(), "limit carries a reset time");
    }

    #[tokio::test]
    async fn shutdown_rejects_further_sends() {
        let mut session = FakeHarness::new().start_session(test_spec()).await.unwrap();
        session.shutdown().await.unwrap();
        assert!(
            session
                .send(UserInput {
                    text: "late".into(),
                    attachments: Vec::new(),
                })
                .await
                .is_err()
        );
    }
}
