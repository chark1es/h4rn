//! Durable append-only token and cost ledger (ADR-024).
//!
//! The ledger owns one row per harness turn, keyed by thread, run, harness,
//! model and credential, and stores the raw usage payload alongside the
//! normalized numbers. Counter-style upstreams are enrichment, never truth.

/// Crate name, used by skeleton tests and startup diagnostics.
pub const CRATE_NAME: &str = "h4rn-meter";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_wired() {
        assert_eq!(super::CRATE_NAME, "h4rn-meter");
    }
}
