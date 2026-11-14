//! Remote-action audit.
//!
//! A remote device that spawns a harness, opens a terminal or writes a file
//! leaves a line in `<data_dir>/remote-audit.jsonl` and, when the relay is
//! reachable, the same entry is mirrored to `POST /v1/audit` on h4rn-connect.
//! The local file is written first so an offline daemon still has the record.
//!
//! Metadata is deliberately metadata-only, matching the connect audit contract:
//! ids and counts, never prompts, terminal output or file contents.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use h4rn_protocol::AuditIngestRequest;
use serde::{Deserialize, Serialize};

use super::{ConnectClient, DeviceRegistry, SessionOrigin};

/// File name of the local audit trail.
pub const AUDIT_FILE: &str = "remote-audit.jsonl";

/// One remote action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditRecord {
    /// When it happened.
    pub at: DateTime<Utc>,
    /// `secure` or `relay`.
    pub path: String,
    /// Connect device id, when the relay supplied it.
    pub device_id: Option<String>,
    /// Authenticated Noise static key, hex.
    pub device_key: Option<String>,
    /// Action name, e.g. `thread.created`.
    pub action: String,
    /// Target type, e.g. `thread`.
    pub target_type: Option<String>,
    /// Target id.
    pub target_id: Option<String>,
    /// Structured metadata.
    pub metadata: serde_json::Value,
}

/// Local-first audit trail that mirrors to connect when online.
pub struct RemoteAudit {
    data_dir: PathBuf,
    client: Option<Arc<ConnectClient>>,
    devices: Arc<DeviceRegistry>,
}

impl RemoteAudit {
    /// Creates a trail rooted at `data_dir`.
    pub fn new(
        data_dir: impl Into<PathBuf>,
        client: Arc<ConnectClient>,
        devices: Arc<DeviceRegistry>,
    ) -> Self {
        Self {
            data_dir: data_dir.into(),
            client: Some(client),
            devices,
        }
    }

    /// Creates a local-only trail (used by tests and by a signed-out daemon).
    pub fn local(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            client: None,
            devices: Arc::new(DeviceRegistry::default()),
        }
    }

    /// Path of the audit file.
    pub fn path(&self) -> PathBuf {
        self.data_dir.join(AUDIT_FILE)
    }

    /// The paired-device registry this audit trail attributes with.
    pub fn devices(&self) -> &DeviceRegistry {
        &self.devices
    }

    /// The connect device id whose Noise key is `key`, when known.

    pub fn device_id_for_key(&self, key: &[u8; h4rn_transport::keys::KEY_LEN]) -> Option<String> {
        self.devices.device_for_key(key)
    }

    /// Records one remote action.
    pub fn record(
        &self,
        origin: &SessionOrigin,
        action: impl Into<String>,
        target_type: Option<&str>,
        target_id: Option<&str>,
        metadata: serde_json::Value,
    ) {
        if !origin.is_remote() {
            return;
        }
        let device_id = origin.device_id.clone().or_else(|| {
            origin
                .device_key
                .and_then(|key| self.devices.device_for_key(&key))
        });
        let record = AuditRecord {
            at: Utc::now(),
            path: origin.path.as_str().to_owned(),
            device_id,
            device_key: origin.device_key.map(hex::encode),
            action: action.into(),
            target_type: target_type.map(ToOwned::to_owned),
            target_id: target_id.map(ToOwned::to_owned),
            metadata,
        };
        if let Err(error) = self.append(&record) {
            tracing::warn!(%error, "could not append the remote audit record");
        }
        self.mirror(record);
    }

    fn append(&self, record: &AuditRecord) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path())?;
        let line = serde_json::to_string(record).unwrap_or_else(|_| "{}".to_owned());
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()
    }

    fn mirror(&self, record: AuditRecord) {
        let Some(client) = self.client.clone() else {
            return;
        };
        // Best-effort: the local file is the durable record. A send failure is
        // logged by the client and never blocks the protocol loop.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            let request = AuditIngestRequest {
                action: record.action,
                target_type: record.target_type,
                target_id: record.target_id,
                metadata: record.metadata,
                at: Some(record.at),
            };
            if let Err(error) = client.post_audit(request).await {
                tracing::debug!(%error, "could not mirror the audit record to connect");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use h4rn_transport::keys::KEY_LEN;

    #[test]
    fn local_sessions_are_not_audited_but_remote_ones_are() {
        let dir = tempfile::tempdir().unwrap();
        let audit = RemoteAudit::local(dir.path());
        audit.record(
            &SessionOrigin::local(),
            "thread.created",
            Some("thread"),
            Some("t1"),
            serde_json::json!({}),
        );
        assert!(!audit.path().exists());

        let origin = SessionOrigin::relay(Some("device-1".into()), Some([9u8; KEY_LEN]));
        audit.record(
            &origin,
            "terminal.opened",
            Some("terminal"),
            Some("term-1"),
            serde_json::json!({"cols": 80}),
        );
        let body = std::fs::read_to_string(audit.path()).unwrap();
        let record: AuditRecord = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(record.device_id.as_deref(), Some("device-1"));
        assert_eq!(record.path, "relay");
        assert_eq!(record.action, "terminal.opened");
    }
}
