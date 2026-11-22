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
/// What a view-scoped reader sees instead of an environment value.
const MASKED_VALUE: &str = "•••";

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
    /// Models from the last successful probe, for view-scoped readers.
    #[serde(default)]
    models: Vec<h4rn_protocol::AcpAgentModel>,
    /// Modes from the last successful probe.
    #[serde(default)]
    modes: Vec<h4rn_protocol::AcpAgentMode>,
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
        // A hand-edited file can hold an entry the add/update path would have
        // refused (a NUL in an argument, an empty command). Validate on load so
        // that entry is dropped here instead of panicking the spawn later.
        let mut seen = std::collections::HashSet::new();
        let agents = file
            .agents
            .into_iter()
            .filter_map(|agent| match validated(agent.spec.clone()) {
                Ok(spec) => Some(StoredAgent { spec, ..agent }),
                Err(error) => {
                    tracing::warn!(
                        id = %agent.spec.id,
                        reason = %error.message,
                        path = %path.display(),
                        "dropping an invalid configured ACP agent"
                    );
                    None
                }
            })
            // Only the first entry for an id is kept. Two entries under one id
            // would make `get_acp_agent` and a probe disagree about which
            // configuration they describe.
            .filter(|agent| {
                if seen.insert(agent.spec.id.clone()) {
                    true
                } else {
                    tracing::warn!(
                        id = %agent.spec.id,
                        path = %path.display(),
                        "dropping a configured ACP agent whose id repeats an earlier entry"
                    );
                    false
                }
            })
            .collect();
        Ok(Self {
            path,
            agents: Mutex::new(agents),
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
        let mut next = agents.clone();
        next.push(StoredAgent {
            spec,
            version: None,
            models: Vec::new(),
            modes: Vec::new(),
        });
        // Persist first: a failed save must leave the live list unchanged.
        save(&self.path, &next)?;
        let added = view(next.last().expect("the agent was just pushed"));
        *agents = next;
        Ok(added)
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
        let mut next = agents.clone();
        // Any edit invalidates the version the old configuration reported: the
        // editor may have changed the command, its arguments, its environment
        // or its working directory. Probe results go with it, because they
        // describe the command that ran, not the one now saved.
        next[index].version = None;
        next[index].models.clear();
        next[index].modes.clear();
        next[index].spec = spec;
        save(&self.path, &next)?;
        let updated = view(&next[index]);
        *agents = next;
        Ok(updated)
    }

    /// Removes one agent. Threads keep their history and their stored events.
    pub fn remove(&self, id: &str) -> Result<(), ProtoError> {
        let mut agents = lock(&self.agents);
        let mut next = agents.clone();
        next.retain(|agent| agent.spec.id != id);
        if next.len() == agents.len() {
            return Err(not_found(id));
        }
        save(&self.path, &next)?;
        *agents = next;
        Ok(())
    }

    /// Starts one agent, reports what it advertises, and stops it.
    ///
    /// The handshake takes seconds and the agent stays editable meanwhile. A
    /// result for a configuration the person has already changed says nothing
    /// about the new one, so it is neither stored nor returned as success: the
    /// caller gets a conflict and the picker keeps the last good answer.
    pub async fn probe(&self, id: &str) -> Result<AcpAgentProbe, ProtoError> {
        let Some(spec) = self.spec(id) else {
            return Err(not_found(id));
        };
        let fingerprint = spec_fingerprint(&spec);
        let probe = probe_acp_agent(&spec).await;
        let mut agents = lock(&self.agents);
        let mut next = agents.clone();
        let Some(agent) = next.iter_mut().find(|agent| agent.spec.id == id) else {
            tracing::debug!(
                id,
                "discarding an ACP probe for an agent that was removed while it ran"
            );
            return Err(superseded(id));
        };
        // A result for a configuration that no longer exists is superseded
        // whether it succeeded or failed: the failure describes the old
        // command line, not the saved one.
        if spec_fingerprint(&agent.spec) != fingerprint {
            tracing::debug!(
                id,
                "discarding an ACP probe whose configuration changed while it ran"
            );
            return Err(superseded(id));
        }
        // A failed probe keeps the last successful result: the picker can
        // still offer the models the agent advertised before, and the failure
        // itself is returned to the admin device that asked.
        if probe.error.is_some() {
            return Ok(probe);
        }
        agent.version = probe.version.clone();
        agent.models = probe.models.clone();
        agent.modes = probe.modes.clone();
        if let Err(error) = save(&self.path, &next) {
            tracing::warn!(error = %error.message, "could not persist an ACP agent's probe result");
        } else {
            *agents = next;
        }
        Ok(probe)
    }
}

