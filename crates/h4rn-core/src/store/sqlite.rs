//! SQLite implementation of the event store (ADR-003, ADR-012).
//!
//! SQLite is the daemon's durable cache and offline buffer, not the system of
//! record for relay-owned data. Writes are serialised through a single pool
//! connection because SQLite has one writer anyway, and that removes the
//! read-max-then-insert race on sequence assignment.

use std::path::Path;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use h4rn_protocol::ThreadId;
use sha2::{Digest, Sha384};
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{SqlitePool, Transaction};

use crate::lifecycle::ThreadEvent;
use crate::store::{EventStore, StoreError, StoredEvent};

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Rewrites the stored checksum of any applied migration whose SQL differs
/// from the embedded copy only in line endings.
///
/// sqlx checksums the exact migration text, so a binary built from a CRLF
/// checkout (the Windows release runner, before `.gitattributes` pinned SQL to
/// LF) refuses a database an LF build created, and the reverse. The daemon
/// then exits at startup. Same SQL, same schema: only the stamp is updated.
async fn restamp_line_ending_checksums(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let tracked: Option<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?;
    if tracked.is_none() {
        return Ok(());
    }
    for migration in MIGRATOR.iter() {
        let lf = migration.sql.replace("\r\n", "\n");
        let crlf = lf.replace('\n', "\r\n");
        for variant in [lf, crlf] {
            let digest = Sha384::digest(variant.as_bytes());
            if digest.as_slice() == migration.checksum.as_ref() {
                continue;
            }
            sqlx::query(
                "UPDATE _sqlx_migrations SET checksum = ? WHERE version = ? AND checksum = ?",
            )
            .bind(migration.checksum.as_ref())
            .bind(migration.version)
            .bind(digest.as_slice())
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Durable SQLite-backed event log.
#[derive(Debug, Clone)]
pub struct SqliteEventStore {
    pool: SqlitePool,
}

impl SqliteEventStore {
    /// Opens (or creates) a database file and applies migrations.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true);
        Self::from_options(options).await
    }

    /// Opens an in-memory database and applies migrations.
    ///
    /// The pool is deliberately capped at one connection so the database is not
    /// destroyed when the pool recycles a connection.
    pub async fn in_memory() -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::new().in_memory(true);
        Self::from_options(options).await
    }

    async fn from_options(options: SqliteConnectOptions) -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        restamp_line_ending_checksums(&pool).await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    /// Appends `events` inside a caller-owned transaction, so a caller can make
    /// the append atomic with writes of its own. The caller commits.
    pub async fn append_in(
        tx: &mut Transaction<'_, sqlx::Sqlite>,
        thread_id: &ThreadId,
        events: &[ThreadEvent],
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let current: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) FROM thread_events WHERE thread_id = ?",
        )
        .bind(thread_id.as_str())
        .fetch_one(&mut **tx)
        .await?;

        let mut stored = Vec::with_capacity(events.len());
        let start = current.max(0) as u64 + 1;
        for (seq, event) in (start..).zip(events) {
            stored.push(insert_event(tx, thread_id, seq, event).await?);
        }
        Ok(stored)
    }

    /// Borrows the underlying pool for projections and diagnostics.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

#[async_trait]
impl EventStore for SqliteEventStore {
    async fn append(
        &self,
        thread_id: &ThreadId,
        events: &[ThreadEvent],
    ) -> Result<Vec<StoredEvent>, StoreError> {
        if events.is_empty() {
            return Ok(Vec::new());
        }

        let mut tx = self.pool.begin().await?;
        let stored = Self::append_in(&mut tx, thread_id, events).await?;
        tx.commit().await?;
        Ok(stored)
    }

