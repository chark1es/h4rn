//! A small in-memory rate limiter for the OAuth endpoints.
//!
//! The endpoints are on loopback, but a misbehaving or malicious local process
//! should not be able to grind client registration or code exchange. One
//! fixed-window list per key is enough: the daemon is one process and the
//! limits are low.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A fixed-window limiter keyed by endpoint and client.
pub struct RateLimiter {
    limit: usize,
    window: Duration,
    hits: Mutex<HashMap<String, Vec<Instant>>>,
}

impl RateLimiter {
    /// A limiter admitting `limit` requests per `window` for each key.
    pub fn new(limit: usize, window: Duration) -> Self {
        Self {
            limit,
            window,
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Records one request. `Err(seconds)` means the caller must wait.
    pub fn check(&self, key: &str) -> Result<(), u64> {
        let now = Instant::now();
        let mut hits = self
            .hits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entries = hits.entry(key.to_owned()).or_default();
        entries.retain(|at| now.duration_since(*at) < self.window);
        if entries.len() >= self.limit {
            let oldest = entries.first().copied().unwrap_or(now);
            let wait = self.window.saturating_sub(now.duration_since(oldest));
            return Err(wait.as_secs().max(1));
        }
        entries.push(now);
        Ok(())
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
}
