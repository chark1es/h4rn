//! User-configured local ACP commands.
//!
//! This adapter runs any executable a person configured in Settings as an ACP
//! agent. It owns no protocol mapping of its own: the shared runtime in
//! [`crate::acp`] does the handshake, prompts, tool calls, plans, approvals,
//! cancellation and session load/resume. Here we only resolve the configured
//! command, launch it directly (no shell, literal argv), and correct the
//! working directory when the configuration asks for a fixed one.
//!
//! # Configuration source
//!
//! The daemon owns the persisted list. The adapter reads it through
//! [`AcpAgentConfigSource`], so adding or removing an agent takes effect for the
//! next session without restarting the daemon.
//!
//! # Security
//!
//! A configured command is third-party code running with the daemon user's
//! privileges. There is no shell expansion, no registry download and no h4rn
//! management of the binary: Settings shows "Managed outside h4rn".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use h4rn_protocol::{
    AcpAgentMode, AcpAgentModel, AcpAgentProbe, AcpAgentSpec, AcpWorkingDir, HarnessKind,
};

use crate::acp::{AcpClient, AcpClientConfig, AcpSessionPolicy, default_mode, map_acp_error};
use crate::{Harness, HarnessError, HarnessInstall, HarnessSession, SessionSpec};

/// Where the generic ACP adapter reads its configured instances.
pub trait AcpAgentConfigSource: Send + Sync {
    /// Returns one configuration by instance id.
    fn acp_agent(&self, id: &str) -> Option<AcpAgentSpec>;
}

/// A fixed map of configured agents, for tests and embedders.
#[derive(Clone, Debug, Default)]
pub struct StaticAcpAgents {
    agents: BTreeMap<String, AcpAgentSpec>,
}

impl StaticAcpAgents {
    /// An empty source.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces one configuration.
    pub fn with(mut self, spec: AcpAgentSpec) -> Self {
        self.agents.insert(spec.id.clone(), spec);
        self
    }
}

impl AcpAgentConfigSource for StaticAcpAgents {
    fn acp_agent(&self, id: &str) -> Option<AcpAgentSpec> {
        self.agents.get(id).cloned()
    }
}

/// The generic ACP adapter.
pub struct GenericAcpHarness {
    source: Arc<dyn AcpAgentConfigSource>,
}

impl GenericAcpHarness {
    /// Builds an adapter over the daemon's configured-agent list.
    pub fn new(source: Arc<dyn AcpAgentConfigSource>) -> Self {
        Self { source }
    }

    fn config(&self, id: &str) -> Option<AcpAgentSpec> {
        self.source.acp_agent(id)
    }
}

#[async_trait]
impl Harness for GenericAcpHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Acp
    }

    /// A generic agent has no single installation to detect.
    fn detect(&self) -> Option<HarnessInstall> {
        None
    }

    fn has_instance(&self, instance: &str) -> bool {
        self.config(instance).is_some()
    }

    async fn start_session(
        &self,
        spec: SessionSpec,
    ) -> Result<Box<dyn HarnessSession>, HarnessError> {
        let Some(agent_id) = spec
            .agent_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            return Err(HarnessError::Spawn {
                harness: HarnessKind::Acp,
                message: "the thread does not name a configured ACP agent".into(),
            });
        };
        let Some(agent) = self.config(agent_id) else {
            return Err(HarnessError::Spawn {
                harness: HarnessKind::Acp,
                message: format!("no ACP agent `{agent_id}` is configured on this host"),
            });
        };
        let Some(program) = resolve_acp_command(&agent.command) else {
            return Err(HarnessError::Spawn {
                harness: HarnessKind::Acp,
                message: format!(
                    "ACP agent `{}` command `{}` was not found or is not executable",
                    agent.name, agent.command
                ),
            });
        };
        let workspace = agent_workspace(&agent, &spec)?;

        let mut config = AcpClientConfig::new(&program, &workspace).args(agent.args.clone());
        for (key, value) in &spec.env_overrides {
            // h4rn-only variables configure this adapter, not the agent.
            if !key.starts_with("H4RN_") {
                config = config.env(key, value);
            }
        }
        for (key, value) in &agent.env {
            config = config.env(key, value);
        }

        let client =
            AcpClient::spawn(config).map_err(|error| map_acp_error(error, HarnessKind::Acp))?;
        let policy = AcpSessionPolicy {
            harness: HarnessKind::Acp,
            label: agent.name.clone(),
            // A generic agent's limit wording is unknown; only Antigravity's
            // adapter parses provider limits.
            detect_limit: |_, _| None,
            mode_for: default_mode,
            missing_mode_is_error: |_| false,
            default_models: &["default"],
        };
        match crate::acp::session_start(&client, &spec, policy, &workspace).await {
            Ok(session) => Ok(session.into_session()),
            Err(error) => {
                client.kill();
                Err(error)
            }
        }
    }
}

