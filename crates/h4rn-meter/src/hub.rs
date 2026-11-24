//! Daemon-facing metering façade.
//!
//! Owns the ledger, the price table, the auto-resume policy and the ephemeral
//! event broadcast. The thread manager never calculates tokens itself; it
//! observes a turn, asks a [`TurnMeter`](crate::TurnMeter) to finish it and
//! hands the record here.

use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use h4rn_protocol::{
    AutoResumePolicy, HarnessKind, LimitSource, LimitsReport, Resets, ResumeFailover,
    ResumeSchedule, ResumeState, RuntimeMode, ThreadId, UsageGroupBy, UsageProvenance, UsageReport,
    UsageScope, UsageStreamEvent,
};
use tokio::sync::broadcast;

use crate::ledger::{MeterStore, StoreError, UsageFilter, UsageRow};
use crate::limits::{LimitHit, LimitRecord};
use crate::math::{LiveSnapshot, MeteredTurn};
use crate::price::PriceTable;
use crate::report::{ReportQuery, build_report};
use crate::schedule::plan_resume;

const EVENT_CAPACITY: usize = 256;

/// Everything the ledger needs to attribute one turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnContext {
    /// Thread id.
    pub thread_id: Option<String>,
    /// Parent thread when this turn belongs to a subagent child.
    pub parent_thread_id: Option<String>,
    /// Run id (the turn).
    pub run_id: Option<String>,
    /// Workspace path.
    pub workspace: Option<String>,
    /// Machine or host label.
    pub machine: Option<String>,
    /// Provider/account label.
    pub account: Option<String>,
    /// Harness slug.
    pub harness: Option<String>,
    /// Model id.
    pub model: Option<String>,
    /// Provider name.
    pub provider: Option<String>,
    /// Credential identity.
    pub credential: Option<String>,
}

/// The metering hub shared by the daemon.
#[derive(Clone)]
pub struct MeterHub {
    inner: Arc<Inner>,
}

struct Inner {
    store: MeterStore,
    prices: PriceTable,
    policy: RwLock<AutoResumePolicy>,
    events: broadcast::Sender<UsageStreamEvent>,
}

impl MeterHub {
    /// Opens a ledger file.
    pub async fn open(path: impl AsRef<std::path::Path>) -> Result<Self, StoreError> {
        Self::with_store(MeterStore::connect(path).await?).await
    }

    /// An in-memory hub for tests.
    pub async fn in_memory() -> Result<Self, StoreError> {
        Self::with_store(MeterStore::in_memory().await?).await
    }

