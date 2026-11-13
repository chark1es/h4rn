//! Per-harness-session bearer tokens and their scopes (ADR-006).
//!
//! The hub is reachable from an agent process, so it authenticates every
//! request with a token minted for exactly one harness session. The token:
//!
//! - is 256 bits of OS randomness with an `h4rn_mcp_` prefix so a leaked
//!   string is recognisable in logs;
//! - is stored only as a BLAKE3 digest (`hashToken` in T3's design; the same
//!   property matters here, a memory dump of the registry is not a token dump);
//! - expires after a monotonic liveness window, so changing the wall clock
//!   cannot extend a session;
//! - is revoked with the harness session, and revoking a thread revokes every
//!   token minted for it;
//! - carries a scope that filters which servers and tools that session may see,
//!   which is how "this harness kind on this machine may use these servers"
//!   is enforced at the hub instead of trusting the harness.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use h4rn_protocol::{HarnessKind, HostId, McpCaller, ThreadId};
use rand::RngCore;
use serde_json::Value;

use crate::DEFAULT_TOKEN_TTL;

/// What one hub session is allowed to see and do.
#[derive(Clone, Debug)]
pub struct McpScope {
    /// Harness adapter owning the session.
    pub harness: HarnessKind,
    /// Machine whose enable matrix applies.
    pub machine: HostId,
    /// Thread the harness session belongs to, when known.
    pub thread_id: Option<ThreadId>,
    /// Server names this session may see; `None` means every enabled server.
    pub allowed_servers: Option<Vec<String>>,
    /// Fully qualified tool names this session may call; `None` means all.
    pub allowed_tools: Option<Vec<String>>,
    /// Who the hub attributes calls to.
    pub caller: McpCaller,
}

impl McpScope {
    /// A harness session scope with no per-session narrowing beyond the enable
    /// matrix.
    pub fn harness(harness: HarnessKind, machine: HostId, thread_id: Option<ThreadId>) -> Self {
        Self {
            harness,
            machine,
            thread_id: thread_id.clone(),
            allowed_servers: None,
            allowed_tools: None,
            caller: McpCaller::Harness { harness, thread_id },
        }
    }

    /// A local client (UI or CLI) testing tools.
    pub fn client(machine: HostId) -> Self {
        Self {
            harness: HarnessKind::ClaudeCode,
            machine,
            thread_id: None,
            allowed_servers: None,
            allowed_tools: None,
            caller: McpCaller::Client,
        }
    }

    /// A remote machine's hub.
    pub fn remote(machine: HostId, device_id: Option<String>) -> Self {
        Self {
            harness: HarnessKind::ClaudeCode,
            machine: machine.clone(),
            thread_id: None,
            allowed_servers: None,
            allowed_tools: None,
            caller: McpCaller::Remote {
                device_id,
                machine: Some(machine),
            },
        }
    }

    /// Whether the scope admits `server`.
    pub fn permits_server(&self, server: &str) -> bool {
        self.allowed_servers
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|candidate| candidate == server))
    }

    /// Whether the scope admits the qualified tool `server__tool`.
    pub fn permits_tool(&self, server: &str, qualified: &str) -> bool {
        self.permits_server(server)
            && self
                .allowed_tools
                .as_ref()
                .is_none_or(|allowed| allowed.iter().any(|candidate| candidate == qualified))
    }

    /// Restricts a session to an explicit list of server names.
    pub fn with_servers(mut self, servers: Vec<String>) -> Self {
        self.allowed_servers = Some(servers);
        self
    }

    /// Restricts a session to an explicit list of qualified tool names.
    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.allowed_tools = Some(tools);
        self
    }
}

/// A minted token and the opaque MCP session id it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedToken {
    /// Secret bearer value. Returned exactly once, never persisted.
    pub token: String,
    /// Session id the hub binds to the MCP `Mcp-Session-Id` header.
    pub session_id: String,
}

