//! CLIProxyAPI feature detection and per-harness routing (ADR-015).
//!
//! Capability is probed, never inferred from a version string: `HEAD /healthz`
//! for liveness, a keyless management request for the capability headers, then
//! authenticated route probes. The per-harness base-URL recipes live here.

/// Crate name, used by skeleton tests and startup diagnostics.
pub const CRATE_NAME: &str = "h4rn-proxy";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_wired() {
        assert_eq!(super::CRATE_NAME, "h4rn-proxy");
    }
}
