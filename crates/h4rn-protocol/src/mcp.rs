//! The MCP hub contract (ADR-006).
//!
//! The daemon runs exactly one MCP hub per machine. These types are what the UI
//! and remote devices use to browse, configure, share and test the user's MCP
//! servers; they are deliberately **not** the MCP wire protocol itself. The
//! hub speaks MCP 2025-06-18 (and the two older revisions it negotiates) to
//! harnesses and upstream servers inside `h4rn-mcp`, where that protocol is an
//! implementation detail.
//!
//! Two rules shape the contract:
//!
//! - A secret value is **write-only**. It may be sent to the daemon when a
//!   server is added or updated, but it is never returned. Reads expose only
//!   the environment/header *names*.
//! - A server's transport is described by [`McpTransportView`] in every
//!   response. A shared server appears on the receiving machine with
//!   [`McpServerView::runs_on`] naming the machine that actually executes it,
//!   so a phone can tell "my laptop runs this" from "this phone proxies it".
//!
//! The types are append-only: new optional fields and new enum variants are
//! safe because the UI tolerates unknown variants (ADR-002).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::common::HarnessKind;
use crate::ids::{HostId, ThreadId};

/// How a configured MCP server is reached.
///
/// The view never contains a credential. `env_keys`/`header_keys` name the
/// values the daemon holds in its secret store, which is enough for the UI to
/// show "3 environment variables" without revealing any of them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransportView {
    /// A child process speaking MCP over stdin/stdout.
    Stdio {
        /// Executable path or command name.
        command: String,
        /// Arguments passed verbatim.
        args: Vec<String>,
        /// Working directory, when one was configured.
        cwd: Option<String>,
        /// Names of the environment variables whose values are stored as
        /// secrets.
        env_keys: Vec<String>,
    },
    /// A streamable-HTTP endpoint.
    Http {
        /// Endpoint URL.
        url: String,
        /// Names of the headers whose values are stored as secrets.
        header_keys: Vec<String>,
    },
    /// A legacy HTTP+SSE endpoint.
    Sse {
        /// Endpoint URL.
        url: String,
        /// Names of the headers whose values are stored as secrets.
        header_keys: Vec<String>,
    },
}

impl McpTransportView {
    /// Short transport label (`stdio`, `http`, `sse`).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
            Self::Sse { .. } => "sse",
        }
    }
}

/// One secret-valued environment variable or header.
///
/// On an update, a `None` value keeps the stored secret; `Some("")` clears it.
/// The value is never echoed back in any response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct McpSecretInput {
    /// Environment variable or header name.
    pub name: String,
    /// New value, or `None` to keep the stored one.
    pub value: Option<String>,
}

/// Transport configuration supplied when adding or updating a server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransportInput {
    /// A child process.
    Stdio {
        /// Executable path or command name.
        command: String,
        /// Arguments passed verbatim.
        #[serde(default)]
        args: Vec<String>,
        /// Working directory.
        #[serde(default)]
        cwd: Option<String>,
        /// Environment variables; every value is stored as a secret.
        #[serde(default)]
        env: Vec<McpSecretInput>,
    },
    /// A streamable-HTTP endpoint.
    Http {
        /// Endpoint URL.
        url: String,
        /// Headers; every value is stored as a secret.
        #[serde(default)]
        headers: Vec<McpSecretInput>,
    },
    /// A legacy HTTP+SSE endpoint.
    Sse {
        /// Endpoint URL.
        url: String,
        /// Headers; every value is stored as a secret.
        #[serde(default)]
        headers: Vec<McpSecretInput>,
    },
}

/// One per-harness enable switch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct McpHarnessToggle {
    /// Harness adapter.
    pub harness: HarnessKind,
    /// Whether the server is visible to sessions of this harness.
    pub enabled: bool,
}

/// One per-machine enable switch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct McpMachineToggle {
    /// Machine (a daemon host).
    pub machine: HostId,
    /// Whether the server is visible to sessions on this machine.
    pub enabled: bool,
}

/// The two-dimensional enable matrix: harness kind and machine.
///
/// A missing entry means **enabled**, so a newly added server appears
/// everywhere until the user turns it off somewhere. This is the behaviour the
/// hub's tool filtering implements.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct McpEnabledMatrix {
    /// Per-harness overrides.
    #[serde(default)]
    pub by_harness: Vec<McpHarnessToggle>,
    /// Per-machine overrides.
    #[serde(default)]
    pub by_machine: Vec<McpMachineToggle>,
}

