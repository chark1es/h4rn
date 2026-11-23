//! Scheduled tasks and webhook automations.
//!
//! A scheduled task runs a prompt on the daemon without a client attached. It
//! can run on a calendar schedule (once, every N minutes or hours, daily,
//! weekly, or a five-field cron expression) or whenever its webhook URL is
//! called. The daemon owns the timer and the journal; these types are the wire
//! contract the Settings editor reads and writes.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::common::{HarnessKind, RuntimeMode};
use crate::ids::{HostId, ScheduleTaskId, ThreadId};

/// The most placeholders one webhook prompt may carry.
///
/// A prompt is rendered once per delivery; bounding the work keeps a hostile
/// template from turning one small request into an expensive scan.
pub const MAX_WEBHOOK_PLACEHOLDERS: usize = 64;

/// The largest request body a webhook accepts, in bytes.
pub const MAX_WEBHOOK_BODY_BYTES: usize = 64 * 1024;

/// Webhook deliveries accepted for one task in a rolling minute.
pub const WEBHOOK_RATE_LIMIT_PER_MINUTE: u32 = 30;

/// A task's unit for an interval schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum IntervalUnit {
    /// Minutes between runs.
    Minutes,
    /// Hours between runs.
    Hours,
}

impl IntervalUnit {
    /// The unit in words for the schedule summary.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minutes => "minutes",
            Self::Hours => "hours",
        }
    }
}

/// When a task runs.
///
/// Times are wall-clock values in the task's time zone, except [`Self::Every`]
/// which is anchored at the task's creation. An empty webhook path is what
/// `Webhook` means; the daemon generates and stores the secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleSpec {
    /// Run once at a local date and time.
    Once {
        /// Local calendar date (`YYYY-MM-DD`).
        date: String,
        /// Local wall-clock time (`HH:MM`).
        time: String,
    },
    /// Run every `count` minutes or hours, anchored at creation.
    Every {
        /// How many units between runs; at least one.
        #[ts(type = "number")]
        count: u32,
        /// The unit.
        unit: IntervalUnit,
    },
    /// Run every day at a local wall-clock time.
    Daily {
        /// Local wall-clock time (`HH:MM`).
        time: String,
    },
    /// Run on selected weekdays at a local wall-clock time.
    Weekly {
        /// ISO weekdays, Monday = 1 through Sunday = 7.
        days: Vec<u8>,
        /// Local wall-clock time (`HH:MM`).
        time: String,
    },
    /// Run on a five-field cron expression (`minute hour day-of-month month
    /// day-of-week`), evaluated in the task's time zone.
    Cron {
        /// The five fields, whitespace separated.
        expression: String,
    },
    /// Run when the task's webhook URL receives a POST.
    Webhook,
}

impl ScheduleSpec {
    /// Whether this schedule is driven by HTTP rather than the clock.
    pub fn is_webhook(&self) -> bool {
        matches!(self, Self::Webhook)
    }
}

/// Where a run's prompt goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleDestination {
    /// Start a new thread for every run.
    #[default]
    NewThread,
    /// Keep one thread and continue its conversation.
    ContinueThread,
    /// Keep one thread, but start a fresh harness session for every run.
    FreshSession,
}

/// What happened to one run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleRunOutcome {
    /// The thread was started or the message was accepted.
    Started,
    /// The previous run was still working, so this slot was skipped.
    Skipped,
    /// The run could not start; `detail` says why.
    Failed,
}

/// One entry of a task's bounded run history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct ScheduleRun {
    /// Durable run sequence, unique across tasks.
    #[ts(type = "number")]
    pub run_id: u64,
    /// The task that ran.
    pub task_id: ScheduleTaskId,
    /// The slot this run was due for; absent for a manual run or a webhook.
    pub scheduled_for: Option<DateTime<Utc>>,
    /// When the daemon handled the run.
    pub started_at: DateTime<Utc>,
    /// The thread the run started or continued, when it did.
    pub thread_id: Option<ThreadId>,
    /// The outcome.
    pub outcome: ScheduleRunOutcome,
    /// Failure text or the reason a slot was skipped.
    pub detail: Option<String>,
}

