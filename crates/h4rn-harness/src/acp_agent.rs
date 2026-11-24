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
use crate::{Harness, HarnessError, HarnessInstall, HarnessSession, InstanceSnapshot, SessionSpec};

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

    fn instance_workspace(&self, instance: &str, workspace: &Path) -> Option<PathBuf> {
        let agent = self.config(instance)?;
        Some(match &agent.working_dir {
            AcpWorkingDir::ThreadWorkspace => workspace.to_path_buf(),
            AcpWorkingDir::FixedPath { path } => PathBuf::from(path),
        })
    }

    fn instance_fixes_workspace(&self, instance: &str) -> bool {
        self.config(instance)
            .is_some_and(|agent| matches!(agent.working_dir, AcpWorkingDir::FixedPath { .. }))
    }

    fn resolve_instance(&self, instance: &str, workspace: &Path) -> Option<InstanceSnapshot> {
        self.config(instance)
            .map(|agent| InstanceSnapshot::new(command_fingerprint(&agent, workspace), agent))
    }

    async fn start_session(
        &self,
        spec: SessionSpec,
    ) -> Result<Box<dyn HarnessSession>, HarnessError> {
        self.start_session_with(spec, None).await
    }

    async fn start_session_with(
        &self,
        spec: SessionSpec,
        snapshot: Option<InstanceSnapshot>,
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
        // The snapshot is the configuration this startup fingerprinted. An edit
        // made since then must not change the process: the stored resume id
        // names the command that was fingerprinted, not the newer one.
        let agent = snapshot
            .as_ref()
            .and_then(InstanceSnapshot::payload::<AcpAgentSpec>)
            .cloned()
            .or_else(|| self.config(agent_id));
        let Some(agent) = agent else {
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
            failure_label: agent.name.clone(),
            permission_prompt: "The agent is requesting permission",
            // A generic agent's limit wording is unknown; only Antigravity's
            // adapter parses provider limits.
            detect_limit: |_, _| None,
            mode_for: default_mode,
            missing_mode_is_error: |_| false,
            default_models: &["default"],
            model_list_label: "the agent offers",
            case_sensitive_default_models: false,
            // A stored id the agent can no longer load starts a fresh session
            // instead of failing the thread forever.
            fresh_without_load: true,
            // A generic agent's v1 `configOptions` are what
            // `session/set_config_option` selects from, even when it also
            // publishes a legacy catalogue.
            config_options_first: true,
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

/// A stable fingerprint of a configured command line and its working
/// directory.
///
/// The daemon records it beside a thread's resume id, so a session id from a
/// command the user has since edited is not replayed at the new command. The
/// effective directory is part of it: a session resumed in another directory
/// is a different conversation, and a switch between the thread workspace and
/// a fixed path changes it too.
fn command_fingerprint(agent: &AcpAgentSpec, workspace: &Path) -> String {
    let directory = match &agent.working_dir {
        AcpWorkingDir::ThreadWorkspace => workspace,
        AcpWorkingDir::FixedPath { path } => Path::new(path),
    };
    // JSON keeps the three parts separate: an empty argument list and a list
    // holding one empty argument are different command lines, and joining them
    // with a separator would encode both the same way.
    serde_json::json!({
        "command": agent.command,
        "args": agent.args,
        "directory": directory.display().to_string(),
    })
    .to_string()
}

/// Resolves a configured command to an absolute executable path.
///
/// An absolute path that is an executable file wins; otherwise the name is
/// searched on `PATH` (with `.exe` on Windows). Relative `PATH` entries are
/// ignored: the child's working directory is the thread workspace, so a
/// relative entry would resolve against the wrong directory. No shell is
/// involved and no glob or variable is expanded.
///
/// On Windows `.bat` and `.cmd` are refused: they are scripts that only run
/// through `cmd.exe`, and h4rn launches the executable directly.
pub fn resolve_acp_command(command: &str) -> Option<PathBuf> {
    resolve_acp_command_in(command, std::env::var_os("PATH").as_deref())
}

pub(crate) fn resolve_acp_command_in(
    command: &str,
    path_var: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let command = command.trim();
    if command.is_empty() || is_windows_script(command) {
        return None;
    }
    let candidate = Path::new(command);
    if candidate.is_absolute() || command.contains('/') || command.contains('\\') {
        return executable_path(candidate);
    }
    let path_var = path_var?;
    let names: &[&str] = if cfg!(windows) {
        &[command, &format!("{command}.exe")]
    } else {
        &[command]
    };
    for directory in std::env::split_paths(path_var) {
        if !directory.is_absolute() {
            continue;
        }
        for name in names {
            if let Some(path) = executable_path(&directory.join(name)) {
                return Some(path);
            }
        }
    }
    None
}

/// Whether this is a Windows command script rather than an executable.
#[cfg(windows)]
fn is_windows_script(command: &str) -> bool {
    let path = Path::new(command);
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
        })
}

