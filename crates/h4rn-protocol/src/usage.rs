//! Metering, limits and auto-resume wire types (ADR-024).
//!
//! The durable ledger itself lives in `h4rn-meter`; these types are what a
//! client asks for and what the daemon streams back. Durable thread events
//! still own lifecycle changes (`usage_limit_hit`, `thread_upserted`); the
//! live [`UsageStreamEvent`] frames are ephemeral and carry no sequence number,
//! exactly like [`crate::ProxyEvent`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::common::{HarnessKind, RuntimeMode, Usage};
use crate::ids::ThreadId;

/// What a usage query is aggregated over.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum UsageScope {
    /// One thread.
    Thread,
    /// Every thread whose workspace is `scope_id`.
    Workspace,
    /// This daemon's machine.
    Machine,
    /// Every machine and workspace synced to the account.
    Global,
}

/// How a usage report is broken down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum UsageGroupBy {
    /// One totals row only.
    None,
    /// One group per thread.
    Thread,
    /// One group per workspace.
    Workspace,
    /// One group per machine.
    Machine,
    /// One group per harness.
    Harness,
    /// One group per model.
    Model,
}

/// Time bucket width for a usage series.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum UsageBucket {
    /// Calendar-hour buckets.
    Hour,
    /// Calendar-day buckets.
    Day,
}

/// Half-open time range `[since, until)`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct UsageRange {
    /// Inclusive lower bound; `None` means the beginning of the ledger.
    pub since: Option<DateTime<Utc>>,
    /// Exclusive upper bound; `None` means now.
    pub until: Option<DateTime<Utc>>,
}

/// Normalized counters for one row, group or bucket.
///
/// `input_tokens` includes cached input, matching [`Usage`]. Cached read and
/// creation are reported separately for cost reconciliation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, TS)]
pub struct UsageTotals {
    /// Prompt tokens, including cached input.
    #[ts(type = "number")]
    pub input_tokens: u64,
    /// Completion tokens.
    #[ts(type = "number")]
    pub output_tokens: u64,
    /// Input tokens served from cache.
    #[ts(type = "number")]
    pub cached_input_tokens: u64,
    /// Input tokens written to the provider's cache.
    #[ts(type = "number")]
    pub cached_creation_tokens: u64,
    /// Cost in millionths of a US dollar.
    #[ts(type = "number")]
    pub cost_micros: u64,
    /// Mean time to first token across metered turns.
    #[ts(type = "number")]
    pub ttft_ms: u64,
    /// Mean output tokens per generation second, excluding tool time.
    pub tps: f64,
    /// Sum of turn wall-clock time.
    #[ts(type = "number")]
    pub wall_ms: u64,
    /// Sum of generation time, excluding tool time.
    #[ts(type = "number")]
    pub generation_ms: u64,
    /// Wall time during which a turn was running.
    #[ts(type = "number")]
    pub active_ms: u64,
    /// Time between the first and last turn with no turn running.
    #[ts(type = "number")]
    pub idle_ms: u64,
    /// First start to last completion across the queried threads.
    #[ts(type = "number")]
    pub runtime_ms: u64,
    /// Number of metered turns counted.
    #[ts(type = "number")]
    pub turns: u64,
}

/// One group in a usage report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct UsageGroup {
    /// Stable key: thread id, workspace path, model id, harness slug or machine.
    pub key: String,
    /// Aggregated counters.
    pub totals: UsageTotals,
}

/// One time bucket in a usage series.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct UsageBucketPoint {
    /// Inclusive bucket start.
    pub start: DateTime<Utc>,
    /// Exclusive bucket end.
    pub end: DateTime<Utc>,
    /// Aggregated counters.
    pub totals: UsageTotals,
}

/// One stacked series: a group key and its aligned bucket points.
///
/// A chart needs the same time axis for every stack, so every series in one
/// report has exactly the buckets in `UsageReport::buckets`; a key with no
/// rows in a bucket carries a defaulted [`UsageTotals`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct UsageSeries {
    /// Stable group key, matching the `series_group_by` breakdown.
    pub key: String,
    /// Buckets for this key, aligned with the report's buckets.
    pub points: Vec<UsageBucketPoint>,
}

