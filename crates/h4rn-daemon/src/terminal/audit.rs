//! Append-only audit trail for terminal opens.
//!
//! Opening a PTY is the most powerful thing a device can ask a daemon to do:
//! it runs an arbitrary shell as the daemon user. Every open therefore records
//! who asked (device id, name and platform), where (host and cwd) and which
//! shell, in a local JSONL file that survives daemon restarts and is excluded
//! from every network path.
//!
//! The sink is a trait so tests can capture entries without touching disk.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One terminal-open audit record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// When the request was accepted.
    pub at: DateTime<Utc>,
    /// Terminal that was opened.
    pub terminal_id: String,
    /// Owning thread, when any.
    pub thread_id: Option<String>,
    /// Device that asked.
    pub device_id: String,
    /// Device display name.
    pub device_name: String,
    /// Device platform (`linux`, `macos`, `ios`, `web`, ...).
    pub device_platform: String,
    /// Host that ran the PTY.
    pub host: String,
    /// Working directory.
    pub cwd: String,
    /// Shell executable.
    pub shell: String,
    /// Initial size.
    pub cols: u16,
    /// Initial size.
    pub rows: u16,
}

/// A destination for terminal audit records.
pub trait AuditSink: Send + Sync {
    /// Writes one record. Implementations must not block a protocol handler for
    /// long; a failed audit write is logged and does not fail the open.
    fn record(&self, entry: &AuditEntry);
}

/// Writes JSONL to `<data_dir>/terminal-audit.log` with `0600` permissions.
#[derive(Debug)]
pub struct FileAudit {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileAudit {
    /// Creates a sink rooted at the daemon data directory.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: data_dir.as_ref().join("terminal-audit.log"),
            lock: Mutex::new(()),
        }
    }

    /// Path of the audit log.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AuditSink for FileAudit {
    fn record(&self, entry: &AuditEntry) {
        let Ok(line) = serde_json::to_string(entry) else {
            tracing::warn!(terminal_id = %entry.terminal_id, "could not serialise terminal audit entry");
            return;
        };
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut file| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
                }
                writeln!(file, "{line}")
            });
        if let Err(error) = result {
            tracing::warn!(path = %self.path.display(), %error, "could not append terminal audit entry");
        }
    }
}

/// Forwards audit records to `tracing` only.
#[derive(Debug, Default)]
pub struct TracingAudit;

impl AuditSink for TracingAudit {
    fn record(&self, entry: &AuditEntry) {
        tracing::info!(
            terminal_id = %entry.terminal_id,
            thread_id = entry.thread_id.as_deref().unwrap_or(""),
            device_id = %entry.device_id,
            host = %entry.host,
            cwd = %entry.cwd,
            shell = %entry.shell,
            "terminal opened"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> AuditEntry {
        AuditEntry {
            at: Utc::now(),
            terminal_id: "term_1".into(),
            thread_id: Some("th_1".into()),
            device_id: "dev_phone".into(),
            device_name: "Pixel".into(),
            device_platform: "android".into(),
            host: "host_atlas".into(),
            cwd: "/home/you/code/h4rn".into(),
            shell: "/bin/bash".into(),
            cols: 80,
            rows: 24,
        }
    }

    #[test]
    fn file_audit_appends_who_and_where() {
        let dir = tempfile::tempdir().unwrap();
        let sink = FileAudit::new(dir.path());
        sink.record(&entry());
        sink.record(&entry());
        let text = std::fs::read_to_string(sink.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: AuditEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed.device_id, "dev_phone");
        assert_eq!(parsed.cwd, "/home/you/code/h4rn");
    }

    #[test]
    fn tracing_audit_never_panics() {
        TracingAudit.record(&entry());
    }
}
