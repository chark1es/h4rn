//! h4rn as an MCP server: tools for its own agents and for outside agents.
//!
//! Two audiences, two trust paths:
//!
//! - **Agents h4rn runs** reach h4rn's tools through the hub's per-session
//!   token, which is bound to their thread. [`agent_tools`] is that provider;
//!   it registers with the hub exactly like the subagent tools do.
//! - **Outside agents** (Claude Code or Codex in the person's own terminal, or
//!   any other MCP client) connect to `/mcp` with OAuth. [`oauth`]
//!   holds the grants, [`outside`] the tools and their scope checks, and
//!   [`http`] the routes and the approval page.
//!
//! The token model is recorded in `docs/decisions.md` (ADR-046).

pub mod agent_tools;
pub mod http;
pub mod oauth;
pub mod outside;
pub mod rate;
pub mod redact;
pub mod settings;
pub mod url;

pub use agent_tools::AgentToolsProvider;
pub use http::router;
pub use oauth::OAuthStore;
pub use outside::OutsideAgents;
pub use redact::Redactor;
pub use settings::AgentToolSettingsStore;