/// A complete usage report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct UsageReport {
    /// Scope the query used.
    pub scope: UsageScope,
    /// Scope id for thread and workspace queries.
    pub scope_id: Option<String>,
    /// Range actually applied.
    pub range: UsageRange,
    /// Grouping applied.
    pub group_by: UsageGroupBy,
    /// Bucket width, when a series was requested.
    pub bucket: Option<UsageBucket>,
    /// Grouping applied to [`Self::series`], when one was requested.
    pub series_group_by: Option<UsageGroupBy>,
    /// Account-wide totals for the scope.
    pub totals: UsageTotals,
    /// Time series, oldest first.
    pub buckets: Vec<UsageBucketPoint>,
    /// Stacked series, one per `series_group_by` key, aligned with `buckets`.
    pub series: Vec<UsageSeries>,
    /// Breakdowns, sorted by key.
    pub groups: Vec<UsageGroup>,
}

/// Where a row's cost number came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CostSource {
    /// The harness reported a cost for the turn.
    HarnessReported,
    /// Estimated from the price table.
    PriceTable,
    /// Reported by the proxy's performance API.
    ProxyReported,
    /// No price is known; the cost is zero, not free.
    Unknown,
}

/// Whether a row came from h4rn's own stream observation or the proxy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum UsageProvenance {
    /// Observed from harness stream events.
    SelfMetered,
    /// Enrichment from CLIProxyAPI `/v8/management/performance`.
    ProxyReported,
}

/// Where a limit observation came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    /// A harness reported the limit.
    Harness,
    /// The proxy's credential cooldown list reported it.
    Proxy,
    /// A provider account report.
    Provider,
}

/// One observed usage limit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LimitStatus {
    /// Thread the limit applies to, when scoped.
    pub thread_id: Option<ThreadId>,
    /// Harness that hit the limit.
    pub harness: Option<HarnessKind>,
    /// Provider/account label, never a secret.
    pub account: Option<String>,
    /// Provider name such as `anthropic`.
    pub provider: Option<String>,
    /// Model that hit the limit.
    pub model: Option<String>,
    /// When the provider resets, if it said.
    pub reset_at: Option<DateTime<Utc>>,
    /// Human-readable provider message.
    pub message: Option<String>,
    /// Consumed units, when the provider reports a budget.
    #[ts(type = "number")]
    pub consumed: Option<u64>,
    /// Known budget, when the provider reports one.
    #[ts(type = "number")]
    pub limit: Option<u64>,
    /// When h4rn observed it.
    pub observed_at: DateTime<Utc>,
    /// Where the observation came from.
    pub source: LimitSource,
}

/// Compact limit state carried on a thread summary.
///
/// The inbox renders "Limit resets 14:32" without opening the thread, so the
/// full [`LimitStatus`] is too heavy; this keeps only the fields the row uses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LimitBrief {
    /// Harness that hit the limit, when known.
    pub harness: Option<HarnessKind>,
    /// When the provider resets, when it said.
    pub reset_at: Option<DateTime<Utc>>,
    /// Human-readable provider message.
    pub message: Option<String>,
}

/// Auto-resume policy shared by every thread unless overridden.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AutoResumePolicy {
    /// Try a failover model from the active proxy profile when the reset fires.
    pub failover: bool,
    /// Longest wait h4rn will schedule; unknown reset times are retried within it.
    #[ts(type = "number")]
    pub max_wait_ms: u64,
    /// Prompt sent to the resumed session.
    pub prompt: String,
    /// Random jitter added to a provider reset so many threads do not stampede.
    #[ts(type = "number")]
    pub jitter_ms: u64,
}

/// The default prompt sent when a scheduled thread resumes.
pub const DEFAULT_RESUME_PROMPT: &str = "The usage limit reset. Continue the work from where you stopped; do not repeat completed steps.";

impl Default for AutoResumePolicy {
    fn default() -> Self {
        Self {
            failover: false,
            max_wait_ms: 6 * 60 * 60 * 1000,
            prompt: DEFAULT_RESUME_PROMPT.to_owned(),
            jitter_ms: 5_000,
        }
    }
}

