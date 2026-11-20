//! Durable worktree observations.
//!
//! Only what cannot be derived live is stored: worktrees an agent or its
//! harness created or touched, and which threads accessed them. Subagent
//! worktrees come from the supervisor and thread workspaces from the thread
//! list, so neither is persisted here.

use std::path::Path;

use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row as _, SqlitePool};

/// One thread's recorded access to a worktree.
#[derive(Clone, Debug)]
pub struct StoredAccess {
    /// Thread that used the worktree.
    pub thread_id: String,
    /// Subagent id, when the thread is a subagent child.
    pub subagent_id: Option<String>,
    /// When the access was last seen.
    pub last_seen: DateTime<Utc>,
}

/// One observed worktree with its accesses.
#[derive(Clone, Debug)]
pub struct StoredWorktree {
    /// Absolute worktree path.
    pub path: String,
    /// Main worktree of the repository.
    pub repo: String,
    /// Branch when last seen.
    pub branch: Option<String>,
    /// First observation.
    pub first_seen: DateTime<Utc>,
    /// Latest observation.
    pub last_seen: DateTime<Utc>,
    /// Threads that used it.
    pub accesses: Vec<StoredAccess>,
}

/// SQLite handle for observed worktrees.
#[derive(Clone, Debug)]
pub struct WorktreeStore {
    pool: SqlitePool,
}

fn parse_time(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .map(|time| time.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

impl WorktreeStore {
    /// Opens (or creates) `worktrees.db`.
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
            "CREATE TABLE IF NOT EXISTS worktrees (
                path       TEXT PRIMARY KEY,
                repo       TEXT NOT NULL,
                branch     TEXT,
                origin     TEXT NOT NULL,
                first_seen TEXT NOT NULL,
                last_seen  TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS worktree_access (
                path        TEXT NOT NULL,
                thread_id   TEXT NOT NULL,
                subagent_id TEXT,
                last_seen   TEXT NOT NULL,
                PRIMARY KEY (path, thread_id)
            )",
        )
        .execute(&pool)
        .await?;
        Ok(Self { pool })
    }

    /// Records that `thread_id` used the worktree at `path`.
    pub async fn record(
        &self,
        path: &str,
        repo: &str,
        branch: Option<&str>,
        thread_id: &str,
        subagent_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        let now = now.to_rfc3339();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO worktrees (path, repo, branch, origin, first_seen, last_seen)
             VALUES (?, ?, ?, 'harness', ?, ?)
             ON CONFLICT(path) DO UPDATE SET
                repo = excluded.repo,
                branch = excluded.branch,
                last_seen = excluded.last_seen",
        )
        .bind(path)
        .bind(repo)
        .bind(branch)
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO worktree_access (path, thread_id, subagent_id, last_seen)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(path, thread_id) DO UPDATE SET
                subagent_id = excluded.subagent_id,
                last_seen = excluded.last_seen",
        )
        .bind(path)
        .bind(thread_id)
        .bind(subagent_id)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }

    /// Loads every observed worktree with its accesses.
    pub async fn load(&self) -> Result<Vec<StoredWorktree>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT path, repo, branch, first_seen, last_seen FROM worktrees
             ORDER BY last_seen DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut worktrees: Vec<StoredWorktree> = Vec::with_capacity(rows.len());
        for row in rows {
            worktrees.push(StoredWorktree {
                path: row.try_get("path")?,
                repo: row.try_get("repo")?,
                branch: row.try_get("branch")?,
                first_seen: parse_time(&row.try_get::<String, _>("first_seen")?),
                last_seen: parse_time(&row.try_get::<String, _>("last_seen")?),
                accesses: Vec::new(),
            });
        }
        let rows = sqlx::query(
            "SELECT path, thread_id, subagent_id, last_seen FROM worktree_access
             ORDER BY last_seen DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        for row in rows {
            let path: String = row.try_get("path")?;
            if let Some(worktree) = worktrees.iter_mut().find(|entry| entry.path == path) {
                worktree.accesses.push(StoredAccess {
                    thread_id: row.try_get("thread_id")?,
                    subagent_id: row.try_get("subagent_id")?,
                    last_seen: parse_time(&row.try_get::<String, _>("last_seen")?),
                });
            }
        }
        Ok(worktrees)
    }

    /// Forgets one worktree and its accesses.
    pub async fn delete(&self, path: &str) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM worktree_access WHERE path = ?")
            .bind(path)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM worktrees WHERE path = ?")
            .bind(path)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn observations_round_trip_and_delete() {
        let store = WorktreeStore::in_memory().await.expect("store");
        let now = Utc::now();
        store
            .record("/r/wt", "/r", Some("feat"), "th_1", None, now)
            .await
            .expect("record");
        store
            .record("/r/wt", "/r", Some("feat"), "th_2", Some("sa_1"), now)
            .await
            .expect("record again");
        let loaded = store.load().await.expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].accesses.len(), 2);
        assert_eq!(loaded[0].branch.as_deref(), Some("feat"));
        store.delete("/r/wt").await.expect("delete");
        assert!(store.load().await.expect("load").is_empty());
    }
}
