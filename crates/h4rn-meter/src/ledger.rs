//! SQLite-backed ledger, limits and schedules.
//!
//! The ledger is append-only: a row id is stable and a retry of the same row is
//! ignored. Proxy rows are stored next to self-metered rows, with a `deduped`
//! flag that records whether the proxy report was already covered by an
//! observation of the harness stream.

use std::collections::HashSet;
use std::path::Path;

use chrono::{DateTime, Utc};
use h4rn_protocol::{
    CostSource, HarnessKind, LimitSource, ResumeFailover, ResumeSchedule, ResumeState, ThreadId,
    UsageProvenance,
};
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row as _, SqlitePool};

use crate::limits::LimitRecord;

/// Ledger storage failures.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite failed.
    #[error("meter database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A path could not be prepared.
    #[error("meter path error: {0}")]
    Io(#[from] std::io::Error),
    /// A stored JSON payload did not parse.
    #[error("meter data is corrupt: {0}")]
    Data(String),
    /// A stored JSON payload was not valid JSON.
    #[error("meter JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// One durable usage row.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageRow {
    /// Stable row id, used to make sync idempotent.
    pub row_id: String,
    /// Thread the turn belonged to.
    pub thread_id: Option<String>,
    /// Parent thread when this row belongs to a subagent child.
    ///
    /// Thread-scoped reports roll children up through this column; the row is
    /// still attributed to the child for its own report.
    pub parent_thread_id: Option<String>,
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
    pub tokens_in: u64,
    /// Completion tokens.
    pub tokens_out: u64,
    /// Cached read input tokens.
    pub tokens_cached_read: u64,
    /// Cached creation input tokens.
    pub tokens_cached_creation: u64,
    /// Cost in millionths of a US dollar.
    pub cost_micros: u64,
    /// Where the cost came from.
    pub cost_source: CostSource,
    /// Price-table version when estimated.
    pub price_table: Option<String>,
    /// Time to first token.
    pub ttft_ms: Option<u64>,
    /// Output tokens per generation second.
    pub tps: Option<f64>,
    /// Generation time, excluding tool time.
    pub generation_ms: u64,
    /// Wall-clock turn time.
    pub wall_ms: u64,
    /// Turn start.
    pub started_at: DateTime<Utc>,
    /// Turn end.
    pub completed_at: DateTime<Utc>,
    /// Event timestamp used for buckets.
    pub ts: DateTime<Utc>,
    /// Raw provider payload, for reconciliation.
    pub raw: Option<Value>,
    /// A proxy row already covered by a self-metered row.
    pub deduped: bool,
    /// Whether h4rn-connect accepted the row.
    pub synced: bool,
}

impl UsageRow {
    /// The connect sync projection of this row.
    pub fn to_sync_row(&self) -> h4rn_protocol::UsageSyncRow {
        h4rn_protocol::UsageSyncRow {
            row_id: self.row_id.clone(),
            thread_id: self.thread_id.clone(),
            run_id: self.run_id.clone(),
            workspace: self.workspace.clone(),
            machine: self.machine.clone(),
            account: self.account.clone(),
            harness: self.harness.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            credential: self.credential.clone(),
            source: self.source,
            correlation_id: self.correlation_id.clone(),
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            tokens_cached_read: self.tokens_cached_read,
            tokens_cached_creation: self.tokens_cached_creation,
            cost_micros: self.cost_micros,
            cost_source: self.cost_source,
            price_table: self.price_table.clone(),
            ttft_ms: self.ttft_ms,
            tps: self.tps,
            generation_ms: self.generation_ms,
            wall_ms: self.wall_ms,
            started_at: self.started_at,
            completed_at: self.completed_at,
            ts: self.ts,
            raw: self.raw.clone(),
            deduped: self.deduped,
        }
    }
}

/// Filter for ledger reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageFilter {
    /// Inclusive lower bound on `started_at`.
    pub since: Option<DateTime<Utc>>,
    /// Exclusive upper bound on `started_at`.
    pub until: Option<DateTime<Utc>>,
    /// One thread.
    pub thread_id: Option<String>,
    /// Also match rows whose parent is [`Self::thread_id`].
    pub include_children: bool,
    /// One workspace.
    pub workspace: Option<String>,
    /// One machine.
    pub machine: Option<String>,
}