/// The editable half of a task: everything a create or update request carries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct ScheduleDraft {
    /// Human name shown in the list.
    pub name: String,
    /// Host that runs the task. Only this daemon's host is supported.
    pub host: HostId,
    /// Absolute workspace path on the host.
    pub workspace_path: String,
    /// Harness adapter.
    pub harness: HarnessKind,
    /// Configured ACP agent instance when `harness` is [`HarnessKind::Acp`].
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Model slug.
    pub model: String,
    /// Permission autonomy for every thread the task starts.
    pub runtime_mode: RuntimeMode,
    /// The prompt sent on every run.
    pub prompt: String,
    /// When the task runs.
    pub schedule: ScheduleSpec,
    /// IANA time zone name, such as `Europe/Berlin`.
    ///
    /// An empty value means the daemon's own zone, read from `TZ` or the
    /// system, so a client that does not know the daemon's zone can omit it.
    #[serde(default)]
    pub timezone: String,
    /// Where runs go.
    pub destination: ScheduleDestination,
    /// Whether the clock or webhook may start it.
    pub enabled: bool,
}

/// One stored task as the Settings list reads it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct ScheduleTask {
    /// Stable task id.
    pub task_id: ScheduleTaskId,
    /// Human name.
    pub name: String,
    /// Host that runs the task.
    pub host: HostId,
    /// Absolute workspace path.
    pub workspace_path: String,
    /// Harness adapter.
    pub harness: HarnessKind,
    /// Configured ACP agent instance, when any.
    pub agent_id: Option<String>,
    /// Model slug.
    pub model: String,
    /// Permission autonomy.
    pub runtime_mode: RuntimeMode,
    /// The prompt sent on every run.
    pub prompt: String,
    /// When the task runs.
    pub schedule: ScheduleSpec,
    /// IANA time zone name.
    pub timezone: String,
    /// Where runs go.
    pub destination: ScheduleDestination,
    /// Whether the clock or webhook may start it.
    pub enabled: bool,
    /// The one thread a continuing destination uses, once its first run exists.
    pub thread_id: Option<ThreadId>,
    /// The daemon path a webhook posts to, when this is a webhook task.
    ///
    /// The path is the secret: it is shown only to clients that may read the
    /// task, and rotating it invalidates every earlier URL.
    pub webhook_path: Option<String>,
    /// The next clock run, in UTC. Absent for webhook tasks, a spent once
    /// task, and a disabled task.
    pub next_run_at: Option<DateTime<Utc>>,
    /// The most recent run, when one was recorded.
    pub last_run: Option<ScheduleRun>,
    /// When the task was created.
    pub created_at: DateTime<Utc>,
    /// When the task was last edited.
    pub updated_at: DateTime<Utc>,
}

impl ScheduleTask {
    /// The editable fields of this task.
    pub fn draft(&self) -> ScheduleDraft {
        ScheduleDraft {
            name: self.name.clone(),
            host: self.host.clone(),
            workspace_path: self.workspace_path.clone(),
            harness: self.harness,
            agent_id: self.agent_id.clone(),
            model: self.model.clone(),
            runtime_mode: self.runtime_mode,
            prompt: self.prompt.clone(),
            schedule: self.schedule.clone(),
            timezone: self.timezone.clone(),
            destination: self.destination,
            enabled: self.enabled,
        }
    }
}

/// A change to one task's enabled state, as a transient event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleEvent {
    /// A task was created, edited, ran, or enabled/disabled.
    Upserted {
        /// The task's current state.
        task: Box<ScheduleTask>,
    },
    /// A task was deleted.
    Removed {
        /// The deleted task.
        task_id: ScheduleTaskId,
    },
}

/// Parses a local date string from a once schedule.
pub fn parse_once_date(value: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()
}

/// Parses a local time string from a schedule.
pub fn parse_schedule_time(value: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(value, "%H:%M").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_specs_round_trip_as_tagged_json() {
        let once = ScheduleSpec::Once {
            date: "2026-12-01".into(),
            time: "09:30".into(),
        };
        let json = serde_json::to_value(&once).expect("serialise");
        assert_eq!(json["kind"], "once");
        assert_eq!(
            serde_json::from_value::<ScheduleSpec>(json).expect("deserialise"),
            once
        );

        let cron = ScheduleSpec::Cron {
            expression: "*/5 * * * *".into(),
        };
        let json = serde_json::to_value(&cron).expect("serialise");
        assert_eq!(json["kind"], "cron");
        assert_eq!(
            serde_json::from_value::<ScheduleSpec>(json).expect("deserialise"),
            cron
        );
    }

    #[test]
    fn an_unknown_schedule_kind_is_rejected() {
        let error = serde_json::from_value::<ScheduleSpec>(serde_json::json!({
            "kind": "moon_phase",
        }));
        assert!(error.is_err());
    }

    #[test]
    fn time_and_date_helpers_require_the_documented_shape() {
        assert!(parse_once_date("2026-02-30").is_none());
        assert!(parse_once_date("2026-12-01").is_some());
        assert!(parse_schedule_time("24:00").is_none());
        assert!(parse_schedule_time("09:30").is_some());
    }
}