impl AcpAgentConfigSource for AcpAgentService {
    fn acp_agent(&self, id: &str) -> Option<AcpAgentSpec> {
        self.spec(id)
    }
}

/// A fingerprint of everything that describes one configuration.
///
/// Used to tell whether a probe still describes the entry it was started for;
/// the map order is stable and the shape is the same one that is persisted.
fn spec_fingerprint(spec: &AcpAgentSpec) -> String {
    serde_json::to_string(spec).expect("an ACP agent spec is always serializable")
}

fn view(agent: &StoredAgent) -> AcpAgent {
    let path = resolve_acp_command(&agent.spec.command);
    AcpAgent {
        id: agent.spec.id.clone(),
        name: agent.spec.name.clone(),
        command: agent.spec.command.clone(),
        // The literal argument list is not part of the view-scoped shape: it
        // can carry paths or secrets, and only the admin-scoped
        // `GetAcpAgent` returns the real configuration.
        // Environment values can be credentials. A view-scoped device sees
        // which names are set, never their values; the admin-only
        // `GetAcpAgent` returns the real configuration for the edit form.
        env: agent
            .spec
            .env
            .keys()
            .map(|key| (key.clone(), MASKED_VALUE.to_owned()))
            .collect(),
        working_dir: agent.spec.working_dir.clone(),
        installed: path.is_some(),
        path: path.map(|path| path.display().to_string()),
        version: agent.version.clone(),
        models: agent.models.clone(),
        modes: agent.modes.clone(),
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
    if spec.name.chars().count() > 80 {
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

/// The configuration changed (or the agent was removed) while its probe ran.
fn superseded(id: &str) -> ProtoError {
    ProtoError::new(
        ProtoErrorCode::Conflict,
        format!("ACP agent `{id}` changed while its probe was running"),
        "Run Check again; the models from the old command were discarded.",
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
    fn the_view_masks_environment_values_but_the_admin_spec_keeps_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = AcpAgentService::open(dir.path()).expect("open");
        let mut with_secret = spec("demo");
        with_secret
            .env
            .insert("DEMO_TOKEN".into(), "super-secret".into());
        service.add(with_secret).expect("add");

        let listed = service.list();
        assert_eq!(listed[0].env["DEMO_TOKEN"], MASKED_VALUE);
        assert!(
            !format!("{listed:?}").contains("super-secret"),
            "a view-scoped list must not carry the value"
        );
        assert_eq!(
            service.spec("demo").expect("admin spec").env["DEMO_TOKEN"],
            "super-secret"
        );
    }

    #[test]
    fn a_view_reader_sees_the_last_successful_probe_models() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = AcpAgentService::open(dir.path()).expect("open");
        service.add(spec("demo")).expect("add");
        {
            let mut agents = lock(&service.agents);
            agents[0].version = Some("1.2.3".into());
            agents[0].models = vec![h4rn_protocol::AcpAgentModel {
                id: "demo-model".into(),
                name: "Demo Model".into(),
            }];
            agents[0].modes = vec![h4rn_protocol::AcpAgentMode {
                id: "default".into(),
                name: "Default".into(),
                description: None,
            }];
        }

        let listed = service.list();
        assert_eq!(listed[0].version.as_deref(), Some("1.2.3"));
        assert_eq!(listed[0].models[0].id, "demo-model");
        assert_eq!(listed[0].modes[0].id, "default");

        // A command-line edit invalidates what the old command advertised.
        let mut next = spec("demo");
        next.command = "another-agent".into();
        let updated = service.update("demo", next).expect("update");
        assert!(updated.version.is_none());
        assert!(updated.models.is_empty());
        assert!(updated.modes.is_empty());
    }

    #[test]
    fn the_view_omits_the_argument_list_the_admin_spec_keeps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = AcpAgentService::open(dir.path()).expect("open");
        let mut with_args = spec("demo");
        with_args.args = vec!["--token".into(), "super-secret-arg".into()];
        service.add(with_args).expect("add");

        let listed = serde_json::to_string(&service.list()).expect("encode view");
        assert!(
            !listed.contains("super-secret-arg") && !listed.contains("--token"),
            "a view-scoped list must not carry the literal arguments: {listed}"
        );
        assert_eq!(
            service.spec("demo").expect("admin spec").args,
            vec!["--token", "super-secret-arg"],
            "the admin request still carries the configuration the form edits"
        );
    }

    #[test]
    fn an_edit_of_any_field_clears_the_cached_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = AcpAgentService::open(dir.path()).expect("open");
        service.add(spec("demo")).expect("add");
        {
            let mut agents = lock(&service.agents);
            agents[0].version = Some("1.2.3".into());
            agents[0].models = vec![h4rn_protocol::AcpAgentModel {
                id: "demo-model".into(),
                name: "Demo Model".into(),
            }];
        }

        // Only the display name changes: the version the old configuration
        // reported must still go, because Settings would attribute it to the
        // edited entry.
        let mut renamed = spec("demo");
        renamed.name = "Renamed".into();
        let updated = service.update("demo", renamed).expect("update");
        assert!(updated.version.is_none());
        assert!(updated.models.is_empty());
    }

    #[test]
    fn invalid_entries_in_the_file_are_dropped_on_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join(FILE);
        // A hand-edited file: one good entry, one with a NUL in its arguments,
        // which the add path would refuse and which must never reach a spawn.
        std::fs::write(
            &file,
            r#"{"agents":[
                {"id":"good","name":"Good agent","command":"demo-agent","args":["acp"],"env":{},"working_dir":{"kind":"thread_workspace"}},
                {"id":"bad","name":"Bad agent","command":"demo-agent","args":["a\u0000b"],"env":{},"working_dir":{"kind":"thread_workspace"}}
            ]}"#,
        )
        .expect("write");

        let service = AcpAgentService::open(dir.path()).expect("open");
        assert_eq!(
            service
                .list()
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            vec!["good"],
            "an invalid stored entry is dropped, not spawned"
        );
    }

    #[test]
    fn a_repeated_id_in_the_file_keeps_the_first_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join(FILE);
        // A hand-edited or merged file: two entries claim the same id.
        std::fs::write(
            &file,
            r#"{"agents":[
                {"id":"demo","name":"First","command":"demo-agent","args":[],"env":{},"working_dir":{"kind":"thread_workspace"}},
                {"id":"demo","name":"Second","command":"other-agent","args":[],"env":{},"working_dir":{"kind":"thread_workspace"}}
            ]}"#,
        )
        .expect("write");

        let service = AcpAgentService::open(dir.path()).expect("open");
        let agents = service.list();
        assert_eq!(agents.len(), 1, "one id, one entry");
        assert_eq!(agents[0].name, "First");
        assert_eq!(agents[0].command, "demo-agent");
        assert_eq!(service.spec("demo").expect("spec").name, "First");
    }

    #[test]
    fn the_display_name_cap_counts_characters_not_bytes() {
        let mut multi_byte = spec("demo");
        multi_byte.name = "é".repeat(80);
        assert!(
            validated(multi_byte).is_ok(),
            "80 characters are 80 characters"
        );

        let mut too_long = spec("demo");
        too_long.name = "é".repeat(81);
        assert!(validated(too_long).is_err());
    }

    #[test]
    fn a_failed_save_leaves_the_live_list_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = AcpAgentService::open(dir.path()).expect("open");
        service.add(spec("keep")).expect("add");
        // A directory where the file belongs makes the final rename fail.
        std::fs::remove_file(dir.path().join(FILE)).expect("remove file");
        std::fs::create_dir(dir.path().join(FILE)).expect("blocking directory");

        assert!(service.add(spec("second")).is_err());
        assert!(
            service.spec("second").is_none(),
            "a failed save must not publish the new agent"
        );
        assert_eq!(
            service
                .list()
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            vec!["keep"]
        );

        let mut renamed = spec("keep");
        renamed.name = "Renamed".into();
        assert!(service.update("keep", renamed).is_err());
        assert_eq!(service.spec("keep").expect("unchanged").name, "Demo agent");

        assert!(service.remove("keep").is_err());
        assert!(service.spec("keep").is_some());
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
        assert_eq!(
            service.spec("demo").expect("admin spec").args,
            vec!["--acp"],
            "the argument list is stored, and only the admin spec carries it"
        );
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
