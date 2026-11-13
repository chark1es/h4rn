//! The single local MCP hub (ADR-006).
//!
//! One daemon runs exactly one hub. Every harness session is injected with the
//! same entry — `h4rn` — whose streamable-HTTP endpoint lives on loopback and
//! is protected by a per-session bearer token. The hub then hides the details
//! that differ between harnesses and upstream servers:
//!
//! - [`names`] turns a server and a tool into the model-visible name
//!   `server__tool` and resolves collisions deterministically.
//! - [`session`] mints, hashes, scopes and revokes the per-session tokens.
//! - [`provider`] is the internal extension point for h4rn's own tool sets
//!   (subagents, preview, forge). Another crate can register a provider without
//!   touching the hub or the harness adapters.
//! - [`upstream`] manages user-configured stdio, streamable-HTTP and legacy SSE
//!   servers: lazy start, health, restart with backoff, capability caching,
//!   list-changed propagation, timeouts and output caps.
//! - [`registry`] persists the configuration in the daemon's SQLite database;
//!   secrets live only in a [`secrets::SecretStore`], never in SQLite.
//! - [`import`] reads the native MCP configuration of every harness and reports
//!   conflicts instead of overwriting silently.
//! - [`sharing`] lets another machine proxy a local server through the
//!   encrypted transport; the owning machine keeps the credentials, audits the
//!   call and enforces its share policy.
//! - [`server`] is the MCP streamable-HTTP endpoint itself: version
//!   negotiation, initialize, tools, resources, prompts, pagination, progress
//!   and cancellation.
//!
//! The MCP wire protocol is implemented here rather than exposed to callers;
//! the UI contract lives in `h4rn_protocol::mcp`.

#![deny(missing_docs)]

pub mod hub;
pub mod import;
pub mod jsonrpc;
pub mod names;
pub mod provider;
pub mod registry;
pub mod secrets;
pub mod server;
pub mod session;
pub mod sharing;
pub mod types;
pub mod upstream;

/// Crate name, used by skeleton tests and startup diagnostics.
pub const CRATE_NAME: &str = "h4rn-mcp";

/// Stable name the hub registers itself under, and the namespace reserved for
/// h4rn's own tools.
pub const HUB_SERVER_NAME: &str = "h4rn";

/// The MCP revisions the hub speaks, newest first.
///
/// `2025-06-18` is the revision h4rn targets (streamable HTTP, `_meta`
/// progress tokens, `notifications/cancelled`). The two older revisions are
/// accepted because harness versions lag, and the hub answers with the newest
/// revision both sides understand.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// The revision the hub prefers when the client offers none it knows.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";

/// Negotiate the protocol revision with a client.
///
/// The server answers with the client's revision when the hub supports it, and
/// with its own latest otherwise (the spec's "server chooses" rule). Streamable
/// HTTP clients always send a revision in `initialize`.
pub fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|requested| {
            SUPPORTED_PROTOCOL_VERSIONS
                .iter()
                .copied()
                .find(|version| *version == requested)
        })
        .unwrap_or(LATEST_PROTOCOL_VERSION)
}

/// Default liveness window for a harness session token (ADR-006).
pub const DEFAULT_TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_wired() {
        assert_eq!(super::CRATE_NAME, "h4rn-mcp");
    }

    #[test]
    fn negotiation_prefers_the_client_revision_when_supported() {
        assert_eq!(
            super::negotiate_protocol_version(Some("2025-03-26")),
            "2025-03-26"
        );
        assert_eq!(
            super::negotiate_protocol_version(Some("1999-01-01")),
            super::LATEST_PROTOCOL_VERSION
        );
        assert_eq!(
            super::negotiate_protocol_version(None),
            super::LATEST_PROTOCOL_VERSION
        );
    }
}