    /// Wraps an existing store.
    pub async fn with_store(store: MeterStore) -> Result<Self, StoreError> {
        let _ = store.all_rows().await?;
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                prices: PriceTable::builtin(),
                policy: RwLock::new(AutoResumePolicy::default()),
                events,
            }),
        })
    }

    /// The underlying ledger.
    pub fn store(&self) -> &MeterStore {
        &self.inner.store
    }

    /// The price table.
    pub fn prices(&self) -> &PriceTable {
        &self.inner.prices
    }

    /// The policy currently in force.
    pub fn policy(&self) -> AutoResumePolicy {
        self.inner
            .policy
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Replaces the policy.
    pub fn set_policy(&self, policy: AutoResumePolicy) {
        *self
            .inner
            .policy
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = policy;
    }

    /// Subscribes to ephemeral metering frames.
    pub fn subscribe(&self) -> broadcast::Receiver<UsageStreamEvent> {
        self.inner.events.subscribe()
    }

    /// Broadcasts one ephemeral frame.
    pub fn emit(&self, event: UsageStreamEvent) {
        let _ = self.inner.events.send(event);
    }

    /// Emits a live tick for a running turn.
    pub fn tick(
        &self,
        thread_id: &ThreadId,
        run_id: Option<String>,
        live: LiveSnapshot,
        at: DateTime<Utc>,
    ) {
        self.emit(UsageStreamEvent::UsageTick {
            thread_id: thread_id.clone(),
            run_id,
            usage: live.usage,
            ttft_ms: live.ttft_ms,
            tps: live.tps,
            elapsed_ms: live.elapsed_ms,
            tool_ms: live.tool_ms,
            at,
        });
    }

    /// Persists a finished self-metered turn.
    pub async fn record_turn(
        &self,
        context: TurnContext,
        turn: &MeteredTurn,
    ) -> Result<UsageRow, StoreError> {
        let row = UsageRow {
            row_id: format!("usage_{}", uuid::Uuid::now_v7()),
            thread_id: context.thread_id,
            parent_thread_id: context.parent_thread_id,
            run_id: context.run_id.clone(),
            workspace: context.workspace,
            machine: context.machine,
            account: context.account,
            harness: context.harness,
            model: context.model,
            provider: context.provider,
            credential: context.credential,
            source: UsageProvenance::SelfMetered,
            correlation_id: context.run_id,
            tokens_in: turn.usage.input_tokens,
            tokens_out: turn.usage.output_tokens,
            tokens_cached_read: turn.usage.cached_input_tokens,
            tokens_cached_creation: turn.cached_creation_tokens,
            cost_micros: turn.cost_micros,
            cost_source: turn.cost_source,
            price_table: turn.price_table.clone(),
            ttft_ms: turn.ttft_ms,
            tps: turn.tps,
            generation_ms: turn.generation_ms,
            wall_ms: turn.wall_ms,
            started_at: turn.started_at,
            completed_at: turn.completed_at,
            ts: turn.completed_at,
            raw: Some(serde_json::json!({
                "status": turn.status,
                "usage": turn.usage,
                "tool_ms": turn.tool_ms,
                "generation_ms": turn.generation_ms,
            })),
            deduped: false,
            synced: false,
        };
        self.inner.store.insert_row(&row).await?;
        Ok(row)
    }

    /// Stores an enrichment row reported by CLIProxyAPI.
    pub async fn record_proxy_row(&self, row: UsageRow) -> Result<bool, StoreError> {
        self.inner.store.insert_row(&row).await
    }

    /// Persists a limit observation and broadcasts it.
    pub async fn record_limit(&self, hit: LimitHit) -> Result<LimitRecord, StoreError> {
        let record = hit.into_record(Utc::now());
        self.inner.store.insert_limit(&record).await?;
        self.emit(UsageStreamEvent::LimitStatus {
            status: record.to_status(),
        });
        Ok(record)
    }

    /// Arms or replaces the schedule for one thread.
    ///
    /// `grant` is the outside grant that asked for the paused turn. It is
    /// stored on the schedule itself, in the same write that arms it, so the
    /// resume that fires can re-check that its token family is still live.
    // Named arguments rather than a builder: every caller is one place and the
    // list is the whole durable row this write produces.
    #[allow(clippy::too_many_arguments)]
    pub async fn arm_schedule(
        &self,
        thread_id: &ThreadId,
        reset_at: Option<DateTime<Utc>>,
        prompt: String,
        failover: Option<ResumeFailover>,
        now: DateTime<Utc>,
        mode_ceiling: Option<RuntimeMode>,
        grant: Option<h4rn_protocol::WorkGrant>,
    ) -> Result<ResumeSchedule, StoreError> {
        let policy = self.policy();
        let attempt = self
            .inner
            .store
            .schedules()
            .await?
            .into_iter()
            .find(|schedule| &schedule.thread_id == thread_id)
            .map(|schedule| schedule.attempt.saturating_add(1))
            .unwrap_or(0);
        let plan = plan_resume(thread_id, now, reset_at, attempt, &policy);
        let schedule = ResumeSchedule {
            schedule_id: format!("sched_{}", uuid::Uuid::now_v7()),
            thread_id: thread_id.clone(),
            scheduled_at: now,
            resume_at: plan.resume_at,
            attempt,
            state: plan.state,
            reason: plan.reason,
            prompt,
            mode_ceiling,
            grant,
            failover,
            last_error: None,
        };
        self.inner.store.upsert_schedule(&schedule).await?;
        self.emit(UsageStreamEvent::ScheduleChanged {
            schedule: Some(schedule.clone()),
        });
        Ok(schedule)
    }

    /// Cancels a schedule and broadcasts the cleared state.
    ///
    /// Only an attempt that has not fired can be cancelled. When the resume
    /// already claimed the attempt, this returns `None` and leaves the fired
    /// row in place, so a cancellation racing the resume cannot report a state
    /// the ledger does not have.
    pub async fn cancel_schedule(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<ResumeSchedule>, StoreError> {
        let schedule = self.inner.store.cancel_schedule(thread_id).await?;
        if schedule.is_some() {
            self.emit(UsageStreamEvent::ScheduleChanged { schedule: None });
        }
        Ok(schedule)
    }

    /// Claims one due attempt for firing, under the thread record lock.
    ///
    /// `false` means the attempt was cancelled or replaced before this caller
    /// reached the claim, and the caller must not start a turn.
    pub async fn claim_schedule(&self, schedule: &ResumeSchedule) -> Result<bool, StoreError> {
        self.inner
            .store
            .claim_schedule(&schedule.schedule_id, schedule.attempt)
            .await
    }

    /// Marks a schedule fired (or failed) and broadcasts it.
    ///
    /// A cancelled or superseded attempt is refused: the fire that lost the
    /// race must not overwrite the cancellation or a newer attempt.
    pub async fn finish_schedule(
        &self,
        schedule: ResumeSchedule,
        state: ResumeState,
        error: Option<String>,
    ) -> Result<Option<ResumeSchedule>, StoreError> {
        let Some(schedule) = self
            .inner
            .store
            .finish_schedule(&schedule, state, error.as_deref())
            .await?
        else {
            return Ok(None);
        };
        self.emit(UsageStreamEvent::ScheduleChanged {
            schedule: Some(schedule.clone()),
        });
        Ok(Some(schedule))
    }

    /// Rows h4rn-connect has not accepted yet.
    pub async fn unsynced(&self, limit: u32) -> Result<Vec<UsageRow>, StoreError> {
        self.inner.store.unsynced(limit).await
    }

    /// Marks rows accepted by connect.
    pub async fn mark_synced(&self, row_ids: &[String]) -> Result<(), StoreError> {
        self.inner.store.mark_synced(row_ids).await
    }

    /// Schedules due at `now`.
    pub async fn due_schedules(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<ResumeSchedule>, StoreError> {
        self.inner.store.due_schedules(now).await
    }

    /// Every persisted schedule.
    pub async fn schedules(&self) -> Result<Vec<ResumeSchedule>, StoreError> {
        self.inner.store.schedules().await
    }

    /// One persisted schedule by its ledger id, if it still exists.
    pub async fn schedule(&self, schedule_id: &str) -> Result<Option<ResumeSchedule>, StoreError> {
        self.inner.store.schedule(schedule_id).await
    }

    /// Limits and the current policy.
    pub async fn limits_report(&self) -> Result<LimitsReport, StoreError> {
        Ok(LimitsReport {
            limits: self
                .inner
                .store
                .limits(200)
                .await?
                .into_iter()
                .map(|record| record.to_status())
                .collect(),
            policy: self.policy(),
        })
    }

    /// Schedules and limit history.
    pub async fn resets(&self) -> Result<Resets, StoreError> {
        Ok(Resets {
            schedules: self.schedules().await?,
            history: self
                .inner
                .store
                .limits(200)
                .await?
                .into_iter()
                .map(|record| record.to_status())
                .collect(),
        })
    }

    /// Builds a usage report from the local ledger.
    pub async fn report(&self, query: ReportQuery) -> Result<UsageReport, StoreError> {
        let filter = UsageFilter {
            since: query.range.since,
            until: query.range.until,
            thread_id: matches!(query.scope, UsageScope::Thread)
                .then(|| query.scope_id.clone())
                .flatten(),
            include_children: query.include_children,
            workspace: matches!(query.scope, UsageScope::Workspace)
                .then(|| query.scope_id.clone())
                .flatten(),
            machine: matches!(query.scope, UsageScope::Machine)
                .then(|| query.scope_id.clone())
                .flatten(),
        };
        let rows = self.inner.store.rows(&filter).await?;
        Ok(build_report(&rows, &query))
    }

    /// Convenience for the global dashboard query.
    pub async fn global_report(&self, group_by: UsageGroupBy) -> Result<UsageReport, StoreError> {
        self.report(ReportQuery {
            scope: UsageScope::Global,
            scope_id: None,
            range: Default::default(),
            group_by,
            bucket: None,
            series_group_by: None,
            include_children: false,
        })
        .await
    }

    /// A limit with no reset time still gets a visible record.
    pub fn harness_limit(
        thread_id: ThreadId,
        harness: HarnessKind,
        reset_at: Option<DateTime<Utc>>,
        message: Option<String>,
    ) -> LimitHit {
        LimitHit {
            thread_id: Some(thread_id),
            harness: Some(harness),
            account: None,
            provider: None,
            model: None,
            reset_at,
            message,
            consumed: None,
            limit: None,
            source: LimitSource::Harness,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone as _};

    use super::*;
    use crate::math::TurnMeter;
    use h4rn_protocol::Usage;

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap() + Duration::seconds(seconds)
    }

    #[tokio::test]
    async fn a_schedule_survives_a_daemon_restart_and_can_be_cancelled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meter.db");
        let thread_id = ThreadId::new("th_restart");
        let reset_at = at(300);

        {
            let hub = MeterHub::open(&path).await.expect("hub");
            hub.set_policy(AutoResumePolicy {
                jitter_ms: 0,
                ..AutoResumePolicy::default()
            });
            let schedule = hub
                .arm_schedule(
                    &thread_id,
                    Some(reset_at),
                    "continue".into(),
                    None,
                    at(0),
                    None,
                    None,
                )
                .await
                .expect("arm");
            assert_eq!(schedule.resume_at, reset_at);
            assert_eq!(schedule.state, ResumeState::Scheduled);
        }

        let hub = MeterHub::open(&path).await.expect("reopen");
        let schedules = hub.schedules().await.expect("schedules");
        assert_eq!(schedules.len(), 1);
        assert_eq!(schedules[0].thread_id, thread_id);
        assert_eq!(schedules[0].resume_at, reset_at);
        assert!(hub.due_schedules(at(299)).await.unwrap().is_empty());
        assert_eq!(hub.due_schedules(at(301)).await.unwrap().len(), 1);

        hub.cancel_schedule(&thread_id).await.expect("cancel");
        let cancelled = hub.schedules().await.expect("schedules");
        assert_eq!(cancelled[0].state, ResumeState::Cancelled);
        assert!(hub.due_schedules(at(400)).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rearming_increments_the_attempt_and_keeps_the_failover_choice() {
        let hub = MeterHub::in_memory().await.expect("hub");
        hub.set_policy(AutoResumePolicy {
            jitter_ms: 0,
            ..AutoResumePolicy::default()
        });
        let thread_id = ThreadId::new("th_failover");
        let failover = ResumeFailover {
            model: "claude-opus-5".into(),
            account: Some("acct-a".into()),
            account_label: Some("acct-a".into()),
            reason: "same family".into(),
        };
        hub.arm_schedule(
            &thread_id,
            None,
            "continue".into(),
            Some(failover.clone()),
            at(0),
            None,
            None,
        )
        .await
        .expect("arm");
        let second = hub
            .arm_schedule(
                &thread_id,
                None,
                "continue".into(),
                Some(failover.clone()),
                at(1),
                None,
                None,
            )
            .await
            .expect("rearm");
        assert_eq!(second.attempt, 1);
        assert_eq!(second.failover, Some(failover));
        assert_eq!(second.resume_at, at(1) + Duration::seconds(60));
    }

    #[tokio::test]
    async fn a_cancelled_or_superseded_attempt_cannot_claim_or_finish() {
        let hub = MeterHub::in_memory().await.expect("hub");
        hub.set_policy(AutoResumePolicy {
            jitter_ms: 0,
            ..AutoResumePolicy::default()
        });
        let thread_id = ThreadId::new("th_cancel");
        let first = hub
            .arm_schedule(&thread_id, None, "continue".into(), None, at(0), None, None)
            .await
            .expect("arm");

        // Cancelling wins the attempt; the old fire can neither claim it nor
        // write a Fired row over the cancellation.
        assert!(
            hub.cancel_schedule(&thread_id)
                .await
                .expect("cancel")
                .is_some()
        );
        assert!(!hub.claim_schedule(&first).await.expect("claim cancelled"));
        assert!(
            hub.finish_schedule(first.clone(), ResumeState::Fired, None)
                .await
                .expect("finish cancelled")
                .is_none()
        );
        assert_eq!(
            hub.schedules().await.expect("schedules")[0].state,
            ResumeState::Cancelled
        );

        // A newer attempt replaces the row; the old fire is stale for both
        // operations and the new attempt is claimable.
        let second = hub
            .arm_schedule(&thread_id, None, "continue".into(), None, at(1), None, None)
            .await
            .expect("rearm");
        assert_ne!(second.schedule_id, first.schedule_id);
        assert!(!hub.claim_schedule(&first).await.expect("stale claim"));
        assert!(
            hub.finish_schedule(first, ResumeState::Fired, None)
                .await
                .expect("stale finish")
                .is_none()
        );
        assert!(hub.claim_schedule(&second).await.expect("claim newer"));
    }

    #[tokio::test]
    async fn ledger_rows_and_their_report_survive_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("meter.db");
        let thread_id = ThreadId::new("th_usage");
        let context = TurnContext {
            thread_id: Some(thread_id.as_str().to_owned()),
            run_id: Some("run_1".into()),
            workspace: Some("/repo".into()),
            machine: Some("host".into()),
            harness: Some("claude-code".into()),
            model: Some("claude-sonnet-5".into()),
            provider: Some("anthropic".into()),
            ..TurnContext::default()
        };

        {
            let hub = MeterHub::open(&path).await.expect("hub");
            for (index, start) in [0i64, 60].into_iter().enumerate() {
                let mut meter = TurnMeter::new(at(start));
                meter.first_token(at(start + 1));
                meter.add_usage(&Usage {
                    input_tokens: 100,
                    output_tokens: 200,
                    ..Usage::default()
                });
                let turn = meter.finish(
                    at(start + 10),
                    h4rn_protocol::TurnStatus::Completed,
                    hub.prices(),
                    "claude-sonnet-5",
                );
                let mut context = context.clone();
                context.run_id = Some(format!("run_{index}"));
                hub.record_turn(context, &turn).await.expect("record");
            }
        }

        let hub = MeterHub::open(&path).await.expect("reopen");
        let report = hub
            .report(ReportQuery {
                scope: UsageScope::Thread,
                scope_id: Some(thread_id.as_str().to_owned()),
                include_children: false,
                range: Default::default(),
                group_by: UsageGroupBy::None,
                bucket: None,
                series_group_by: None,
            })
            .await
            .expect("report");
        assert_eq!(report.totals.turns, 2);
        assert_eq!(report.totals.input_tokens, 200);
        assert_eq!(report.totals.output_tokens, 400);
        assert_eq!(report.totals.active_ms, 20_000);
        assert_eq!(report.totals.runtime_ms, 70_000);
        assert_eq!(report.totals.idle_ms, 50_000);
        assert!(report.totals.cost_micros > 0);
    }

    #[tokio::test]
    async fn limit_history_and_policy_are_reported() {
        let hub = MeterHub::in_memory().await.expect("hub");
        let thread_id = ThreadId::new("th_limit");
        let hit = MeterHub::harness_limit(
            thread_id.clone(),
            HarnessKind::ClaudeCode,
            Some(at(900)),
            Some("weekly limit".into()),
        );
        hub.record_limit(hit).await.expect("record limit");
        let limits = hub.limits_report().await.expect("limits");
        assert_eq!(limits.limits.len(), 1);
        assert_eq!(limits.limits[0].message.as_deref(), Some("weekly limit"));
        assert_eq!(limits.policy, AutoResumePolicy::default());
        let resets = hub.resets().await.expect("resets");
        assert_eq!(resets.history.len(), 1);
    }
}
