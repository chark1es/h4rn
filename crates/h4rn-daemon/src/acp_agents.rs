//! User-configured local ACP agents.
//!
//! The list is a small JSON file in the daemon's data directory, written
//! atomically. A configured command is third-party code that runs with the
//! daemon user's privileges; h4rn launches it directly with a literal argument
//! list and never through a shell.
//!
//! Nothing here downloads or updates an agent. "Installed" is only whether the
//! configured command resolves to an executable file on this host, and a
//! version is shown only when the agent itself reported one in an ACP
//! `initialize` handshake. Settings therefore says "Managed outside h4rn".

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use h4rn_harness::{AcpAgentConfigSource, probe_acp_agent, resolve_acp_command};
use h4rn_protocol::{
    AcpAgent, AcpAgentProbe, AcpAgentSpec, AcpWorkingDir, ProtoError, ProtoErrorCode,
};
use serde::{Deserialize, Serialize};

const FILE: &str = "acp-agents.json";

#[derive(Default, Serialize, Deserialize)]
struct AgentsFile {
    #[serde(default)]
    agents: Vec<StoredAgent>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredAgent {
    #[serde(flatten)]
    spec: AcpAgentSpec,
    /// Version the agent last reported in `initialize`, when it did.
    #[serde(default)]
    version: Option<String>,
}

/// Owns `acp-agents.json` for this daemon.
pub struct AcpAgentService {
    path: PathBuf,
    agents: Mutex<Vec<StoredAgent>>,
}

fn invalid(message: impl Into<String>, hint: impl Into<String>) -> ProtoError {
    ProtoError::new(ProtoErrorCode::InvalidRequest, message, hint)
}

impl AcpAgentService {
    /// Loads the list from `data_dir`, treating a missing file as empty.
    pub fn open(data_dir: &Path) -> anyhow::Result<Self> {
        let path = data_dir.join(FILE);
        let file: AgentsFile = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, path = %path.display(), "ignoring an unreadable ACP agent list");
                AgentsFile::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => AgentsFile::default(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            agents: Mutex::new(file.agents),
        })
    }

    /// Configured agents, in creation order, with installation facts.
    pub fn list(&self) -> Vec<AcpAgent> {
        lock(&self.agents).iter().map(view).collect()
    }

    /// One configured agent, in its writable form.
    pub fn spec(&self, id: &str) -> Option<AcpAgentSpec> {
        lock(&self.agents)
            .iter()
            .find(|agent| agent.spec.id == id)
            .map(|agent| agent.spec.clone())
    }

    /// Adds one agent. The id must not be in use.
    pub fn add(&self, spec: AcpAgentSpec) -> Result<AcpAgent, ProtoError> {
        let spec = validated(spec)?;
        let mut agents = lock(&self.agents);
        if agents.iter().any(|agent| agent.spec.id == spec.id) {
            return Err(invalid(
                format!("an ACP agent with id `{}` already exists", spec.id),
                "Choose another instance id, or edit the existing agent.",
            ));
        }
        agents.push(StoredAgent {
            spec: spec.clone(),
            version: None,
        });
        save(&self.path, &agents)?;
        Ok(view(agents.last().expect("the agent was just pushed")))
    }

    /// Replaces one agent's configuration.
    pub fn update(&self, id: &str, spec: AcpAgentSpec) -> Result<AcpAgent, ProtoError> {
        let spec = validated(spec)?;
        if spec.id != id {
            return Err(invalid(
                "the instance id cannot be changed",
                "Remove the agent and add it again under the new id.",
            ));
        }
        let mut agents = lock(&self.agents);
        let Some(index) = agents.iter().position(|agent| agent.spec.id == id) else {
            return Err(not_found(id));
        };
        // A version reported by the old command says nothing about a new one.
        if agents[index].spec.command != spec.command || agents[index].spec.args != spec.args {
            agents[index].version = None;
        }
        agents[index].spec = spec;
        save(&self.path, &agents)?;
        Ok(view(&agents[index]))
    }

    /// Removes one agent. Threads keep their history and their stored events.
    pub fn remove(&self, id: &str) -> Result<(), ProtoError> {
        let mut agents = lock(&self.agents);
        let before = agents.len();
        agents.retain(|agent| agent.spec.id != id);
        if agents.len() == before {
            return Err(not_found(id));
        }
        save(&self.path, &agents)
    }

    /// Starts one agent, reports what it advertises, and stops it.
    pub async fn probe(&self, id: &str) -> Result<AcpAgentProbe, ProtoError> {
        let Some(spec) = self.spec(id) else {
            return Err(not_found(id));
        };
        let probe = probe_acp_agent(&spec).await;
        if let Some(version) = probe.version.as_deref() {
            let mut agents = lock(&self.agents);
            if let Some(agent) = agents.iter_mut().find(|agent| agent.spec.id == id) {
                agent.version = Some(version.to_string());
                if let Err(error) = save(&self.path, &agents) {
                    tracing::warn!(error = %error.message, "could not persist an ACP agent's reported version");
                }
            }
        }
        Ok(probe)
    }
}

