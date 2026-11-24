//! Durable daemon journal.
//!
//! `h4rn-core` owns the domain event log (lifecycle events) and the per-thread
//! sequence numbers of ADR-003. The daemon additionally stores the **projected
//! wire events** so a reconnecting client can replay exactly what it missed,
//! including streamed `MessageDelta` frames that are not part of the lifecycle
//! state machine. Both tables live in the same SQLite file and therefore in the
//! same cache the relay will eventually mirror.
//!
//! Sequence numbers here are daemon-global and strictly increasing, which is
//! what a client subscribed to several threads needs. `GetThread` replays by
//! that global sequence and filters one thread.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use h4rn_core::{EventStore, SqliteEventStore, StoreError, ThreadEvent};
use h4rn_protocol::{
    Event, ForgeKind, ForgeRepoRef, HarnessKind, HostId, RuntimeMode, ScheduleDestination,
    ScheduleRun, ScheduleRunOutcome, ScheduleSpec, ScheduleTask, ScheduleTaskId, SubagentId,
    ThreadId, Usage, WorktreeRequest,
};
use serde_json::json;
use sqlx::{Row, SqlitePool};

use crate::issue_links::IssueLinkRecord;
use crate::pr_links::PrLinkRecord;

/// Where a thread's current title came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TitleSource {
    /// The first prompt line, or the workspace folder name.
    #[default]
    Prompt,
    /// Written by the configured title model.
    Generated,
    /// Typed by the person; never replaced automatically.
    Renamed,
}

impl TitleSource {
    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Generated => "generated",
            Self::Renamed => "renamed",
        }
    }

    /// Reads a stored spelling, defaulting to the prompt-derived title.
    pub fn parse(value: &str) -> Self {
        match value {
            "generated" => Self::Generated,
            "renamed" => Self::Renamed,
            _ => Self::Prompt,
        }
    }
}

/// Everything the daemon needs to rebuild a thread row without replaying it.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadMeta {
    /// Thread id.
    pub thread_id: ThreadId,
    /// Thread that delegated this one, when this is a subagent child.
    pub parent_thread_id: Option<ThreadId>,
    /// Subagent record this child belongs to, when it is one.
    pub subagent_id: Option<SubagentId>,
    /// Host running the harness.
    pub host: HostId,
    /// Harness adapter.
    pub harness: HarnessKind,
    /// Configured ACP agent instance, when the harness is generic ACP.
    pub agent_id: Option<String>,
    /// Human title, derived from the first prompt.
    pub title: String,
    /// Where the current title came from.
    ///
    /// A generated title only replaces a prompt-derived one, never a name the
    /// person typed, and the origin survives a daemon restart.
    pub title_source: TitleSource,
    /// Absolute workspace path.
    pub workspace_path: String,
    /// Model slug.
    pub model: String,
    /// Permission autonomy chosen at creation.
    pub runtime_mode: RuntimeMode,
    /// Reasoning effort in the harness's own vocabulary, when one was chosen.
    ///
    /// Absent in rows written before thread controls; those threads keep the
    /// harness default.
    pub reasoning_effort: Option<String>,
    /// Whether the thread asks for the harness's fast tier.
    pub fast: bool,
    /// Worktree intent recorded at creation.
    pub worktree: Option<WorktreeRequest>,
    /// Creation time, used as the initial lifecycle clock.
    pub created_at: DateTime<Utc>,
    /// Cumulative usage.
    pub usage: Usage,
    /// Harness-native session id for resume.
    pub resume_id: Option<String>,
    /// Fingerprint of the configured ACP command line `resume_id` belongs to.
    ///
    /// A different value means the stored session id describes a command the
    /// thread no longer runs, so the next start opens a fresh session.
    pub agent_fingerprint: Option<String>,
    /// A prompt a paused thread still has to deliver.
    ///
    /// A failed write puts its prompt back and the typed limit pauses the
    /// thread; the schedule is what delivers it. Without this record a daemon
    /// restart would forget the prompt and fire the policy's resume prompt
    /// instead, so the person's message would silently turn into "continue".
    pub parked: Option<ParkedPrompt>,
    /// A provider-history rewind the next session start must apply.
    ///
    /// The thread's transcript is already truncated when this is written; it
    /// survives a restart so the provider conversation is truncated too, even
    /// if the daemon stops before the next prompt.
    pub pending_rewind: Option<h4rn_protocol::PendingRewind>,
    /// The saved conversation this thread was imported from, when it was.
    ///
    /// An imported thread starts with no live session. The kept messages, with
    /// their timestamps, are what its first new turn hands to a fresh session
    /// (or a native resume, when the harness can take one).
    pub imported: Option<ImportedOrigin>,
}

/// The saved conversation a thread was imported from.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ImportedOrigin {
    /// Which harness saved it.
    pub source: h4rn_protocol::ImportSource,
    /// Transcript file it came from (read-only).
    pub source_path: String,
    /// Harness-native session id, when the transcript records one.
    pub session_id: Option<String>,
    /// The checkout the conversation belongs to, when it differed from the
    /// working directory it ran in (a linked worktree or a subdirectory).
    ///
    /// The working directory stays what a native resume needs; this is only
    /// the sidebar's grouping, saved so it survives a restart.
    #[serde(default)]
    pub project_path: Option<String>,
    /// Whether the thread's one native resume has not been spent yet.
    ///
    /// Cleared when the first session starts: after that the thread has a live
    /// conversation of its own, and its handoff must not replay the imported
    /// messages over the turns made in h4rn.
    pub native_resume: bool,
}

/// One message of a thread's visible history.
///
/// Read back from the journaled items when a handoff needs it, so the imported
/// origin does not hold a second copy of the transcript.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ImportedMessage {
    /// Who authored it.
    pub role: h4rn_protocol::MessageRole,
    /// Text, already redacted before it reached the journal.
    pub text: String,
    /// When it was written in the source conversation.
    pub at: DateTime<Utc>,
}

/// One source conversation already imported, keyed by its identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedSourceRecord {
    /// Source key: harness plus session id, or the transcript path.
    pub key: String,
    /// Thread the conversation became.
    pub thread_id: ThreadId,
    /// Transcript path.
    pub path: String,
    /// When it was imported.
    pub imported_at: DateTime<Utc>,
}

/// A prompt a paused thread still has to deliver, persisted with the thread.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ParkedPrompt {
    /// The turn the prompt belongs to, already recorded in the journal.
    pub turn_id: h4rn_protocol::TurnId,
    /// Prompt text exactly as the person sent it.
    pub text: String,
    /// Attachments to deliver with it.
    pub attachments: Vec<ParkedAttachment>,
    /// The permission ceiling the sender attached to this prompt, if any.
    ///
    /// It survives a restart with the prompt: a supervised agent's message must
    /// not come back at a more permissive mode just because the daemon paused.
    #[serde(default)]
    pub mode_ceiling: Option<h4rn_protocol::RuntimeMode>,
    /// The outside grant that asked for this prompt, when one did.
    ///
    /// It survives a restart with the prompt: the resumed turn must re-check
    /// that the exact token family is still live, and refuse to run if the grant
    /// was revoked while the daemon was down.
    #[serde(default)]
    pub grant: Option<h4rn_protocol::WorkGrant>,
}

/// One attachment of a [`ParkedPrompt`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ParkedAttachment {
    /// Content-addressed blob id.
    pub id: h4rn_protocol::AttachmentId,
    /// Absolute path of the blob on the daemon host.
    pub path: String,
    /// Media type, when known.
    pub media_type: String,
}

/// One ordered, projected event as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct WireEvent {
    /// Daemon-global monotonic sequence.
    pub seq: u64,
    /// Owning thread.
    pub thread_id: ThreadId,
    /// The event sent to clients.
    pub event: Event,
}

