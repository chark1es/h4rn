//! Remote access glue: connect login, the authenticated relay connector,
//! pairing, Tailscale detection and the remote-action audit trail.
//!
//! Everything here is daemon-side. The UI remains a local client; these modules
//! make the same daemon reachable from another device over the h4rn transport
//! (ADR-009, ADR-010, ADR-018). The relay is a dumb, untrusted byte forwarder:
//! Noise authenticates both ends on every path.

pub mod audit;
pub mod connect;
pub mod credentials;
pub mod network;
pub mod pairing;
pub mod peers;
pub mod relay;
pub mod tailscale;

use h4rn_transport::keys::KEY_LEN;

pub use audit::{AuditRecord, RemoteAudit};
pub use connect::{ConnectClient, DeviceGrantSummary, LoginOutcome};
pub use credentials::Credentials;
pub use network::{advertised_addresses, local_ipv4};
pub use pairing::{DirectPairingFile, render_qr};
pub use relay::{DeviceRegistry, run_relay};
pub use tailscale::TailscaleMode;

/// Which physical path a daemon session arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginPath {
    /// The loopback `/ws` endpoint, authenticated by the local access token.
    Local,
    /// A Noise session dialled directly (LAN or tailnet) or through the relay.
    Secure,
    /// A Noise session whose outer frames arrived through h4rn-connect.
    Relay,
}

impl OriginPath {
    /// Stable spelling used in the audit log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Secure => "secure",
            Self::Relay => "relay",
        }
    }
}

/// Who opened a daemon session.
///
/// A local session is the user at the machine; a remote session is identified
/// by its authenticated Noise static key and, when the relay told us, by the
/// connect device id. This is what the audit trail records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionOrigin {
    /// Physical path.
    pub path: OriginPath,
    /// Connect device id, when known.
    pub device_id: Option<String>,
    /// Authenticated device X25519 static key, when remote.
    pub device_key: Option<[u8; KEY_LEN]>,
}

impl SessionOrigin {
    /// A loopback session.
    pub fn local() -> Self {
        Self {
            path: OriginPath::Local,
            device_id: None,
            device_key: None,
        }
    }

    /// A direct Noise session.
    pub fn secure(device_key: [u8; KEY_LEN]) -> Self {
        Self {
            path: OriginPath::Secure,
            device_id: None,
            device_key: Some(device_key),
        }
    }

    /// A relayed Noise session; the relay supplies the device id.
    pub fn relay(device_id: Option<String>, device_key: Option<[u8; KEY_LEN]>) -> Self {
        Self {
            path: OriginPath::Relay,
            device_id,
            device_key,
        }
    }

    /// Whether this session came from another device.
    pub fn is_remote(&self) -> bool {
        self.path != OriginPath::Local
    }

    /// A short label for logs and audit entries.
    pub fn label(&self) -> String {
        match (&self.device_id, &self.device_key) {
            (Some(id), _) => format!("{}:{}", self.path.as_str(), id),
            (None, Some(key)) => format!("{}:{}", self.path.as_str(), hex::encode(key)),
            (None, None) => self.path.as_str().to_owned(),
        }
    }
}

/// Shared remote state: the connect client, the audit trail and the device map.
pub struct RemoteState {
    /// Authenticated connect session and API client.
    pub client: std::sync::Arc<ConnectClient>,
    /// Local-first audit trail mirrored to connect.
    pub audit: std::sync::Arc<RemoteAudit>,
    /// Paired devices learned from the relay snapshot and events.
    pub devices: std::sync::Arc<DeviceRegistry>,
}

impl RemoteState {
    /// Loads the connect session, if the daemon is signed in.
    pub fn open(
        data_dir: impl Into<std::path::PathBuf>,
        identity: h4rn_transport::keys::DeviceIdentity,
    ) -> anyhow::Result<Option<std::sync::Arc<Self>>> {
        let data_dir = data_dir.into();
        let client = std::sync::Arc::new(ConnectClient::new(data_dir.clone(), identity)?);
        if !client.is_logged_in() {
            return Ok(None);
        }
        let devices = std::sync::Arc::new(DeviceRegistry::default());
        let audit = std::sync::Arc::new(RemoteAudit::new(
            data_dir,
            std::sync::Arc::clone(&client),
            std::sync::Arc::clone(&devices),
        ));
        Ok(Some(std::sync::Arc::new(Self {
            client,
            audit,
            devices,
        })))
    }
}