impl AcpAgentConfigSource for AcpAgentService {
    fn acp_agent(&self, id: &str) -> Option<AcpAgentSpec> {
        self.spec(id)
    }
}

fn view(agent: &StoredAgent) -> AcpAgent {
    let path = resolve_acp_command(&agent.spec.command);
    AcpAgent {
        id: agent.spec.id.clone(),
        name: agent.spec.name.clone(),
        command: agent.spec.command.clone(),
        args: agent.spec.args.clone(),
        env: agent.spec.env.clone(),
        working_dir: agent.spec.working_dir.clone(),
        installed: path.is_some(),
        path: path.map(|path| path.display().to_string()),
        version: agent.version.clone(),
    }
}

/// Validates and normalises a configuration.
fn validated(mut spec: AcpAgentSpec) -> Result<AcpAgentSpec, ProtoError> {
    spec.id = spec.id.trim().to_string();
    spec.name = spec.name.trim().to_string();
    spec.command = spec.command.trim().to_string();

    if spec.id.is_empty() {
        return Err(invalid(
            "the instance id is required",
            "Use a short lowercase slug, for example `deepseek`.",
        ));
    }
    if spec.id.len() > 64 {
        return Err(invalid(
            "the instance id is longer than 64 characters",
            "Shorten the slug.",
        ));
    }
    let id_ok = !spec.id.starts_with('-')
        && !spec.id.ends_with('-')
        && !spec.id.contains("--")
        && spec.id.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
        && spec
            .id
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric());
    if !id_ok {
        return Err(invalid(
            format!("`{}` is not a valid instance id", spec.id),
            "Use lowercase letters, digits and single dashes, starting and ending with a letter or digit.",
        ));
    }

    if spec.name.is_empty() {
        return Err(invalid(
            "a display name is required",
            "This is the name Settings and the new-thread picker show.",
        ));
    }
    if spec.name.len() > 80 {
        return Err(invalid(
            "the display name is longer than 80 characters",
            "Shorten the name.",
        ));
    }
    if spec.name.contains(['\n', '\r', '\0']) {
        return Err(invalid(
            "the display name cannot contain newlines or control characters",
            "Use a single line.",
        ));
    }

    if spec.command.is_empty() {
        return Err(invalid(
            "a command is required",
            "Enter the executable name or an absolute path.",
        ));
    }
    if spec.command.contains(['\n', '\r', '\0']) {
        return Err(invalid(
            "the command cannot contain newlines or control characters",
            "Enter one executable name or path.",
        ));
    }
    if (spec.command.contains('/') || spec.command.contains('\\'))
        && !Path::new(&spec.command).is_absolute()
    {
        return Err(invalid(
            format!("`{}` is a relative path", spec.command),
            "Use an executable name on PATH, or an absolute path.",
        ));
    }
    if let AcpWorkingDir::FixedPath { path } = &mut spec.working_dir {
        let trimmed = path.trim().to_string();
        if trimmed.is_empty() {
            return Err(invalid(
                "the fixed working directory is empty",
                "Choose an absolute directory path.",
            ));
        }
        if !Path::new(&trimmed).is_absolute() {
            return Err(invalid(
                format!("`{trimmed}` is not an absolute path"),
                "Choose an absolute directory path on this host.",
            ));
        }
        *path = trimmed;
    }

    for argument in &spec.args {
        if argument.contains('\0') {
            return Err(invalid(
                "an argument contains a NUL byte",
                "Remove the control character.",
            ));
        }
    }

    for (key, value) in &spec.env {
        let mut characters = key.chars();
        let first_ok = characters
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic() || character == '_');
        let rest_ok =
            characters.all(|character| character.is_ascii_alphanumeric() || character == '_');
        if !first_ok || !rest_ok {
            return Err(invalid(
                format!("`{key}` is not a valid environment variable name"),
                "Use letters, digits and underscores, starting with a letter or underscore.",
            ));
        }
        if value.contains('\0') {
            return Err(invalid(
                format!("the value of `{key}` contains a NUL byte"),
                "Remove the control character.",
            ));
        }
    }

    Ok(spec)
}

fn save(path: &Path, agents: &[StoredAgent]) -> Result<(), ProtoError> {
    let body = serde_json::to_vec_pretty(&AgentsFile {
        agents: agents.to_vec(),
    })
    .map_err(|error| {
        ProtoError::plain(
            ProtoErrorCode::Internal,
            format!("could not encode the ACP agent list: {error}"),
        )
    })?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).map_err(|error| {
        ProtoError::plain(
            ProtoErrorCode::Internal,
            format!("could not save the ACP agent list: {error}"),
        )
    })?;
    // Environment overrides can carry credentials, so the file is private.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(
            |error| {
                ProtoError::plain(
                    ProtoErrorCode::Internal,
                    format!("could not protect the ACP agent list: {error}"),
                )
            },
        )?;
    }
    std::fs::rename(&tmp, path).map_err(|error| {
        ProtoError::plain(
            ProtoErrorCode::Internal,
            format!("could not save the ACP agent list: {error}"),
        )
    })
}