impl McpEnabledMatrix {
    /// Whether `harness` on `machine` may see the server.
    pub fn is_enabled(&self, harness: HarnessKind, machine: &HostId) -> bool {
        let harness_enabled = self
            .by_harness
            .iter()
            .find(|toggle| toggle.harness == harness)
            .map(|toggle| toggle.enabled)
            .unwrap_or(true);
        let machine_enabled = self
            .by_machine
            .iter()
            .find(|toggle| &toggle.machine == machine)
            .map(|toggle| toggle.enabled)
            .unwrap_or(true);
        harness_enabled && machine_enabled
    }

    /// Sets one harness switch, replacing an existing one.
    pub fn set_harness(&mut self, harness: HarnessKind, enabled: bool) {
        match self
            .by_harness
            .iter_mut()
            .find(|toggle| toggle.harness == harness)
        {
            Some(toggle) => toggle.enabled = enabled,
            None => self.by_harness.push(McpHarnessToggle { harness, enabled }),
        }
    }

    /// Sets one machine switch, replacing an existing one.
    pub fn set_machine(&mut self, machine: HostId, enabled: bool) {
        match self
            .by_machine
            .iter_mut()
            .find(|toggle| toggle.machine == machine)
        {
            Some(toggle) => toggle.enabled = enabled,
            None => self.by_machine.push(McpMachineToggle { machine, enabled }),
        }
    }
}

/// Runtime state of one upstream server, as far as the hub knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum McpServerStatusKind {
    /// Turned off by the enable matrix; never started.
    Disabled,
    /// Configured but not started yet (lazy start).
    Stopped,
    /// The handshake is in flight or a restart is in progress.
    Starting,
    /// The initialize handshake succeeded and tools are cached.
    Ready,
    /// The last start or health check failed; the hub will retry with
    /// exponential backoff.
    Error,
    /// The server runs on another machine and is reached through that
    /// machine's hub over the encrypted transport.
    Remote,
}

/// Why a tool call was made, for the audit trail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpCaller {
    /// A harness session.
    Harness {
        /// Harness adapter that owns the session.
        harness: HarnessKind,
        /// Thread the session belongs to, when known.
        thread_id: Option<ThreadId>,
    },
    /// The local UI or CLI testing a tool.
    Client,
    /// Another machine's hub.
    Remote {
        /// Verified device id of the remote daemon, when connect supplied one.
        device_id: Option<String>,
        /// Machine that asked.
        machine: Option<HostId>,
    },
}

/// Sharing policy attached to one `server -> machine` grant.
///
/// The policy is enforced by the machine that runs the server. The least
/// privilege default is: every tool visible, nothing pre-confirmed, and only
/// the target machine admitted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct McpSharePolicy {
    /// Tool names (unqualified, on this server) the remote machine may call.
    /// `None` allows every tool.
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// Tools that need an explicit approval on the owning machine before each
    /// call. `confirm_all` forces the same for every tool.
    #[serde(default)]
    pub confirm_tools: Vec<String>,
    /// Require an approval for every call, regardless of `confirm_tools`.
    #[serde(default)]
    pub confirm_all: bool,
    /// Device ids allowed to use the grant. These are **connect device ids of
    /// peer daemons** (the daemon device in Settings → Devices), not ids of
    /// phones or other user devices, and never a value claimed in a handshake
    /// frame. `None` admits the target machine's own daemon only.
    #[serde(default)]
    pub allowed_devices: Option<Vec<String>>,
}

impl McpSharePolicy {
    /// Whether `tool` may be called at all.
    pub fn permits(&self, tool: &str) -> bool {
        self.allowed_tools
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|candidate| candidate == tool))
    }

    /// Whether `tool` needs an approval on the owning machine.
    pub fn requires_confirmation(&self, tool: &str) -> bool {
        self.confirm_all || self.confirm_tools.iter().any(|candidate| candidate == tool)
    }
}

/// One remote tool call waiting for the owner's decision.
///
/// The owner's daemon raises it when a `confirm_tools` / `confirm_all` policy
/// matches. It fails closed: when it expires, is cancelled or the owner
/// disconnects, the call is denied. `arguments_preview` is a bounded,
/// human-readable rendering for the approval card, never a secret value the
/// policy did not already send.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct McpShareApproval {
    /// Opaque daemon-assigned id echoed back with the decision.
    pub approval_id: String,
    /// Machine that asked.
    pub machine: HostId,
    /// Server name.
    pub server: String,
    /// Unqualified tool name.
    pub tool: String,
    /// Bounded JSON rendering of the arguments.
    pub arguments_preview: String,
    /// When the request was raised.
    pub requested_at: DateTime<Utc>,
    /// When it stops waiting and the call is denied.
    pub expires_at: DateTime<Utc>,
}

