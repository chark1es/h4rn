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
use h4rn_protocol::{ApprovalDecision, HarnessKind, ToolCall, ToolCallStatus, TurnStatus, Usage};

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
/// handle instead of ending the stream.
#[derive(Clone, Debug, Default)]
pub struct EventInjector(Arc<Mutex<Option<EventSink>>>);

impl EventInjector {
    /// Queues one event on the most recently started session's stream.
    pub fn emit(&self, event: HarnessEvent) {
        let sender = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(sender) = sender {
            sender.try_send(event);
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
    /// A test-only latch before a write directive's file appears.
    write_gate: Option<Gate>,
    /// A test-only count of prompts the session was asked to write.
    send_count: Option<SendCount>,
    /// A test-only count of interrupts the session received.
    interrupt_count: Option<InterruptCount>,
    /// Cancels still due to fail with a transient error (the session stays
    /// open), so a test can prove a second Stop retries the cancel.
    interrupt_failures: Option<Arc<AtomicUsize>>,
    /// A test-only latch before a session is returned from `start_session`.
    start_gate: Option<Gate>,
    /// A test-only message: after the start gate, `start_session` fails.
    start_failure: Option<String>,
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
    /// A test-only count of sessions this harness shut down.
    shutdown_count: Option<ShutdownCount>,
    /// A test-only switch: prompts after the first `n` fail as a dead transport.
    dead_sends_after: Option<usize>,
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
/// it made instead of attaching it.
#[derive(Clone, Debug, Default)]
pub struct ShutdownCount(Arc<AtomicUsize>);

impl ShutdownCount {
    /// How many sessions the fake harness has shut down so far.
    pub fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
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
            write_gate: None,
            send_count: None,
            interrupt_count: None,
            interrupt_failures: None,
            start_gate: None,
            start_failure: None,
            start_failure_budget: None,
            start_attempts: None,
            session_models: None,
            completion_gate: None,
            completion_gate_first_send: true,
            session_count: None,
            shutdown_count: None,
            dead_sends_after: None,
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

    /// Records the model of every session this harness starts.
    pub fn with_session_models(mut self) -> (Self, SessionModels) {
        let models = SessionModels::default();
        self.session_models = Some(models.clone());
        (self, models)
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
    pub fn with_event_injector(mut self) -> (Self, EventInjector) {
        let injector = EventInjector::default();
        self.event_injector = Some(injector.clone());
        (self, injector)
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
        if let Some(models) = &self.session_models {
            models
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(spec.model.clone());
        }
        if let Some(gate) = &self.start_gate {
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
        sender
            .send(HarnessEvent::SessionStarted { session_id })
            .await;
        if let Some(injector) = &self.event_injector {
            *injector
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sender.clone());
        }

        let session_scripts = self.session_scripts.clone();
        let primed = session_scripts.is_some();
        let initial = match &session_scripts {
            Some(scripts) => scripts.next_script(),
            None => self.script.clone().unwrap_or_default(),
        };
        let scripted = session_scripts.is_some() || self.script.is_some();
        if scripted {
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
            sender: Some(sender),
            receiver: Some(receiver),
            script: self.script.clone(),
            session_scripts,
            primed,
            delay,
            send_gate: self.send_gate.clone(),
            send_count: self.send_count.clone(),
            interrupt_count: self.interrupt_count.clone(),
            interrupt_failures: self.interrupt_failures.clone(),
            dead_sends_after: self.dead_sends_after,
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
            health: TransportHealth::new(),
            streaming_send: self.streaming_send.clone(),
            diagnostic_before_death: self.diagnostic_before_death.clone(),
            immediate_stream_end: self.immediate_stream_end,
            generic_death_gate: self.generic_death_gate.clone(),
        }))
    }
}

/// The session returned by [`FakeHarness`].
pub struct FakeSession {
    kind: HarnessKind,
    workspace: PathBuf,
    /// The stream's producer. `None` (or dropped) ends the stream after the
    /// events already queued.
    sender: Option<EventSink>,
    receiver: Option<BoxStream<'static, HarnessEvent>>,
    script: Option<Vec<HarnessEvent>>,
    session_scripts: Option<Arc<SessionScripts>>,
    /// The daemon's first `send` is the input that started the scripted turn.
    primed: bool,
    /// Env-gated pause before an echo turn, for cancel-flow e2e tests.
    delay: Duration,
    /// A test-controlled latch on a turn's input.
    send_gate: Option<Gate>,
    /// A shared count of prompts this session was asked to write.
    send_count: Option<SendCount>,
    /// A shared count of cancels this session received.
    interrupt_count: Option<InterruptCount>,
    /// Cancels still due to fail with a transient error.
    interrupt_failures: Option<Arc<AtomicUsize>>,
    /// Sends after this many fail as a dead transport and close the session.
    dead_sends_after: Option<usize>,
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
}

#[async_trait]
impl HarnessSession for FakeSession {
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
        if let Some(gate) = &self.send_gate
            && !first_send
        {
            gate.wait().await;
        }

        if let Some((after, failure)) = self.send_failure_after.clone()
            && self.sends > after
        {
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
            count.0.fetch_add(1, Ordering::SeqCst);
        }
        // Dropping the sender would end the stream; keep it alive until the
        // session is dropped so an in-flight event is not lost.
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
        resume_id: None,
        mcp_endpoint: Some("http://127.0.0.1:9100/mcp".into()),
        env_overrides: BTreeMap::new(),
        attachments: Vec::new(),
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
