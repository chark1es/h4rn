//! Durable subagent records (ADR-038).
//!
//! The supervisor's live map is a cache. Every mutation is also written to
//! `subagents.db` so the parent's `Delegated` card, the child's status and its
//! result survive a daemon restart. The journal still owns the parent's
//! transcript; this table is what the panel reads after a restart, before any
//! child turn can happen again.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use h4rn_protocol::{SubagentView, ThreadId};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row as _, SqlitePool};

/// One persisted supervisor record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredRecord {
    /// The wire view shown to clients.
    pub view: SubagentView,
    /// Turns the child started.
    pub turns_started: u32,
    /// Turns the child completed.
    pub turns_completed: u32,
    /// A cancel raced or interrupted the child.
    pub cancel_requested: bool,
    /// Why a budget was exceeded, when it was.
    pub budget_error: Option<String>,
    /// Last harness error observed on the child.
    pub last_error: Option<String>,
    /// Parent repository, when the child ran in a worktree.
    pub repo: Option<PathBuf>,
    /// Worktree path, when the child ran in one.
    pub worktree: Option<PathBuf>,
    /// Wall-clock time spent paused on a usage limit.
    pub paused_ms: u64,
    /// When the current pause started, as Unix milliseconds.
    pub paused_since_ms: Option<i64>,
}

impl StoredRecord {
    /// Timestamp used for `updated_at`.
    pub fn updated_at(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// SQLite handle for supervisor records.
#[derive(Clone, Debug)]
pub struct SubagentStore {
    pool: SqlitePool,
}

impl SubagentStore {
    /// Opens (or creates) the record database.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, sqlx::Error> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(3)
            .connect_with(options)
            .await?;
        Self::from_pool(pool).await
    }

    /// An in-memory database for tests.
    pub async fn in_memory() -> Result<Self, sqlx::Error> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        Self::from_pool(pool).await
    }

    async fn from_pool(pool: SqlitePool) -> Result<Self, sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemon_subagents (
                subagent_id      TEXT PRIMARY KEY,
                parent_thread_id TEXT NOT NULL,
                child_thread_id  TEXT,
                state            TEXT NOT NULL,
                created_at       TEXT NOT NULL,
                updated_at       TEXT NOT NULL,
                record_json      TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS daemon_subagents_parent_idx
             ON daemon_subagents (parent_thread_id, created_at)",
        )
        .execute(&pool)
        .await?;
        Ok(Self { pool })
    }

    /// Inserts or replaces one record.
    pub async fn upsert(&self, record: &StoredRecord) -> Result<(), sqlx::Error> {
        let json =
            serde_json::to_string(record).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
        sqlx::query(
            "INSERT INTO daemon_subagents
                (subagent_id, parent_thread_id, child_thread_id, state, created_at, updated_at,
                 record_json)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(subagent_id) DO UPDATE SET
                parent_thread_id = excluded.parent_thread_id,
                child_thread_id = excluded.child_thread_id,
                state = excluded.state,
                updated_at = excluded.updated_at,
                record_json = excluded.record_json",
        )
        .bind(record.view.subagent_id.as_str())
        .bind(record.view.parent_thread_id.as_str())
        .bind(record.view.child_thread_id.as_ref().map(ThreadId::as_str))
        .bind(format!("{:?}", record.view.state))
        .bind(record.view.created_at.to_rfc3339())
        .bind(record.updated_at().to_rfc3339())
        .bind(json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Loads every record, newest first.
    pub async fn load(&self) -> Result<Vec<StoredRecord>, sqlx::Error> {
        let rows = sqlx::query("SELECT record_json FROM daemon_subagents ORDER BY created_at DESC")
            .fetch_all(&self.pool)
            .await?;
        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let json: String = row.try_get("record_json")?;
            match serde_json::from_str::<StoredRecord>(&json) {
                Ok(record) => records.push(record),
                // A row written by an older build with an incompatible shape
                // is skipped rather than failing daemon boot.
                Err(error) => tracing::warn!(%error, "skipping an unreadable subagent record"),
            }
        }
        Ok(records)
    }

    /// Removes one record (tests and explicit cleanup).
    pub async fn remove(&self, subagent_id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM daemon_subagents WHERE subagent_id = ?")
            .bind(subagent_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_round_trip_through_sqlite() {
        let store = SubagentStore::in_memory().await.expect("store");
        let view = SubagentView {
            subagent_id: h4rn_protocol::SubagentId::new("sa_1"),
            parent_thread_id: ThreadId::new("th_parent"),
            child_thread_id: Some(ThreadId::new("th_child")),
            task: "do the thing".into(),
            harness: h4rn_protocol::HarnessKind::Codex,
            host: h4rn_protocol::HostId::new("host_local"),
            model: "gpt-5".into(),
            isolation: h4rn_protocol::SubagentIsolation::Shared,
            workspace_path: "/tmp/repo".into(),
            branch: None,
            depth: 1,
            state: h4rn_protocol::SubagentState::Scheduled,
            agent_state: h4rn_protocol::AgentState::Scheduled,
            progress: Default::default(),
            budget: Default::default(),
            context: Default::default(),
            credential: h4rn_protocol::SubagentCredential {
                label: "parent".into(),
                shared_with_parent: true,
            },
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            result: None,
        };
        store
            .upsert(&StoredRecord {
                view: view.clone(),
                turns_started: 1,
                turns_completed: 0,
                cancel_requested: false,
                budget_error: None,
                last_error: None,
                repo: None,
                worktree: None,
                paused_ms: 0,
                paused_since_ms: None,
            })
            .await
            .expect("upsert");
        let loaded = store.load().await.expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].view, view);
        assert_eq!(
            loaded[0].view.state,
            h4rn_protocol::SubagentState::Scheduled
        );
    }
}