fn not_found(id: &str) -> ProtoError {
    ProtoError::plain(
        ProtoErrorCode::NotFound,
        format!("no ACP agent `{id}` is configured on this host"),
    )
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn spec(id: &str) -> AcpAgentSpec {
        AcpAgentSpec {
            id: id.into(),
            name: "Demo agent".into(),
            command: "demo-agent".into(),
            args: vec!["acp".into()],
            env: BTreeMap::new(),
            working_dir: AcpWorkingDir::ThreadWorkspace,
        }
    }

    #[test]
    fn validation_accepts_a_normal_configuration() {
        let mut spec = spec("demo-agent");
        spec.env.insert("DEMO_TOKEN".into(), "secret".into());
        let validated = validated(spec).expect("valid");
        assert_eq!(validated.id, "demo-agent");
    }

    #[test]
    fn validation_rejects_bad_ids_and_empty_fields() {
        for id in [
            "",
            "Demo",
            "-demo",
            "demo-",
            "demo--agent",
            "demo agent",
            &"a".repeat(65),
        ] {
            let mut candidate = spec(id);
            candidate.id = id.to_string();
            assert!(validated(candidate).is_err(), "{id:?} must be rejected");
        }
        let mut empty = spec("demo");
        empty.name = "  ".into();
        assert!(validated(empty).is_err());
        let mut empty = spec("demo");
        empty.command = "".into();
        assert!(validated(empty).is_err());
        let mut relative = spec("demo");
        relative.command = "bin/agent".into();
        assert!(validated(relative).is_err());
    }

    #[test]
    fn validation_checks_environment_names_and_fixed_paths() {
        let mut bad_key = spec("demo");
        bad_key.env.insert("1BAD".into(), "x".into());
        assert!(validated(bad_key).is_err());

        let mut relative = spec("demo");
        relative.working_dir = AcpWorkingDir::FixedPath {
            path: "relative/dir".into(),
        };
        assert!(validated(relative).is_err());

        let mut absolute = spec("demo");
        absolute.working_dir = AcpWorkingDir::FixedPath {
            path: "/tmp".into(),
        };
        assert!(validated(absolute).is_ok());
    }

    #[test]
    fn the_list_persists_and_reloads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = AcpAgentService::open(dir.path()).expect("open");
        service.add(spec("demo")).expect("add");

        let reloaded = AcpAgentService::open(dir.path()).expect("reopen");
        let agents = reloaded.list();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, "demo");
        assert_eq!(agents[0].command, "demo-agent");
        // The command is not on this host.
        assert!(!agents[0].installed);
        assert!(agents[0].path.is_none());
    }

    #[test]
    fn duplicate_ids_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = AcpAgentService::open(dir.path()).expect("open");
        service.add(spec("demo")).expect("add");
        let error = service.add(spec("demo")).expect_err("duplicate");
        assert_eq!(error.code, ProtoErrorCode::InvalidRequest);
    }

    #[test]
    fn update_requires_a_matching_known_id_and_clears_a_stale_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = AcpAgentService::open(dir.path()).expect("open");
        service.add(spec("demo")).expect("add");
        {
            let mut agents = lock(&service.agents);
            agents[0].version = Some("1.2.3".into());
        }

        let mut next = spec("demo");
        next.args = vec!["--acp".into()];
        let updated = service.update("demo", next.clone()).expect("update");
        assert_eq!(updated.args, vec!["--acp"]);
        assert!(
            updated.version.is_none(),
            "a new command line cannot keep the old command's version"
        );

        let mut renamed = next;
        renamed.id = "other".into();
        assert!(service.update("demo", renamed).is_err());
        assert!(service.update("missing", spec("missing")).is_err());
    }

    #[test]
    fn remove_deletes_only_the_named_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = AcpAgentService::open(dir.path()).expect("open");
        service.add(spec("one")).expect("add one");
        service.add(spec("two")).expect("add two");
        service.remove("one").expect("remove");
        assert_eq!(
            service
                .list()
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            vec!["two"]
        );
        assert!(service.remove("one").is_err());
    }

    #[test]
    fn the_config_source_returns_the_stored_spec() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = AcpAgentService::open(dir.path()).expect("open");
        service.add(spec("demo")).expect("add");
        let stored = AcpAgentConfigSource::acp_agent(&service, "demo").expect("spec");
        assert_eq!(stored.name, "Demo agent");
        assert!(service.spec("missing").is_none());
    }
}