/// Journal failures.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// The core event store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A daemon-owned SQLite query failed.
    #[error("journal database error: {0}")]
    Database(#[from] sqlx::Error),
    /// The data directory could not be prepared.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A projected event could not be serialised.
    #[error("journal serialisation error: {0}")]
    Serialisation(#[from] serde_json::Error),
}

/// Durable log of domain and projected events.
#[async_trait]
pub trait Journal: Send + Sync {
    /// Inserts or updates metadata for one thread.
    async fn upsert_thread(&self, meta: &ThreadMeta) -> Result<(), JournalError>;

    /// All thread metadata, oldest first.
    async fn list_threads(&self) -> Result<Vec<ThreadMeta>, JournalError>;

    /// Appends domain events plus their projected wire events in one call.
    ///
    /// The metadata upsert happens before the append so resume ids and usage
    /// survive a crash alongside the event that changed them.
    async fn append(
        &self,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
    ) -> Result<Vec<u64>, JournalError>;

    /// Appends projected events that are derived (for example a tab change
    /// produced by the background ticker) without a lifecycle event.
    async fn append_wire(
        &self,
        thread_id: &ThreadId,
        wire: &[Event],
    ) -> Result<Vec<u64>, JournalError>;

    /// Projected events for one thread with `seq > after`, in order.
    async fn read_wire_since(
        &self,
        thread_id: &ThreadId,
        after: u64,
    ) -> Result<Vec<WireEvent>, JournalError>;

    /// Projected events across every thread with `seq > after`, in order.
    ///
    /// This is the global replay a reconnecting subscriber asks for; the
    /// caller holds the append lock so the result is a gap-free prefix.
    async fn read_wire_since_global(&self, after: u64) -> Result<Vec<WireEvent>, JournalError>;

    /// The highest sequence number the journal has allocated, or 0 when empty.
    ///
    /// Read while the append lock is held, this is the boundary between the
    /// subscribe replay and live events.
    async fn head(&self) -> Result<u64, JournalError>;

    /// The domain log for one thread, used to rebuild lifecycle state.
    async fn read_domain(&self, thread_id: &ThreadId) -> Result<Vec<ThreadEvent>, JournalError>;

    /// Inserts or refreshes one thread → pull request link.
    async fn upsert_pr_link(&self, link: &PrLinkRecord) -> Result<(), JournalError>;

    /// Removes one thread → pull request link.
    async fn remove_pr_link(
        &self,
        thread_id: &ThreadId,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<(), JournalError>;

    /// Every PR link one thread holds, oldest link first.
    async fn pr_links_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<PrLinkRecord>, JournalError>;

    /// Every thread that links one PR, oldest link first.
    async fn pr_links_for_pr(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<Vec<PrLinkRecord>, JournalError>;

    /// The SQLite pool backing this journal, when it has one.
    ///
    /// Guest links live in the same database file as the thread journal
    /// (ADR-044), so the daemon needs the pool rather than a second writer.
    fn sql_pool(&self) -> Option<&SqlitePool> {
        None
    }

    /// Inserts or refreshes one thread → issue link.
    async fn upsert_issue_link(&self, link: &IssueLinkRecord) -> Result<(), JournalError>;

    /// Removes one thread → issue link.
    async fn remove_issue_link(
        &self,
        thread_id: &ThreadId,
        scope: &h4rn_protocol::IssueScope,
        issue_id: &str,
    ) -> Result<(), JournalError>;

    /// Every issue link one thread holds, oldest link first.
    async fn issue_links_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<IssueLinkRecord>, JournalError>;

    /// Every scheduled task, oldest first, with its most recent run.
    async fn list_schedules(&self) -> Result<Vec<ScheduleTask>, JournalError>;

    /// One scheduled task, when it exists.
    async fn schedule(
        &self,
        task_id: &ScheduleTaskId,
    ) -> Result<Option<ScheduleTask>, JournalError>;

    /// Inserts or replaces one scheduled task.
    async fn upsert_schedule(&self, task: &ScheduleTask) -> Result<(), JournalError>;

    /// Deletes one task and its run history, returning whether the task existed.
    async fn delete_schedule(&self, task_id: &ScheduleTaskId) -> Result<bool, JournalError>;

    /// Appends one run, fills in its durable id, and trims the task's history
    /// to the newest `keep` entries.
    async fn append_schedule_run(
        &self,
        run: &mut ScheduleRun,
        keep: usize,
    ) -> Result<(), JournalError>;

    /// Appends one run and replaces the task's scheduling state in one
    /// transaction.
    ///
    /// A run and the advancement it describes land together or not at all, so
    /// a crash cannot leave a slot either double-run or lost.
    async fn commit_schedule_run(
        &self,
        run: &mut ScheduleRun,
        task: &ScheduleTask,
        keep: usize,
    ) -> Result<(), JournalError>;

    /// A task's runs, newest first, at most `limit`.
    async fn schedule_runs(
        &self,
        task_id: &ScheduleTaskId,
        limit: usize,
    ) -> Result<Vec<ScheduleRun>, JournalError>;

    /// Every source conversation already imported, oldest first.
    async fn list_imported_sources(&self) -> Result<Vec<ImportedSourceRecord>, JournalError>;

    /// Remembers that a source conversation became `thread_id`.
    async fn record_imported_source(
        &self,
        record: &ImportedSourceRecord,
    ) -> Result<(), JournalError>;

    /// Claims a source conversation and writes its thread in one transaction.
    ///
    /// Returns the new thread's wire sequence numbers, or `None` when another
    /// run had already claimed the source. A crash between the claim and the
    /// thread write rolls both back, so a retry imports the conversation once.
    async fn commit_thread_import(
        &self,
        claim: &ImportedSourceRecord,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
    ) -> Result<Option<Vec<u64>>, JournalError>;
}

/// SQLite-backed journal.
#[derive(Clone, Debug)]
pub struct SqliteJournal {
    store: SqliteEventStore,
    pool: SqlitePool,
}

impl SqliteJournal {
    /// Opens (or creates) a database file and applies both schemas.
    pub async fn connect(path: impl AsRef<std::path::Path>) -> Result<Self, JournalError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            crate::config::create_private_dir(parent)
                .map_err(|error| JournalError::Io(std::io::Error::other(error.to_string())))?;
        }
        let store = SqliteEventStore::connect(path).await?;
        Self::from_store(store).await
    }

    /// Opens an in-memory journal for tests.
    pub async fn in_memory() -> Result<Self, JournalError> {
        let store = SqliteEventStore::in_memory().await?;
        Self::from_store(store).await
    }

    /// Opens (or creates) a journal file.
    pub async fn open(path: impl AsRef<std::path::Path>) -> Result<Self, JournalError> {
        let store = SqliteEventStore::connect(path).await?;
        Self::from_store(store).await
    }

    async fn from_store(store: SqliteEventStore) -> Result<Self, JournalError> {
        let pool = store.pool().clone();
        let journal = Self { store, pool };
        journal.ensure_schema().await?;
        Ok(journal)
    }

    async fn ensure_schema(&self) -> Result<(), JournalError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_threads (
                thread_id      TEXT PRIMARY KEY,
                host           TEXT NOT NULL,
                harness        TEXT NOT NULL,
                title          TEXT NOT NULL,
                title_source   TEXT NOT NULL DEFAULT 'prompt',
                workspace_path TEXT NOT NULL,
                model          TEXT NOT NULL,
                created_at     TEXT NOT NULL,
                usage_json     TEXT NOT NULL,
                resume_id      TEXT,
                runtime_mode   TEXT NOT NULL DEFAULT 'approval_required',
                reasoning_effort TEXT,
                fast           INTEGER NOT NULL DEFAULT 0,
                worktree_json  TEXT,
                parent_thread_id TEXT,
                subagent_id    TEXT,
                agent_id       TEXT,
                agent_fingerprint TEXT,
                parked_json    TEXT,
                imported_json  TEXT,
                rewind_json    TEXT
            )",
        )
        .execute(&self.pool)
        .await?;

        // `CREATE TABLE IF NOT EXISTS` cannot add columns to a database written
        // by an older daemon, so migrate the two Phase 1c fields explicitly.
        self.ensure_column(
            "daemon_threads",
            "runtime_mode",
            "TEXT NOT NULL DEFAULT 'approval_required'",
        )
        .await?;
        self.ensure_column("daemon_threads", "worktree_json", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "parent_thread_id", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "subagent_id", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "agent_id", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "agent_fingerprint", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "parked_json", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "imported_json", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "rewind_json", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "reasoning_effort", "TEXT")
            .await?;
        self.ensure_column("daemon_threads", "fast", "INTEGER NOT NULL DEFAULT 0")
            .await?;
        self.ensure_column(
            "daemon_threads",
            "title_source",
            "TEXT NOT NULL DEFAULT 'prompt'",
        )
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_wire_events (
                seq         INTEGER PRIMARY KEY AUTOINCREMENT,
                thread_id   TEXT NOT NULL,
                event_json  TEXT NOT NULL,
                recorded_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_daemon_wire_events_thread_seq
             ON daemon_wire_events (thread_id, seq)",
        )
        .execute(&self.pool)
        .await?;

        // Thread ↔ pull request links live beside the thread rows so they
        // survive a restart. The PR's state is not stored: it is read from the
        // forge whenever it is shown.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_pr_links (
                thread_id  TEXT NOT NULL,
                account_id TEXT NOT NULL,
                forge      TEXT NOT NULL,
                base_url   TEXT NOT NULL,
                owner      TEXT NOT NULL,
                repo       TEXT NOT NULL,
                number     INTEGER NOT NULL,
                linked_at  TEXT NOT NULL,
                PRIMARY KEY (thread_id, forge, base_url, owner, repo, number)
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_daemon_pr_links_pr
             ON daemon_pr_links (forge, base_url, owner, repo, number)",
        )
        .execute(&self.pool)
        .await?;

        // Issue links keep the scope as JSON so a future tracker does not need
        // a schema change; `scope_key` is the stable identity for conflicts.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_issue_links (
                thread_id  TEXT NOT NULL,
                account_id TEXT NOT NULL,
                scope_kind TEXT NOT NULL,
                scope_key  TEXT NOT NULL,
                scope_json TEXT NOT NULL,
                issue_id   TEXT NOT NULL,
                linked_at  TEXT NOT NULL,
                PRIMARY KEY (thread_id, scope_kind, scope_key, issue_id)
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_daemon_issue_links_thread
             ON daemon_issue_links (thread_id)",
        )
        .execute(&self.pool)
        .await?;

        // Scheduled tasks live in the same journal file as threads so a
        // restart restores the timer, the webhook secret and the run history.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_schedules (
                task_id        TEXT PRIMARY KEY,
                name           TEXT NOT NULL,
                host           TEXT NOT NULL,
                workspace_path TEXT NOT NULL,
                harness        TEXT NOT NULL,
                agent_id       TEXT,
                model          TEXT NOT NULL,
                runtime_mode   TEXT NOT NULL,
                prompt         TEXT NOT NULL,
                schedule_json  TEXT NOT NULL,
                timezone       TEXT NOT NULL,
                destination    TEXT NOT NULL,
                enabled        INTEGER NOT NULL,
                thread_id      TEXT,
                webhook_secret TEXT,
                once_consumed  INTEGER NOT NULL DEFAULT 0,
                once_slot      TEXT,
                next_run_at    TEXT,
                created_at     TEXT NOT NULL,
                updated_at     TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;

        // A database written before the once-consumed marker exists needs the
        // column added; the default keeps every stored task re-runnable.
        self.ensure_column(
            "daemon_schedules",
            "once_consumed",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        // The spent marker now carries the slot's identity; a database from
        // before that column derives it from the stored schedule on read.
        self.ensure_column("daemon_schedules", "once_slot", "TEXT")
            .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_schedule_runs (
                run_id        INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id       TEXT NOT NULL,
                scheduled_for TEXT,
                started_at    TEXT NOT NULL,
                thread_id     TEXT,
                outcome       TEXT NOT NULL,
                detail        TEXT
            )",
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_daemon_schedule_runs_task
             ON daemon_schedule_runs (task_id, run_id)",
        )
        .execute(&self.pool)
        .await?;

        // One row per source conversation that became a thread, so a second
        // import run never duplicates it.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_imported_sources (
                source_key  TEXT PRIMARY KEY,
                thread_id   TEXT NOT NULL,
                path        TEXT NOT NULL,
                imported_at TEXT NOT NULL
            )",
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn ensure_column(
        &self,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<(), JournalError> {
        let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(&self.pool)
            .await?;
        let exists = rows.iter().any(|row| {
            row.try_get::<String, _>("name")
                .is_ok_and(|name| name == column)
        });
        if !exists {
            sqlx::query(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition}"
            ))
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }
}

