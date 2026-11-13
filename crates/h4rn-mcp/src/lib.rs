//! The single local MCP hub (ADR-006).
//!
//! Phase 1a only establishes the crate boundary. Later phases add the
//! streamable-HTTP endpoint, per-session bearer tokens, aggregation of the
//! user's own MCP servers, and the h4rn tools (`spawn_subagent`,
//! `task_status`, `task_cancel`, preview/render and forge operations).

/// Crate name, used by skeleton tests and startup diagnostics.
pub const CRATE_NAME: &str = "h4rn-mcp";

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_wired() {
        assert_eq!(super::CRATE_NAME, "h4rn-mcp");
    }
}
