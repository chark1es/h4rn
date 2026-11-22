//! Importing MCP configuration from the harnesses' native files.
//!
//! Every harness grew its own place to configure MCP servers before h4rn
//! existed. Importing rather than ignoring them is what makes the hub useful on
//! day one, but an import must not silently overwrite a server the user already
//! configured here. [`preview`] therefore separates plain candidates from
//! conflicts and lists the resolutions a conflict admits.
//!
//! Values found in a native file (environment variables, headers) are returned
//! separately from their names so the caller can put them in the secret store;
//! [`McpTransportView`] — the type that leaves the daemon — only ever carries
//! the names.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use h4rn_protocol::{
    HarnessKind, McpConflictResolution, McpImportCandidate, McpImportConflict, McpImportPreview,
    McpTransportView,
};
use serde_json::Value;

use crate::names;
use crate::registry::{ServerRecord, StoredTransport};

/// An import failure (a file that exists but cannot be read or parsed).
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// A file could not be read.
    #[error("could not read {path}: {error}")]
    Read {
        /// File that failed.
        path: String,
        /// OS error.
        error: String,
    },
    /// A file was not valid JSON/TOML.
    #[error("could not parse {path}: {error}")]
    Parse {
        /// File that failed.
        path: String,
        /// Parser error.
        error: String,
    },
}

/// One secret-valued setting found during an import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedSecret {
    /// Environment variable or header name.
    pub name: String,
    /// Plain value read from the native file.
    pub value: String,
}

/// A transport with its secret values intact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportedTransport {
    /// A child process.
    Stdio {
        /// Executable.
        command: String,
        /// Arguments.
        args: Vec<String>,
        /// Working directory.
        cwd: Option<String>,
        /// Environment values.
        env: Vec<ImportedSecret>,
    },
    /// A streamable-HTTP endpoint.
    Http {
        /// Endpoint.
        url: String,
        /// Header values.
        headers: Vec<ImportedSecret>,
    },
    /// A legacy SSE endpoint.
    Sse {
        /// Endpoint.
        url: String,
        /// Header values.
        headers: Vec<ImportedSecret>,
    },
}

impl ImportedTransport {
    /// The redacted view that is safe to return to a client.
    pub fn view(&self) -> McpTransportView {
        match self {
            Self::Stdio {
                command,
                args,
                cwd,
                env,
            } => McpTransportView::Stdio {
                command: command.clone(),
                args: args.clone(),
                cwd: cwd.clone(),
                env_keys: env.iter().map(|secret| secret.name.clone()).collect(),
            },
            Self::Http { url, headers } => McpTransportView::Http {
                url: url.clone(),
                header_keys: headers.iter().map(|secret| secret.name.clone()).collect(),
            },
            Self::Sse { url, headers } => McpTransportView::Sse {
                url: url.clone(),
                header_keys: headers.iter().map(|secret| secret.name.clone()).collect(),
            },
        }
    }

    /// Secret values, keyed by the kind used in a secret reference.
    pub fn secrets(&self) -> Vec<(&'static str, &ImportedSecret)> {
        match self {
            Self::Stdio { env, .. } => env.iter().map(|secret| ("env", secret)).collect(),
            Self::Http { headers, .. } | Self::Sse { headers, .. } => {
                headers.iter().map(|secret| ("header", secret)).collect()
            }
        }
    }
}

/// One server read from a native configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedServer {
    /// Normalised name.
    pub name: String,
    /// Transport including values.
    pub transport: ImportedTransport,
    /// File it came from.
    pub origin: String,
}

/// Everything one harness contributed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportScan {
    /// Files that were read, including empty ones.
    pub origins: Vec<String>,
    /// Servers found.
    pub servers: Vec<ImportedServer>,
}

