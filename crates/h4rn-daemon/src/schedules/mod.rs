//! Scheduled tasks and webhook automations.
//!
//! The daemon owns one timer for every task. A task is stored in the journal
//! (so schedules, secrets and history survive a restart), the runner sleeps
//! until the next due instant instead of polling, and every run goes through
//! the existing [`ThreadManager`] entry points. Webhook deliveries render the
//! request into the prompt and start the same run path.
//!
//! Missed slots collapse into one run: a task that was due while the daemon
//! was down runs once at startup, then moves to its next real occurrence. A
//! run that is still working makes the next slot skip, and the skip is kept in
//! the bounded history.
//!
//! # One run per slot
//!
//! Every trigger goes through [`ScheduleService::admit`], which rechecks the
//! task under a per-task lock (still enabled, still due, nothing already in
//! flight) and reserves it before a harness is asked to start. A second
//! delivery for the same task is recorded as a skip, and a slot picked by the
//! clock before a pause is refused rather than dispatched.
//!
//! # One writer per task
//!
//! All task writes share a per-task lock and run bookkeeping re-reads the
//! current row before it commits. A rotation or an edit that races a run is
//! therefore never undone by the run's snapshot, and the run history and the
//! task's advanced clock are committed in one journal transaction.

mod math;
mod webhook;

pub use math::{
    CronExpr, MAX_INTERVAL_MINUTES, ScheduleError as ScheduleMathError, daemon_timezone, describe,
    next_after, parse_timezone, resolve_timezone,
};
pub use webhook::{
    MAX_RENDERED_PROMPT_BYTES, UNTRUSTED_NOTICE, WebhookRequest, placeholder_count, render_prompt,
};

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use h4rn_protocol::{
    HarnessKind, ProtoError, ProtoErrorCode, ScheduleDestination, ScheduleDraft, ScheduleEvent,
    ScheduleRun, ScheduleRunOutcome, ScheduleSpec, ScheduleTask, ScheduleTaskId,
    WEBHOOK_RATE_LIMIT_PER_MINUTE,
};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast};
use tokio::task::JoinHandle;

use crate::journal::{Journal, JournalError};
use crate::threads::{Clock, NewThread, ThreadManager};

/// How many runs one task keeps.
pub const HISTORY_LIMIT: usize = 100;

/// Longest task name the daemon accepts, in characters.
pub const MAX_NAME_CHARS: usize = 120;

/// Longest prompt the daemon accepts, in bytes.
pub const MAX_PROMPT_BYTES: usize = 256 * 1024;

/// How often a runner with nothing scheduled re-checks the clock.
///
/// A new task, an edit and a webhook wake the runner immediately; this only
/// covers a system clock or time-zone change on a daemon with no tasks.
const IDLE_WAKE: Duration = Duration::from_secs(3600);

/// First wait after a run that could not be recorded.
const RETRY_BASE_SECS: i64 = 30;

/// Longest wait after repeated recording failures.
const RETRY_CAP_SECS: i64 = 3600;

/// The prefix of a webhook task's public path.
pub const WEBHOOK_PREFIX: &str = "/webhooks/";

/// Why a schedule request failed.
#[derive(Debug)]
pub enum ScheduleServiceError {
    /// The task does not exist.
    NotFound(ScheduleTaskId),
    /// The fields cannot be saved as written.
    Invalid(String),
    /// The request conflicts with current state.
    Conflict(String),
    /// Durable storage failed.
    Internal(String),
}

impl std::fmt::Display for ScheduleServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "no scheduled task `{id}` exists on this daemon"),
            Self::Invalid(message) => f.write_str(message),
            Self::Conflict(message) => f.write_str(message),
            Self::Internal(message) => f.write_str(message),
        }
    }
}

impl ScheduleServiceError {
    /// Maps to the wire error, preserving a user-facing hint.
    pub fn into_proto(self) -> ProtoError {
        match self {
            Self::NotFound(id) => ProtoError::plain(
                ProtoErrorCode::NotFound,
                format!("no scheduled task `{id}` exists on this daemon"),
            ),
            Self::Invalid(message) => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                message,
                "Fix the highlighted field and save again",
            ),
            Self::Conflict(message) => ProtoError::plain(ProtoErrorCode::Conflict, message),
            Self::Internal(message) => ProtoError::plain(ProtoErrorCode::Internal, message),
        }
    }
}

impl From<crate::journal::JournalError> for ScheduleServiceError {
    fn from(error: crate::journal::JournalError) -> Self {
        Self::Internal(format!("could not save the scheduled task: {error}"))
    }
}

/// What the HTTP endpoint should answer for a webhook delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebhookOutcome {
    /// The run was accepted and starts asynchronously.
    Accepted,
    /// Another delivery for this task is starting; this one was recorded as a
    /// skip.
    Skipped,
    /// No enabled webhook task owns this path (or it was rotated away).
    NotFound,
    /// The task is over its per-minute delivery limit.
    RateLimited,
    /// The delivery was understood but cannot be used.
    Rejected(String),
}

