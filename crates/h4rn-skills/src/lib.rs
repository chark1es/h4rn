//! Skill discovery and per-harness enablement.
//!
//! Later phases discover skills from the harness's own directories and expose
//! a normalized inventory with per-harness toggles.

/// Crate name, used by skeleton tests and startup diagnostics.
pub const CRATE_NAME: &str = "h4rn-skills";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_wired() {
        assert_eq!(super::CRATE_NAME, "h4rn-skills");
    }
}