/// Where a schedule is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ResumeState {
    /// Armed and waiting for `resume_at`.
    Scheduled,
    /// Waiting on an unknown reset; h4rn retries with capped backoff.
    Waiting,
    /// Fired and a resumed turn was started.
    Fired,
    /// Cancelled by the user or because the thread ended.
    Cancelled,
    /// The maximum wait elapsed without a usable provider reset.
    Exhausted,
}

/// A visible failover decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ResumeFailover {
    /// Replacement model.
    pub model: String,
    /// Credential/account to prefer, when the proxy picked one.
    pub account: Option<String>,
    /// [`Self::account`] as a person would read it.
    #[serde(default)]
    pub account_label: Option<String>,
    /// Why it was chosen.
    pub reason: String,
}

/// Where a limit-hit thread could go instead of waiting for its reset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FailoverOptions {
    /// Thread the options are for.
    pub thread_id: ThreadId,
    /// The model the thread is on now.
    pub current_model: String,
    /// What the daemon would pick; the head of `options`.
    pub default: Option<crate::proxy::ProxyFailoverChoice>,
    /// Every eligible model and account, best first.
    pub options: Vec<crate::proxy::ProxyFailoverChoice>,
    /// Why there are no options, when there are none ("No proxy profile
    /// serves Claude Code", "Every other model is cooling down").
    pub unavailable: Option<String>,
}

/// The authority an outside grant carries into work that can start a turn later.
///
/// A grant is a `(client, token family, ceiling)` triple: which client asked,
/// which of its token families is the credential, and the most autonomy the
/// work it asked for may run with. Work that outlives the request that created
/// it (a parked prompt, a resume schedule, a PR watch, a delegated child) keeps
/// this, and the family is re-checked when the turn is actually delivered.
///
/// The family never reaches a client: a [`ResumeSchedule`] omits it on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkGrant {
    /// Public client id of the grant that asked for the work.
    pub client_id: String,
    /// The token family the work was asked under. Daemon-internal.
    pub family: String,
    /// The most autonomy the work may run with.
    #[serde(default)]
    pub ceiling: Option<RuntimeMode>,
}

/// One persisted auto-resume schedule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ResumeSchedule {
    /// Stable schedule id (`sched_*`).
    pub schedule_id: String,
    /// Thread that will resume.
    pub thread_id: ThreadId,
    /// When the schedule was armed.
    pub scheduled_at: DateTime<Utc>,
    /// When h4rn will try to resume.
    pub resume_at: DateTime<Utc>,
    /// Attempt number, starting at zero.
    #[ts(type = "number")]
    pub attempt: u32,
    /// Current state.
    pub state: ResumeState,
    /// Why this time was chosen, written for the user.
    pub reason: String,
    /// Prompt sent on resume.
    pub prompt: String,
    /// The most autonomy the turn that reported the limit was allowed.
    ///
    /// It lives on the schedule itself, in the same write that arms it, so a
    /// resume can never run at a wider mode than the work it continues -- even
    /// when the daemon stops before any other record is updated.
    #[serde(default)]
    pub mode_ceiling: Option<RuntimeMode>,
    /// The exact grant that asked for the turn this schedule continues.
    ///
    /// Daemon state, never part of a schedule a client reads: the resuming turn
    /// must re-check that this token family is still live, and the policy prompt
    /// that fires is the same work the grant asked for.
    #[serde(skip)]
    #[ts(skip)]
    pub grant: Option<WorkGrant>,
    /// Failover choice, when one was made.
    pub failover: Option<ResumeFailover>,
    /// Last resume error, when a previous attempt failed.
    pub last_error: Option<String>,
}

/// Limits plus the auto-resume policy in force.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct LimitsReport {
    /// Known limits, newest first.
    pub limits: Vec<LimitStatus>,
    /// Current policy.
    pub policy: AutoResumePolicy,
}

/// Scheduled resumes and the limit history behind them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct Resets {
    /// Every persisted schedule, newest first.
    pub schedules: Vec<ResumeSchedule>,
    /// Observed limit hits, newest first.
    pub history: Vec<LimitStatus>,
}