/// Reads the native configuration of `harness`.
///
/// `workspace` adds the project-level file the harness supports; `home` is the
/// user's home directory. Missing files are not an error: a harness the user
/// never configured simply contributes nothing.
pub fn read_native(
    harness: HarnessKind,
    workspace: Option<&Path>,
    home: &Path,
) -> Result<ImportScan, ImportError> {
    let mut scan = ImportScan::default();
    match harness {
        HarnessKind::ClaudeCode => {
            read_json_file(&home.join(".claude.json"), &mut scan)?;
            if let Some(workspace) = workspace {
                read_json_file(&workspace.join(".mcp.json"), &mut scan)?;
            }
        }
        HarnessKind::Codex => {
            read_codex_file(&home.join(".codex").join("config.toml"), &mut scan)?;
        }
        HarnessKind::Opencode => {
            read_json_file(
                &home.join(".config").join("opencode").join("opencode.json"),
                &mut scan,
            )?;
            if let Some(workspace) = workspace {
                read_json_file(&workspace.join("opencode.json"), &mut scan)?;
            }
        }
        HarnessKind::Antigravity => {
            read_json_file(
                &home.join(".gemini").join("config").join("mcp_config.json"),
                &mut scan,
            )?;
        }
        // A user-configured ACP command has no standard configuration file
        // h4rn can read; its servers come from the agent itself.
        HarnessKind::Acp => {}
    }
    Ok(scan)
}

fn read_json_file(path: &Path, scan: &mut ImportScan) -> Result<(), ImportError> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    let origin = path.display().to_string();
    scan.origins.push(origin.clone());
    let value: Value = serde_json::from_str(&text).map_err(|error| ImportError::Parse {
        path: origin.clone(),
        error: error.to_string(),
    })?;
    let Some(servers) = value
        .get("mcpServers")
        .or_else(|| value.get("mcp"))
        .and_then(Value::as_object)
    else {
        return Ok(());
    };
    for (raw_name, entry) in servers {
        if let Some(server) = parse_entry(raw_name, entry, &origin) {
            scan.servers.push(server);
        }
    }
    Ok(())
}

fn read_codex_file(path: &Path, scan: &mut ImportScan) -> Result<(), ImportError> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(());
    };
    let origin = path.display().to_string();
    scan.origins.push(origin.clone());
    let value: toml::Value = toml::from_str(&text).map_err(|error| ImportError::Parse {
        path: origin.clone(),
        error: error.to_string(),
    })?;
    let Some(servers) = value.get("mcp_servers").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for (raw_name, entry) in servers {
        let json = serde_json::to_value(entry).unwrap_or(Value::Null);
        if let Some(server) = parse_entry(raw_name, &json, &origin) {
            scan.servers.push(server);
        }
    }
    Ok(())
}

