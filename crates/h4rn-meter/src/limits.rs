//! Limit observations: what hit, when it resets and how much was consumed.

use chrono::{DateTime, Utc};
use h4rn_protocol::{HarnessKind, LimitSource, LimitStatus, ThreadId};

/// An inbound limit observation from a harness or proxy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitHit {
    /// Thread the limit applies to.
    pub thread_id: Option<ThreadId>,
    /// Harness that hit it.
    pub harness: Option<HarnessKind>,
    /// Provider/account label.
    pub account: Option<String>,
    /// Provider name.
    pub provider: Option<String>,
    /// Model that hit the limit.
    pub model: Option<String>,
    /// When the provider resets, if known.
    pub reset_at: Option<DateTime<Utc>>,
    /// Provider message.
    pub message: Option<String>,
    /// Consumed units, when reported.
    pub consumed: Option<u64>,
    /// Known budget, when reported.
    pub limit: Option<u64>,
    /// Where it came from.
    pub source: LimitSource,
}

/// A persisted limit observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitRecord {
    /// Stable record id.
    pub record_id: String,
    /// Thread the limit applies to.
    pub thread_id: Option<ThreadId>,
    /// Harness that hit it.
    pub harness: Option<HarnessKind>,
    /// Provider/account label.
    pub account: Option<String>,
    /// Provider name.
    pub provider: Option<String>,
    /// Model that hit the limit.
    pub model: Option<String>,
    /// When the provider resets, if known.
    pub reset_at: Option<DateTime<Utc>>,
    /// Provider message.
    pub message: Option<String>,
    /// Consumed units, when reported.
    pub consumed: Option<u64>,
    /// Known budget, when reported.
    pub limit: Option<u64>,
    /// When h4rn observed it.
    pub observed_at: DateTime<Utc>,
    /// Where it came from.
    pub source: LimitSource,
}

impl LimitHit {
    /// Stamps an observation with a record id and the current time.
    pub fn into_record(self, now: DateTime<Utc>) -> LimitRecord {
        LimitRecord {
            record_id: format!("limit_{}", uuid::Uuid::now_v7()),
            thread_id: self.thread_id,
            harness: self.harness,
            account: self.account,
            provider: self.provider,
            model: self.model,
            reset_at: self.reset_at,
            message: self.message,
            consumed: self.consumed,
            limit: self.limit,
            observed_at: now,
            source: self.source,
        }
    }
}

impl LimitRecord {
    /// Projects the record onto the wire.
    pub fn to_status(&self) -> LimitStatus {
        LimitStatus {
            thread_id: self.thread_id.clone(),
            harness: self.harness,
            account: self.account.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            reset_at: self.reset_at,
            message: self.message.clone(),
            consumed: self.consumed,
            limit: self.limit,
            observed_at: self.observed_at,
            source: self.source,
        }
    }
}
