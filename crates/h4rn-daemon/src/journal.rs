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
    Event, HarnessKind, HostId, RuntimeMode, SubagentId, ThreadId, Usage, WorktreeRequest,
};
use serde_json::json;
use sqlx::{Row, SqlitePool};

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
                worktree_json  TEXT,
                parent_thread_id TEXT,
                subagent_id    TEXT,
                agent_id       TEXT,
                agent_fingerprint TEXT,
                parked_json    TEXT
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
    sqlx::query(
        "INSERT INTO daemon_threads
            (thread_id, host, harness, title, title_source, workspace_path, model, created_at, usage_json, resume_id, runtime_mode, worktree_json, parent_thread_id, subagent_id, agent_id, agent_fingerprint, parked_json)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
            worktree_json = excluded.worktree_json,
            parent_thread_id = excluded.parent_thread_id,
            subagent_id = excluded.subagent_id,
            agent_id = excluded.agent_id,
            agent_fingerprint = excluded.agent_fingerprint,
            parked_json = excluded.parked_json",
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
    .bind(worktree_json)
    .bind(meta.parent_thread_id.as_ref().map(ThreadId::as_str))
    .bind(meta.subagent_id.as_ref().map(SubagentId::as_str))
    .bind(meta.agent_id.as_deref())
    .bind(meta.agent_fingerprint.as_deref())
    .bind(parked_json)
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
    async fn upsert_thread(&self, meta: &ThreadMeta) -> Result<(), JournalError> {
        let mut conn = self.pool.acquire().await?;
        upsert_thread_on(&mut conn, meta).await
    }

    async fn list_threads(&self) -> Result<Vec<ThreadMeta>, JournalError> {
        let rows = sqlx::query(
            "SELECT thread_id, host, harness, title, title_source, workspace_path, model, created_at, usage_json, resume_id, runtime_mode, worktree_json, parent_thread_id, subagent_id, agent_id, agent_fingerprint, parked_json
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
        "worktree": meta.worktree,
        "usage": meta.usage,
        "resume_id": meta.resume_id,
        "agent_fingerprint": meta.agent_fingerprint,
        "parked": meta.parked,
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
            worktree: None,
            created_at: Utc::now(),
            usage: Usage::default(),
            resume_id: None,
            agent_fingerprint: None,
            parked: None,
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
            branch: Some("h4rn/first-prompt".into()),
            model: "test-model".into(),
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
            unread: false,
            resume_at: None,
            limit: None,
        }
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
}