/// One ledger row as synced to h4rn-connect.
///
/// `row_id` is the idempotency key: connect stores one row per
/// `(user_id, row_id)` and ignores a retry of the same id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct UsageSyncRow {
    /// Stable local row id.
    pub row_id: String,
    /// Thread the turn belonged to.
    pub thread_id: Option<String>,
    /// Run within the thread.
    pub run_id: Option<String>,
    /// Workspace path.
    pub workspace: Option<String>,
    /// Machine id or host label.
    pub machine: Option<String>,
    /// Account/provider label, never a secret.
    pub account: Option<String>,
    /// Harness slug.
    pub harness: Option<String>,
    /// Model id.
    pub model: Option<String>,
    /// Provider that served the call.
    pub provider: Option<String>,
    /// Credential identity, when known.
    pub credential: Option<String>,
    /// Whether h4rn observed this or the proxy reported it.
    pub source: UsageProvenance,
    /// Proxy request id or other correlation key.
    pub correlation_id: Option<String>,
    /// Prompt tokens, including cached input.
    #[ts(type = "number")]
    pub tokens_in: u64,
    /// Completion tokens.
    #[ts(type = "number")]
    pub tokens_out: u64,
    /// Cached read input tokens.
    #[ts(type = "number")]
    pub tokens_cached_read: u64,
    /// Cached creation input tokens.
    #[ts(type = "number")]
    pub tokens_cached_creation: u64,
    /// Cost in millionths of a US dollar.
    #[ts(type = "number")]
    pub cost_micros: u64,
    /// Where the cost came from.
    pub cost_source: CostSource,
    /// Price-table version when estimated.
    pub price_table: Option<String>,
    /// Time to first token.
    #[ts(type = "number")]
    pub ttft_ms: Option<u64>,
    /// Output tokens per generation second.
    pub tps: Option<f64>,
    /// Generation time, excluding tool time.
    #[ts(type = "number")]
    pub generation_ms: u64,
    /// Wall-clock turn time.
    #[ts(type = "number")]
    pub wall_ms: u64,
    /// Turn start.
    pub started_at: DateTime<Utc>,
    /// Turn end.
    pub completed_at: DateTime<Utc>,
    /// Event timestamp used for buckets.
    pub ts: DateTime<Utc>,
    /// Raw provider payload, for reconciliation.
    pub raw: Option<serde_json::Value>,
    /// A proxy row already covered by a self-metered row.
    pub deduped: bool,
}

/// A batch of ledger rows pushed to connect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct UsageSyncRequest {
    /// Rows to store. Retrying the same batch is safe.
    pub rows: Vec<UsageSyncRow>,
}

/// Result of one idempotent sync batch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct UsageSyncResponse {
    /// Rows newly stored.
    #[ts(type = "number")]
    pub accepted: u64,
    /// Rows connect already had.
    #[ts(type = "number")]
    pub duplicates: u64,
}

/// Ephemeral live metering frames.
///
/// Like a proxy event these carry no durable sequence: the ledger row and the
/// durable `usage_limit_hit` event are what a reconnecting client replays.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UsageStreamEvent {
    /// A running turn's counters, throttled to about one per second.
    UsageTick {
        /// Thread being metered.
        thread_id: ThreadId,
        /// Active turn within the thread, when known.
        run_id: Option<String>,
        /// Current turn counters.
        usage: Usage,
        /// Time to first token, when one has arrived.
        #[ts(type = "number")]
        ttft_ms: Option<u64>,
        /// Live output tokens per generation second, excluding tool time.
        tps: Option<f64>,
        /// Time since the turn started.
        #[ts(type = "number")]
        elapsed_ms: u64,
        /// Time spent in tools so far.
        #[ts(type = "number")]
        tool_ms: u64,
        /// When the tick was produced.
        at: DateTime<Utc>,
    },
    /// A limit was observed or its status changed.
    LimitStatus {
        /// The observed limit.
        status: LimitStatus,
    },
    /// An auto-resume schedule was armed, moved or cleared.
    ScheduleChanged {
        /// The schedule, or `None` when it was cleared.
        schedule: Option<ResumeSchedule>,
    },
}