impl IssuedToken {
    /// Redacts the token in logs.
    pub fn redacted(&self) -> String {
        format!("{}…", &self.token[..self.token.len().min(20)])
    }
}

/// One authorised session, as returned by [`McpSessionRegistry::authorize`].
#[derive(Clone, Debug)]
pub struct AuthorizedSession {
    /// Opaque session id.
    pub session_id: String,
    /// Scope attached at mint time.
    pub scope: McpScope,
    /// Protocol revision negotiated for this MCP session, if the HTTP layer
    /// recorded one.
    pub protocol_version: Option<String>,
}

struct SessionEntry {
    scope: McpScope,
    expires_at: Instant,
    protocol_version: Option<String>,
}

/// Thread-safe registry of live hub sessions.
pub struct McpSessionRegistry {
    ttl: Duration,
    inner: Mutex<HashMap<String, SessionEntry>>,
    /// Token-digest -> session entry key. Kept separate so revocation can be
    /// by token without scanning.
    token_index: Mutex<HashMap<String, String>>,
}

impl McpSessionRegistry {
    /// Creates a registry with the default 24-hour liveness window.
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_TOKEN_TTL)
    }

    /// Creates a registry with an explicit liveness window (tests use a short
    /// one to prove expiry).
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(HashMap::new()),
            token_index: Mutex::new(HashMap::new()),
        }
    }

    /// Mints a token for `scope`, returning the only copy of the secret.
    pub fn issue(&self, scope: McpScope) -> IssuedToken {
        self.issue_with_servers(scope, Vec::new())
    }

    /// Mints a token and restricts it to `session_servers` (used when a share
    /// narrows what a remote session may see). An empty vector means every
    /// enabled server.
    pub fn issue_with_servers(&self, scope: McpScope, session_servers: Vec<String>) -> IssuedToken {
        let token = random_token();
        let session_id = random_session_id();
        let digest = digest(&token);
        let scope = if session_servers.is_empty() {
            scope
        } else {
            scope.with_servers(session_servers)
        };
        let now = Instant::now();
        let entry = SessionEntry {
            scope,
            expires_at: now + self.ttl,
            protocol_version: None,
        };
        {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.insert(session_id.clone(), entry);
        }
        self.token_index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(digest, session_id.clone());
        IssuedToken { token, session_id }
    }

    /// Validates a bearer token, returning its session when live.
    pub fn authorize(&self, token: &str) -> Option<AuthorizedSession> {
        let digest = digest(token);
        let session_id = self
            .token_index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&digest)
            .cloned()?;
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = inner.get(&session_id)?;
        if Instant::now() >= entry.expires_at {
            inner.remove(&session_id);
            drop(inner);
            self.token_index
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&digest);
            return None;
        }
        Some(AuthorizedSession {
            session_id,
            scope: entry.scope.clone(),
            protocol_version: entry.protocol_version.clone(),
        })
    }

    /// Records the protocol revision negotiated on the MCP session.
    pub fn set_protocol_version(&self, session_id: &str, version: &str) {
        if let Some(entry) = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(session_id)
        {
            entry.protocol_version = Some(version.to_owned());
        }
    }

    /// Revokes one token.
    pub fn revoke(&self, token: &str) {
        let digest = digest(token);
        let session_id = self
            .token_index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&digest);
        if let Some(session_id) = session_id {
            self.inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&session_id);
        }
    }

    /// Revokes every token minted for `thread_id`.
    pub fn revoke_thread(&self, thread_id: &ThreadId) -> usize {
        let removed: Vec<String> = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let removed: Vec<String> = inner
                .iter()
                .filter(|(_, entry)| {
                    entry
                        .scope
                        .thread_id
                        .as_ref()
                        .is_some_and(|candidate| candidate == thread_id)
                })
                .map(|(session_id, _)| session_id.clone())
                .collect();
            for session_id in &removed {
                inner.remove(session_id);
            }
            removed
        };
        let mut index = self
            .token_index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        index.retain(|_, session_id| !removed.contains(session_id));
        removed.len()
    }

    /// Drops expired sessions and their token digests.
    ///
    /// The daemon calls this from its background health tick, so a long-lived
    /// hub does not accumulate dead sessions.
    pub fn prune_expired(&self) -> usize {
        let now = Instant::now();
        let removed: Vec<String> = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let removed: Vec<String> = inner
                .iter()
                .filter(|(_, entry)| now >= entry.expires_at)
                .map(|(session_id, _)| session_id.clone())
                .collect();
            for session_id in &removed {
                inner.remove(session_id);
            }
            removed
        };
        if !removed.is_empty() {
            self.token_index
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|_, session_id| !removed.contains(session_id));
        }
        removed.len()
    }

    /// Number of live sessions (diagnostics and tests).
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Liveness window in seconds.
    pub fn ttl_secs(&self) -> u64 {
        self.ttl.as_secs()
    }
}