/// SQLite ledger handle.
#[derive(Clone, Debug)]
pub struct MeterStore {
    pool: SqlitePool,
    /// Test-only: makes [`Self::finish_schedule`] fail, so a test can prove a
    /// delivery path survives a storage error instead of stranding the turn it
    /// was releasing.
    fail_schedule_finish: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl MeterStore {
    /// Opens (or creates) a ledger file.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;
        Self::from_pool(pool).await
    }

    /// An in-memory ledger for tests.
    pub async fn in_memory() -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        Self::from_pool(pool).await
    }

    async fn from_pool(pool: SqlitePool) -> Result<Self, StoreError> {
        let store = Self {
            pool,
            fail_schedule_finish: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Makes every later [`Self::finish_schedule`] fail.
    ///
    /// Test hook: a release path that gives up when the ledger cannot record
    /// the outcome would leave a prompt with no delivery owner, so the failure
    /// path itself needs a deterministic test. Nothing in production calls
    /// this.
    #[doc(hidden)]
    pub fn fail_schedule_finish_for_tests(&self) {
        self.fail_schedule_finish
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Adds a column to an existing database created before it existed.
    async fn ensure_column(
        &self,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<(), StoreError> {
        let existing: Vec<String> = sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .filter_map(|row| row.try_get::<String, _>("name").ok())
            .collect();
        if existing.iter().any(|name| name == column) {
            return Ok(());
        }
        sqlx::query(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn ensure_schema(&self) -> Result<(), StoreError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS meter_usage (
                row_id                 TEXT PRIMARY KEY,
                thread_id              TEXT,
                parent_thread_id       TEXT,
                run_id                 TEXT,
                workspace              TEXT,
                machine                TEXT,
                account                TEXT,
                harness                TEXT,
                model                  TEXT,
                provider               TEXT,
                credential             TEXT,
                source                 TEXT NOT NULL,
                correlation_id         TEXT,
                tokens_in              INTEGER NOT NULL DEFAULT 0,
                tokens_out             INTEGER NOT NULL DEFAULT 0,
                tokens_cached_read     INTEGER NOT NULL DEFAULT 0,
                tokens_cached_creation INTEGER NOT NULL DEFAULT 0,
                cost_micros            INTEGER NOT NULL DEFAULT 0,
                cost_source            TEXT NOT NULL DEFAULT 'unknown',
                price_table            TEXT,
                ttft_ms                INTEGER,
                tps                    REAL,
                generation_ms          INTEGER NOT NULL DEFAULT 0,
                wall_ms                INTEGER NOT NULL DEFAULT 0,
                started_at             TEXT NOT NULL,
                completed_at           TEXT NOT NULL,
                ts                     TEXT NOT NULL,
                raw_json               TEXT,
                deduped                INTEGER NOT NULL DEFAULT 0,
                synced                 INTEGER NOT NULL DEFAULT 0,
                synced_at              TEXT
            )",
        )
        .execute(&self.pool)
        .await?;
        self.ensure_column("meter_usage", "parent_thread_id", "TEXT")
            .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS meter_usage_thread_idx
             ON meter_usage (thread_id, started_at)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS meter_usage_parent_idx
             ON meter_usage (parent_thread_id, started_at)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS meter_usage_workspace_idx
             ON meter_usage (workspace, started_at)",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS meter_usage_synced_idx
             ON meter_usage (synced, completed_at)",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS meter_limits (
                record_id   TEXT PRIMARY KEY,
                thread_id   TEXT,
                harness     TEXT,
                account     TEXT,
                provider    TEXT,
                model       TEXT,
                reset_at    TEXT,
                message     TEXT,
                consumed    INTEGER,
                limit_units INTEGER,
                observed_at TEXT NOT NULL,
                source      TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS meter_schedules (
                schedule_id   TEXT PRIMARY KEY,
                thread_id     TEXT NOT NULL UNIQUE,
                scheduled_at  TEXT NOT NULL,
                resume_at     TEXT NOT NULL,
                attempt       INTEGER NOT NULL,
                state         TEXT NOT NULL,
                reason        TEXT NOT NULL,
                prompt        TEXT NOT NULL,
                failover_json TEXT,
                last_error    TEXT
            )",
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Inserts a row, ignoring an exact retry by `row_id`.
    ///
    /// Returns `true` when a row was actually written.
    pub async fn insert_row(&self, row: &UsageRow) -> Result<bool, StoreError> {
        let deduped = if row.source == UsageProvenance::ProxyReported {
            self.proxy_is_deduped(row).await? || row.deduped
        } else {
            row.deduped
        };
        let result = sqlx::query(
            "INSERT OR IGNORE INTO meter_usage
             (row_id, thread_id, parent_thread_id, run_id, workspace, machine, account, harness,
              model, provider, credential, source, correlation_id, tokens_in, tokens_out,
              tokens_cached_read, tokens_cached_creation, cost_micros, cost_source, price_table,
              ttft_ms, tps, generation_ms, wall_ms, started_at, completed_at, ts, raw_json,
              deduped, synced)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&row.row_id)
        .bind(&row.thread_id)
        .bind(&row.parent_thread_id)
        .bind(&row.run_id)
        .bind(&row.workspace)
        .bind(&row.machine)
        .bind(&row.account)
        .bind(&row.harness)
        .bind(&row.model)
        .bind(&row.provider)
        .bind(&row.credential)
        .bind(provenance_str(row.source))
        .bind(&row.correlation_id)
        .bind(row.tokens_in as i64)
        .bind(row.tokens_out as i64)
        .bind(row.tokens_cached_read as i64)
        .bind(row.tokens_cached_creation as i64)
        .bind(row.cost_micros as i64)
        .bind(cost_source_str(row.cost_source))
        .bind(&row.price_table)
        .bind(row.ttft_ms.map(|value| value as i64))
        .bind(row.tps)
        .bind(row.generation_ms as i64)
        .bind(row.wall_ms as i64)
        .bind(row.started_at.to_rfc3339())
        .bind(row.completed_at.to_rfc3339())
        .bind(row.ts.to_rfc3339())
        .bind(row.raw.as_ref().map(serde_json::to_string).transpose()?)
        .bind(i64::from(deduped))
        .bind(i64::from(row.synced))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Whether a proxy report is already covered by a self-metered row.
    ///
    /// The documented rule is `(thread_id, run_id)` or `correlation_id`. A
    /// third guard — same provider/model inside two minutes — keeps a proxy
    /// report from double-counting a turn h4rn already observed when the proxy
    /// cannot echo a per-request id.
    async fn proxy_is_deduped(&self, row: &UsageRow) -> Result<bool, StoreError> {
        let since = row.completed_at - chrono::Duration::seconds(120);
        let until = row.completed_at + chrono::Duration::seconds(120);
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM meter_usage
             WHERE source = 'self_metered'
               AND (
                    (thread_id IS NOT NULL AND thread_id = ? AND run_id IS NOT NULL AND run_id = ?)
                 OR (correlation_id IS NOT NULL AND correlation_id = ?)
                 OR (provider IS NOT NULL AND provider = ? AND model IS NOT NULL AND model = ?
                     AND completed_at BETWEEN ? AND ?)
               )",
        )
        .bind(&row.thread_id)
        .bind(&row.run_id)
        .bind(&row.correlation_id)
        .bind(&row.provider)
        .bind(&row.model)
        .bind(since.to_rfc3339())
        .bind(until.to_rfc3339())
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    /// Reads rows matching a filter, oldest first.
    pub async fn rows(&self, filter: &UsageFilter) -> Result<Vec<UsageRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM meter_usage
             WHERE (? IS NULL OR started_at >= ?)
               AND (? IS NULL OR started_at < ?)
               AND (? IS NULL OR thread_id = ? OR (? = 1 AND parent_thread_id = ?))
               AND (? IS NULL OR workspace = ?)
               AND (? IS NULL OR machine = ?)
             ORDER BY started_at ASC",
        )
        .bind(filter.since.map(|value| value.to_rfc3339()))
        .bind(filter.since.map(|value| value.to_rfc3339()))
        .bind(filter.until.map(|value| value.to_rfc3339()))
        .bind(filter.until.map(|value| value.to_rfc3339()))
        .bind(&filter.thread_id)
        .bind(&filter.thread_id)
        .bind(i64::from(filter.include_children))
        .bind(&filter.thread_id)
        .bind(&filter.workspace)
        .bind(&filter.workspace)
        .bind(&filter.machine)
        .bind(&filter.machine)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_from_sql).collect()
    }

    /// Every row, oldest first (tests and sync bootstrap).
    pub async fn all_rows(&self) -> Result<Vec<UsageRow>, StoreError> {
        self.rows(&UsageFilter::default()).await
    }

    /// Rows h4rn-connect has not accepted yet.
    pub async fn unsynced(&self, limit: u32) -> Result<Vec<UsageRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM meter_usage WHERE synced = 0
             ORDER BY completed_at ASC LIMIT ?",
        )
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_from_sql).collect()
    }

    /// Marks rows accepted by connect.
    pub async fn mark_synced(&self, row_ids: &[String]) -> Result<(), StoreError> {
        if row_ids.is_empty() {
            return Ok(());
        }
        let unique: HashSet<&String> = row_ids.iter().collect();
        let mut transaction = self.pool.begin().await?;
        let now = Utc::now().to_rfc3339();
        for row_id in unique {
            sqlx::query("UPDATE meter_usage SET synced = 1, synced_at = ? WHERE row_id = ?")
                .bind(&now)
                .bind(row_id)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Appends a limit observation.
    pub async fn insert_limit(&self, record: &LimitRecord) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT OR IGNORE INTO meter_limits
             (record_id, thread_id, harness, account, provider, model, reset_at, message,
              consumed, limit_units, observed_at, source)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&record.record_id)
        .bind(record.thread_id.as_ref().map(ThreadId::as_str))
        .bind(record.harness.map(|kind| kind.slug()))
        .bind(&record.account)
        .bind(&record.provider)
        .bind(&record.model)
        .bind(record.reset_at.map(|value| value.to_rfc3339()))
        .bind(&record.message)
        .bind(record.consumed.map(|value| value as i64))
        .bind(record.limit.map(|value| value as i64))
        .bind(record.observed_at.to_rfc3339())
        .bind(limit_source_str(record.source))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Newest limit observations first.
    pub async fn limits(&self, limit: u32) -> Result<Vec<LimitRecord>, StoreError> {
        let rows = sqlx::query("SELECT * FROM meter_limits ORDER BY observed_at DESC LIMIT ?")
            .bind(i64::from(limit))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(limit_from_sql).collect()
    }

    /// Inserts or replaces the schedule for one thread.
    pub async fn upsert_schedule(&self, schedule: &ResumeSchedule) -> Result<(), StoreError> {
        let failover = schedule
            .failover
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        sqlx::query(
            "INSERT INTO meter_schedules
             (schedule_id, thread_id, scheduled_at, resume_at, attempt, state, reason, prompt,
              failover_json, last_error)
             VALUES (?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(thread_id) DO UPDATE SET
                schedule_id = excluded.schedule_id,
                scheduled_at = excluded.scheduled_at,
                resume_at = excluded.resume_at,
                attempt = excluded.attempt,
                state = excluded.state,
                reason = excluded.reason,
                prompt = excluded.prompt,
                failover_json = excluded.failover_json,
                last_error = excluded.last_error",
        )
        .bind(&schedule.schedule_id)
        .bind(schedule.thread_id.as_str())
        .bind(schedule.scheduled_at.to_rfc3339())
        .bind(schedule.resume_at.to_rfc3339())
        .bind(i64::from(schedule.attempt))
        .bind(state_str(schedule.state))
        .bind(&schedule.reason)
        .bind(&schedule.prompt)
        .bind(failover)
        .bind(&schedule.last_error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Cancels one thread's schedule if it has not fired yet.
    ///
    /// Cancellation and the fire claim are both single conditional updates, so
    /// exactly one of them wins an attempt: a resume that already claimed the
    /// row cannot be cancelled out from under its turn, and a cancelled or
    /// superseded row can never be claimed.
    pub async fn cancel_schedule(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<ResumeSchedule>, StoreError> {
        let row = sqlx::query(
            "UPDATE meter_schedules SET state = ?
             WHERE thread_id = ? AND state IN (?, ?)
             RETURNING *",
        )
        .bind(state_str(ResumeState::Cancelled))
        .bind(thread_id.as_str())
        .bind(state_str(ResumeState::Scheduled))
        .bind(state_str(ResumeState::Waiting))
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(schedule_from_sql).transpose()
    }

    /// Claims one schedule attempt for firing.
    ///
    /// Returns `true` only for the caller that moved this exact attempt from
    /// `Scheduled`/`Waiting` to `Fired`. A retry of the same attempt, a newer
    /// attempt (which replaced the row) and a concurrent cancellation all see
    /// `false` and must not start a turn.
    pub async fn claim_schedule(
        &self,
        schedule_id: &str,
        attempt: u32,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query(
            "UPDATE meter_schedules SET state = ?, last_error = NULL
             WHERE schedule_id = ? AND attempt = ? AND state IN (?, ?)",
        )
        .bind(state_str(ResumeState::Fired))
        .bind(schedule_id)
        .bind(i64::from(attempt))
        .bind(state_str(ResumeState::Scheduled))
        .bind(state_str(ResumeState::Waiting))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Records the outcome of one attempt, unless it was cancelled or
    /// superseded by a newer attempt.
    ///
    /// Returns the stored row, or `None` when this attempt no longer owns the
    /// schedule. Callers must not broadcast a `None` outcome as if it landed:
    /// the ledger already reflects whichever row won.
    pub async fn finish_schedule(
        &self,
        schedule: &ResumeSchedule,
        state: ResumeState,
        error: Option<&str>,
    ) -> Result<Option<ResumeSchedule>, StoreError> {
        if self
            .fail_schedule_finish
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError::Data(
                "injected schedule write failure".to_owned(),
            ));
        }
        let failover = schedule
            .failover
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let row = sqlx::query(
            "UPDATE meter_schedules SET state = ?, last_error = ?, failover_json = ?
             WHERE schedule_id = ? AND attempt = ? AND state <> ?
             RETURNING *",
        )
        .bind(state_str(state))
        .bind(error)
        .bind(failover)
        .bind(&schedule.schedule_id)
        .bind(i64::from(schedule.attempt))
        .bind(state_str(ResumeState::Cancelled))
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(schedule_from_sql).transpose()
    }

    /// Every schedule, soonest first.
    pub async fn schedules(&self) -> Result<Vec<ResumeSchedule>, StoreError> {
        let rows = sqlx::query("SELECT * FROM meter_schedules ORDER BY resume_at ASC")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(schedule_from_sql).collect()
    }

    /// Schedules due at `now`, soonest first.
    pub async fn due_schedules(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<ResumeSchedule>, StoreError> {
        Ok(self
            .schedules()
            .await?
            .into_iter()
            .filter(|schedule| {
                matches!(
                    schedule.state,
                    ResumeState::Scheduled | ResumeState::Waiting
                ) && schedule.resume_at <= now
            })
            .collect())
    }

    /// Removes one thread's schedule.
    pub async fn delete_schedule(&self, thread_id: &ThreadId) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM meter_schedules WHERE thread_id = ?")
            .bind(thread_id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn row_from_sql(row: &sqlx::sqlite::SqliteRow) -> Result<UsageRow, StoreError> {
    Ok(UsageRow {
        row_id: row.try_get("row_id")?,
        thread_id: row.try_get("thread_id")?,
        parent_thread_id: row.try_get("parent_thread_id")?,
        run_id: row.try_get("run_id")?,
        workspace: row.try_get("workspace")?,
        machine: row.try_get("machine")?,
        account: row.try_get("account")?,
        harness: row.try_get("harness")?,
        model: row.try_get("model")?,
        provider: row.try_get("provider")?,
        credential: row.try_get("credential")?,
        source: parse_provenance(&row.try_get::<String, _>("source")?)?,
        correlation_id: row.try_get("correlation_id")?,
        tokens_in: row.try_get::<i64, _>("tokens_in")?.max(0) as u64,
        tokens_out: row.try_get::<i64, _>("tokens_out")?.max(0) as u64,
        tokens_cached_read: row.try_get::<i64, _>("tokens_cached_read")?.max(0) as u64,
        tokens_cached_creation: row.try_get::<i64, _>("tokens_cached_creation")?.max(0) as u64,
        cost_micros: row.try_get::<i64, _>("cost_micros")?.max(0) as u64,
        cost_source: parse_cost_source(&row.try_get::<String, _>("cost_source")?)?,
        price_table: row.try_get("price_table")?,
        ttft_ms: row
            .try_get::<Option<i64>, _>("ttft_ms")?
            .map(|value| value.max(0) as u64),
        tps: row.try_get("tps")?,
        generation_ms: row.try_get::<i64, _>("generation_ms")?.max(0) as u64,
        wall_ms: row.try_get::<i64, _>("wall_ms")?.max(0) as u64,
        started_at: parse_time(&row.try_get::<String, _>("started_at")?)?,
        completed_at: parse_time(&row.try_get::<String, _>("completed_at")?)?,
        ts: parse_time(&row.try_get::<String, _>("ts")?)?,
        raw: row
            .try_get::<Option<String>, _>("raw_json")?
            .map(|json| serde_json::from_str(&json))
            .transpose()?,
        deduped: row.try_get::<i64, _>("deduped")? != 0,
        synced: row.try_get::<i64, _>("synced")? != 0,
    })
}

fn limit_from_sql(row: &sqlx::sqlite::SqliteRow) -> Result<LimitRecord, StoreError> {
    Ok(LimitRecord {
        record_id: row.try_get("record_id")?,
        thread_id: row
            .try_get::<Option<String>, _>("thread_id")?
            .map(ThreadId::new),
        harness: row
            .try_get::<Option<String>, _>("harness")?
            .and_then(|slug| harness_from_slug(&slug)),
        account: row.try_get("account")?,
        provider: row.try_get("provider")?,
        model: row.try_get("model")?,
        reset_at: row
            .try_get::<Option<String>, _>("reset_at")?
            .map(|value| parse_time(&value))
            .transpose()?,
        message: row.try_get("message")?,
        consumed: row
            .try_get::<Option<i64>, _>("consumed")?
            .map(|value| value.max(0) as u64),
        limit: row
            .try_get::<Option<i64>, _>("limit_units")?
            .map(|value| value.max(0) as u64),
        observed_at: parse_time(&row.try_get::<String, _>("observed_at")?)?,
        source: parse_limit_source(&row.try_get::<String, _>("source")?)?,
    })
}

fn schedule_from_sql(row: &sqlx::sqlite::SqliteRow) -> Result<ResumeSchedule, StoreError> {
    let failover: Option<ResumeFailover> = row
        .try_get::<Option<String>, _>("failover_json")?
        .map(|json| serde_json::from_str(&json))
        .transpose()?;
    Ok(ResumeSchedule {
        schedule_id: row.try_get("schedule_id")?,
        thread_id: ThreadId::new(row.try_get::<String, _>("thread_id")?),
        scheduled_at: parse_time(&row.try_get::<String, _>("scheduled_at")?)?,
        resume_at: parse_time(&row.try_get::<String, _>("resume_at")?)?,
        attempt: row.try_get::<i64, _>("attempt")?.max(0) as u32,
        state: parse_state(&row.try_get::<String, _>("state")?)?,
        reason: row.try_get("reason")?,
        prompt: row.try_get("prompt")?,
        failover,
        last_error: row.try_get("last_error")?,
    })
}

fn parse_time(value: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|error| StoreError::Data(error.to_string()))
}

fn provenance_str(value: UsageProvenance) -> &'static str {
    match value {
        UsageProvenance::SelfMetered => "self_metered",
        UsageProvenance::ProxyReported => "proxy_reported",
    }
}

fn parse_provenance(value: &str) -> Result<UsageProvenance, StoreError> {
    match value {
        "self_metered" | "self" => Ok(UsageProvenance::SelfMetered),
        "proxy_reported" | "proxy" => Ok(UsageProvenance::ProxyReported),
        other => Err(StoreError::Data(format!("unknown usage source `{other}`"))),
    }
}

fn cost_source_str(value: CostSource) -> &'static str {
    match value {
        CostSource::HarnessReported => "harness_reported",
        CostSource::PriceTable => "price_table",
        CostSource::ProxyReported => "proxy_reported",
        CostSource::Unknown => "unknown",
    }
}

fn parse_cost_source(value: &str) -> Result<CostSource, StoreError> {
    match value {
        "harness_reported" => Ok(CostSource::HarnessReported),
        "price_table" => Ok(CostSource::PriceTable),
        "proxy_reported" => Ok(CostSource::ProxyReported),
        "unknown" => Ok(CostSource::Unknown),
        other => Err(StoreError::Data(format!("unknown cost source `{other}`"))),
    }
}

fn limit_source_str(value: LimitSource) -> &'static str {
    match value {
        LimitSource::Harness => "harness",
        LimitSource::Proxy => "proxy",
        LimitSource::Provider => "provider",
    }
}

fn parse_limit_source(value: &str) -> Result<LimitSource, StoreError> {
    match value {
        "harness" => Ok(LimitSource::Harness),
        "proxy" => Ok(LimitSource::Proxy),
        "provider" => Ok(LimitSource::Provider),
        other => Err(StoreError::Data(format!("unknown limit source `{other}`"))),
    }
}

fn state_str(value: ResumeState) -> &'static str {
    match value {
        ResumeState::Scheduled => "scheduled",
        ResumeState::Waiting => "waiting",
        ResumeState::Fired => "fired",
        ResumeState::Cancelled => "cancelled",
        ResumeState::Exhausted => "exhausted",
    }
}

fn parse_state(value: &str) -> Result<ResumeState, StoreError> {
    match value {
        "scheduled" => Ok(ResumeState::Scheduled),
        "waiting" => Ok(ResumeState::Waiting),
        "fired" => Ok(ResumeState::Fired),
        "cancelled" => Ok(ResumeState::Cancelled),
        "exhausted" => Ok(ResumeState::Exhausted),
        other => Err(StoreError::Data(format!("unknown resume state `{other}`"))),
    }
}

fn harness_from_slug(slug: &str) -> Option<HarnessKind> {
    match slug {
        "claude-code" => Some(HarnessKind::ClaudeCode),
        "codex" => Some(HarnessKind::Codex),
        "opencode" => Some(HarnessKind::Opencode),
        "antigravity" => Some(HarnessKind::Antigravity),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(row_id: &str, source: UsageProvenance) -> UsageRow {
        let now = Utc::now();
        UsageRow {
            row_id: row_id.into(),
            thread_id: Some("th_1".into()),
            parent_thread_id: None,
            run_id: Some("run_1".into()),
            workspace: Some("/repo".into()),
            machine: Some("host".into()),
            account: None,
            harness: Some("claude-code".into()),
            model: Some("claude-sonnet".into()),
            provider: Some("anthropic".into()),
            credential: None,
            source,
            correlation_id: None,
            tokens_in: 10,
            tokens_out: 2,
            tokens_cached_read: 1,
            tokens_cached_creation: 0,
            cost_micros: 5,
            cost_source: CostSource::PriceTable,
            price_table: Some("test".into()),
            ttft_ms: Some(10),
            tps: Some(1.0),
            generation_ms: 2_000,
            wall_ms: 3_000,
            started_at: now,
            completed_at: now,
            ts: now,
            raw: None,
            deduped: false,
            synced: false,
        }
    }

    #[tokio::test]
    async fn duplicated_row_ids_are_ignored() {
        let store = MeterStore::in_memory().await.unwrap();
        assert!(
            store
                .insert_row(&row("row_1", UsageProvenance::SelfMetered))
                .await
                .unwrap()
        );
        assert!(
            !store
                .insert_row(&row("row_1", UsageProvenance::SelfMetered))
                .await
                .unwrap()
        );
        assert_eq!(store.all_rows().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_child_rows_parent_is_persisted_and_filterable() {
        let store = MeterStore::in_memory().await.unwrap();
        let mut child = row("child_1", UsageProvenance::SelfMetered);
        child.thread_id = Some("th_child".into());
        child.parent_thread_id = Some("th_parent".into());
        store.insert_row(&child).await.unwrap();

        let stored = store.all_rows().await.unwrap();
        assert_eq!(stored[0].parent_thread_id.as_deref(), Some("th_parent"));

        let only_parent = store
            .rows(&UsageFilter {
                thread_id: Some("th_parent".into()),
                ..UsageFilter::default()
            })
            .await
            .unwrap();
        assert!(only_parent.is_empty());
        let rolled_up = store
            .rows(&UsageFilter {
                thread_id: Some("th_parent".into()),
                include_children: true,
                ..UsageFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(rolled_up.len(), 1);
        assert_eq!(rolled_up[0].row_id, "child_1");
    }

    #[tokio::test]
    async fn a_proxy_row_covered_by_a_self_row_is_deduped() {
        let store = MeterStore::in_memory().await.unwrap();
        store
            .insert_row(&row("self_1", UsageProvenance::SelfMetered))
            .await
            .unwrap();
        let proxy = row("proxy_1", UsageProvenance::ProxyReported);
        store.insert_row(&proxy).await.unwrap();
        let stored = store.all_rows().await.unwrap();
        let proxy = stored
            .iter()
            .find(|row| row.row_id == "proxy_1")
            .expect("proxy row stored");
        assert!(proxy.deduped, "proxy enrichment must not double count");
    }

    #[tokio::test]
    async fn correlation_ids_dedupe_even_when_the_provider_label_differs() {
        let store = MeterStore::in_memory().await.unwrap();
        let mut observed = row("self_1", UsageProvenance::SelfMetered);
        observed.correlation_id = Some("req_1".into());
        store.insert_row(&observed).await.unwrap();

        let mut reported = row("proxy_1", UsageProvenance::ProxyReported);
        reported.correlation_id = Some("req_1".into());
        reported.provider = Some("unknown-vendor".into());
        reported.model = Some("unknown-model".into());
        store.insert_row(&reported).await.unwrap();

        let stored = store.all_rows().await.unwrap();
        assert!(
            stored
                .iter()
                .find(|row| row.row_id == "proxy_1")
                .expect("proxy row stored")
                .deduped
        );
    }

    #[tokio::test]
    async fn an_unmatched_proxy_row_is_not_deduped() {
        let store = MeterStore::in_memory().await.unwrap();
        store
            .insert_row(&row("self_1", UsageProvenance::SelfMetered))
            .await
            .unwrap();
        let mut other = row("proxy_1", UsageProvenance::ProxyReported);
        other.thread_id = Some("th_2".into());
        other.run_id = Some("run_2".into());
        other.correlation_id = Some("req_2".into());
        other.provider = Some("openai".into());
        other.model = Some("gpt-6.1-sol".into());
        store.insert_row(&other).await.unwrap();
        let stored = store.all_rows().await.unwrap();
        assert!(
            !stored
                .iter()
                .find(|row| row.row_id == "proxy_1")
                .expect("proxy row stored")
                .deduped,
            "a distinct proxy call is real usage and must count"
        );
    }

    #[tokio::test]
    async fn schedules_round_trip_and_are_due_only_after_resume_at() {
        let store = MeterStore::in_memory().await.unwrap();
        let now = Utc::now();
        let schedule = ResumeSchedule {
            schedule_id: "sched_1".into(),
            thread_id: ThreadId::new("th_1"),
            scheduled_at: now,
            resume_at: now + chrono::Duration::seconds(5),
            attempt: 0,
            state: ResumeState::Scheduled,
            reason: "reset".into(),
            prompt: "continue".into(),
            failover: None,
            last_error: None,
        };
        store.upsert_schedule(&schedule).await.unwrap();
        assert_eq!(store.schedules().await.unwrap(), vec![schedule.clone()]);
        assert!(store.due_schedules(now).await.unwrap().is_empty());
        assert_eq!(
            store
                .due_schedules(now + chrono::Duration::seconds(6))
                .await
                .unwrap(),
            vec![schedule]
        );
    }
}
