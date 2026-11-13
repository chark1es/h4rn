//! Multipath, path-agnostic session transport (ADR-009).
//!
//! The session layer, not a socket, owns sequence numbers and acks so a switch
//! between LAN, Tailscale and the relay does not drop the session. Path probes
//! are smoothed and switching requires a sustained RTT margin.

/// Crate name, used by skeleton tests and startup diagnostics.
pub const CRATE_NAME: &str = "h4rn-transport";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_wired() {
        assert_eq!(super::CRATE_NAME, "h4rn-transport");
    }
}
