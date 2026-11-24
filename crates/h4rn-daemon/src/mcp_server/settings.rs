//! Per-harness switches for h4rn's own MCP tools.
//!
//! The hub's upstream servers have an enable matrix per server; the built-in
//! tools are not an upstream server, so they keep a small matrix of their own.
//! A missing entry means enabled, exactly like [`McpEnabledMatrix`], so a new
//! harness sees the tools until the person turns them off.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use h4rn_protocol::{AgentToolSettings, HarnessKind, HostId, McpEnabledMatrix};
use serde::{Deserialize, Serialize};

/// File inside the data directory.
const FILE: &str = "mcp-agent-tools.json";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    enabled: McpEnabledMatrix,
}

/// Reads and persists [`AgentToolSettings`] atomically.
pub struct AgentToolSettingsStore {
    path: PathBuf,
    host: HostId,
    settings: Mutex<AgentToolSettings>,
}

impl AgentToolSettingsStore {
    /// Loads the settings, treating a missing or unreadable file as defaults.
    pub fn open(data_dir: &Path, host: HostId) -> Self {
        let path = data_dir.join(FILE);
        let file: SettingsFile = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, path = %path.display(), "ignoring unreadable h4rn tool switches");
                SettingsFile::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => SettingsFile::default(),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "could not read h4rn tool switches");
                SettingsFile::default()
            }
        };
        Self {
            path,
            host,
            settings: Mutex::new(AgentToolSettings {
                enabled: file.enabled,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AgentToolSettings> {
        self.settings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether `harness` sessions on this machine get h4rn's own tools.
    pub fn is_enabled(&self, harness: HarnessKind) -> bool {
        self.lock().enabled.is_enabled(harness, &self.host)
    }

    /// Current settings.
    pub fn settings(&self) -> AgentToolSettings {
        self.lock().clone()
    }

    /// Stores one harness switch and returns the new settings.
    pub fn set_harness(
        &self,
        harness: HarnessKind,
        enabled: bool,
    ) -> std::io::Result<AgentToolSettings> {
        let mut current = self.lock();
        // Build the candidate, put it on disk, and only then make it the value
        // the daemon answers with. A setting the file does not hold must not be
        // reported as on: it would look saved until the next restart.
        let mut candidate = current.clone();
        candidate.enabled.set_harness(harness, enabled);
        self.write(&candidate)?;
        *current = candidate.clone();
        Ok(candidate)
    }

    fn write(&self, settings: &AgentToolSettings) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = SettingsFile {
            enabled: settings.enabled.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
        let temp = self.path.with_extension("tmp");
        std::fs::write(&temp, bytes)?;
        std::fs::rename(&temp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A switch the file cannot hold must not look saved in memory.
    #[test]
    fn a_failed_write_leaves_the_setting_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The data directory is a regular file, so the settings write fails.
        let blocked = dir.path().join("not-a-directory");
        std::fs::write(&blocked, b"x").expect("write");
        let store = AgentToolSettingsStore::open(&blocked, HostId::new("host_test"));
        assert!(store.is_enabled(HarnessKind::Codex));
        store
            .set_harness(HarnessKind::Codex, false)
            .expect_err("the write must fail");
        assert!(
            store.is_enabled(HarnessKind::Codex),
            "a failed write was reported as a saved switch"
        );

        let store = AgentToolSettingsStore::open(dir.path(), HostId::new("host_test"));
        store
            .set_harness(HarnessKind::Codex, false)
            .expect("the write succeeds");
        assert!(!store.is_enabled(HarnessKind::Codex));
        let reopened = AgentToolSettingsStore::open(dir.path(), HostId::new("host_test"));
        assert!(!reopened.is_enabled(HarnessKind::Codex));
    }
}