/// One active share grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct McpShareView {
    /// Machine allowed to proxy this server.
    pub machine: HostId,
    /// Policy enforced on the owning machine.
    pub policy: McpSharePolicy,
    /// When the grant was created.
    pub created_at: DateTime<Utc>,
}

/// One configured MCP server as the UI sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct McpServerView {
    /// Stable daemon-owned id (`srv_...`).
    pub id: String,
    /// Unique server name; it is the tool namespace.
    pub name: String,
    /// Transport description with secrets redacted.
    pub transport: McpTransportView,
    /// Free-form tags used for filtering.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Harness/machine enable matrix.
    pub enabled: McpEnabledMatrix,
    /// Current runtime status.
    pub status: McpServerStatusKind,
    /// Number of tools the last successful handshake reported.
    pub tool_count: u32,
    /// Last start, health or call failure, redacted.
    pub last_error: Option<String>,
    /// Machine that executes the server. `None` means this daemon's machine;
    /// `Some` means this is a proxy to another machine's hub.
    pub runs_on: Option<HostId>,
    /// Machines this server is shared with.
    #[serde(default)]
    pub shared_to: Vec<HostId>,
    /// Where the configuration came from (`claude`, `codex`, `opencode`,
    /// `antigravity`, `user`), useful after an import.
    pub imported_from: Option<String>,
    /// Last configuration change.
    pub updated_at: DateTime<Utc>,
}

/// Request body for adding or updating an MCP server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct McpServerInput {
    /// Unique name; the tool namespace. Must not contain `__`.
    pub name: String,
    /// Transport configuration.
    pub transport: McpTransportInput,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Enable matrix; missing entries default to enabled.
    #[serde(default)]
    pub enabled: McpEnabledMatrix,
}

/// One server found in a harness's native configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct McpImportCandidate {
    /// Server name from the native config.
    pub name: String,
    /// Transport, with secrets already replaced by secret-store references.
    pub transport: McpTransportView,
    /// File the entry came from.
    pub origin: String,
    /// Whether the hub already has a server with this name.
    pub existing: bool,
}

/// A name that exists both locally and in a native config with a different
/// configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct McpImportConflict {
    /// Conflicting server name.
    pub name: String,
    /// The configuration the hub already has.
    pub existing: McpTransportView,
    /// The configuration the native file wants to import.
    pub incoming: McpTransportView,
    /// Resolutions this conflict admits.
    pub options: Vec<McpConflictResolution>,
}

/// How the user resolves an import conflict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum McpConflictResolution {
    /// Keep the hub's existing server.
    KeepMine,
    /// Replace the hub's server with the imported one.
    UseTheirs,
    /// Import under a different name.
    Rename {
        /// New, unused name.
        name: String,
    },
    /// Do not import this entry.
    Skip,
}

/// Preview returned before anything is written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct McpImportPreview {
    /// Harness whose native config was read.
    pub harness: HarnessKind,
    /// Files that were read, even if they contained no servers.
    pub origins: Vec<String>,
    /// Entries that can be imported without a conflict.
    pub candidates: Vec<McpImportCandidate>,
    /// Entries that collide by name with a different configuration.
    pub conflicts: Vec<McpImportConflict>,
}

/// One tool exposed by the hub, with its namespace split out.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct McpToolInfo {
    /// Fully qualified name the model sees (`server__tool`).
    pub name: String,
    /// Server namespace.
    pub server: String,
    /// Tool name inside the server.
    pub tool: String,
    /// Model-facing description.
    pub description: Option<String>,
    /// JSON Schema for the arguments.
    pub input_schema: Value,
    /// The server annotated the tool as read-only.
    pub read_only: bool,
}

/// The observable result of a test call, flattened to text for the UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct McpTestCallResult {
    /// Text content concatenated from the MCP result.
    pub content: String,
    /// The server reported a tool error.
    pub is_error: bool,
    /// Output was cut at the hub's size cap.
    pub truncated: bool,
    /// Round trip in milliseconds.
    pub duration_ms: u64,
}

/// What an outside agent (an MCP client h4rn did not start) may do.
///
/// The level is chosen on the approval page and enforced by the daemon for
/// every tool call, never trusted from the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum OutsideAgentLevel {
    /// Read projects, threads, providers and models.
    ReadOnly,
    /// Read, and start or message threads, which run at approval-required.
    Supervised,
    /// Read, start and message threads at the mode the caller chooses.
    FullAccess,
}

