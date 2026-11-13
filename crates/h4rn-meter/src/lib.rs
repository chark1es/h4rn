//! Durable append-only token and cost ledger (ADR-024).
//!
//! The ledger owns one row per harness turn, keyed by thread, run, harness,
//! model and credential, and stores the raw usage payload alongside the
//! normalized numbers. Counter-style upstreams are enrichment, never truth.
//!
//! The crate is deliberately transport-free: [`TurnMeter`] is pure arithmetic,
//! [`PriceTable`] is a deterministic estimate, [`report`] aggregates rows and
//! [`schedule`] decides when an auto-resume fires. [`MeterStore`] persists them
//! in SQLite and [`MeterHub`] is the daemon-facing façade that also broadcasts
//! the ephemeral ticks.

#![deny(missing_docs)]

mod hub;
mod ledger;
mod limits;
mod math;
mod price;
mod report;
mod schedule;

pub use hub::{MeterHub, TurnContext};
pub use ledger::{MeterStore, StoreError, UsageFilter, UsageRow};
pub use limits::{LimitHit, LimitRecord};
pub use math::{LiveSnapshot, MeteredTurn, TurnMeter};
pub use price::{CostEstimate, ModelPrice, PriceTable};
pub use report::{ReportQuery, build_report, counted_rows};
pub use schedule::{SchedulePlan, is_due, plan_resume};

/// Crate name, used by skeleton tests and startup diagnostics.
pub const CRATE_NAME: &str = "h4rn-meter";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_wired() {
        assert_eq!(super::CRATE_NAME, "h4rn-meter");
    }
}
