//! Headless VT abstraction over `alacritty_terminal` (ADR-007).
//!
//! h4rn owns this trait so `alacritty_terminal` (pre-1.0, churning) stays
//! replaceable by `vt100`. It provides damage tracking, resize reflow and the
//! compact screen snapshot used on reconnect.

/// Crate name, used by skeleton tests and startup diagnostics.
pub const CRATE_NAME: &str = "h4rn-vt";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_wired() {
        assert_eq!(super::CRATE_NAME, "h4rn-vt");
    }
}
