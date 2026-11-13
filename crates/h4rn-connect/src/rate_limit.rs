//! In-process rate limiting for the control plane.
//!
//! Limits are intentionally per-process. A deployment that runs several relay
//! replicas should apply the same limits at the edge; the self-hosted default
//! is one process (ADR-018). The limiter is a fixed window rather than a token
//! bucket because the API only needs "how many in the last minute/hour" and a
//! fixed window is cheap and easy to reason about.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::{AppError, Result};

#[derive(Debug)]
struct Bucket {
    window_start: Instant,
    count: u32,
}

/// Counters keyed by `bucket:key`.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// Builds an empty limiter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Charges one request against `bucket` for `key`.
    ///
    /// A `limit` of zero disables the check. On success returns the number of
    /// requests remaining in the window.
    pub fn check(&self, bucket: &str, key: &str, limit: u32, window: Duration) -> Result<u32> {
        if limit == 0 {
            return Ok(u32::MAX);
        }
        let now = Instant::now();
        let composite = format!("{bucket}:{key}");
        let mut buckets = self.buckets.lock().expect("rate limiter poisoned");

        // Opportunistic pruning keeps a long-lived process from growing without
        // bound when clients rotate addresses.
        if buckets.len() > 16_384 {
            buckets.retain(|_, bucket| now.duration_since(bucket.window_start) < window);
        }

        let entry = buckets.entry(composite).or_insert(Bucket {
            window_start: now,
            count: 0,
        });
        if now.duration_since(entry.window_start) >= window {
            entry.window_start = now;
            entry.count = 0;
        }
        if entry.count >= limit {
            let elapsed = now.duration_since(entry.window_start);
            let retry = window.saturating_sub(elapsed).as_secs().max(1);
            return Err(AppError::rate_limited(retry));
        }
        entry.count += 1;
        Ok(limit.saturating_sub(entry.count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_requests_within_a_window() {
        let limiter = RateLimiter::new();
        assert!(
            limiter
                .check("b", "1.2.3.4", 2, Duration::from_secs(60))
                .is_ok()
        );
        assert!(
            limiter
                .check("b", "1.2.3.4", 2, Duration::from_secs(60))
                .is_ok()
        );
        let error = limiter
            .check("b", "1.2.3.4", 2, Duration::from_secs(60))
            .unwrap_err();
        assert_eq!(error.status().as_u16(), 429);
        // A different key has its own counter.
        assert!(
            limiter
                .check("b", "5.6.7.8", 2, Duration::from_secs(60))
                .is_ok()
        );
    }

    #[test]
    fn zero_disables_the_limit() {
        let limiter = RateLimiter::new();
        for _ in 0..100 {
            assert!(limiter.check("b", "k", 0, Duration::from_secs(60)).is_ok());
        }
    }
}