/// Upserts a thread row on any connection (a pooled one or a transaction's).
async fn upsert_thread_on(
    conn: &mut sqlx::SqliteConnection,
    meta: &ThreadMeta,
) -> Result<(), JournalError> {
    let usage_json = serde_json::to_string(&meta.usage)?;
    let harness = serde_json::to_string(&meta.harness)?;
    let harness = harness.trim_matches('"').to_owned();
    let runtime_mode = serde_json::to_string(&meta.runtime_mode)?;
    let runtime_mode = runtime_mode.trim_matches('"').to_owned();
    let worktree_json = serde_json::to_string(&meta.worktree)?;
    let parked_json = serde_json::to_string(&meta.parked)?;
    let imported_json = serde_json::to_string(&meta.imported)?;
    let rewind_json = serde_json::to_string(&meta.pending_rewind)?;
    sqlx::query(
        "INSERT INTO daemon_threads
            (thread_id, host, harness, title, title_source, workspace_path, model, created_at, usage_json, resume_id, runtime_mode, reasoning_effort, fast, worktree_json, parent_thread_id, subagent_id, agent_id, agent_fingerprint, parked_json, imported_json, rewind_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(thread_id) DO UPDATE SET
            host = excluded.host,
            harness = excluded.harness,
            title = excluded.title,
            title_source = excluded.title_source,
            workspace_path = excluded.workspace_path,
            model = excluded.model,
            usage_json = excluded.usage_json,
            resume_id = excluded.resume_id,
            runtime_mode = excluded.runtime_mode,
            reasoning_effort = excluded.reasoning_effort,
            fast = excluded.fast,
            worktree_json = excluded.worktree_json,
            parent_thread_id = excluded.parent_thread_id,
            subagent_id = excluded.subagent_id,
            agent_id = excluded.agent_id,
            agent_fingerprint = excluded.agent_fingerprint,
            parked_json = excluded.parked_json,
            imported_json = excluded.imported_json,
            rewind_json = excluded.rewind_json",
    )
    .bind(meta.thread_id.as_str())
    .bind(meta.host.as_str())
    .bind(harness)
    .bind(&meta.title)
    .bind(meta.title_source.as_str())
    .bind(&meta.workspace_path)
    .bind(&meta.model)
    .bind(meta.created_at.to_rfc3339())
    .bind(usage_json)
    .bind(meta.resume_id.as_deref())
    .bind(runtime_mode)
    .bind(meta.reasoning_effort.as_deref())
    .bind(meta.fast as i64)
    .bind(worktree_json)
    .bind(meta.parent_thread_id.as_ref().map(ThreadId::as_str))
    .bind(meta.subagent_id.as_ref().map(SubagentId::as_str))
    .bind(meta.agent_id.as_deref())
    .bind(meta.agent_fingerprint.as_deref())
    .bind(parked_json)
    .bind(imported_json)
    .bind(rewind_json)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Inserts wire events on any connection and returns their global sequences.
async fn append_wire_on(
    conn: &mut sqlx::SqliteConnection,
    thread_id: &ThreadId,
    wire: &[Event],
) -> Result<Vec<u64>, JournalError> {
    let recorded_at = Utc::now().to_rfc3339();
    let mut seqs = Vec::with_capacity(wire.len());
    for event in wire {
        let event_json = serde_json::to_string(event)?;
        let result = sqlx::query(
            "INSERT INTO daemon_wire_events (thread_id, event_json, recorded_at) VALUES (?, ?, ?)",
        )
        .bind(thread_id.as_str())
        .bind(event_json)
        .bind(&recorded_at)
        .execute(&mut *conn)
        .await?;
        seqs.push(result.last_insert_rowid() as u64);
    }
    Ok(seqs)
}

#[async_trait]
impl Journal for SqliteJournal {
    fn sql_pool(&self) -> Option<&SqlitePool> {
        Some(&self.pool)
    }

    async fn upsert_thread(&self, meta: &ThreadMeta) -> Result<(), JournalError> {
        let mut conn = self.pool.acquire().await?;
        upsert_thread_on(&mut conn, meta).await
    }

    async fn list_threads(&self) -> Result<Vec<ThreadMeta>, JournalError> {
        let rows = sqlx::query(
            "SELECT thread_id, host, harness, title, title_source, workspace_path, model, created_at, usage_json, resume_id, runtime_mode, reasoning_effort, fast, worktree_json, parent_thread_id, subagent_id, agent_id, agent_fingerprint, parked_json, imported_json, rewind_json
             FROM daemon_threads ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;

        let mut metas = Vec::with_capacity(rows.len());
        for row in rows {
            let thread_id: String = row.try_get("thread_id")?;
            let host: String = row.try_get("host")?;
            let harness: String = row.try_get("harness")?;
            let usage_json: String = row.try_get("usage_json")?;
            let created_at: String = row.try_get("created_at")?;
            let runtime_mode: String = row.try_get("runtime_mode")?;
            let worktree_json: Option<String> = row.try_get("worktree_json")?;
            let created_at = DateTime::parse_from_rfc3339(&created_at)
                .map(|value| value.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            metas.push(ThreadMeta {
                thread_id: ThreadId::new(thread_id),
                parent_thread_id: row
                    .try_get::<Option<String>, _>("parent_thread_id")?
                    .map(ThreadId::new),
                subagent_id: row
                    .try_get::<Option<String>, _>("subagent_id")?
                    .map(SubagentId::new),
                host: HostId::new(host),
                harness: serde_json::from_str(&format!("\"{harness}\""))
                    .unwrap_or(HarnessKind::ClaudeCode),
                agent_id: row.try_get("agent_id")?,
                title: row.try_get("title")?,
                title_source: TitleSource::parse(
                    &row.try_get::<String, _>("title_source")
                        .unwrap_or_else(|_| "prompt".to_owned()),
                ),
                workspace_path: row.try_get("workspace_path")?,
                model: row.try_get("model")?,
                runtime_mode: serde_json::from_str(&format!("\"{runtime_mode}\""))
                    .unwrap_or_default(),
                reasoning_effort: row.try_get("reasoning_effort")?,
                fast: row.try_get::<i64, _>("fast")? != 0,
                worktree: worktree_json
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .flatten(),
                created_at,
                usage: serde_json::from_str(&usage_json).unwrap_or_default(),
                resume_id: row.try_get("resume_id")?,
                agent_fingerprint: row.try_get("agent_fingerprint")?,
                parked: row
                    .try_get::<Option<String>, _>("parked_json")?
                    .and_then(|json| serde_json::from_str(&json).ok()),
                imported: row
                    .try_get::<Option<String>, _>("imported_json")?
                    .and_then(|json| serde_json::from_str(&json).ok()),
                pending_rewind: row
                    .try_get::<Option<String>, _>("rewind_json")?
                    .and_then(|json| serde_json::from_str(&json).ok()),
            });
        }
        Ok(metas)
    }

    async fn append(
        &self,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
    ) -> Result<Vec<u64>, JournalError> {
        // One transaction: the thread row, the domain log and the wire log
        // commit together or not at all, so a crash or a failed insert cannot
        // leave a domain event without its wire event.
        let mut tx = self.pool.begin().await?;
        upsert_thread_on(&mut tx, meta).await?;
        if !domain.is_empty() {
            SqliteEventStore::append_in(&mut tx, &meta.thread_id, domain).await?;
        }
        let seqs = if wire.is_empty() {
            Vec::new()
        } else {
            append_wire_on(&mut tx, &meta.thread_id, wire).await?
        };
        tx.commit().await?;
        Ok(seqs)
    }

    async fn append_wire(
        &self,
        thread_id: &ThreadId,
        wire: &[Event],
    ) -> Result<Vec<u64>, JournalError> {
        if wire.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.pool.acquire().await?;
        append_wire_on(&mut conn, thread_id, wire).await
    }

    async fn read_wire_since(
        &self,
        thread_id: &ThreadId,
        after: u64,
    ) -> Result<Vec<WireEvent>, JournalError> {
        let rows = sqlx::query(
            "SELECT seq, event_json FROM daemon_wire_events
             WHERE thread_id = ? AND seq > ? ORDER BY seq ASC",
        )
        .bind(thread_id.as_str())
        .bind(after as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let seq: i64 = row.try_get("seq")?;
            let event_json: String = row.try_get("event_json")?;
            events.push(WireEvent {
                seq: seq as u64,
                thread_id: thread_id.clone(),
                event: serde_json::from_str(&event_json)?,
            });
        }
        Ok(events)
    }

    async fn read_wire_since_global(&self, after: u64) -> Result<Vec<WireEvent>, JournalError> {
        let rows = sqlx::query(
            "SELECT seq, thread_id, event_json FROM daemon_wire_events
             WHERE seq > ? ORDER BY seq ASC",
        )
        .bind(after as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let seq: i64 = row.try_get("seq")?;
            let thread_id: String = row.try_get("thread_id")?;
            let event_json: String = row.try_get("event_json")?;
            events.push(WireEvent {
                seq: seq as u64,
                thread_id: ThreadId::new(thread_id),
                event: serde_json::from_str(&event_json)?,
            });
        }
        Ok(events)
    }

    async fn head(&self) -> Result<u64, JournalError> {
        let head: Option<i64> = sqlx::query_scalar("SELECT MAX(seq) FROM daemon_wire_events")
            .fetch_one(&self.pool)
            .await?;
        Ok(head.unwrap_or(0) as u64)
    }

    async fn read_domain(&self, thread_id: &ThreadId) -> Result<Vec<ThreadEvent>, JournalError> {
        Ok(self
            .store
            .read_since(thread_id, 0)
            .await?
            .into_iter()
            .map(|stored| stored.event)
            .collect())
    }

    async fn upsert_pr_link(&self, link: &PrLinkRecord) -> Result<(), JournalError> {
        sqlx::query(
            "INSERT INTO daemon_pr_links
                (thread_id, account_id, forge, base_url, owner, repo, number, linked_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(thread_id, forge, base_url, owner, repo, number) DO UPDATE SET
                account_id = excluded.account_id",
        )
        .bind(link.thread_id.as_str())
        .bind(&link.account_id)
        .bind(link.repo.forge.slug())
        .bind(&link.repo.base_url)
        .bind(&link.repo.owner)
        .bind(&link.repo.repo)
        .bind(i64::from(link.number))
        .bind(link.linked_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn remove_pr_link(
        &self,
        thread_id: &ThreadId,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<(), JournalError> {
        sqlx::query(
            "DELETE FROM daemon_pr_links
             WHERE thread_id = ? AND forge = ? AND base_url = ? AND owner = ? AND repo = ?
               AND number = ?",
        )
        .bind(thread_id.as_str())
        .bind(repo.forge.slug())
        .bind(&repo.base_url)
        .bind(&repo.owner)
        .bind(&repo.repo)
        .bind(i64::from(number))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn pr_links_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<PrLinkRecord>, JournalError> {
        let rows = sqlx::query(
            "SELECT thread_id, account_id, forge, base_url, owner, repo, number, linked_at
             FROM daemon_pr_links WHERE thread_id = ?
             ORDER BY linked_at ASC, rowid ASC",
        )
        .bind(thread_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(pr_link_from_row).collect()
    }

    async fn pr_links_for_pr(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<Vec<PrLinkRecord>, JournalError> {
        let rows = sqlx::query(
            "SELECT thread_id, account_id, forge, base_url, owner, repo, number, linked_at
             FROM daemon_pr_links
             WHERE forge = ? AND base_url = ? AND owner = ? AND repo = ? AND number = ?
             ORDER BY linked_at ASC, rowid ASC",
        )
        .bind(repo.forge.slug())
        .bind(&repo.base_url)
        .bind(&repo.owner)
        .bind(&repo.repo)
        .bind(i64::from(number))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(pr_link_from_row).collect()
    }

    async fn upsert_issue_link(&self, link: &IssueLinkRecord) -> Result<(), JournalError> {
        let (kind, key) = issue_scope_parts(&link.scope);
        let scope_json = serde_json::to_string(&link.scope)
            .map_err(|error| JournalError::Database(sqlx::Error::Protocol(error.to_string())))?;
        sqlx::query(
            "INSERT INTO daemon_issue_links
                (thread_id, account_id, scope_kind, scope_key, scope_json, issue_id, linked_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(thread_id, scope_kind, scope_key, issue_id) DO UPDATE SET
                account_id = excluded.account_id",
        )
        .bind(link.thread_id.as_str())
        .bind(&link.account_id)
        .bind(kind)
        .bind(key)
        .bind(scope_json)
        .bind(&link.issue_id)
        .bind(link.linked_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn remove_issue_link(
        &self,
        thread_id: &ThreadId,
        scope: &h4rn_protocol::IssueScope,
        issue_id: &str,
    ) -> Result<(), JournalError> {
        let (kind, key) = issue_scope_parts(scope);
        sqlx::query(
            "DELETE FROM daemon_issue_links
             WHERE thread_id = ? AND scope_kind = ? AND scope_key = ? AND issue_id = ?",
        )
        .bind(thread_id.as_str())
        .bind(kind)
        .bind(key)
        .bind(issue_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn issue_links_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<IssueLinkRecord>, JournalError> {
        let rows = sqlx::query(
            "SELECT thread_id, account_id, scope_json, issue_id, linked_at
             FROM daemon_issue_links WHERE thread_id = ?
             ORDER BY linked_at ASC, rowid ASC",
        )
        .bind(thread_id.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(issue_link_from_row).collect()
    }

    async fn list_schedules(&self) -> Result<Vec<ScheduleTask>, JournalError> {
        let rows = sqlx::query(
            "SELECT task_id, name, host, workspace_path, harness, agent_id, model,
                    runtime_mode, prompt, schedule_json, timezone, destination, enabled,
                    thread_id, webhook_secret, once_consumed, once_slot, next_run_at, created_at, updated_at
             FROM daemon_schedules ORDER BY created_at ASC, rowid ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut tasks = Vec::with_capacity(rows.len());
        for row in rows {
            let mut task = schedule_task_from_row(row)?;
            task.last_run = self.schedule_runs(&task.task_id, 1).await?.pop();
            tasks.push(task);
        }
        Ok(tasks)
    }

    async fn schedule(
        &self,
        task_id: &ScheduleTaskId,
    ) -> Result<Option<ScheduleTask>, JournalError> {
        let row = sqlx::query(
            "SELECT task_id, name, host, workspace_path, harness, agent_id, model,
                    runtime_mode, prompt, schedule_json, timezone, destination, enabled,
                    thread_id, webhook_secret, once_consumed, once_slot, next_run_at, created_at, updated_at
             FROM daemon_schedules WHERE task_id = ?",
        )
        .bind(task_id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut task = schedule_task_from_row(row)?;
        task.last_run = self.schedule_runs(task_id, 1).await?.pop();
        Ok(Some(task))
    }

    async fn upsert_schedule(&self, task: &ScheduleTask) -> Result<(), JournalError> {
        let mut tx = self.pool.begin().await?;
        upsert_schedule_row(&mut tx, task).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_schedule(&self, task_id: &ScheduleTaskId) -> Result<bool, JournalError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM daemon_schedule_runs WHERE task_id = ?")
            .bind(task_id.as_str())
            .execute(&mut *tx)
            .await?;
        let deleted = sqlx::query("DELETE FROM daemon_schedules WHERE task_id = ?")
            .bind(task_id.as_str())
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        tx.commit().await?;
        Ok(deleted)
    }

    async fn append_schedule_run(
        &self,
        run: &mut ScheduleRun,
        keep: usize,
    ) -> Result<(), JournalError> {
        let mut tx = self.pool.begin().await?;
        insert_schedule_run(&mut tx, run).await?;
        trim_schedule_runs(&mut tx, &run.task_id, keep).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn commit_schedule_run(
        &self,
        run: &mut ScheduleRun,
        task: &ScheduleTask,
        keep: usize,
    ) -> Result<(), JournalError> {
        let mut tx = self.pool.begin().await?;
        insert_schedule_run(&mut tx, run).await?;
        trim_schedule_runs(&mut tx, &run.task_id, keep).await?;
        upsert_schedule_row(&mut tx, task).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn schedule_runs(
        &self,
        task_id: &ScheduleTaskId,
        limit: usize,
    ) -> Result<Vec<ScheduleRun>, JournalError> {
        let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let rows = sqlx::query(
            "SELECT run_id, task_id, scheduled_for, started_at, thread_id, outcome, detail
             FROM daemon_schedule_runs WHERE task_id = ?
             ORDER BY run_id DESC LIMIT ?",
        )
        .bind(task_id.as_str())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(schedule_run_from_row).collect()
    }

    async fn list_imported_sources(&self) -> Result<Vec<ImportedSourceRecord>, JournalError> {
        let rows = sqlx::query(
            "SELECT source_key, thread_id, path, imported_at
             FROM daemon_imported_sources ORDER BY imported_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let imported_at: String = row.try_get("imported_at")?;
            records.push(ImportedSourceRecord {
                key: row.try_get("source_key")?,
                thread_id: ThreadId::new(row.try_get::<String, _>("thread_id")?),
                path: row.try_get("path")?,
                imported_at: DateTime::parse_from_rfc3339(&imported_at)
                    .map(|value| value.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            });
        }
        Ok(records)
    }

    async fn record_imported_source(
        &self,
        record: &ImportedSourceRecord,
    ) -> Result<(), JournalError> {
        sqlx::query(
            "INSERT INTO daemon_imported_sources (source_key, thread_id, path, imported_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(source_key) DO UPDATE SET
                thread_id = excluded.thread_id,
                path = excluded.path,
                imported_at = excluded.imported_at",
        )
        .bind(&record.key)
        .bind(record.thread_id.as_str())
        .bind(&record.path)
        .bind(record.imported_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn commit_thread_import(
        &self,
        claim: &ImportedSourceRecord,
        meta: &ThreadMeta,
        domain: &[ThreadEvent],
        wire: &[Event],
    ) -> Result<Option<Vec<u64>>, JournalError> {
        // One transaction: the claim, the thread row, the domain log and the
        // wire log commit together or not at all. A concurrent run loses the
        // claim insert and writes nothing; a crash rolls everything back, so
        // the retry is still the conversation's first import.
        let mut tx = self.pool.begin().await?;
        let claimed = sqlx::query(
            "INSERT INTO daemon_imported_sources (source_key, thread_id, path, imported_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(source_key) DO NOTHING",
        )
        .bind(&claim.key)
        .bind(claim.thread_id.as_str())
        .bind(&claim.path)
        .bind(claim.imported_at.to_rfc3339())
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() == 0 {
            tx.rollback().await?;
            return Ok(None);
        }
        upsert_thread_on(&mut tx, meta).await?;
        if !domain.is_empty() {
            SqliteEventStore::append_in(&mut tx, &meta.thread_id, domain).await?;
        }
        let seqs = if wire.is_empty() {
            Vec::new()
        } else {
            append_wire_on(&mut tx, &meta.thread_id, wire).await?
        };
        tx.commit().await?;
        Ok(Some(seqs))
    }
}

#[cfg(test)]
impl SqliteJournal {
    /// Stores a raw `next_run_at` so an upgrade test can write a timestamp the
    /// daemon cannot decode.
    pub(crate) async fn set_raw_next_run_at(
        &self,
        task_id: &ScheduleTaskId,
        raw: Option<&str>,
    ) -> Result<(), JournalError> {
        sqlx::query("UPDATE daemon_schedules SET next_run_at = ? WHERE task_id = ?")
            .bind(raw)
            .bind(task_id.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The stored `next_run_at` exactly as written, undecoded.
    pub(crate) async fn raw_next_run_at(
        &self,
        task_id: &ScheduleTaskId,
    ) -> Result<Option<String>, JournalError> {
        let row = sqlx::query("SELECT next_run_at FROM daemon_schedules WHERE task_id = ?")
            .bind(task_id.as_str())
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let raw: Option<String> = row.try_get("next_run_at")?;
        Ok(raw)
    }
}

/// Inserts one run row inside a transaction and fills in its durable id.
async fn insert_schedule_run(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    run: &mut ScheduleRun,
) -> Result<(), JournalError> {
    let outcome = serde_json::to_string(&run.outcome)?;
    let outcome = outcome.trim_matches('"').to_owned();
    let result = sqlx::query(
        "INSERT INTO daemon_schedule_runs
            (task_id, scheduled_for, started_at, thread_id, outcome, detail)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(run.task_id.as_str())
    .bind(run.scheduled_for.map(|at| at.to_rfc3339()))
    .bind(run.started_at.to_rfc3339())
    .bind(run.thread_id.as_ref().map(ThreadId::as_str))
    .bind(outcome)
    .bind(run.detail.as_deref())
    .execute(&mut **tx)
    .await?;
    run.run_id = result.last_insert_rowid() as u64;
    Ok(())
}

/// Keeps only the newest `keep` runs of one task inside a transaction.
async fn trim_schedule_runs(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    task_id: &ScheduleTaskId,
    keep: usize,
) -> Result<(), JournalError> {
    let keep = i64::try_from(keep.max(1)).unwrap_or(i64::MAX);
    sqlx::query(
        "DELETE FROM daemon_schedule_runs
         WHERE task_id = ? AND run_id NOT IN (
            SELECT run_id FROM daemon_schedule_runs
            WHERE task_id = ? ORDER BY run_id DESC LIMIT ?
         )",
    )
    .bind(task_id.as_str())
    .bind(task_id.as_str())
    .bind(keep)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Inserts or replaces one task row inside a transaction.
async fn upsert_schedule_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    task: &ScheduleTask,
) -> Result<(), JournalError> {
    let harness = serde_json::to_string(&task.harness)?;
    let harness = harness.trim_matches('"').to_owned();
    let runtime_mode = serde_json::to_string(&task.runtime_mode)?;
    let runtime_mode = runtime_mode.trim_matches('"').to_owned();
    let destination = serde_json::to_string(&task.destination)?;
    let destination = destination.trim_matches('"').to_owned();
    let schedule_json = serde_json::to_string(&task.schedule)?;
    sqlx::query(
        "INSERT INTO daemon_schedules
            (task_id, name, host, workspace_path, harness, agent_id, model,
             runtime_mode, prompt, schedule_json, timezone, destination, enabled,
             thread_id, webhook_secret, once_consumed, once_slot, next_run_at, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(task_id) DO UPDATE SET
            name = excluded.name,
            host = excluded.host,
            workspace_path = excluded.workspace_path,
            harness = excluded.harness,
            agent_id = excluded.agent_id,
            model = excluded.model,
            runtime_mode = excluded.runtime_mode,
            prompt = excluded.prompt,
            schedule_json = excluded.schedule_json,
            timezone = excluded.timezone,
            destination = excluded.destination,
            enabled = excluded.enabled,
            thread_id = excluded.thread_id,
            webhook_secret = excluded.webhook_secret,
            once_consumed = excluded.once_consumed,
            once_slot = excluded.once_slot,
            next_run_at = excluded.next_run_at,
            updated_at = excluded.updated_at",
    )
    .bind(task.task_id.as_str())
    .bind(&task.name)
    .bind(task.host.as_str())
    .bind(&task.workspace_path)
    .bind(harness)
    .bind(task.agent_id.as_deref())
    .bind(&task.model)
    .bind(runtime_mode)
    .bind(&task.prompt)
    .bind(schedule_json)
    .bind(&task.timezone)
    .bind(destination)
    .bind(task.enabled as i64)
    .bind(task.thread_id.as_ref().map(ThreadId::as_str))
    .bind(webhook_secret_of(task))
    .bind(task.once_consumed as i64)
    .bind(task.once_consumed_slot.as_deref())
    .bind(task.next_run_at.map(|at| at.to_rfc3339()))
    .bind(task.created_at.to_rfc3339())
    .bind(task.updated_at.to_rfc3339())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The webhook secret embedded in a task's public path.
fn webhook_secret_of(task: &ScheduleTask) -> Option<&str> {
    task.webhook_path
        .as_deref()
        .and_then(|path| path.strip_prefix("/webhooks/"))
        .filter(|secret| !secret.is_empty())
}

fn decode_enum<T: serde::de::DeserializeOwned>(
    value: &str,
    column: &str,
) -> Result<T, JournalError> {
    serde_json::from_str(&format!("\"{value}\"")).map_err(|error| {
        JournalError::Database(sqlx::Error::Protocol(format!(
            "unknown value {value:?} in {column}: {error}"
        )))
    })
}

fn schedule_task_from_row(row: sqlx::sqlite::SqliteRow) -> Result<ScheduleTask, JournalError> {
    let task_id: String = row.try_get("task_id")?;
    let harness: String = row.try_get("harness")?;
    let runtime_mode: String = row.try_get("runtime_mode")?;
    let destination: String = row.try_get("destination")?;
    let schedule_json: String = row.try_get("schedule_json")?;
    let created_at: String = row.try_get("created_at")?;
    let updated_at: String = row.try_get("updated_at")?;
    let next_run_at: Option<String> = row.try_get("next_run_at")?;
    let webhook_secret: Option<String> = row.try_get("webhook_secret")?;
    let webhook_path_set = webhook_secret
        .as_deref()
        .is_some_and(|secret| !secret.is_empty());
    let schedule = serde_json::from_str::<ScheduleSpec>(&schedule_json)?;
    // A row written before the slot identity existed only has the flag; the
    // stored schedule names the slot it spent. The zone joins the identity in
    // `ScheduleService::open`, which migrates this older form.
    let once_slot: Option<String> = row.try_get("once_slot")?;
    let legacy_consumed: i64 = row.try_get("once_consumed")?;
    let once_consumed_slot = once_slot.or_else(|| {
        if legacy_consumed == 0 {
            None
        } else {
            schedule.legacy_once_slot()
        }
    });
    let once_consumed = once_consumed_slot.is_some();
    let timezone: String = row.try_get("timezone")?;
    Ok(ScheduleTask {
        task_id: ScheduleTaskId::new(task_id),
        name: row.try_get("name")?,
        host: HostId::new(row.try_get::<String, _>("host")?),
        workspace_path: row.try_get("workspace_path")?,
        harness: decode_enum(&harness, "daemon_schedules.harness")?,
        agent_id: row.try_get("agent_id")?,
        model: row.try_get("model")?,
        runtime_mode: decode_enum(&runtime_mode, "daemon_schedules.runtime_mode")?,
        prompt: row.try_get("prompt")?,
        schedule,
        timezone,
        destination: serde_json::from_str::<ScheduleDestination>(&format!("\"{destination}\""))
            .map_err(|error| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unknown destination {destination:?}: {error}"
                )))
            })?,
        enabled: row.try_get::<i64, _>("enabled")? != 0,
        thread_id: row
            .try_get::<Option<String>, _>("thread_id")?
            .map(ThreadId::new),
        webhook_path: webhook_secret
            .filter(|secret| !secret.is_empty())
            .map(|secret| format!("/webhooks/{secret}")),
        webhook_path_set,
        once_consumed,
        once_consumed_slot,
        next_run_at: next_run_at.and_then(|at| {
            DateTime::parse_from_rfc3339(&at)
                .ok()
                .map(|value| value.with_timezone(&Utc))
        }),
        last_run: None,
        created_at: DateTime::parse_from_rfc3339(&created_at)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|error| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unparsable created_at {created_at:?} in daemon_schedules: {error}"
                )))
            })?,
        updated_at: DateTime::parse_from_rfc3339(&updated_at)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|error| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unparsable updated_at {updated_at:?} in daemon_schedules: {error}"
                )))
            })?,
    })
}

fn schedule_run_from_row(row: sqlx::sqlite::SqliteRow) -> Result<ScheduleRun, JournalError> {
    let run_id: i64 = row.try_get("run_id")?;
    let task_id: String = row.try_get("task_id")?;
    let scheduled_for: Option<String> = row.try_get("scheduled_for")?;
    let started_at: String = row.try_get("started_at")?;
    let outcome: String = row.try_get("outcome")?;
    Ok(ScheduleRun {
        run_id: u64::try_from(run_id).map_err(|_| {
            JournalError::Database(sqlx::Error::Protocol(format!(
                "run id {run_id} does not fit in daemon_schedule_runs"
            )))
        })?,
        task_id: ScheduleTaskId::new(task_id),
        scheduled_for: scheduled_for.and_then(|at| {
            DateTime::parse_from_rfc3339(&at)
                .ok()
                .map(|value| value.with_timezone(&Utc))
        }),
        started_at: DateTime::parse_from_rfc3339(&started_at)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|error| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unparsable started_at {started_at:?} in daemon_schedule_runs: {error}"
                )))
            })?,
        thread_id: row
            .try_get::<Option<String>, _>("thread_id")?
            .map(ThreadId::new),
        outcome: decode_enum::<ScheduleRunOutcome>(&outcome, "daemon_schedule_runs.outcome")?,
        detail: row.try_get("detail")?,
    })
}

fn pr_link_from_row(row: sqlx::sqlite::SqliteRow) -> Result<PrLinkRecord, JournalError> {
    let forge: String = row.try_get("forge")?;
    let linked_at: String = row.try_get("linked_at")?;
    let number: i64 = row.try_get("number")?;
    Ok(PrLinkRecord {
        thread_id: ThreadId::new(row.try_get::<String, _>("thread_id")?),
        account_id: row.try_get("account_id")?,
        repo: ForgeRepoRef {
            forge: ForgeKind::parse(&forge).ok_or_else(|| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unknown forge slug {forge:?} in daemon_pr_links"
                )))
            })?,
            base_url: row.try_get("base_url")?,
            owner: row.try_get("owner")?,
            repo: row.try_get("repo")?,
        },
        number: u32::try_from(number).map_err(|_| {
            JournalError::Database(sqlx::Error::Protocol(format!(
                "PR number {number} does not fit in daemon_pr_links"
            )))
        })?,
        linked_at: DateTime::parse_from_rfc3339(&linked_at)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|error| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unparsable linked_at {linked_at:?} in daemon_pr_links: {error}"
                )))
            })?,
    })
}

/// The stable, queryable identity of an issue scope.
fn issue_scope_parts(scope: &h4rn_protocol::IssueScope) -> (&'static str, String) {
    match scope {
        h4rn_protocol::IssueScope::Forge { repo } => (
            "forge",
            format!(
                "{}|{}|{}|{}",
                repo.forge.slug(),
                repo.base_url,
                repo.owner,
                repo.repo
            ),
        ),
        h4rn_protocol::IssueScope::Linear { team_id } => ("linear", team_id.clone()),
    }
}

fn issue_link_from_row(row: sqlx::sqlite::SqliteRow) -> Result<IssueLinkRecord, JournalError> {
    let scope_json: String = row.try_get("scope_json")?;
    let linked_at: String = row.try_get("linked_at")?;
    Ok(IssueLinkRecord {
        thread_id: ThreadId::new(row.try_get::<String, _>("thread_id")?),
        account_id: row.try_get("account_id")?,
        scope: serde_json::from_str(&scope_json).map_err(|error| {
            JournalError::Database(sqlx::Error::Protocol(format!(
                "unparsable issue scope in daemon_issue_links: {error}"
            )))
        })?,
        issue_id: row.try_get("issue_id")?,
        linked_at: DateTime::parse_from_rfc3339(&linked_at)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|error| {
                JournalError::Database(sqlx::Error::Protocol(format!(
                    "unparsable linked_at {linked_at:?} in daemon_issue_links: {error}"
                )))
            })?,
    })
}

