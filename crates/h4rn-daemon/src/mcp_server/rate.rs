//! A small in-memory rate limiter for the OAuth endpoints.
//!
//! The endpoints are on loopback, but a misbehaving or malicious local process
//! (or a page that reached the listener through DNS rebinding) should not be
//! able to grind client registration or code exchange.
//!
//! Two rules keep the limiter itself from becoming the attack:
//!
//! - **Buckets are keyed by validated identifiers.** A caller that is not a
//!   registered client gets the aggregate bucket, never a bucket of its own.
//! - **Memory is bounded.** Expired buckets are dropped, and when the map is
//!   still over its cap the oldest buckets go first.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many distinct buckets one limiter keeps before evicting the oldest.
pub const MAX_BUCKETS: usize = 512;

/// A fixed-window limiter keyed by endpoint and validated identifier.
pub struct RateLimiter {
    limit: usize,
    window: Duration,
    hits: Mutex<Buckets>,
}

struct Buckets {
    /// Key -> the hits still inside the window.
    hits: HashMap<String, Vec<Instant>>,
    /// Keys in the order they were first seen, for eviction.
    order: Vec<String>,
}

impl Buckets {
    fn new() -> Self {
        Self {
            hits: HashMap::new(),
            order: Vec::new(),
        }
    }

    /// Drops every bucket whose newest hit has left the window.
    fn prune(&mut self, now: Instant, window: Duration) {
        self.hits.retain(|_, entries| {
            entries.retain(|at| now.duration_since(*at) < window);
            !entries.is_empty()
        });
        self.order.retain(|key| self.hits.contains_key(key));
    }

    /// Makes room for one more bucket, oldest first.
    fn evict_to(&mut self, cap: usize) {
        while self.hits.len() >= cap {
            let Some(oldest) = self.order.first().cloned() else {
                break;
            };
            self.order.remove(0);
            self.hits.remove(&oldest);
        }
    }
}

impl RateLimiter {
    /// A limiter admitting `limit` requests per `window` for each key.
    pub fn new(limit: usize, window: Duration) -> Self {
        Self {
            limit,
            window,
            hits: Mutex::new(Buckets::new()),
        }
    }

    /// Records one request. `Err(seconds)` means the caller must wait.
    pub fn check(&self, key: &str) -> Result<(), u64> {
        self.check_many(key, 1)
    }

    /// Records `count` requests for one key, for a batch counted as a whole.
    pub fn check_many(&self, key: &str, count: usize) -> Result<(), u64> {
        let now = Instant::now();
        let mut buckets = self
            .hits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !buckets.hits.contains_key(key) {
            buckets.evict_to(MAX_BUCKETS);
            buckets.order.push(key.to_owned());
        }
        let entries = buckets.hits.entry(key.to_owned()).or_default();
        entries.retain(|at| now.duration_since(*at) < self.window);
        if entries.len() + count > self.limit {
            let oldest = entries.first().copied().unwrap_or(now);
            let wait = self.window.saturating_sub(now.duration_since(oldest));
            return Err(wait.as_secs().max(1));
        }
        for _ in 0..count {
            entries.push(now);
        }
        if buckets.hits.len() >= MAX_BUCKETS {
            buckets.prune(now, self.window);
        }
        Ok(())
    }

    /// How many buckets are held, for the flood test.
    pub fn bucket_count(&self) -> usize {
        self.hits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .hits
            .len()
    }
}

/// The limits the OAuth and MCP endpoints apply.
///
/// One aggregate limiter covers the unauthenticated mutation endpoints, checked
/// before any body is parsed or any per-client state is allocated. Per-key
/// buckets only exist for identifiers the daemon has already accepted.
pub struct EndpointLimits {
    /// Every `register` and `token` call, whoever makes it.
    pub aggregate: RateLimiter,
    /// One bucket per validated client or token family.
    pub per_key: RateLimiter,
}

impl Default for EndpointLimits {
    fn default() -> Self {
        Self {
            aggregate: RateLimiter::new(120, Duration::from_secs(60)),
            per_key: RateLimiter::new(120, Duration::from_secs(60)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_up_to_the_limit_then_reports_a_wait() {
        let limiter = RateLimiter::new(2, Duration::from_secs(60));
        assert!(limiter.check("token:client").is_ok());
        assert!(limiter.check("token:client").is_ok());
        assert!(limiter.check("token:client").is_err());
        assert!(limiter.check("token:other").is_ok());
    }

    #[test]
    fn a_batch_costs_one_hit_per_item() {
        let limiter = RateLimiter::new(3, Duration::from_secs(60));
        assert!(limiter.check_many("mcp:client", 3).is_ok());
        assert!(limiter.check("mcp:client").is_err());
        assert!(limiter.check_many("mcp:client", 1).is_err());
    }

    #[test]
    fn random_keys_do_not_grow_memory() {
        let limiter = RateLimiter::new(5, Duration::from_secs(60));
        for index in 0..(MAX_BUCKETS * 4) {
            assert!(limiter.check(&format!("random:{index}")).is_ok());
            assert!(
                limiter.bucket_count() <= MAX_BUCKETS,
                "the limiter kept {} buckets",
                limiter.bucket_count()
            );
        }
    }
}