impl Default for McpSessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// BLAKE3 digest of a token, hex encoded. Never log the input.
pub fn digest(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

/// Generates an opaque session id with the `mcp_` prefix.
fn random_session_id() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    format!("mcp_{}", hex::encode(bytes))
}

/// Generates a 256-bit bearer token.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    format!("h4rn_mcp_{}", hex::encode(bytes))
}

/// Extracts a values object from a JSON-RPC params field, tolerating `null`.
pub fn params_object(params: &Value) -> Option<&serde_json::Map<String, Value>> {
    params.as_object()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> McpScope {
        McpScope::harness(
            HarnessKind::Codex,
            HostId::new("host_1"),
            Some(ThreadId::new("th_1")),
        )
    }

    #[test]
    fn tokens_authorise_and_revoke() {
        let registry = McpSessionRegistry::new();
        let issued = registry.issue(scope());
        assert!(issued.token.starts_with("h4rn_mcp_"));
        assert!(issued.token.len() > 50);
        let authorized = registry.authorize(&issued.token).expect("live token");
        assert_eq!(authorized.scope.harness, HarnessKind::Codex);
        assert_eq!(
            authorized.scope.thread_id.as_ref().unwrap().as_str(),
            "th_1"
        );
        registry.revoke(&issued.token);
        assert!(registry.authorize(&issued.token).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn only_the_digest_is_stored() {
        let registry = McpSessionRegistry::new();
        let issued = registry.issue(scope());
        let index = registry
            .token_index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(!index.contains_key(&issued.token));
        assert!(index.contains_key(&digest(&issued.token)));
    }

    #[test]
    fn threads_revoke_together() {
        let registry = McpSessionRegistry::new();
        let a = registry.issue(scope());
        let b = registry.issue(scope());
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.revoke_thread(&ThreadId::new("th_1")), 2);
        assert!(registry.authorize(&a.token).is_none());
        assert!(registry.authorize(&b.token).is_none());
    }

    #[test]
    fn expiry_is_monotonic() {
        let registry = McpSessionRegistry::with_ttl(Duration::from_millis(10));
        let issued = registry.issue(scope());
        std::thread::sleep(Duration::from_millis(20));
        assert!(registry.authorize(&issued.token).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn scope_server_and_tool_filters_compose() {
        let scope = scope()
            .with_servers(vec!["github".into()])
            .with_tools(vec!["github__read".into()]);
        assert!(scope.permits_tool("github", "github__read"));
        assert!(!scope.permits_tool("github", "github__write"));
        assert!(!scope.permits_tool("other", "other__read"));
    }

    #[test]
    fn session_server_narrowing_is_applied() {
        let registry = McpSessionRegistry::new();
        let issued = registry.issue_with_servers(scope(), vec!["github".into()]);
        let authorized = registry.authorize(&issued.token).unwrap();
        assert!(authorized.scope.permits_server("github"));
        assert!(!authorized.scope.permits_server("jira"));
    }
}