impl OutsideAgentLevel {
    /// Stable scope string used in OAuth responses.
    pub fn scope(self) -> &'static str {
        match self {
            Self::ReadOnly => "read",
            Self::Supervised => "supervised",
            Self::FullAccess => "full",
        }
    }

    /// Parses the level the approval page submits.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read_only" | "read" => Some(Self::ReadOnly),
            "supervised" => Some(Self::Supervised),
            "full_access" | "full" => Some(Self::FullAccess),
            _ => None,
        }
    }

    /// Whether this level may change anything.
    pub fn can_write(self) -> bool {
        !matches!(self, Self::ReadOnly)
    }
}

/// One outside-agent sign-in waiting for the person in h4rn.
///
/// The browser that opened the client's authorize page cannot grant anything:
/// it has no h4rn session. It shows the client's name and callback and sends
/// the person to h4rn, where this record appears and the person picks a level.
/// `request_id` is the only thing the decision names, and it is unguessable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct OutsideAgentSignIn {
    /// Opaque approval id the page was opened with.
    pub request_id: String,
    /// Name the client registered with. The client chose it; nothing verifies it.
    pub client_name: String,
    /// Exact callback the browser will be sent to after approval.
    pub redirect_uri: String,
    /// Origin of that callback, shown prominently on both sides.
    pub redirect_origin: String,
    /// When the client asked.
    pub requested_at: DateTime<Utc>,
    /// When the request stops waiting.
    pub expires_at: DateTime<Utc>,
}

/// One outside agent with a live OAuth grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct OutsideAgentView {
    /// Public OAuth client id (never a token).
    pub client_id: String,
    /// Name the client registered with.
    pub name: String,
    /// Highest level any live token for this client carries.
    pub level: OutsideAgentLevel,
    /// When the client first connected.
    pub connected_at: DateTime<Utc>,
    /// When the client last used a token, when it has.
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
    /// When the longest-lived token for this client expires.
    pub expires_at: DateTime<Utc>,
}

/// The h4rn tools every harness session sees, and their per-harness switches.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AgentToolSettings {
    /// Per-harness overrides; a missing entry means enabled.
    pub enabled: McpEnabledMatrix,
}

