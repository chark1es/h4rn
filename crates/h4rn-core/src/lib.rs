//! Thread lifecycle, placement and durable event logs.
//!
//! `h4rn-core` is the behaviour both the daemon and the view share: the
//! Working / Done / Settled state machine of ADR-004 and the append-only event
//! log of ADR-003. It contains no UI and no transport code.

#![deny(missing_docs)]

pub mod lifecycle;
pub mod store;

pub use lifecycle::{
    DEFAULT_AUTO_SETTLE, DEFAULT_STALE_BYPASS, DEFAULT_WORKING_EXIT_GRACE, LifecycleSettings,
    ManualOverride, MessageSource, Phase, Placement, ThreadEvent, ThreadState, UsageLimit,
    apply_all, is_snoozed, needs_attention, placement, tab_for,
};
pub use store::{EventStore, InMemoryEventStore, SqliteEventStore, StoreError, StoredEvent};