fn parse_entry(raw_name: &str, entry: &Value, origin: &str) -> Option<ImportedServer> {
    let name = names::normalise_server_name(raw_name);
    if names::validate_server_name(&name).is_err() {
        return None;
    }
    let transport = if let Some(command) = entry.get("command") {
        let (command, args) = match command {
            Value::String(command) => (command.clone(), string_array(entry.get("args"))),
            Value::Array(items) => {
                let mut commands = items.iter().filter_map(Value::as_str);
                let command = commands.next()?.to_owned();
                (command, commands.map(str::to_owned).collect())
            }
            _ => return None,
        };
        if command.trim().is_empty() {
            return None;
        }
        ImportedTransport::Stdio {
            command,
            args,
            cwd: entry.get("cwd").and_then(Value::as_str).map(str::to_owned),
            env: secret_object(entry.get("env").or_else(|| entry.get("environment"))),
        }
    } else {
        let url = entry.get("url").and_then(Value::as_str)?;
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("http");
        let headers = secret_object(
            entry
                .get("headers")
                .or_else(|| entry.get("http_headers"))
                .or_else(|| entry.get("httpHeaders")),
        );
        if kind.eq_ignore_ascii_case("sse") {
            ImportedTransport::Sse {
                url: url.to_owned(),
                headers,
            }
        } else {
            ImportedTransport::Http {
                url: url.to_owned(),
                headers,
            }
        }
    };
    Some(ImportedServer {
        name,
        transport,
        origin: origin.to_owned(),
    })
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn secret_object(value: Option<&Value>) -> Vec<ImportedSecret> {
    value
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .filter_map(|(name, value)| {
                    Some(ImportedSecret {
                        name: name.clone(),
                        value: value.as_str()?.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The redacted view of a stored record.
pub fn stored_view(transport: &StoredTransport) -> McpTransportView {
    match transport {
        StoredTransport::Stdio {
            command,
            args,
            cwd,
            env,
        } => McpTransportView::Stdio {
            command: command.clone(),
            args: args.clone(),
            cwd: cwd.clone(),
            env_keys: env.iter().map(|secret| secret.name.clone()).collect(),
        },
        StoredTransport::Http { url, headers } => McpTransportView::Http {
            url: url.clone(),
            header_keys: headers.iter().map(|secret| secret.name.clone()).collect(),
        },
        StoredTransport::Sse { url, headers } => McpTransportView::Sse {
            url: url.clone(),
            header_keys: headers.iter().map(|secret| secret.name.clone()).collect(),
        },
    }
}

/// Builds a preview against the servers already configured in the hub.
///
/// Two entries with identical transports are not a conflict: re-importing the
/// same file is idempotent. Only a name whose transport differs becomes a
/// conflict, because that is the case where choosing silently would lose work.
pub fn preview(
    harness: HarnessKind,
    scan: ImportScan,
    existing: &[ServerRecord],
) -> McpImportPreview {
    let existing_by_name: BTreeMap<&str, &ServerRecord> = existing
        .iter()
        .map(|record| (record.name.as_str(), record))
        .collect();
    let mut candidates = Vec::new();
    let mut conflicts = Vec::new();
    let mut seen: BTreeMap<String, McpTransportView> = BTreeMap::new();
    for server in scan.servers {
        let view = server.transport.view();
        if let Some(previous) = seen.get(&server.name) {
            if previous != &view {
                conflicts.push(conflict(
                    &server.name,
                    previous.clone(),
                    view,
                    &existing_by_name,
                ));
            }
            continue;
        }
        seen.insert(server.name.clone(), view.clone());
        match existing_by_name.get(server.name.as_str()) {
            None => candidates.push(McpImportCandidate {
                name: server.name,
                transport: view,
                origin: server.origin,
                existing: false,
            }),
            Some(record) if stored_view(&record.transport) == view => {
                candidates.push(McpImportCandidate {
                    name: server.name,
                    transport: view,
                    origin: server.origin,
                    existing: true,
                });
            }
            Some(record) => {
                conflicts.push(conflict(
                    &server.name,
                    stored_view(&record.transport),
                    view,
                    &existing_by_name,
                ));
            }
        }
    }
    McpImportPreview {
        harness,
        origins: scan.origins,
        candidates,
        conflicts,
    }
}

fn conflict(
    name: &str,
    existing: McpTransportView,
    incoming: McpTransportView,
    existing_by_name: &BTreeMap<&str, &ServerRecord>,
) -> McpImportConflict {
    let mut options = vec![
        McpConflictResolution::KeepMine,
        McpConflictResolution::UseTheirs,
    ];
    if let Some(renamed) = free_name(name, existing_by_name) {
        options.push(McpConflictResolution::Rename { name: renamed });
    }
    options.push(McpConflictResolution::Skip);
    McpImportConflict {
        name: name.to_owned(),
        existing,
        incoming,
        options,
    }
}

fn free_name(name: &str, existing: &BTreeMap<&str, &ServerRecord>) -> Option<String> {
    for suffix in 2u32..100 {
        let candidate = format!("{name}-{suffix}");
        if !existing.contains_key(candidate.as_str())
            && names::validate_server_name(&candidate).is_ok()
        {
            return Some(candidate);
        }
    }
    None
}

/// The files an importer looks at, for diagnostics and tests.
pub fn candidate_paths(
    harness: HarnessKind,
    workspace: Option<&Path>,
    home: &Path,
) -> Vec<PathBuf> {
    match harness {
        HarnessKind::ClaudeCode => {
            let mut paths = vec![home.join(".claude.json")];
            if let Some(workspace) = workspace {
                paths.push(workspace.join(".mcp.json"));
            }
            paths
        }
        HarnessKind::Codex => vec![home.join(".codex").join("config.toml")],
        HarnessKind::Opencode => {
            let mut paths = vec![home.join(".config").join("opencode").join("opencode.json")];
            if let Some(workspace) = workspace {
                paths.push(workspace.join("opencode.json"));
            }
            paths
        }
        HarnessKind::Antigravity => {
            vec![home.join(".gemini").join("config").join("mcp_config.json")]
        }
        // A user-configured ACP command keeps its own configuration.
        HarnessKind::Acp => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use h4rn_protocol::{McpEnabledMatrix, McpSharePolicy};

    fn record(name: &str, transport: StoredTransport) -> ServerRecord {
        ServerRecord::new(
            name,
            transport,
            Vec::new(),
            McpEnabledMatrix::default(),
            None,
        )
    }

    #[test]
    fn claude_user_and_project_files_are_merged() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let workspace = dir.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            home.join(".claude.json"),
            serde_json::json!({
                "mcpServers": {
                    "GitHub": {
                        "command": "npx",
                        "args": ["-y", "server-github"],
                        "env": {"GITHUB_TOKEN": "ghp_secret"}
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            workspace.join(".mcp.json"),
            serde_json::json!({
                "mcpServers": {
                    "remote": {
                        "type": "sse",
                        "url": "https://example.test/sse",
                        "headers": {"Authorization": "Bearer x"}
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let scan = read_native(HarnessKind::ClaudeCode, Some(&workspace), home).unwrap();
        assert_eq!(scan.origins.len(), 2);
        assert_eq!(scan.servers.len(), 2);
        let github = scan
            .servers
            .iter()
            .find(|server| server.name == "github")
            .unwrap();
        assert_eq!(github.transport.secrets().len(), 1);
        assert!(!format!("{:?}", github.transport.view()).contains("ghp_secret"));
    }

    #[test]
    fn codex_toml_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let codex = dir.path().join(".codex");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(
            codex.join("config.toml"),
            "[mcp_servers.files]\ncommand = \"node\"\nargs = [\"server.js\"]\n[mcp_servers.files.env]\nTOKEN = \"x\"\n",
        )
        .unwrap();
        let scan = read_native(HarnessKind::Codex, None, dir.path()).unwrap();
        assert_eq!(scan.servers.len(), 1);
        assert_eq!(scan.servers[0].name, "files");
        assert_eq!(scan.servers[0].transport.secrets()[0].1.value, "x");
    }

    #[test]
    fn opencode_local_command_array_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join("opencode.json"),
            serde_json::json!({
                "mcp": {
                    "docs": {
                        "type": "local",
                        "command": ["uvx", "mcp-docs"],
                        "environment": {"KEY": "v"}
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let scan = read_native(HarnessKind::Opencode, Some(&workspace), dir.path()).unwrap();
        assert_eq!(scan.servers.len(), 1);
        match &scan.servers[0].transport {
            ImportedTransport::Stdio { command, args, .. } => {
                assert_eq!(command, "uvx");
                assert_eq!(args, &["mcp-docs".to_owned()]);
            }
            other => panic!("unexpected transport {other:?}"),
        }
    }

    #[test]
    fn preview_separates_conflicts_from_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join(".gemini").join("config");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("mcp_config.json"),
            serde_json::json!({
                "mcpServers": {
                    "same": {"command": "node", "args": ["a.js"]},
                    "different": {"command": "node", "args": ["b.js"]},
                    "new": {"command": "node", "args": ["c.js"]}
                }
            })
            .to_string(),
        )
        .unwrap();
        let scan = read_native(HarnessKind::Antigravity, None, dir.path()).unwrap();
        let existing = vec![
            record(
                "same",
                StoredTransport::Stdio {
                    command: "node".into(),
                    args: vec!["a.js".into()],
                    cwd: None,
                    env: Vec::new(),
                },
            ),
            record(
                "different",
                StoredTransport::Stdio {
                    command: "node".into(),
                    args: vec!["old.js".into()],
                    cwd: None,
                    env: Vec::new(),
                },
            ),
        ];
        let preview = preview(HarnessKind::Antigravity, scan, &existing);
        assert_eq!(preview.candidates.len(), 2);
        assert_eq!(preview.conflicts.len(), 1);
        assert_eq!(preview.conflicts[0].name, "different");
        assert!(
            preview.conflicts[0]
                .options
                .iter()
                .any(|option| matches!(option, McpConflictResolution::Rename { .. }))
        );
    }

    #[test]
    fn share_policy_remains_independent_of_import() {
        // Import must never invent sharing; a fresh record has no shares.
        let record = record(
            "x",
            StoredTransport::Http {
                url: "https://example.test".into(),
                headers: Vec::new(),
            },
        );
        assert!(record.tags.is_empty());
        let _ = McpSharePolicy::default();
        let _ = Utc::now();
    }
}
