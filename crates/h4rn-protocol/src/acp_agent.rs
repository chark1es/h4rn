//! User-configured ACP agents ("local ACP commands").
//!
//! An ACP agent is any executable that speaks the Agent Client Protocol on
//! stdio. h4rn launches it directly, with a literal argument list and no shell,
//! so the same command a person would type works here unchanged. The daemon
//! persists the list in its data directory; these types are the wire shape the
//! settings screen and the new-thread picker read.
//!
//! Nothing here downloads or manages an agent. A configured command is
//! third-party code that runs with the daemon user's privileges, which is why
//! every write needs the `admin` scope.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Where a configured agent is started.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcpWorkingDir {
    /// The thread's workspace (the default).
    #[default]
    ThreadWorkspace,
    /// One fixed directory on the host.
    FixedPath {
        /// Absolute path of the directory.
        path: String,
    },
}

/// What a user configures for one ACP agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AcpAgentSpec {
    /// Stable instance id: a lowercase slug, unique in this daemon.
    pub id: String,
    /// Display name shown in Settings and the new-thread picker.
    pub name: String,
    /// Executable name (resolved on `PATH`) or an absolute path.
    pub command: String,
    /// Literal arguments, one row per argument. No shell expansion.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Where the agent starts.
    #[serde(default)]
    pub working_dir: AcpWorkingDir,
}

/// A configured agent and the installation facts this host knows.
///
/// This is the view-scoped shape: environment values are masked, because a
/// value can be a credential. The real values are only returned by the
/// admin-scoped [`crate::ClientRequest::GetAcpAgent`] request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AcpAgent {
    /// Stable instance id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Configured executable.
    pub command: String,
    /// Literal arguments.
    pub args: Vec<String>,
    /// Environment overrides, with each value masked.
    pub env: BTreeMap<String, String>,
    /// Working-directory policy.
    pub working_dir: AcpWorkingDir,
    /// Whether the executable resolves to an executable file on this host.
    pub installed: bool,
    /// Absolute executable path when it resolved.
    #[serde(default)]
    #[ts(optional)]
    pub path: Option<String>,
    /// Version the agent reported in `initialize`, when it ever reported one.
    #[serde(default)]
    #[ts(optional)]
    pub version: Option<String>,
    /// Models from the last successful probe, so a view-scoped device can
    /// offer them without starting the agent.
    #[serde(default)]
    pub models: Vec<AcpAgentModel>,
    /// Modes from the last successful probe.
    #[serde(default)]
    pub modes: Vec<AcpAgentMode>,
}

/// One model an agent advertised over ACP.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AcpAgentModel {
    /// Model id passed back to the agent.
    pub id: String,
    /// Human label.
    pub name: String,
}

/// One mode an agent advertised over ACP.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AcpAgentMode {
    /// Mode id passed back to the agent.
    pub id: String,
    /// Human label.
    pub name: String,
    /// Optional longer description.
    #[serde(default)]
    #[ts(optional)]
    pub description: Option<String>,
}

/// The result of asking a configured agent to introduce itself.
///
/// A probe starts the command, performs the ACP `initialize` handshake and
/// opens a session so the models and modes the agent advertises can be shown.
/// `error` is set when the command exists but could not complete that exchange;
/// the configuration is still saved exactly as the user wrote it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AcpAgentProbe {
    /// Instance id probed.
    pub id: String,
    /// Whether the executable was found on this host.
    pub installed: bool,
    /// Absolute executable path when found.
    #[serde(default)]
    #[ts(optional)]
    pub path: Option<String>,
    /// Version from the agent's `initialize` reply.
    #[serde(default)]
    #[ts(optional)]
    pub version: Option<String>,
    /// Models the agent advertises.
    #[serde(default)]
    pub models: Vec<AcpAgentModel>,
    /// Modes the agent advertises.
    #[serde(default)]
    pub modes: Vec<AcpAgentMode>,
    /// Why the probe did not complete, when it did not.
    #[serde(default)]
    #[ts(optional)]
    pub error: Option<String>,
}