#[cfg(not(windows))]
fn is_windows_script(_command: &str) -> bool {
    false
}

/// The absolute path of an existing executable file.
fn executable_path(path: &Path) -> Option<PathBuf> {
    if is_windows_script(&path.display().to_string()) || !is_executable(path) {
        return None;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    // Symlinks are resolved so the child starts from the absolute target and
    // detection, probing and launch all name the same file.
    Some(std::fs::canonicalize(&absolute).unwrap_or(absolute))
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
        // The picker must offer the catalogue a start will select from. A
        // generic agent's v1 `configOptions` win over a legacy
        // `models`/`modes` object it also publishes, exactly as at startup, so
        // the probe reads the same catalogue the adapter uses.
        let session = client
            .new_session(&workspace, &[])
            .await?
            .prefer_config_options();
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
    use std::sync::Mutex;

    use super::*;

    /// A source a test can edit while a resolved snapshot is still in use.
    #[derive(Default)]
    struct EditableAgents(Mutex<BTreeMap<String, AcpAgentSpec>>);

    impl EditableAgents {
        fn set(&self, agent: AcpAgentSpec) {
            self.0
                .lock()
                .expect("agents")
                .insert(agent.id.clone(), agent);
        }
    }

    impl AcpAgentConfigSource for EditableAgents {
        fn acp_agent(&self, id: &str) -> Option<AcpAgentSpec> {
            self.0.lock().expect("agents").get(id).cloned()
        }
    }

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
        executable_file(&path);
        assert_eq!(resolve_acp_command(&path.display().to_string()), Some(path));
    }

    #[test]
    fn a_missing_command_does_not_resolve() {
        assert!(resolve_acp_command("h4rn-no-such-agent-command").is_none());
        assert!(resolve_acp_command("").is_none());
    }

    #[cfg(unix)]
    fn executable_file(path: &Path) {
        crate::test_scripts::write_script(path, "#!/bin/sh\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_relative_path_entry_is_ignored() {
        let current = std::env::current_dir().expect("cwd");
        let temp = tempfile::Builder::new()
            .prefix("h4rn-rel-path-")
            .tempdir_in(&current)
            .expect("tempdir");
        executable_file(&temp.path().join("demo-agent"));
        let relative = temp
            .path()
            .strip_prefix(&current)
            .expect("relative")
            .display()
            .to_string();
        let relative_var = std::ffi::OsString::from(relative);

        assert!(
            resolve_acp_command_in("demo-agent", Some(relative_var.as_os_str())).is_none(),
            "a relative PATH entry must not resolve against the child's working directory"
        );
        let absolute_var = temp.path().display().to_string();
        let resolved =
            resolve_acp_command_in("demo-agent", Some(std::ffi::OsStr::new(&absolute_var)))
                .expect("absolute PATH entry resolves");
        assert!(resolved.is_absolute());
        assert_eq!(
            resolved,
            std::fs::canonicalize(temp.path().join("demo-agent")).expect("canonical")
        );
    }

    #[test]
    fn an_instance_fingerprint_follows_the_command_line_and_directory() {
        let workspace = tempfile::tempdir().expect("workspace");
        let harness =
            GenericAcpHarness::new(Arc::new(StaticAcpAgents::new().with(spec("demo", "agent"))));
        let first = harness
            .instance_fingerprint("demo", workspace.path())
            .expect("fingerprint");
        assert!(
            harness
                .instance_fingerprint("missing", workspace.path())
                .is_none()
        );

        let harness = GenericAcpHarness::new(Arc::new(
            StaticAcpAgents::new().with(spec("demo", "other-agent")),
        ));
        assert_ne!(
            first,
            harness
                .instance_fingerprint("demo", workspace.path())
                .expect("fingerprint"),
            "a different command line must not reuse the old session id"
        );

        // The effective working directory is part of the fingerprint: a thread
        // that moves, or an agent switched to a fixed path, starts fresh.
        let other = tempfile::tempdir().expect("other workspace");
        let harness =
            GenericAcpHarness::new(Arc::new(StaticAcpAgents::new().with(spec("demo", "agent"))));
        assert_ne!(
            first,
            harness
                .instance_fingerprint("demo", other.path())
                .expect("fingerprint"),
            "another thread workspace is another conversation"
        );
        let mut fixed = spec("demo", "agent");
        fixed.working_dir = AcpWorkingDir::FixedPath {
            path: other.path().display().to_string(),
        };
        let harness = GenericAcpHarness::new(Arc::new(StaticAcpAgents::new().with(fixed)));
        assert_ne!(
            first,
            harness
                .instance_fingerprint("demo", workspace.path())
                .expect("fingerprint"),
            "a fixed directory replaces the thread workspace"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_snapshot_wins_over_an_edit_made_before_the_start() {
        let workspace = tempfile::tempdir().expect("workspace");
        let old = tempfile::tempdir().expect("old command");
        let new = tempfile::tempdir().expect("new command");
        let old_marker = old.path().join("old-ran");
        let new_marker = new.path().join("new-ran");
        let old_script = old.path().join("agent");
        let new_script = new.path().join("agent");
        crate::test_scripts::write_script(
            &old_script,
            &format!("#!/bin/sh\n: > '{}'\nexit 1\n", old_marker.display()),
        );
        crate::test_scripts::write_script(
            &new_script,
            &format!("#!/bin/sh\n: > '{}'\nexit 1\n", new_marker.display()),
        );

        let source = Arc::new(EditableAgents::default());
        source.set(spec("demo", &old_script.display().to_string()));
        let harness = GenericAcpHarness::new(source.clone());
        let snapshot = harness
            .resolve_instance("demo", workspace.path())
            .expect("resolved instance");
        let fingerprint = snapshot.fingerprint().to_owned();
        assert_eq!(
            harness.instance_fingerprint("demo", workspace.path()),
            Some(fingerprint.clone())
        );

        // The person edits the command after the daemon fingerprinted the old
        // one. The start must still run the snapshot the fingerprint names, so
        // the stored resume id can never reach the new command.
        source.set(spec("demo", &new_script.display().to_string()));
        let session = SessionSpec {
            workspace: workspace.path().to_path_buf(),
            model: "default".into(),
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            fast: false,
            resume_id: Some("stored-session".into()),
            mcp_endpoint: None,
            env_overrides: BTreeMap::new(),
            attachments: Vec::new(),
            agent_id: Some("demo".into()),
        };
        assert!(
            harness
                .start_session_with(session, Some(snapshot))
                .await
                .is_err(),
            "the test script is not an ACP agent"
        );
        assert!(
            old_marker.exists(),
            "the fingerprinted command line is the one that runs"
        );
        assert!(
            !new_marker.exists(),
            "the command saved after the fingerprint must not run with the old session id"
        );
    }

    #[test]
    fn a_fixed_working_directory_is_the_instances_effective_workspace() {
        let thread = tempfile::tempdir().expect("thread");
        let fixed = tempfile::tempdir().expect("fixed");
        let mut agent = spec("demo", "agent");
        agent.working_dir = AcpWorkingDir::FixedPath {
            path: fixed.path().display().to_string(),
        };
        let harness = GenericAcpHarness::new(Arc::new(StaticAcpAgents::new().with(agent)));
        assert_eq!(
            harness.instance_workspace("demo", thread.path()),
            Some(fixed.path().to_path_buf())
        );
        let harness =
            GenericAcpHarness::new(Arc::new(StaticAcpAgents::new().with(spec("demo", "agent"))));
        assert_eq!(
            harness.instance_workspace("demo", thread.path()),
            Some(thread.path().to_path_buf())
        );
        assert_eq!(harness.instance_workspace("missing", thread.path()), None);
    }

    #[test]
    fn an_empty_argument_is_not_the_same_as_no_argument() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut none = spec("demo", "agent");
        none.args = Vec::new();
        let mut empty = spec("demo", "agent");
        empty.args = vec![String::new()];
        let fingerprint = |agent: AcpAgentSpec| {
            GenericAcpHarness::new(Arc::new(StaticAcpAgents::new().with(agent)))
                .instance_fingerprint("demo", workspace.path())
                .expect("fingerprint")
        };
        assert_ne!(
            fingerprint(none),
            fingerprint(empty),
            "an empty argument list and one empty argument are different command lines"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_command_script_is_refused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("agent.cmd");
        std::fs::write(&path, "@echo off\n").expect("write");
        assert_eq!(
            resolve_acp_command(&path.display().to_string()),
            None,
            ".cmd only runs through cmd.exe, and h4rn launches directly"
        );
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
            reasoning_effort: None,
            fast: false,
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
            reasoning_effort: None,
            fast: false,
            resume_id: None,
            mcp_endpoint: None,
            env_overrides: BTreeMap::new(),
            attachments: Vec::new(),
            agent_id: Some("demo".into()),
        };
        assert!(agent_workspace(&agent, &session).is_err());
    }
}
