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

mod math;
mod webhook;

pub use math::{
    CronExpr, ScheduleError as ScheduleMathError, describe, next_after, parse_timezone,
};
pub use webhook::{UNTRUSTED_NOTICE, WebhookRequest, placeholder_count, render_prompt};

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

use crate::journal::Journal;
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
    /// No enabled webhook task owns this path (or it was rotated away).
    NotFound,
    /// The task is over its per-minute delivery limit.
    RateLimited,
    /// The delivery was understood but cannot be used.
    Rejected(String),
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
    events: broadcast::Sender<ScheduleEvent>,
    wake: Notify,
    closed: AtomicBool,
    runner: Mutex<Option<JoinHandle<()>>>,
    /// Rolling delivery times per task, for the webhook rate limit.
    deliveries: AsyncMutex<HashMap<String, VecDeque<Instant>>>,
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
                events,
                wake: Notify::new(),
                closed: AtomicBool::new(false),
                runner: Mutex::new(None),
                deliveries: AsyncMutex::new(HashMap::new()),
            }),
        })
    }

    /// Subscribes to task changes.
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
    /// it belongs to daemon downtime).
    pub async fn set_enabled(
        &self,
        task_id: &ScheduleTaskId,
        enabled: bool,
    ) -> Result<ScheduleTask, ScheduleServiceError> {
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
            let tz = parse_timezone(&task.timezone).map_err(|error| {
                ScheduleServiceError::Invalid(format!("The task has an invalid time zone: {error}"))
            })?;
            task.next_run_at = next_after(&task.schedule, tz, task.created_at, now)
                .map_err(|error| ScheduleServiceError::Invalid(error.message().to_owned()))?;
            if task.next_run_at.is_none() && matches!(task.schedule, ScheduleSpec::Once { .. }) {
                return Err(ScheduleServiceError::Invalid(
                    "This once task has already passed its date and cannot run again.".into(),
                ));
            }
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

    /// Replaces a webhook task's URL path.
    pub async fn rotate_webhook(
        &self,
        task_id: &ScheduleTaskId,
    ) -> Result<ScheduleTask, ScheduleServiceError> {
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
        Ok(task)
    }

    /// Runs a task once, outside its schedule.
    pub async fn run_now(
        &self,
        task_id: &ScheduleTaskId,
    ) -> Result<ScheduleRun, ScheduleServiceError> {
        let task = self
            .inner
            .tasks
            .lock()
            .await
            .get(task_id)
            .cloned()
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
        self.inner
            .run_task(&task.task_id, Trigger::Manual, task.prompt.clone())
            .await
    }

    /// Handles one webhook POST.
    ///
    /// The caller has already enforced the body cap. This checks the path, the
    /// enabled flag and the rolling rate limit, renders the prompt, then starts
    /// the run on its own task so the HTTP answer does not wait for a harness.
    pub async fn deliver_webhook(&self, secret: &str, request: WebhookRequest) -> WebhookOutcome {
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
            return WebhookOutcome::NotFound;
        };
        if placeholder_count(&task.prompt) > h4rn_protocol::MAX_WEBHOOK_PLACEHOLDERS {
            return WebhookOutcome::Rejected(
                "The prompt has too many placeholders; edit the task.".into(),
            );
        }
        if !self.take_delivery_slot(&task.task_id).await {
            return WebhookOutcome::RateLimited;
        }
        let prompt = match render_prompt(&task.prompt, &request) {
            Ok(prompt) => prompt,
            Err(message) => return WebhookOutcome::Rejected(message),
        };
        let service = self.clone();
        tokio::spawn(async move {
            if let Err(error) = service
                .inner
                .run_task(&task.task_id, Trigger::Webhook, prompt)
                .await
            {
                tracing::warn!(task = %task.task_id, error = %error, "webhook run failed to start");
            }
        });
        WebhookOutcome::Accepted
    }

    /// Runs every task whose next run is due, once each.
    ///
    /// The runner loop calls this; tests call it with a clock they control.
    pub async fn run_due(&self) {
        let now = self.inner.clock.now();
        let due: Vec<ScheduleTask> = {
            let tasks = self.inner.tasks.lock().await;
            tasks
                .values()
                .filter(|task| {
                    task.enabled
                        && !task.schedule.is_webhook()
                        && task.next_run_at.is_some_and(|at| at <= now)
                })
                .cloned()
                .collect()
        };
        for task in due {
            if let Err(error) = self
                .inner
                .run_task(&task.task_id, Trigger::Scheduled, task.prompt.clone())
                .await
            {
                tracing::warn!(task = %task.task_id, error = %error, "scheduled run failed to start");
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
            && (existing.workspace_path != workspace || existing.host != draft.host)
        {
            // Moving a one-thread task to another workspace starts a fresh
            // thread there on its next run, as the reference does.
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
        let next_run_at = if draft.schedule.is_webhook() {
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
        let spent_once = existing.is_some_and(|task| {
            task.last_run.is_some()
                && task.schedule == draft.schedule
                && matches!(task.schedule, ScheduleSpec::Once { .. })
        });
        if !draft.schedule.is_webhook() && next_run_at.is_none() && !spent_once {
            return Err(ScheduleServiceError::Invalid(
                "This once task's date and time are in the past. Choose a future time.".into(),
            ));
        }
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
            webhook_path,
            next_run_at,
            last_run: existing.and_then(|task| task.last_run.clone()),
            created_at,
            updated_at: now,
        })
    }
}

impl Inner {
    /// Runs one task and records the outcome.
    async fn run_task(
        &self,
        task_id: &ScheduleTaskId,
        trigger: Trigger,
        prompt: String,
    ) -> Result<ScheduleRun, ScheduleServiceError> {
        let task = self
            .tasks
            .lock()
            .await
            .get(task_id)
            .cloned()
            .ok_or_else(|| ScheduleServiceError::NotFound(task_id.clone()))?;
        let started_at = self.clock.now();
        let scheduled_for = match trigger {
            Trigger::Scheduled => task.next_run_at,
            Trigger::Manual | Trigger::Webhook => None,
        };
        if let Some(thread_id) = last_thread(&task)
            && self.manager.thread_is_busy(&thread_id).await
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
            return self.record(task, run, trigger, started_at).await;
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
        let recorded = self.record(task, run, trigger, started_at).await?;
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
                .manager
                .create_thread(open(prompt))
                .await
                .map(created)
                .map_err(|error| error.to_string())?,
            ScheduleDestination::ContinueThread => {
                if let Some(thread_id) = &task.thread_id {
                    self.manager
                        .send_message(thread_id, prompt, Vec::new())
                        .await
                        .map(|accepted| Some(accepted.thread_id))
                        .map_err(|error| error.to_string())
                } else {
                    self.manager
                        .create_thread(open(prompt))
                        .await
                        .map(created)
                        .map_err(|error| error.to_string())?
                }
            }
            ScheduleDestination::FreshSession => {
                if let Some(thread_id) = &task.thread_id {
                    self.manager
                        .send_message_fresh(thread_id, prompt)
                        .await
                        .map(|accepted| Some(accepted.thread_id))
                        .map_err(|error| error.to_string())
                } else {
                    self.manager
                        .create_thread(open(prompt))
                        .await
                        .map(created)
                        .map_err(|error| error.to_string())?
                }
            }
        }
    }

    /// Appends a run, updates the task's clock and last outcome, and tells
    /// every connected client.
    async fn record(
        &self,
        task: ScheduleTask,
        mut run: ScheduleRun,
        trigger: Trigger,
        now: DateTime<Utc>,
    ) -> Result<ScheduleRun, ScheduleServiceError> {
        // A task deleted while its run was starting must not be resurrected by
        // the run's own bookkeeping.
        let Some(mut task) = self.tasks.lock().await.get(&task.task_id).cloned() else {
            return Ok(run);
        };
        self.journal
            .append_schedule_run(&mut run, HISTORY_LIMIT)
            .await?;
        if task.destination != ScheduleDestination::NewThread && task.thread_id.is_none() {
            task.thread_id = run.thread_id.clone();
        }
        if matches!(trigger, Trigger::Scheduled) {
            let tz = parse_timezone(&task.timezone).map_err(|error| {
                ScheduleServiceError::Internal(format!(
                    "the task has an invalid time zone: {error}"
                ))
            })?;
            task.next_run_at = match next_after(&task.schedule, tz, task.created_at, now) {
                Ok(next) => next,
                Err(error) => {
                    tracing::warn!(task = %task.task_id, error = %error, "schedule has no next run");
                    None
                }
            };
        }
        task.last_run = Some(run.clone());
        self.journal.upsert_schedule(&task).await?;
        self.tasks
            .lock()
            .await
            .insert(task.task_id.clone(), task.clone());
        let _ = self.events.send(ScheduleEvent::Upserted {
            task: Box::new(task),
        });
        Ok(run)
    }
}

/// What caused a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    Scheduled,
    Manual,
    Webhook,
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