impl audit::RemoteAudit {
    /// Checks that the device behind `origin` may send `request`.
    ///
    /// Local sessions and devices without a recorded restriction (direct
    /// pairings, links made before scopes existed) pass. A device paired
    /// through connect holds exactly the scopes its pairing granted, read live
    /// so a re-pairing takes effect on the next request.
    pub fn authorize(
        &self,
        origin: &SessionOrigin,
        request: &h4rn_protocol::ClientRequest,
    ) -> Result<(), h4rn_protocol::ProtoError> {
        authorize_scopes(self.devices(), origin, request)
    }
}

/// Scope gate shared by [`RemoteState::authorize`] and its tests.
pub fn authorize_scopes(
    devices: &DeviceRegistry,
    origin: &SessionOrigin,
    request: &h4rn_protocol::ClientRequest,
) -> Result<(), h4rn_protocol::ProtoError> {
    if !origin.is_remote() {
        return Ok(());
    }
    let device_id = origin.device_id.clone().or_else(|| {
        origin
            .device_key
            .as_ref()
            .and_then(|key| devices.device_for_key(key))
    });
    let Some(scopes) = device_id.and_then(|id| devices.scopes_for(&id)) else {
        return Ok(());
    };
    let needed = request.required_scope();
    if scopes.contains(&needed) {
        return Ok(());
    }
    Err(h4rn_protocol::ProtoError::new(
        h4rn_protocol::ProtoErrorCode::Forbidden,
        format!(
            "this device was paired without the `{}` permission",
            needed.as_str()
        ),
        "Pair the device again and enable that permission in Settings → Devices",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use h4rn_protocol::{ApprovalDecision, ClientRequest, PairScope, ProtoErrorCode, ThreadId};

    fn approve() -> ClientRequest {
        ClientRequest::RespondApproval {
            thread_id: ThreadId::new("t1"),
            approval_id: "a1".into(),
            decision: ApprovalDecision::Approve,
            option_id: None,
        }
    }

    #[test]
    fn a_view_only_device_cannot_approve_but_can_read() {
        let devices = DeviceRegistry::default();
        devices.insert_scoped("phone", [1u8; KEY_LEN], vec![PairScope::View]);
        let origin = SessionOrigin::relay(Some("phone".into()), Some([1u8; KEY_LEN]));
        let denied = authorize_scopes(&devices, &origin, &approve()).unwrap_err();
        assert_eq!(denied.code, ProtoErrorCode::Forbidden);
        assert!(denied.message.contains("approve"), "{}", denied.message);
        assert!(authorize_scopes(&devices, &origin, &ClientRequest::ListHosts).is_ok());
    }

    #[test]
    fn granted_scopes_pass_and_others_stay_blocked() {
        let devices = DeviceRegistry::default();
        devices.insert_scoped(
            "phone",
            [1u8; KEY_LEN],
            vec![PairScope::View, PairScope::Approve],
        );
        // Resolved by key when the relay did not name the device.
        let origin = SessionOrigin::secure([1u8; KEY_LEN]);
        assert!(authorize_scopes(&devices, &origin, &approve()).is_ok());
        let terminal = ClientRequest::TerminalList { thread_id: None };
        assert_eq!(
            authorize_scopes(&devices, &origin, &terminal)
                .unwrap_err()
                .code,
            ProtoErrorCode::Forbidden
        );
    }

    #[test]
    fn local_and_unrestricted_origins_are_not_gated() {
        let devices = DeviceRegistry::default();
        assert!(authorize_scopes(&devices, &SessionOrigin::local(), &approve()).is_ok());
        devices.insert("legacy", [2u8; KEY_LEN]);
        let legacy = SessionOrigin::relay(Some("legacy".into()), None);
        assert!(authorize_scopes(&devices, &legacy, &approve()).is_ok());
        let unknown = SessionOrigin::secure([9u8; KEY_LEN]);
        assert!(authorize_scopes(&devices, &unknown, &approve()).is_ok());
    }

    #[test]
    fn re_pairing_replaces_scopes() {
        let devices = DeviceRegistry::default();
        devices.insert_scoped("d", [3u8; KEY_LEN], vec![PairScope::View]);
        let origin = SessionOrigin::relay(Some("d".into()), None);
        assert!(authorize_scopes(&devices, &origin, &approve()).is_err());
        devices.insert_scoped(
            "d",
            [3u8; KEY_LEN],
            vec![PairScope::View, PairScope::Approve],
        );
        assert!(authorize_scopes(&devices, &origin, &approve()).is_ok());
    }

    #[test]
    fn origins_are_labelled_unambiguously() {
        assert!(!SessionOrigin::local().is_remote());
        let secure = SessionOrigin::secure([7u8; KEY_LEN]);
        assert!(secure.is_remote());
        assert!(secure.label().starts_with("secure:"));
        let relay = SessionOrigin::relay(Some("device-1".into()), None);
        assert_eq!(relay.label(), "relay:device-1");
    }
}