/// The working directory this agent runs in.
fn agent_workspace(agent: &AcpAgentSpec, spec: &SessionSpec) -> Result<PathBuf, HarnessError> {
    let path = match &agent.working_dir {
        AcpWorkingDir::ThreadWorkspace => spec.workspace.clone(),
        AcpWorkingDir::FixedPath { path } => PathBuf::from(path),
    };
    if !path.is_dir() {
        return Err(HarnessError::Spawn {
            harness: HarnessKind::Acp,
            message: format!(
                "the working directory for ACP agent `{}` is not a directory: {}",
                agent.name,
                path.display()
            ),
        });
    }
    Ok(path)
}

/// Resolves a configured command to an executable path.
///
/// An absolute or relative path that is an executable file wins; otherwise the
/// name is searched on `PATH` (with `.exe` on Windows). No shell is involved
/// and no glob or variable is expanded.
pub fn resolve_acp_command(command: &str) -> Option<PathBuf> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    let candidate = Path::new(command);
    if candidate.is_absolute() || command.contains(std::path::MAIN_SEPARATOR) {
        return is_executable(candidate).then(|| candidate.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    let names: &[&str] = if cfg!(windows) {
        &[command, &format!("{command}.exe")]
    } else {
        &[command]
    };
    for directory in std::env::split_paths(&path_var) {
        for name in names {
            let path = directory.join(name);
            if is_executable(&path) {
                return Some(path);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Starts a configured agent, reports what it advertises, and stops it.
///
/// This is the Settings "Check" action and the new-thread picker's model
/// source. It always stops the process it started. A command that exists but
/// does not complete the ACP exchange still returns a probe with `error` set;
/// the configuration itself is untouched.
pub async fn probe_acp_agent(agent: &AcpAgentSpec) -> AcpAgentProbe {
    let mut probe = AcpAgentProbe {
        id: agent.id.clone(),
        installed: false,
        path: None,
        version: None,
        models: Vec::new(),
        modes: Vec::new(),
        error: None,
    };
    let Some(program) = resolve_acp_command(&agent.command) else {
        probe.error = Some(format!(
            "`{}` was not found on PATH and is not an executable path",
            agent.command
        ));
        return probe;
    };
    probe.installed = true;
    probe.path = Some(program.display().to_string());

    let workspace = match &agent.working_dir {
        // A probe has no thread workspace yet. The temp directory keeps the
        // handshake away from whatever directory the daemon was started in.
        AcpWorkingDir::ThreadWorkspace => std::env::temp_dir(),
        AcpWorkingDir::FixedPath { path } => PathBuf::from(path),
    };
    if !workspace.is_dir() {
        probe.error = Some(format!(
            "the working directory is not a directory: {}",
            workspace.display()
        ));
        return probe;
    }

    let mut config = AcpClientConfig::new(&program, &workspace).args(agent.args.clone());
    for (key, value) in &agent.env {
        config = config.env(key, value);
    }
    let client = match AcpClient::spawn(config) {
        Ok(client) => client,
        Err(error) => {
            probe.error = Some(error.to_string());
            return probe;
        }
    };

    let exchange = async {
        let info = client.initialize().await?;
        probe.version = (!info.agent_version.trim().is_empty()).then(|| info.agent_version.clone());
        let session = client.new_session(&workspace, &[]).await?;
        probe.models = session
            .models
            .iter()
            .map(|model| AcpAgentModel {
                id: model.model_id.clone(),
                name: model.name.clone(),
            })
            .collect();
        probe.modes = session
            .modes
            .iter()
            .map(|mode| AcpAgentMode {
                id: mode.id.clone(),
                name: mode.name.clone(),
                description: mode.description.clone(),
            })
            .collect();
        Ok::<(), crate::acp::AcpError>(())
    };
    // A probe must not hang Settings: bound the whole exchange and kill the
    // process on expiry.
    match tokio::time::timeout(std::time::Duration::from_secs(30), exchange).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => probe.error = Some(error.to_string()),
        Err(_) => probe.error = Some("the agent did not answer within 30 seconds".into()),
    }
    let _ = client.shutdown().await;
    client.kill();
    probe
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str, command: &str) -> AcpAgentSpec {
        AcpAgentSpec {
            id: id.into(),
            name: "Test agent".into(),
            command: command.into(),
            args: vec!["acp".into()],
            env: BTreeMap::new(),
            working_dir: AcpWorkingDir::ThreadWorkspace,
        }
    }

    #[test]
    fn an_absolute_executable_resolves() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("agent");
        std::fs::write(&path, "#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&path, permissions).expect("chmod");
        }
        assert_eq!(resolve_acp_command(&path.display().to_string()), Some(path));
    }

    #[test]
    fn a_missing_command_does_not_resolve() {
        assert!(resolve_acp_command("h4rn-no-such-agent-command").is_none());
        assert!(resolve_acp_command("").is_none());
    }

    #[test]
    fn an_instance_is_known_only_when_configured() {
        let harness =
            GenericAcpHarness::new(Arc::new(StaticAcpAgents::new().with(spec("demo", "agent"))));
        assert_eq!(harness.kind(), HarnessKind::Acp);
        assert!(harness.has_instance("demo"));
        assert!(!harness.has_instance("other"));
        assert!(harness.detect().is_none());
    }

    #[test]
    fn a_fixed_working_directory_is_used() {
        let temp = tempfile::tempdir().expect("tempdir");
        let thread = tempfile::tempdir().expect("thread");
        let mut agent = spec("demo", "agent");
        agent.working_dir = AcpWorkingDir::FixedPath {
            path: temp.path().display().to_string(),
        };
        let session = SessionSpec {
            workspace: thread.path().to_path_buf(),
            model: "default".into(),
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            resume_id: None,
            mcp_endpoint: None,
            env_overrides: BTreeMap::new(),
            attachments: Vec::new(),
            agent_id: Some("demo".into()),
        };
        assert_eq!(
            agent_workspace(&agent, &session).expect("workspace"),
            temp.path()
        );
    }

    #[test]
    fn a_missing_fixed_working_directory_is_an_error() {
        let thread = tempfile::tempdir().expect("thread");
        let mut agent = spec("demo", "agent");
        agent.working_dir = AcpWorkingDir::FixedPath {
            path: thread.path().join("missing").display().to_string(),
        };
        let session = SessionSpec {
            workspace: thread.path().to_path_buf(),
            model: "default".into(),
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            resume_id: None,
            mcp_endpoint: None,
            env_overrides: BTreeMap::new(),
            attachments: Vec::new(),
            agent_id: Some("demo".into()),
        };
        assert!(agent_workspace(&agent, &session).is_err());
    }
}