/// A helper for tests and diagnostics: the JSON shape of a thread row.
pub fn thread_meta_json(meta: &ThreadMeta) -> serde_json::Value {
    json!({
        "thread_id": meta.thread_id,
        "parent_thread_id": meta.parent_thread_id,
        "subagent_id": meta.subagent_id,
        "host": meta.host,
        "harness": meta.harness,
        "agent_id": meta.agent_id,
        "title": meta.title,
        "title_source": meta.title_source.as_str(),
        "workspace_path": meta.workspace_path,
        "model": meta.model,
        "created_at": meta.created_at,
        "runtime_mode": meta.runtime_mode,
        "reasoning_effort": meta.reasoning_effort,
        "fast": meta.fast,
        "worktree": meta.worktree,
        "usage": meta.usage,
        "resume_id": meta.resume_id,
        "agent_fingerprint": meta.agent_fingerprint,
        "parked": meta.parked,
        "pending_rewind": meta.pending_rewind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use h4rn_protocol::{AgentState, ThreadSummary};

    fn meta(thread_id: &str) -> ThreadMeta {
        ThreadMeta {
            thread_id: ThreadId::new(thread_id),
            parent_thread_id: None,
            subagent_id: None,
            host: HostId::new("host_test"),
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            title: "first prompt".into(),
            title_source: TitleSource::Prompt,
            workspace_path: "/tmp/work".into(),
            model: "test-model".into(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            fast: false,
            worktree: None,
            created_at: Utc::now(),
            usage: Usage::default(),
            resume_id: None,
            agent_fingerprint: None,
            parked: None,
            pending_rewind: None,
            imported: None,
        }
    }

    fn summary() -> ThreadSummary {
        ThreadSummary {
            thread_id: ThreadId::new("th_one"),
            parent_thread_id: None,
            host: HostId::new("host_test"),
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            title: "first prompt".into(),
            workspace_path: "/tmp/work".into(),
            project_path: None,
            branch: Some("h4rn/first-prompt".into()),
            model: "test-model".into(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            reasoning_efforts: Vec::new(),
            models: Vec::new(),
            fast: false,
            tab: h4rn_protocol::ThreadTab::Working,
            placed_by_user: false,
            pinned: false,
            snoozed_until: None,
            last_activity: Utc::now(),
            agent_state: AgentState::Thinking,
            usage: Usage::default(),
            ttft_ms: None,
            context_window: None,
            context_fill_percent: None,
            needs_attention: false,
            pending_guest_requests: 0,
            unread: false,
            resume_at: None,
            limit: None,
            rewind: Default::default(),
            file_rewind: false,
        }
    }

    #[tokio::test]
    async fn a_parked_prompt_keeps_its_mode_ceiling_across_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.sqlite");
        let journal = SqliteJournal::connect(&path).await.expect("journal");
        let mut one = meta("th_parked");
        one.parked = Some(ParkedPrompt {
            turn_id: h4rn_protocol::TurnId::new("turn_one"),
            text: "carry on".into(),
            attachments: Vec::new(),
            mode_ceiling: Some(RuntimeMode::ApprovalRequired),
            grant: Some(h4rn_protocol::WorkGrant {
                client_id: "client-1".into(),
                family: "family-1".into(),
                ceiling: Some(RuntimeMode::ApprovalRequired),
            }),
        });
        journal.upsert_thread(&one).await.unwrap();
        drop(journal);

        let reopened = SqliteJournal::connect(&path).await.expect("reopen");
        let rows = reopened.list_threads().await.unwrap();
        assert_eq!(rows, vec![one]);
        assert_eq!(
            rows[0]
                .parked
                .as_ref()
                .and_then(|parked| parked.mode_ceiling),
            Some(RuntimeMode::ApprovalRequired),
            "the parked prompt lost its ceiling"
        );
    }

    #[tokio::test]
    async fn a_thread_row_written_before_agent_id_migrates_with_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("old.db");
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("open old database");
        sqlx::query(
            "CREATE TABLE daemon_threads (
                thread_id      TEXT PRIMARY KEY,
                host           TEXT NOT NULL,
                harness        TEXT NOT NULL,
                title          TEXT NOT NULL,
                workspace_path TEXT NOT NULL,
                model          TEXT NOT NULL,
                created_at     TEXT NOT NULL,
                usage_json     TEXT NOT NULL,
                resume_id      TEXT,
                runtime_mode   TEXT NOT NULL DEFAULT 'approval_required',
                worktree_json  TEXT,
                parent_thread_id TEXT,
                subagent_id    TEXT
            )",
        )
        .execute(&pool)
        .await
        .expect("old schema");
        sqlx::query(
            "INSERT INTO daemon_threads
                (thread_id, host, harness, title, workspace_path, model, created_at, usage_json)
             VALUES ('th_old', 'host_local', 'claude_code', 'old', '/tmp/work', 'm', '2026-01-01T00:00:00Z', '{}')",
        )
        .execute(&pool)
        .await
        .expect("old row");
        pool.close().await;

        let journal = SqliteJournal::connect(&path).await.expect("open journal");
        let threads = journal.list_threads().await.expect("list");
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id.as_str(), "th_old");
        assert_eq!(threads[0].agent_id, None);
        assert_eq!(threads[0].harness, HarnessKind::ClaudeCode);
    }

    #[tokio::test]
    async fn wire_sequences_are_global_and_monotonic() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_one");
        let two = meta("th_two");

        let first = journal
            .append(
                &one,
                &[ThreadEvent::TurnQueued { at: Utc::now() }],
                &[Event::ThreadUpserted(summary())],
            )
            .await
            .unwrap();
        let second = journal
            .append_wire(&two.thread_id, &[Event::ThreadUpserted(summary())])
            .await
            .unwrap();
        assert_eq!(first, vec![1]);
        assert_eq!(second, vec![2], "sequences are daemon-global");
    }

    #[tokio::test]
    async fn metadata_round_trips() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_one");
        journal.upsert_thread(&one).await.unwrap();
        let rows = journal.list_threads().await.unwrap();
        assert_eq!(rows, vec![one]);
    }

    #[tokio::test]
    async fn replay_filters_by_thread_and_sequence() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_one");
        let two = meta("th_two");

        journal
            .append(
                &one,
                &[ThreadEvent::TurnQueued { at: Utc::now() }],
                &[
                    Event::ThreadUpserted(summary()),
                    Event::AgentStateChanged {
                        thread_id: ThreadId::new("th_one"),
                        state: AgentState::Thinking,
                    },
                ],
            )
            .await
            .unwrap();
        journal
            .append_wire(&two.thread_id, &[Event::ThreadUpserted(summary())])
            .await
            .unwrap();

        let all = journal.read_wire_since(&one.thread_id, 0).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].seq, 1);
        assert_eq!(all[1].seq, 2);

        let tail = journal.read_wire_since(&one.thread_id, 1).await.unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].seq, 2);
        assert!(
            journal
                .read_wire_since(&one.thread_id, 2)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn head_reports_the_last_allocated_sequence() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        assert_eq!(journal.head().await.unwrap(), 0);

        let one = meta("th_one");
        journal
            .append(
                &one,
                &[ThreadEvent::TurnQueued { at: Utc::now() }],
                &[Event::ThreadUpserted(summary())],
            )
            .await
            .unwrap();
        journal
            .append_wire(&one.thread_id, &[Event::ThreadUpserted(summary())])
            .await
            .unwrap();

        assert_eq!(journal.head().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn global_replay_spans_threads_in_sequence_order() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_one");
        let two = meta("th_two");
        journal
            .append(
                &one,
                &[ThreadEvent::TurnQueued { at: Utc::now() }],
                &[Event::ThreadUpserted(summary())],
            )
            .await
            .unwrap();
        journal
            .append_wire(&two.thread_id, &[Event::ThreadUpserted(summary())])
            .await
            .unwrap();

        let replayed = journal.read_wire_since_global(1).await.unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].seq, 2);
        assert_eq!(replayed[0].thread_id, two.thread_id);
    }

    #[tokio::test]
    async fn domain_log_is_readable_for_restart_replay() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_one");
        journal
            .append(
                &one,
                &[
                    ThreadEvent::TurnStarted { at: Utc::now() },
                    ThreadEvent::TurnCompleted {
                        status: h4rn_protocol::TurnStatus::Completed,
                        at: Utc::now(),
                    },
                ],
                &[],
            )
            .await
            .unwrap();
        let domain = journal.read_domain(&one.thread_id).await.unwrap();
        assert_eq!(domain.len(), 2);
    }

    #[tokio::test]
    async fn append_is_atomic_when_the_wire_insert_fails() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let one = meta("th_one");
        sqlx::query("DROP TABLE daemon_wire_events")
            .execute(&journal.pool)
            .await
            .unwrap();

        let result = journal
            .append(
                &one,
                &[ThreadEvent::TurnStarted { at: Utc::now() }],
                &[Event::ThreadUpserted(summary())],
            )
            .await;
        assert!(result.is_err(), "the wire insert must fail");

        assert!(
            journal
                .read_domain(&one.thread_id)
                .await
                .unwrap()
                .is_empty(),
            "a failed append must not leave a domain event behind"
        );
        assert!(
            journal.list_threads().await.unwrap().is_empty(),
            "a failed append must not leave the thread row behind"
        );
    }

    #[tokio::test]
    async fn a_claimed_source_commits_its_thread_once() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let imported_meta = meta("th_imported");
        let claim = ImportedSourceRecord {
            key: "claude_code:session-1".into(),
            thread_id: imported_meta.thread_id.clone(),
            path: "/tmp/session-1.jsonl".into(),
            imported_at: Utc::now(),
        };
        let wire = vec![Event::ThreadUpserted(summary())];

        let first = journal
            .commit_thread_import(&claim, &imported_meta, &[], &wire)
            .await
            .unwrap();
        assert!(first.is_some(), "the first claim writes the thread");
        assert_eq!(journal.list_threads().await.unwrap().len(), 1);

        // A second run with the same source writes nothing, as an overlapping
        // run or a retry after a completed import would.
        let second_meta = meta("th_other");
        let mut second_claim = claim.clone();
        second_claim.thread_id = second_meta.thread_id.clone();
        let second = journal
            .commit_thread_import(&second_claim, &second_meta, &[], &wire)
            .await
            .unwrap();
        assert!(second.is_none(), "an existing claim is not imported again");
        let metas = journal.list_threads().await.unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].thread_id.as_str(), "th_imported");
    }

    #[tokio::test]
    async fn a_failed_import_write_leaves_no_claim_behind() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let imported_meta = meta("th_imported");
        let claim = ImportedSourceRecord {
            key: "claude_code:session-2".into(),
            thread_id: imported_meta.thread_id.clone(),
            path: "/tmp/session-2.jsonl".into(),
            imported_at: Utc::now(),
        };
        // A wire event that cannot be written rolls the claim back with it.
        let wire = vec![Event::ThreadUpserted(summary())];
        sqlx::query("CREATE TRIGGER fail_wire BEFORE INSERT ON daemon_wire_events BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
            .execute(&journal.pool)
            .await
            .unwrap();
        assert!(
            journal
                .commit_thread_import(&claim, &imported_meta, &[], &wire)
                .await
                .is_err()
        );
        assert!(journal.list_imported_sources().await.unwrap().is_empty());
        assert!(journal.list_threads().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unparsable_pr_link_timestamp_is_a_journal_error() {
        let journal = SqliteJournal::in_memory().await.unwrap();
        let thread_id = ThreadId::new("th_one");
        journal
            .upsert_pr_link(&PrLinkRecord {
                thread_id: thread_id.clone(),
                account_id: "acct_1".into(),
                repo: ForgeRepoRef {
                    forge: ForgeKind::GitHub,
                    base_url: "https://github.com".into(),
                    owner: "h4rn".into(),
                    repo: "api".into(),
                },
                number: 7,
                linked_at: Utc::now(),
            })
            .await
            .unwrap();

        // A row written by a damaged file or an older schema: the timestamp is
        // not RFC 3339. The row is rejected, never silently stamped "now".
        sqlx::query("UPDATE daemon_pr_links SET linked_at = 'not-a-time'")
            .execute(&journal.pool)
            .await
            .unwrap();

        let error = journal
            .pr_links_for_thread(&thread_id)
            .await
            .expect_err("an unparsable linked_at must not be replaced with now");
        assert!(
            matches!(
                error,
                JournalError::Database(sqlx::Error::Protocol(ref message))
                    if message.contains("linked_at")
            ),
            "{error:?}"
        );
    }
}