/// One MCP operation from the UI.
///
/// Every mutation is idempotent by name: adding an existing name updates it,
/// removing a missing name is an error the UI can show as "already gone".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpRequest {
    /// List every server with status, machine and enable matrix.
    ListServers,
    /// Add or update one server.
    Upsert {
        /// Full configuration.
        input: McpServerInput,
    },
    /// Remove one server and its secrets.
    Remove {
        /// Server name.
        name: String,
    },
    /// Toggle one dimension of the enable matrix.
    SetEnabled {
        /// Server name.
        name: String,
        /// Harness dimension, when toggling a harness.
        harness: Option<HarnessKind>,
        /// Machine dimension, when toggling a machine.
        machine: Option<HostId>,
        /// New value.
        enabled: bool,
    },
    /// Preview servers found in a harness's native configuration.
    ///
    /// `workspace` is the project root whose `.mcp.json` (Claude) or
    /// `opencode.json` should also be read; `None` reads user-level files only.
    Import {
        /// Harness whose configuration format to read.
        harness: HarnessKind,
        /// Workspace to inspect for project-level configuration.
        workspace: Option<String>,
    },
    /// Apply every non-conflicting candidate from the last preview.
    ///
    /// Conflicts are never applied by this operation; they need an explicit
    /// [`McpRequest::ResolveConflict`]. Re-reading the same files makes this
    /// idempotent.
    ApplyImport {
        /// Harness whose configuration format to read.
        harness: HarnessKind,
        /// Workspace to inspect for project-level configuration.
        workspace: Option<String>,
    },
    /// Apply one resolution from an [`McpImportPreview`].
    ResolveConflict {
        /// Harness whose config produced the conflict.
        harness: HarnessKind,
        /// Conflicting name.
        name: String,
        /// Resolution chosen by the user.
        resolution: McpConflictResolution,
        /// Workspace used for the preview, so the same file is re-read.
        workspace: Option<String>,
    },
    /// Share a locally running server with another machine.
    Share {
        /// Server name.
        name: String,
        /// Machine that may proxy the server.
        to_machine: HostId,
        /// Policy enforced on this machine.
        policy: McpSharePolicy,
    },
    /// Revoke a share grant.
    Unshare {
        /// Server name.
        name: String,
        /// Machine to revoke.
        machine: HostId,
    },
    /// List remote tool calls waiting for the owner's decision.
    PendingApprovals,
    /// Approve or deny one pending remote tool call.
    RespondApproval {
        /// Id from [`McpShareApproval`].
        approval_id: String,
        /// `approve` runs the call; `deny` and `cancel` fail it closed.
        decision: crate::ApprovalDecision,
    },
    /// List the tools one server currently exposes, starting it if needed.
    ListTools {
        /// Server name.
        name: String,
    },
    /// Call one tool for diagnostics, using the local user's authority.
    TestCall {
        /// Server name.
        name: String,
        /// Unqualified tool name.
        tool: String,
        /// JSON arguments.
        #[ts(type = "unknown")]
        arguments: Value,
    },
    /// List outside agents connected through OAuth, and the URL they use.
    ListOutsideAgents,
    /// Revoke every token issued to one outside agent.
    RevokeOutsideAgent {
        /// Client id from [`OutsideAgentView::client_id`].
        client_id: String,
    },
    /// Read h4rn's own tool switches.
    AgentTools,
    /// Turn h4rn's own tools on or off for one harness.
    SetAgentToolsEnabled {
        /// Harness whose sessions get the tools.
        harness: HarnessKind,
        /// New value.
        enabled: bool,
    },
    /// List outside-agent sign-ins waiting for the person's decision.
    ///
    /// This is the only way a client's authorization request can become a
    /// grant: the answer runs over the authenticated h4rn session, never over
    /// the public page the client opened.
    ListOutsideAgentSignIns,
    /// Approve or deny one waiting outside-agent sign-in.
    ///
    /// `level` is required to approve and ignored to deny. The level is the
    /// person's explicit choice; nothing the client sent preselects it.
    RespondOutsideAgentSignIn {
        /// `request_id` from [`OutsideAgentSignIn`].
        request_id: String,
        /// Approve or deny.
        decision: crate::ApprovalDecision,
        /// Level to grant, when approving.
        level: Option<OutsideAgentLevel>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_matrix_defaults_to_enabled() {
        let matrix = McpEnabledMatrix::default();
        assert!(matrix.is_enabled(HarnessKind::ClaudeCode, &HostId::new("host_1")));
    }

    #[test]
    fn enabled_matrix_combines_both_dimensions() {
        let mut matrix = McpEnabledMatrix::default();
        matrix.set_harness(HarnessKind::Codex, false);
        matrix.set_machine(HostId::new("host_2"), false);
        let host1 = HostId::new("host_1");
        assert!(!matrix.is_enabled(HarnessKind::Codex, &host1));
        assert!(matrix.is_enabled(HarnessKind::ClaudeCode, &host1));
        assert!(!matrix.is_enabled(HarnessKind::ClaudeCode, &HostId::new("host_2")));
    }

    #[test]
    fn share_policy_checks_tools_and_confirmation() {
        let policy = McpSharePolicy {
            allowed_tools: Some(vec!["read".into()]),
            confirm_tools: vec!["write".into()],
            confirm_all: false,
            allowed_devices: None,
        };
        assert!(policy.permits("read"));
        assert!(!policy.permits("write"));
        assert!(!policy.requires_confirmation("read"));
        let all = McpSharePolicy {
            confirm_all: true,
            ..McpSharePolicy::default()
        };
        assert!(all.requires_confirmation("anything"));
    }

    #[test]
    fn requests_use_snake_case_discriminants() {
        let value = serde_json::to_value(McpRequest::ListServers).unwrap();
        assert_eq!(value["type"], "list_servers");
        let value = serde_json::to_value(McpRequest::SetEnabled {
            name: "github".into(),
            harness: Some(HarnessKind::Codex),
            machine: None,
            enabled: false,
        })
        .unwrap();
        assert_eq!(value["type"], "set_enabled");
        assert_eq!(value["harness"], "codex");
    }

    #[test]
    fn transport_views_never_carry_values() {
        let input = McpTransportInput::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "server".into()],
            cwd: None,
            env: vec![McpSecretInput {
                name: "GITHUB_TOKEN".into(),
                value: Some("ghp_secret".into()),
            }],
        };
        let json = serde_json::to_value(&input).unwrap();
        assert_eq!(json["env"][0]["name"], "GITHUB_TOKEN");
        // The input type has a value field; the *view* type is what is ever
        // returned, and it has no value field at all.
        let view = McpTransportView::Stdio {
            command: "npx".into(),
            args: vec!["-y".into(), "server".into()],
            cwd: None,
            env_keys: vec!["GITHUB_TOKEN".into()],
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["env_keys"][0], "GITHUB_TOKEN");
        assert!(!json.to_string().contains("ghp_secret"));
    }
}