/// The first half of a webhook delivery: the path was accepted, a rate slot
/// and the task were reserved, and the caller may now read the body.
///
/// Keeping this separate from [`ScheduleService::deliver_webhook`] is what lets
/// the HTTP handler reject a wrong path before it reads a single body byte.
pub enum WebhookStart {
    /// The delivery may proceed; read the request and hand it back.
    Start(Box<WebhookAdmission>),
    /// No enabled task owns the path.
    NotFound,
    /// The task is over its per-minute limit.
    RateLimited,
    /// A delivery for this task is already starting; this one was skipped.
    Skipped,
    /// The task or its template cannot accept a delivery.
    Rejected(String),
}

/// A reserved webhook delivery, holding the task's in-flight slot until it is
/// rendered and handed to the run path (or dropped).
pub struct WebhookAdmission {
    reservation: Reservation,
    task: ScheduleTask,
}

/// One service instance for the daemon.
#[derive(Clone)]
pub struct ScheduleService {
    inner: Arc<Inner>,
}

struct Inner {
    journal: Arc<dyn Journal>,
    manager: Arc<ThreadManager>,
    clock: Arc<dyn Clock>,
    host: h4rn_protocol::HostId,
    tasks: AsyncMutex<HashMap<ScheduleTaskId, ScheduleTask>>,
    /// One lock per task, serialising every read-modify-write of its row
    /// (edits, rotation, deletion and run bookkeeping).
    task_locks: Mutex<HashMap<ScheduleTaskId, Arc<AsyncMutex<()>>>>,
    /// Tasks with a run that is being admitted or started, so a second
    /// trigger cannot start the same slot.
    in_flight: Mutex<HashSet<ScheduleTaskId>>,
    /// Consecutive failed recordings per task, for the retry backoff.
    retries: Mutex<HashMap<ScheduleTaskId, u32>>,
    events: broadcast::Sender<ScheduleEvent>,
    wake: Notify,
    closed: AtomicBool,
    runner: Mutex<Option<JoinHandle<()>>>,
    /// Rolling delivery times per task, for the webhook rate limit.
    deliveries: AsyncMutex<HashMap<String, VecDeque<Instant>>>,
    #[cfg(test)]
    test: TestControl,
}

/// Test-only switches around the journal commit.
#[cfg(test)]
#[derive(Default)]
struct TestControl {
    fail_commit: AtomicBool,
}

