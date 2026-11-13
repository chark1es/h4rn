//! The thread event store (ADR-003).
//!
//! Threads are append-only event logs. This module defines the backend-neutral
//! [`EventStore`] trait, an in-memory implementation for tests and small
//! projections, and the durable SQLite cache used by the daemon.
//!
//! Sequence numbers are per thread and strictly increasing. A client resumes by
//! asking for events after the last sequence it applied, which is why
//! [`EventStore::read_since`] is the only read shape.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use h4rn_protocol::ThreadId;
use serde::{Deserialize, Serialize};

use crate::lifecycle::ThreadEvent;

mod sqlite;

pub use sqlite::SqliteEventStore;

/// One event as stored, with its assigned sequence and append time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredEvent {
    /// Per-thread monotonic sequence, starting at 1.
    pub seq: u64,
    /// Thread that owns the event.
    pub thread_id: ThreadId,
    /// When the daemon appended the event.
    pub recorded_at: DateTime<Utc>,
    /// The domain event.
    pub event: ThreadEvent,
}

/// Durable append-only event log.
#[async_trait]
pub trait EventStore: Send + Sync {
    /// Appends events to one thread and returns them with assigned sequence
    /// numbers. A failed append leaves the log unchanged.
    async fn append(
        &self,
        thread_id: &ThreadId,
        events: &[ThreadEvent],
    ) -> Result<Vec<StoredEvent>, StoreError>;

    /// Returns events with `seq > after`, in ascending order.
    async fn read_since(
        &self,
        thread_id: &ThreadId,
        after: u64,
    ) -> Result<Vec<StoredEvent>, StoreError>;
}

/// Event-store failures.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The backend returned an error.
    #[error("event store database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Migrations could not be applied.
    #[error("event store migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    /// An event could not be serialised.
    #[error("event store serialisation error: {0}")]
    Serialisation(#[from] serde_json::Error),
    /// A stored timestamp was not RFC 3339.
    #[error("event {seq} has an invalid timestamp {value:?}")]
    InvalidTimestamp {
        /// Sequence of the offending row.
        seq: u64,
        /// The value that failed to parse.
        value: String,
    },
}

/// In-memory event store for tests and ephemeral projections.
#[derive(Debug, Default)]
pub struct InMemoryEventStore {
    logs: Mutex<HashMap<String, Vec<StoredEvent>>>,
}

impl InMemoryEventStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replays one thread from the beginning.
    pub fn replay(&self, thread_id: &ThreadId) -> Vec<StoredEvent> {
        self.lock()
            .get(thread_id.as_str())
            .cloned()
            .unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<StoredEvent>>> {
        // A poisoned lock still contains a consistent map because every critical
        // section below is panic-free.
        self.logs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl EventStore for InMemoryEventStore {
    async fn append(
        &self,
        thread_id: &ThreadId,
        events: &[ThreadEvent],
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let mut logs = self.lock();
        let log = logs.entry(thread_id.to_string()).or_default();
        let start = log.last().map_or(1, |event| event.seq + 1);

        let mut stored = Vec::with_capacity(events.len());
        for (seq, event) in (start..).zip(events) {
            stored.push(StoredEvent {
                seq,
                thread_id: thread_id.clone(),
                recorded_at: Utc::now(),
                event: event.clone(),
            });
        }
        log.extend(stored.iter().cloned());
        Ok(stored)
    }

    async fn read_since(
        &self,
        thread_id: &ThreadId,
        after: u64,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        Ok(self
            .lock()
            .get(thread_id.as_str())
            .map(|log| {
                log.iter()
                    .filter(|event| event.seq > after)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::{ThreadEvent, ThreadState, apply_all};
    use chrono::TimeZone;

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + seconds, 0).unwrap()
    }

    fn turn_events() -> Vec<ThreadEvent> {
        vec![
            ThreadEvent::TurnQueued { at: at(1) },
            ThreadEvent::TurnStarted { at: at(2) },
            ThreadEvent::TurnCompleted {
                status: h4rn_protocol::TurnStatus::Completed,
                at: at(3),
            },
        ]
    }

    #[tokio::test]
    async fn appends_assign_monotonic_sequences_per_thread() {
        let store = InMemoryEventStore::new();
        let one = ThreadId::new("th_one");
        let two = ThreadId::new("th_two");

        let first = store.append(&one, &turn_events()).await.unwrap();
        assert_eq!(first.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2, 3]);

        let second = store
            .append(&one, &[ThreadEvent::Acknowledge { at: at(4) }])
            .await
            .unwrap();
        assert_eq!(second[0].seq, 4);

        let other = store
            .append(&two, &[ThreadEvent::TurnQueued { at: at(1) }])
            .await
            .unwrap();
        assert_eq!(other[0].seq, 1, "sequences are per thread");
    }

    #[tokio::test]
    async fn read_since_is_strict_and_ordered() {
        let store = InMemoryEventStore::new();
        let thread = ThreadId::new("th_one");
        store.append(&thread, &turn_events()).await.unwrap();

        let rest = store.read_since(&thread, 1).await.unwrap();
        assert_eq!(rest.iter().map(|e| e.seq).collect::<Vec<_>>(), [2, 3]);
        assert!(store.read_since(&thread, 3).await.unwrap().is_empty());
        assert_eq!(store.read_since(&thread, 0).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn replay_rebuilds_the_lifecycle_state() {
        let store = InMemoryEventStore::new();
        let thread = ThreadId::new("th_one");
        store.append(&thread, &turn_events()).await.unwrap();

        let events = store.read_since(&thread, 0).await.unwrap();
        let domain: Vec<_> = events.into_iter().map(|stored| stored.event).collect();
        let state = apply_all(ThreadState::new(at(0)), &domain);
        assert_eq!(state.turn_count, 1);
        assert_eq!(
            state.last_turn_status,
            Some(h4rn_protocol::TurnStatus::Completed)
        );
        assert_eq!(store.replay(&thread).len(), 3);
    }

    #[tokio::test]
    async fn empty_append_is_a_no_op() {
        let store = InMemoryEventStore::new();
        let thread = ThreadId::new("th_one");
        assert!(store.append(&thread, &[]).await.unwrap().is_empty());
        assert!(store.read_since(&thread, 0).await.unwrap().is_empty());
    }
}