    async fn read_since(
        &self,
        thread_id: &ThreadId,
        after: u64,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let rows: Vec<(i64, String, String)> = sqlx::query_as(
            "SELECT seq, event_json, recorded_at \
             FROM thread_events WHERE thread_id = ? AND seq > ? ORDER BY seq ASC",
        )
        .bind(thread_id.as_str())
        .bind(after as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut events = Vec::with_capacity(rows.len());
        for (seq, event_json, recorded_at) in rows {
            let seq = seq as u64;
            let event = serde_json::from_str(&event_json)?;
            let recorded_at = DateTime::parse_from_rfc3339(&recorded_at)
                .map(|value| value.with_timezone(&Utc))
                .map_err(|_| StoreError::InvalidTimestamp {
                    seq,
                    value: recorded_at,
                })?;
            events.push(StoredEvent {
                seq,
                thread_id: thread_id.clone(),
                recorded_at,
                event,
            });
        }
        Ok(events)
    }
}

async fn insert_event(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    thread_id: &ThreadId,
    seq: u64,
    event: &ThreadEvent,
) -> Result<StoredEvent, StoreError> {
    let recorded_at = Utc::now();
    let event_json = serde_json::to_string(event)?;
    sqlx::query(
        "INSERT INTO thread_events (thread_id, seq, event_json, recorded_at) VALUES (?, ?, ?, ?)",
    )
    .bind(thread_id.as_str())
    .bind(seq as i64)
    .bind(&event_json)
    .bind(recorded_at.to_rfc3339())
    .execute(&mut **tx)
    .await?;
    Ok(StoredEvent {
        seq,
        thread_id: thread_id.clone(),
        recorded_at,
        event: event.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{ThreadState, apply_all};
    use h4rn_protocol::TurnStatus;

    fn events() -> Vec<ThreadEvent> {
        vec![
            ThreadEvent::TurnQueued { at: Utc::now() },
            ThreadEvent::TurnStarted { at: Utc::now() },
            ThreadEvent::TurnCompleted {
                status: TurnStatus::Completed,
                at: Utc::now(),
            },
        ]
    }

    #[tokio::test]
    async fn sqlite_append_read_and_resume() {
        let store = SqliteEventStore::in_memory().await.unwrap();
        let thread = ThreadId::new("th_sqlite");
        let expected = events();

        let stored = store.append(&thread, &expected).await.unwrap();
        assert_eq!(stored.len(), 3);
        assert_eq!(stored[0].seq, 1);

        let tail = store.read_since(&thread, 2).await.unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].seq, 3);
        assert_eq!(tail[0].event, expected[2]);

        let all = store.read_since(&thread, 0).await.unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn sqlite_persists_across_reconnect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let thread = ThreadId::new("th_persist");

        {
            let store = SqliteEventStore::connect(&path).await.unwrap();
            store.append(&thread, &events()).await.unwrap();
        }

        let reopened = SqliteEventStore::connect(&path).await.unwrap();
        let all = reopened.read_since(&thread, 0).await.unwrap();
        assert_eq!(all.len(), 3, "the log survives a daemon restart");
        assert_eq!(all[2].seq, 3);
    }

    /// Stamps migration 1 with `checksum`, as another build would have.
    async fn stamp_first_migration(path: &Path, checksum: &[u8]) {
        let store = SqliteEventStore::connect(path).await.unwrap();
        sqlx::query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 1")
            .bind(checksum)
            .execute(&store.pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn sqlite_opens_a_database_stamped_by_a_crlf_build() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let thread = ThreadId::new("th_crlf");
        let first = MIGRATOR.iter().next().unwrap();
        let other_endings = if first.sql.contains("\r\n") {
            first.sql.replace("\r\n", "\n")
        } else {
            first.sql.replace('\n', "\r\n")
        };
        stamp_first_migration(&path, &Sha384::digest(other_endings.as_bytes())).await;
        {
            let store = SqliteEventStore::connect(&path).await.unwrap();
            store.append(&thread, &events()).await.unwrap();
        }
        let reopened = SqliteEventStore::connect(&path)
            .await
            .expect("a line-ending-only checksum difference does not block startup");
        assert_eq!(reopened.read_since(&thread, 0).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn sqlite_still_rejects_a_modified_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        stamp_first_migration(&path, &Sha384::digest(b"-- some other schema\n")).await;
        let error = SqliteEventStore::connect(&path).await.unwrap_err();
        assert!(matches!(error, StoreError::Migration(_)), "{error}");
    }

    #[tokio::test]
    async fn sqlite_per_thread_sequences_and_isolation() {
        let store = SqliteEventStore::in_memory().await.unwrap();
        let one = ThreadId::new("th_one");
        let two = ThreadId::new("th_two");

        store.append(&one, &events()).await.unwrap();
        let other = store
            .append(&two, &[ThreadEvent::TurnQueued { at: Utc::now() }])
            .await
            .unwrap();
        assert_eq!(other[0].seq, 1);
        assert_eq!(store.read_since(&one, 0).await.unwrap().len(), 3);
        assert_eq!(store.read_since(&two, 0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn sqlite_replay_rebuilds_state() {
        let store = SqliteEventStore::in_memory().await.unwrap();
        let thread = ThreadId::new("th_replay");
        store.append(&thread, &events()).await.unwrap();

        let domain: Vec<ThreadEvent> = store
            .read_since(&thread, 0)
            .await
            .unwrap()
            .into_iter()
            .map(|stored| stored.event)
            .collect();
        let state = apply_all(ThreadState::new(Utc::now()), &domain);
        assert_eq!(state.turn_count, 1);
    }
}