impl ScheduleService {
    /// Opens the service and restores every stored task.
    ///
    /// `host` is the only host this daemon can start a thread on. Tasks naming
    /// another host are rejected at save time rather than failing at run time.
    pub async fn open(
        journal: Arc<dyn Journal>,
        manager: Arc<ThreadManager>,
        host: h4rn_protocol::HostId,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ScheduleServiceError> {
        let stored = journal.list_schedules().await?;
        let tasks = stored
            .into_iter()
            .map(|task| (task.task_id.clone(), task))
            .collect();
        let (events, _) = broadcast::channel(64);
        Ok(Self {
            inner: Arc::new(Inner {
                journal,
                manager,
                clock,
                host,
                tasks: AsyncMutex::new(tasks),
                task_locks: Mutex::new(HashMap::new()),
                in_flight: Mutex::new(HashSet::new()),
                retries: Mutex::new(HashMap::new()),
                events,
                wake: Notify::new(),
                closed: AtomicBool::new(false),
                runner: Mutex::new(None),
                deliveries: AsyncMutex::new(HashMap::new()),
                #[cfg(test)]
                test: TestControl::default(),
            }),
        })
    }

    /// Subscribes to task changes.
    ///
    /// Events carry a [`ScheduleTask`] whose secret webhook path is skipped in
    /// serialisation, so a view client can listen without learning it.
    pub fn subscribe(&self) -> broadcast::Receiver<ScheduleEvent> {
        self.inner.events.subscribe()
    }

    /// Every task, oldest first.
    pub async fn list(&self) -> Vec<ScheduleTask> {
        let tasks = self.inner.tasks.lock().await;
        let mut list: Vec<ScheduleTask> = tasks.values().cloned().collect();
        list.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.task_id.as_str().cmp(right.task_id.as_str()))
        });
        list
    }

    /// One task.
    pub async fn task(&self, task_id: &ScheduleTaskId) -> Option<ScheduleTask> {
        self.inner.tasks.lock().await.get(task_id).cloned()
    }

    /// A task's runs, newest first.
    pub async fn runs(
        &self,
        task_id: &ScheduleTaskId,
    ) -> Result<Vec<ScheduleRun>, ScheduleServiceError> {
        if !self.inner.tasks.lock().await.contains_key(task_id) {
            return Err(ScheduleServiceError::NotFound(task_id.clone()));
        }
        Ok(self
            .inner
            .journal
            .schedule_runs(task_id, HISTORY_LIMIT)
            .await?)
    }

    /// A webhook task's secret path, for an admin request.
    pub async fn webhook_url(
        &self,
        task_id: &ScheduleTaskId,
    ) -> Result<Option<String>, ScheduleServiceError> {
        let task = self
            .inner
            .tasks
            .lock()
            .await
            .get(task_id)
            .cloned()
            .ok_or_else(|| ScheduleServiceError::NotFound(task_id.clone()))?;
        Ok(task.webhook_path)
    }

    /// Creates a task from the editor's fields.
    pub async fn create(&self, draft: ScheduleDraft) -> Result<ScheduleTask, ScheduleServiceError> {
        let now = self.inner.clock.now();
        let task_id = ScheduleTaskId::new_uuid_v7();
        let task = self.prepare(task_id, draft, now, None)?;
        self.inner.journal.upsert_schedule(&task).await?;
        self.inner
            .tasks
            .lock()
            .await
            .insert(task.task_id.clone(), task.clone());
        let _ = self.inner.events.send(ScheduleEvent::Upserted {
            task: Box::new(task.clone()),
        });
        self.wake();
        Ok(task)
    }

    /// Replaces a task's editable fields.
    pub async fn update(
        &self,
        task_id: &ScheduleTaskId,
        draft: ScheduleDraft,
    ) -> Result<ScheduleTask, ScheduleServiceError> {
        let _write = self.write_lock(task_id).await;
        let now = self.inner.clock.now();
        let existing = self
            .inner
            .tasks
            .lock()
            .await
            .get(task_id)
            .cloned()
            .ok_or_else(|| ScheduleServiceError::NotFound(task_id.clone()))?;
        let task = self.prepare(task_id.clone(), draft, now, Some(&existing))?;
        self.inner.journal.upsert_schedule(&task).await?;
        self.inner
            .tasks
            .lock()
            .await
            .insert(task.task_id.clone(), task.clone());
        let _ = self.inner.events.send(ScheduleEvent::Upserted {
            task: Box::new(task.clone()),
        });
        self.wake();
        Ok(task)
    }

    /// Deletes a task and its run history.
    pub async fn delete(&self, task_id: &ScheduleTaskId) -> Result<(), ScheduleServiceError> {
        let _write = self.write_lock(task_id).await;
        if !self.inner.journal.delete_schedule(task_id).await? {
            return Err(ScheduleServiceError::NotFound(task_id.clone()));
        }
        self.inner.tasks.lock().await.remove(task_id);
        let _ = self.inner.events.send(ScheduleEvent::Removed {
            task_id: task_id.clone(),
        });
        self.wake();
        Ok(())
    }

    /// Enables or disables a task.
    ///
    /// Enabling recomputes the next run from now. That is not a catch-up: a
    /// task paused for a week resumes on its next real slot instead of firing
    /// immediately for every missed slot (there is only ever one catch-up, and
    /// it belongs to daemon downtime). Disabling clears `next_run_at`, because
    /// a paused task has no next run.
    pub async fn set_enabled(
        &self,
        task_id: &ScheduleTaskId,
        enabled: bool,
    ) -> Result<ScheduleTask, ScheduleServiceError> {
        let _write = self.write_lock(task_id).await;
        let now = self.inner.clock.now();
        let mut task = self
            .inner
            .tasks
            .lock()
            .await
            .get(task_id)
            .cloned()
            .ok_or_else(|| ScheduleServiceError::NotFound(task_id.clone()))?;
        task.enabled = enabled;
        task.updated_at = now;
        if enabled && !task.schedule.is_webhook() {
            if task.once_consumed && matches!(task.schedule, ScheduleSpec::Once { .. }) {
                return Err(ScheduleServiceError::Invalid(
                    "This once task has already run and cannot run again.".into(),
                ));
            }
            task.next_run_at = self.advance(&task, now)?;
            if task.next_run_at.is_none() && matches!(task.schedule, ScheduleSpec::Once { .. }) {
                return Err(ScheduleServiceError::Invalid(
                    "This once task has already passed its date and cannot run again.".into(),
                ));
            }
        } else {
            task.next_run_at = None;
        }
        self.inner.journal.upsert_schedule(&task).await?;
        self.inner
            .tasks
            .lock()
            .await
            .insert(task.task_id.clone(), task.clone());
        let _ = self.inner.events.send(ScheduleEvent::Upserted {
            task: Box::new(task.clone()),
        });
        self.wake();
        Ok(task)
    }

    /// Replaces a webhook task's URL path, returning the task and its new path.
    ///
    /// The path is returned from this admin-only call rather than stored in the
    /// task body, which never carries it.
    pub async fn rotate_webhook(
        &self,
        task_id: &ScheduleTaskId,
    ) -> Result<(ScheduleTask, Option<String>), ScheduleServiceError> {
        let _write = self.write_lock(task_id).await;
        let mut task = self
            .inner
            .tasks
            .lock()
            .await
            .get(task_id)
            .cloned()
            .ok_or_else(|| ScheduleServiceError::NotFound(task_id.clone()))?;
        if !task.schedule.is_webhook() {
            return Err(ScheduleServiceError::Invalid(
                "Only a webhook task has a URL to rotate.".into(),
            ));
        }
        task.webhook_path = Some(new_webhook_path());
        task.webhook_path_set = true;
        task.updated_at = self.inner.clock.now();
        self.inner.journal.upsert_schedule(&task).await?;
        self.inner
            .tasks
            .lock()
            .await
            .insert(task.task_id.clone(), task.clone());
        let _ = self.inner.events.send(ScheduleEvent::Upserted {
            task: Box::new(task.clone()),
        });
        Ok((task.clone(), task.webhook_path))
    }

    /// Runs a task once, outside its schedule.
    pub async fn run_now(
        &self,
        task_id: &ScheduleTaskId,
    ) -> Result<ScheduleRun, ScheduleServiceError> {
        let task = self
            .task(task_id)
            .await
            .ok_or_else(|| ScheduleServiceError::NotFound(task_id.clone()))?;
        if task.schedule.is_webhook() {
            return Err(ScheduleServiceError::Conflict(
                "A webhook task only runs when its URL is called.".into(),
            ));
        }
        if let Some(thread_id) = last_thread(&task)
            && self.inner.manager.thread_is_busy(&thread_id).await
        {
            return Err(ScheduleServiceError::Conflict(
                "The previous run is still working. Wait for it to finish.".into(),
            ));
        }
        match self.admit(task_id, Trigger::Manual, None).await {
            Ok((task, reservation)) => {
                let prompt = task.prompt.clone();
                self.execute(reservation, task, Trigger::Manual, prompt)
                    .await
            }
            Err(AdmissionError::Busy) => Err(ScheduleServiceError::Conflict(
                "A run for this task is already starting.".into(),
            )),
            Err(_) => Err(ScheduleServiceError::NotFound(task_id.clone())),
        }
    }

    /// The first half of a webhook delivery.
    ///
    /// This checks the secret path and takes a rate-limit slot before the
    /// caller reads the body, parses the query or copies headers. A wrong path
    /// therefore costs the daemon nothing but a hash comparison.
    pub async fn begin_webhook(&self, secret: &str) -> WebhookStart {
        let task = {
            let tasks = self.inner.tasks.lock().await;
            tasks
                .values()
                .find(|task| {
                    task.schedule.is_webhook()
                        && task.enabled
                        && task
                            .webhook_path
                            .as_deref()
                            .and_then(|path| path.strip_prefix(WEBHOOK_PREFIX))
                            .is_some_and(|stored| constant_time_eq(stored, secret))
                })
                .cloned()
        };
        let Some(task) = task else {
            return WebhookStart::NotFound;
        };
        if placeholder_count(&task.prompt) > h4rn_protocol::MAX_WEBHOOK_PLACEHOLDERS {
            return WebhookStart::Rejected(
                "The prompt has too many placeholders; edit the task.".into(),
            );
        }
        if !self.take_delivery_slot(&task.task_id).await {
            return WebhookStart::RateLimited;
        }
        match self.admit(&task.task_id, Trigger::Webhook, None).await {
            Ok((task, reservation)) => {
                WebhookStart::Start(Box::new(WebhookAdmission { reservation, task }))
            }
            Err(AdmissionError::Busy) => {
                self.record_skip(
                    &task.task_id,
                    None,
                    "Another delivery for this task was already starting.".into(),
                    Trigger::Webhook,
                )
                .await;
                WebhookStart::Skipped
            }
            Err(_) => WebhookStart::NotFound,
        }
    }

    /// Renders a reserved delivery and starts its run on its own task.
    ///
    /// The HTTP answer does not wait for a harness. The reservation is held
    /// until the run is recorded, so a second delivery that arrives meanwhile
    /// is skipped with a reason.
    pub async fn deliver_webhook(
        &self,
        admission: WebhookAdmission,
        request: WebhookRequest,
    ) -> WebhookOutcome {
        let prompt = match render_prompt(&admission.task.prompt, &request) {
            Ok(prompt) => prompt,
            Err(message) => return WebhookOutcome::Rejected(message),
        };
        let service = self.clone();
        let task = admission.task.clone();
        let reservation = admission.reservation;
        tokio::spawn(async move {
            if let Err(error) = service
                .execute(reservation, task, Trigger::Webhook, prompt)
                .await
            {
                tracing::warn!(error = %error, "webhook run failed to start");
            }
        });
        WebhookOutcome::Accepted
    }

    /// Runs every task whose next run is due, once each.
    ///
    /// The runner loop calls this; tests call it with a clock they control.
    pub async fn run_due(&self) {
        let now = self.inner.clock.now();
        let due: Vec<(ScheduleTaskId, DateTime<Utc>)> = {
            let tasks = self.inner.tasks.lock().await;
            tasks
                .values()
                .filter(|task| {
                    task.enabled
                        && !task.schedule.is_webhook()
                        && task.next_run_at.is_some_and(|at| at <= now)
                })
                .filter_map(|task| task.next_run_at.map(|at| (task.task_id.clone(), at)))
                .collect()
        };
        for (task_id, slot) in due {
            match self.admit(&task_id, Trigger::Scheduled, Some(slot)).await {
                Ok((task, reservation)) => {
                    let prompt = task.prompt.clone();
                    if let Err(error) = self
                        .execute(reservation, task, Trigger::Scheduled, prompt)
                        .await
                    {
                        tracing::warn!(task = %task_id, error = %error, "scheduled run failed to start");
                    }
                }
                Err(AdmissionError::Busy) => {
                    self.record_skip(
                        &task_id,
                        Some(slot),
                        "The previous run is still working.".into(),
                        Trigger::Scheduled,
                    )
                    .await;
                }
                // A pause, an edit or another runner already advanced this
                // slot. Nothing is dispatched.
                Err(_) => {}
            }
        }
    }

    /// The next instant any enabled task is due, if there is one.
    pub async fn next_due(&self) -> Option<DateTime<Utc>> {
        self.inner
            .tasks
            .lock()
            .await
            .values()
            .filter(|task| task.enabled && !task.schedule.is_webhook())
            .filter_map(|task| task.next_run_at)
            .min()
    }

    /// Starts the timer.
    pub fn start(&self) {
        let mut slot = lock(&self.inner.runner);
        if slot.is_some() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let handle = tokio::spawn(async move {
            loop {
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                if inner.closed.load(Ordering::Relaxed) {
                    break;
                }
                let service = ScheduleService {
                    inner: Arc::clone(&inner),
                };
                service.run_due().await;
                let now = inner.clock.now();
                let next = {
                    let tasks = inner.tasks.lock().await;
                    tasks
                        .values()
                        .filter(|task| task.enabled && !task.schedule.is_webhook())
                        .filter_map(|task| task.next_run_at)
                        .min()
                };
                let wait = next
                    .map(|at| (at - now).to_std().unwrap_or(Duration::ZERO))
                    .unwrap_or(IDLE_WAKE)
                    .max(Duration::from_millis(1));
                tokio::select! {
                    _ = inner.wake.notified() => {}
                    _ = tokio::time::sleep(wait) => {}
                }
            }
        });
        *slot = Some(handle);
    }

    /// Stops the timer.
    pub async fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        self.inner.wake.notify_one();
        let handle = lock(&self.inner.runner).take();
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }
    }

    /// Threads an enabled task continues, so worktree cleanup can keep them.
    pub async fn kept_threads(&self) -> HashSet<h4rn_protocol::ThreadId> {
        self.inner
            .tasks
            .lock()
            .await
            .values()
            .filter(|task| task.enabled)
            .filter_map(|task| task.thread_id.clone())
            .collect()
    }

    /// Records one delivery against the rolling limit, or refuses it.
    async fn take_delivery_slot(&self, task_id: &ScheduleTaskId) -> bool {
        let now = Instant::now();
        let cutoff = now - Duration::from_secs(60);
        let mut deliveries = self.inner.deliveries.lock().await;
        let entries = deliveries.entry(task_id.as_str().to_owned()).or_default();
        while entries.front().is_some_and(|at| *at < cutoff) {
            entries.pop_front();
        }
        if entries.len() >= WEBHOOK_RATE_LIMIT_PER_MINUTE as usize {
            return false;
        }
        entries.push_back(now);
        true
    }

    fn wake(&self) {
        self.inner.wake.notify_one();
    }

    /// Takes the per-task write lock.
    async fn write_lock(&self, task_id: &ScheduleTaskId) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = lock(&self.inner.task_locks);
            Arc::clone(
                locks
                    .entry(task_id.clone())
                    .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
            )
        };
        lock.lock_owned().await
    }

    /// Atomically rechecks eligibility and reserves a task for one trigger.
    async fn admit(
        &self,
        task_id: &ScheduleTaskId,
        trigger: Trigger,
        slot: Option<DateTime<Utc>>,
    ) -> Result<(ScheduleTask, Reservation), AdmissionError> {
        let _write = self.write_lock(task_id).await;
        let task = self
            .inner
            .tasks
            .lock()
            .await
            .get(task_id)
            .cloned()
            .ok_or(AdmissionError::NotFound)?;
        if !task.enabled {
            return Err(AdmissionError::Disabled);
        }
        match trigger {
            Trigger::Scheduled => {
                let expected = slot.ok_or(AdmissionError::NotDue)?;
                // The slot must still be the task's current one. A pause, an
                // edit or another runner may have moved it since the runner
                // collected its due list.
                if task.next_run_at != Some(expected) {
                    return Err(AdmissionError::StaleSlot);
                }
                if expected > self.inner.clock.now() {
                    return Err(AdmissionError::NotDue);
                }
            }
            Trigger::Webhook => {
                if !task.schedule.is_webhook() {
                    return Err(AdmissionError::NotFound);
                }
            }
            Trigger::Manual => {}
        }
        {
            let mut in_flight = lock(&self.inner.in_flight);
            if !in_flight.insert(task_id.clone()) {
                return Err(AdmissionError::Busy);
            }
        }
        // A previous run's thread may still be working even though this task
        // has no run in flight. Check it under the same admission so a webhook
        // answer can say "skipped" instead of "accepted".
        if let Some(thread_id) = last_thread(&task)
            && self.inner.manager.thread_is_busy(&thread_id).await
        {
            lock(&self.inner.in_flight).remove(task_id);
            return Err(AdmissionError::Busy);
        }
        Ok((
            task,
            Reservation {
                inner: Arc::clone(&self.inner),
                task_id: task_id.clone(),
            },
        ))
    }

    /// Starts or continues the right thread for one run, then records it.
    async fn execute(
        &self,
        _reservation: Reservation,
        task: ScheduleTask,
        trigger: Trigger,
        prompt: String,
    ) -> Result<ScheduleRun, ScheduleServiceError> {
        let started_at = self.inner.clock.now();
        let scheduled_for = match trigger {
            Trigger::Scheduled => task.next_run_at,
            Trigger::Manual | Trigger::Webhook => None,
        };
        if let Some(thread_id) = last_thread(&task)
            && self.inner.manager.thread_is_busy(&thread_id).await
        {
            let run = ScheduleRun {
                run_id: 0,
                task_id: task.task_id.clone(),
                scheduled_for,
                started_at,
                thread_id: Some(thread_id),
                outcome: ScheduleRunOutcome::Skipped,
                detail: Some("The previous run is still working.".into()),
            };
            return self.record(&task.task_id, run, trigger, started_at).await;
        }
        let result = self.start_thread(&task, prompt).await;
        let (thread_id, outcome, detail) = match result {
            Ok(thread_id) => (thread_id, ScheduleRunOutcome::Started, None),
            Err(error) => {
                let target = if task.destination == ScheduleDestination::NewThread {
                    None
                } else {
                    last_thread(&task)
                };
                (target, ScheduleRunOutcome::Failed, Some(error))
            }
        };
        let run = ScheduleRun {
            run_id: 0,
            task_id: task.task_id.clone(),
            scheduled_for,
            started_at,
            thread_id,
            outcome,
            detail,
        };
        let recorded = self.record(&task.task_id, run, trigger, started_at).await?;
        if outcome == ScheduleRunOutcome::Failed {
            return Err(ScheduleServiceError::Internal(
                recorded
                    .detail
                    .clone()
                    .unwrap_or_else(|| "The run could not start.".into()),
            ));
        }
        Ok(recorded)
    }

    /// Starts or continues the right thread for one run.
    ///
    /// A one-thread destination first reconciles the thread's permission mode
    /// and model with the task, so a run uses the settings the task was saved
    /// with rather than whatever the thread last switched to.
    async fn start_thread(
        &self,
        task: &ScheduleTask,
        prompt: String,
    ) -> Result<Option<h4rn_protocol::ThreadId>, String> {
        let open = |prompt: String| NewThread {
            thread_id: None,
            parent_thread_id: None,
            subagent_id: None,
            host: task.host.clone(),
            harness: task.harness,
            agent_id: task.agent_id.clone(),
            workspace_path: task.workspace_path.clone(),
            model: task.model.clone(),
            prompt,
            attachments: Vec::new(),
            runtime_mode: task.runtime_mode,
            reasoning_effort: None,
            fast: false,
            worktree: None,
        };
        let created = |outcome: crate::threads::CreatedThread| Ok(Some(outcome.thread_id));
        match task.destination {
            ScheduleDestination::NewThread => self
                .inner
                .manager
                .create_thread(open(prompt))
                .await
                .map(created)
                .map_err(|error| error.to_string())?,
            ScheduleDestination::ContinueThread => {
                if let Some(thread_id) = &task.thread_id {
                    self.reconcile_thread(thread_id, task).await?;
                    self.inner
                        .manager
                        .send_message(thread_id, prompt, Vec::new())
                        .await
                        .map(|accepted| Some(accepted.thread_id))
                        .map_err(|error| error.to_string())
                } else {
                    self.inner
                        .manager
                        .create_thread(open(prompt))
                        .await
                        .map(created)
                        .map_err(|error| error.to_string())?
                }
            }
            ScheduleDestination::FreshSession => {
                if let Some(thread_id) = &task.thread_id {
                    self.reconcile_thread(thread_id, task).await?;
                    self.inner
                        .manager
                        .send_message_fresh(thread_id, prompt)
                        .await
                        .map(|accepted| Some(accepted.thread_id))
                        .map_err(|error| error.to_string())
                } else {
                    self.inner
                        .manager
                        .create_thread(open(prompt))
                        .await
                        .map(created)
                        .map_err(|error| error.to_string())?
                }
            }
        }
    }

    /// Makes a continuing thread's settings match the task.
    async fn reconcile_thread(
        &self,
        thread_id: &h4rn_protocol::ThreadId,
        task: &ScheduleTask,
    ) -> Result<(), String> {
        let Some(meta) = self.inner.manager.thread_meta(thread_id).await else {
            return Ok(());
        };
        if meta.runtime_mode != task.runtime_mode {
            self.inner
                .manager
                .set_thread_runtime_mode(thread_id, task.runtime_mode)
                .await
                .map_err(|error| error.to_string())?;
        }
        if meta.model != task.model {
            self.inner
                .manager
                .set_thread_model(thread_id, task.model.clone())
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Records a slot that was skipped, with its reason.
    async fn record_skip(
        &self,
        task_id: &ScheduleTaskId,
        slot: Option<DateTime<Utc>>,
        detail: String,
        trigger: Trigger,
    ) -> Option<ScheduleRun> {
        let task = self.task(task_id).await?;
        let run = ScheduleRun {
            run_id: 0,
            task_id: task_id.clone(),
            scheduled_for: slot,
            started_at: self.inner.clock.now(),
            thread_id: last_thread(&task),
            outcome: ScheduleRunOutcome::Skipped,
            detail: Some(detail),
        };
        match self
            .record(task_id, run, trigger, self.inner.clock.now())
            .await
        {
            Ok(run) => Some(run),
            Err(error) => {
                tracing::warn!(%task_id, error = %error, "could not record a skipped slot");
                None
            }
        }
    }

    /// Appends a run, advances the task's clock and tells every client.
    ///
    /// The current row is re-read under the per-task lock, so only bookkeeping
    /// fields are written: an edit or rotation that happened while the run was
    /// starting is kept, never reverted to the run's snapshot.
    async fn record(
        &self,
        task_id: &ScheduleTaskId,
        mut run: ScheduleRun,
        trigger: Trigger,
        now: DateTime<Utc>,
    ) -> Result<ScheduleRun, ScheduleServiceError> {
        let _write = self.write_lock(task_id).await;
        // A task deleted while its run was starting must not be resurrected by
        // the run's own bookkeeping.
        let Some(mut task) = self.inner.tasks.lock().await.get(task_id).cloned() else {
            return Ok(run);
        };
        if task.destination != ScheduleDestination::NewThread && task.thread_id.is_none() {
            task.thread_id = run.thread_id.clone();
        }
        if matches!(trigger, Trigger::Scheduled)
            && run.scheduled_for.is_some()
            && task.next_run_at == run.scheduled_for
        {
            // A once slot is consumed even if recording has to be retried:
            // the thread already started, so this slot must never run twice
            // (including the second occurrence of a repeated hour).
            if matches!(task.schedule, ScheduleSpec::Once { .. }) {
                task.once_consumed = true;
                task.next_run_at = None;
            } else {
                match self.advance(&task, now) {
                    Ok(next) => task.next_run_at = next,
                    Err(error) => return Err(self.back_off(task, now, error.to_string()).await),
                }
            }
        }
        task.last_run = Some(run.clone());
        if let Err(error) = self.commit(&mut run, &task).await {
            return Err(self
                .back_off(task, now, format!("The run could not be saved: {error}"))
                .await);
        }
        lock(&self.inner.retries).remove(task_id);
        self.inner
            .tasks
            .lock()
            .await
            .insert(task.task_id.clone(), task.clone());
        let _ = self.inner.events.send(ScheduleEvent::Upserted {
            task: Box::new(task),
        });
        Ok(run)
    }

    /// Commits the run and the task together, with the test-only journal hook.
    async fn commit(&self, run: &mut ScheduleRun, task: &ScheduleTask) -> Result<(), JournalError> {
        #[cfg(test)]
        if self.inner.test.fail_commit.swap(false, Ordering::SeqCst) {
            return Err(JournalError::Database(sqlx::Error::Protocol(
                "test: the journal commit failed".into(),
            )));
        }
        self.inner
            .journal
            .commit_schedule_run(run, task, HISTORY_LIMIT)
            .await
    }

    /// Moves a task to a growing backoff instead of retrying every millisecond.
    ///
    /// The in-memory row always advances, so the runner cannot hot-loop even
    /// when the journal is the thing that is failing; the journal write is a
    /// best effort that preserves the backoff across a restart.
    async fn back_off(
        &self,
        mut task: ScheduleTask,
        now: DateTime<Utc>,
        reason: String,
    ) -> ScheduleServiceError {
        let failures = {
            let mut retries = lock(&self.inner.retries);
            let failures = retries.entry(task.task_id.clone()).or_insert(0);
            *failures = failures.saturating_add(1);
            *failures
        };
        tracing::warn!(
            task = %task.task_id,
            failures,
            %reason,
            "a scheduled run could not be recorded; backing off"
        );
        if matches!(task.schedule, ScheduleSpec::Once { .. }) {
            // The run already started a thread. Re-arming the same once slot
            // would start a second thread for it.
            task.once_consumed = true;
            task.next_run_at = None;
        } else {
            task.next_run_at = Some(now + backoff_for(failures));
        }
        self.inner
            .tasks
            .lock()
            .await
            .insert(task.task_id.clone(), task.clone());
        if let Err(error) = self.inner.journal.upsert_schedule(&task).await {
            tracing::warn!(task = %task.task_id, %error, "could not persist the backoff");
        }
        let _ = self.inner.events.send(ScheduleEvent::Upserted {
            task: Box::new(task),
        });
        ScheduleServiceError::Internal(reason)
    }

    /// The next instant after `now` for a task's schedule.
    fn advance(
        &self,
        task: &ScheduleTask,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, ScheduleServiceError> {
        if task.once_consumed && matches!(task.schedule, ScheduleSpec::Once { .. }) {
            return Ok(None);
        }
        let tz = parse_timezone(&task.timezone).map_err(|error| {
            ScheduleServiceError::Internal(format!("the task has an invalid time zone: {error}"))
        })?;
        next_after(&task.schedule, tz, task.created_at, now).map_err(|error| {
            ScheduleServiceError::Internal(format!("the schedule has no next run: {error}"))
        })
    }

    /// Validates a draft and builds the stored task.
    fn prepare(
        &self,
        task_id: ScheduleTaskId,
        draft: ScheduleDraft,
        now: DateTime<Utc>,
        existing: Option<&ScheduleTask>,
    ) -> Result<ScheduleTask, ScheduleServiceError> {
        let name = draft.name.trim();
        if name.is_empty() {
            return Err(ScheduleServiceError::Invalid(
                "Give the task a name so it can be recognized in the list.".into(),
            ));
        }
        if name.chars().count() > MAX_NAME_CHARS {
            return Err(ScheduleServiceError::Invalid(format!(
                "A task name is at most {MAX_NAME_CHARS} characters."
            )));
        }
        if name.contains(['\n', '\r']) {
            return Err(ScheduleServiceError::Invalid(
                "A task name must be one line.".into(),
            ));
        }
        if draft.host != self.inner.host {
            return Err(ScheduleServiceError::Invalid(format!(
                "Scheduled tasks run on this daemon's host ({}).",
                self.inner.host
            )));
        }
        let workspace = draft.workspace_path.trim();
        if workspace.is_empty() || !std::path::Path::new(workspace).is_absolute() {
            return Err(ScheduleServiceError::Invalid(
                "The workspace must be an absolute path on this host.".into(),
            ));
        }
        let model = draft.model.trim();
        if model.is_empty() {
            return Err(ScheduleServiceError::Invalid("Choose a model.".into()));
        }
        let prompt = draft.prompt.trim();
        if prompt.is_empty() {
            return Err(ScheduleServiceError::Invalid(
                "Write the prompt the task should send.".into(),
            ));
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(ScheduleServiceError::Invalid(format!(
                "A prompt is at most {} KiB.",
                MAX_PROMPT_BYTES / 1024
            )));
        }
        let agent_id = match draft.harness {
            HarnessKind::Acp => {
                let agent = draft.agent_id.as_deref().map(str::trim).unwrap_or_default();
                if agent.is_empty() {
                    return Err(ScheduleServiceError::Invalid(
                        "An ACP task must name a configured agent.".into(),
                    ));
                }
                Some(agent.to_owned())
            }
            _ => None,
        };
        let timezone = if draft.timezone.trim().is_empty() {
            math::daemon_timezone().1
        } else {
            draft.timezone.trim().to_owned()
        };
        let tz = parse_timezone(&timezone)
            .map_err(|error| ScheduleServiceError::Invalid(error.message().to_owned()))?;
        math::validate(&draft.schedule)
            .map_err(|error| ScheduleServiceError::Invalid(error.message().to_owned()))?;
        if draft.schedule.is_webhook()
            && placeholder_count(prompt) > h4rn_protocol::MAX_WEBHOOK_PLACEHOLDERS
        {
            return Err(ScheduleServiceError::Invalid(format!(
                "A webhook prompt may use at most {} placeholders.",
                h4rn_protocol::MAX_WEBHOOK_PLACEHOLDERS
            )));
        }
        let mut thread_id = existing.and_then(|task| task.thread_id.clone());
        if draft.destination == ScheduleDestination::NewThread {
            thread_id = None;
        }
        if let Some(existing) = existing
            && (existing.workspace_path != workspace
                || existing.host != draft.host
                || existing.harness != draft.harness
                || existing.agent_id != agent_id)
        {
            // Moving a one-thread task to another workspace or harness starts a
            // fresh thread on its next run: the old conversation belongs to a
            // different agent or checkout.
            thread_id = None;
        }
        let webhook_path = if draft.schedule.is_webhook() {
            match existing.and_then(|task| task.webhook_path.clone()) {
                Some(path) => Some(path),
                None => Some(new_webhook_path()),
            }
        } else {
            None
        };
        // A spent once slot stays spent when the task is edited or resumed;
        // only naming a different once date/time arms it again.
        let once_consumed = existing.is_some_and(|task| {
            task.once_consumed
                && matches!(draft.schedule, ScheduleSpec::Once { .. })
                && task.schedule == draft.schedule
        });
        let schedule_is_webhook = draft.schedule.is_webhook();
        let computed_next = if schedule_is_webhook || once_consumed {
            None
        } else {
            next_after(
                &draft.schedule,
                tz,
                existing.map_or(now, |task| task.created_at),
                now,
            )
            .map_err(|error| ScheduleServiceError::Invalid(error.message().to_owned()))?
        };
        if !schedule_is_webhook && !once_consumed && computed_next.is_none() {
            return Err(ScheduleServiceError::Invalid(
                "This once task's date and time are in the past. Choose a future time.".into(),
            ));
        }
        // A disabled task has no next run; enabling it computes one from now.
        let next_run_at = if draft.enabled { computed_next } else { None };
        let created_at = existing.map_or(now, |task| task.created_at);
        Ok(ScheduleTask {
            task_id,
            name: name.to_owned(),
            host: draft.host,
            workspace_path: workspace.to_owned(),
            harness: draft.harness,
            agent_id,
            model: model.to_owned(),
            runtime_mode: draft.runtime_mode,
            prompt: prompt.to_owned(),
            schedule: draft.schedule,
            timezone,
            destination: draft.destination,
            enabled: draft.enabled,
            thread_id,
            webhook_path_set: webhook_path.is_some(),
            webhook_path,
            once_consumed,
            next_run_at,
            last_run: existing.and_then(|task| task.last_run.clone()),
            created_at,
            updated_at: now,
        })
    }

    /// Test seam: make the next journal commit fail.
    #[cfg(test)]
    pub(crate) fn fail_next_commit(&self) {
        self.inner.test.fail_commit.store(true, Ordering::SeqCst);
    }
}

/// A task's reserved in-flight slot, released when the run path lets go.
struct Reservation {
    inner: Arc<Inner>,
    task_id: ScheduleTaskId,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        lock(&self.inner.in_flight).remove(&self.task_id);
    }
}

/// Why admission refused a trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdmissionError {
    NotFound,
    Disabled,
    Busy,
    NotDue,
    StaleSlot,
}

/// What caused a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    Scheduled,
    Manual,
    Webhook,
}

/// The wait before retrying a run whose recording failed.
fn backoff_for(failures: u32) -> chrono::Duration {
    let factor = 1i64 << failures.saturating_sub(1).min(16);
    chrono::Duration::seconds(RETRY_BASE_SECS.saturating_mul(factor).min(RETRY_CAP_SECS))
}

/// The thread a previous run used, for the no-overlap check.
fn last_thread(task: &ScheduleTask) -> Option<h4rn_protocol::ThreadId> {
    task.thread_id
        .clone()
        .or_else(|| task.last_run.as_ref().and_then(|run| run.thread_id.clone()))
}

fn new_webhook_path() -> String {
    format!("{WEBHOOK_PREFIX}{}", crate::auth::generate_token())
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    left.len() == right.len() && left.as_bytes().ct_eq(right.as_bytes()).unwrap_u8() == 1
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests;
