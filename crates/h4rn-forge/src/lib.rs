//! Forge (GitHub/GitLab) operations for the UI and the agent.
//!
//! Later phases own pull-request listing, check status, comments and handoff;
//! credentials stay on the host that holds them.

/// Crate name, used by skeleton tests and startup diagnostics.
pub const CRATE_NAME: &str = "h4rn-forge";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_wired() {
        assert_eq!(super::CRATE_NAME, "h4rn-forge");
    }
}
