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
pub mod peer_relay;
pub mod peers;
pub mod registry;
pub mod relay;
pub mod tailscale;

use h4rn_transport::keys::KEY_LEN;

pub use audit::{AuditRecord, RemoteAudit};
pub use connect::{ConnectClient, DeviceGrantSummary, LoginOutcome};
pub use credentials::Credentials;
pub use network::{advertised_addresses, local_ipv4};
pub use pairing::{DirectPairingFile, render_qr};
pub use peer_relay::PeerRelayConnector;
pub use registry::{Authorization, ConnectEntry, DeviceRegistry, Provenance, Role};
pub use relay::{RelayDialer, run_relay};
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
        devices: std::sync::Arc<DeviceRegistry>,
    ) -> anyhow::Result<Option<std::sync::Arc<Self>>> {
        let data_dir = data_dir.into();
        let client = std::sync::Arc::new(ConnectClient::new(data_dir.clone(), identity)?);
        if !client.is_logged_in() {
            return Ok(None);
        }
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

/// Checks that the device behind `origin` may send `request`.
///
/// See [`authorize_scope`].
pub fn authorize_scopes(
    devices: &DeviceRegistry,
    origin: &SessionOrigin,
    request: &h4rn_protocol::ClientRequest,
) -> Result<(), h4rn_protocol::ProtoError> {
    authorize_scope(devices, origin, request.required_scope())
}

/// Scope gate shared by text requests, binary frames and their tests.
///
/// Only a loopback session passes unconditionally. A remote session is
/// identified by the Noise key its handshake authenticated; a relay-supplied
/// device id is never consulted. A key without a registry entry, or one that is
/// a peer daemon rather than a client device, is denied: authorisation that is
/// missing is never assumed.
pub fn authorize_scope(
    devices: &DeviceRegistry,
    origin: &SessionOrigin,
    needed: h4rn_protocol::PairScope,
) -> Result<(), h4rn_protocol::ProtoError> {
    use h4rn_protocol::{ProtoError, ProtoErrorCode};
    if !origin.is_remote() {
        return Ok(());
    }
    let Some(key) = origin.device_key else {
        return Err(ProtoError::new(
            ProtoErrorCode::Forbidden,
            "this session has no authenticated device key",
            "Pair the device again",
        ));
    };
    let Some(scopes) = devices.scopes_for_key(&key) else {
        return Err(ProtoError::new(
            ProtoErrorCode::Forbidden,
            "this device has no recorded authorisation on this daemon",
            "Pair the device again; a pairing must record what the device may do",
        ));
    };
    if scopes.contains(&needed) {
        return Ok(());
    }
    Err(ProtoError::new(
        ProtoErrorCode::Forbidden,
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
        let devices = DeviceRegistry::in_memory();
        devices.insert_scoped("phone", [1u8; KEY_LEN], vec![PairScope::View]);
        let origin = SessionOrigin::secure([1u8; KEY_LEN]);
        let denied = authorize_scopes(&devices, &origin, &approve()).unwrap_err();
        assert_eq!(denied.code, ProtoErrorCode::Forbidden);
        assert!(denied.message.contains("approve"), "{}", denied.message);
        assert!(authorize_scopes(&devices, &origin, &ClientRequest::ListHosts).is_ok());
    }

    #[test]
    fn granted_scopes_pass_and_others_stay_blocked() {
        let devices = DeviceRegistry::in_memory();
        devices.insert_scoped(
            "phone",
            [1u8; KEY_LEN],
            vec![PairScope::View, PairScope::Approve],
        );
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
    fn a_key_without_a_registry_entry_is_denied() {
        // Previously a failed lookup granted unrestricted access.
        let devices = DeviceRegistry::in_memory();
        assert!(authorize_scopes(&devices, &SessionOrigin::local(), &approve()).is_ok());
        let unknown = SessionOrigin::secure([9u8; KEY_LEN]);
        assert_eq!(
            authorize_scopes(&devices, &unknown, &ClientRequest::ListHosts)
                .unwrap_err()
                .code,
            ProtoErrorCode::Forbidden
        );
        // A remote session that somehow lacks a key is denied too.
        let keyless = SessionOrigin::relay(Some("phone".into()), None);
        assert!(authorize_scopes(&devices, &keyless, &ClientRequest::ListHosts).is_err());
    }

    #[test]
    fn a_relay_claimed_device_id_cannot_borrow_another_devices_scopes() {
        let devices = DeviceRegistry::in_memory();
        devices.insert_scoped("admin", [1u8; KEY_LEN], PairScope::ALL.to_vec());
        devices.insert_scoped("viewer", [2u8; KEY_LEN], vec![PairScope::View]);
        // The viewer's handshake key, but the relay claims the admin's id.
        let origin = SessionOrigin::relay(Some("admin".into()), Some([2u8; KEY_LEN]));
        assert!(authorize_scopes(&devices, &origin, &approve()).is_err());
    }

    #[test]
    fn a_peer_daemon_key_is_not_a_client_device() {
        let devices = DeviceRegistry::in_memory();
        devices.register_host(h4rn_protocol::HostId::new("b"), [4u8; KEY_LEN], None);
        let origin = SessionOrigin::secure([4u8; KEY_LEN]);
        assert!(authorize_scopes(&devices, &origin, &ClientRequest::ListHosts).is_err());
    }

    #[test]
    fn a_direct_pairing_carries_the_armed_scopes() {
        let devices = DeviceRegistry::in_memory();
        devices.arm_direct(vec![PairScope::View]);
        devices.complete_direct([5u8; KEY_LEN]);
        let origin = SessionOrigin::secure([5u8; KEY_LEN]);
        assert!(authorize_scopes(&devices, &origin, &approve()).is_err());
        assert!(authorize_scopes(&devices, &origin, &ClientRequest::ListHosts).is_ok());
        let auth = devices.authorization(&[5u8; KEY_LEN]).unwrap();
        assert_eq!(auth.provenance, Provenance::Direct);
    }

    #[test]
    fn an_unarmed_direct_pairing_gets_every_scope() {
        let devices = DeviceRegistry::in_memory();
        devices.complete_direct([5u8; KEY_LEN]);
        let origin = SessionOrigin::secure([5u8; KEY_LEN]);
        assert!(authorize_scopes(&devices, &origin, &approve()).is_ok());
    }

    #[test]
    fn re_pairing_replaces_scopes() {
        let devices = DeviceRegistry::in_memory();
        devices.insert_scoped("d", [3u8; KEY_LEN], vec![PairScope::View]);
        let origin = SessionOrigin::secure([3u8; KEY_LEN]);
        assert!(authorize_scopes(&devices, &origin, &approve()).is_err());
        devices.insert_scoped(
            "d",
            [3u8; KEY_LEN],
            vec![PairScope::View, PairScope::Approve],
        );
        assert!(authorize_scopes(&devices, &origin, &approve()).is_ok());
    }

    #[test]
    fn the_registry_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("devices.json");
        let devices = DeviceRegistry::open(&path);
        devices.insert_scoped("phone", [1u8; KEY_LEN], vec![PairScope::View]);
        devices.arm_direct(vec![PairScope::View, PairScope::Files]);
        devices.complete_direct([2u8; KEY_LEN]);
        drop(devices);
        let reopened = DeviceRegistry::open(&path);
        assert_eq!(
            reopened.scopes_for_key(&[1u8; KEY_LEN]),
            Some(vec![PairScope::View])
        );
        assert_eq!(
            reopened.scopes_for_key(&[2u8; KEY_LEN]),
            Some(vec![PairScope::View, PairScope::Files])
        );
        assert_eq!(reopened.key_for_device("phone"), Some([1u8; KEY_LEN]));
    }

    #[test]
    fn a_corrupt_registry_denies_everyone_instead_of_opening_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("devices.json");
        std::fs::write(&path, b"not a registry").unwrap();
        let devices = DeviceRegistry::open(&path);
        assert!(devices.is_empty());
    }

    #[test]
    fn a_connect_snapshot_is_authoritative_for_connect_devices_only() {
        let devices = DeviceRegistry::in_memory();
        devices.insert_scoped("old", [1u8; KEY_LEN], vec![PairScope::View]);
        devices.insert_scoped("kept", [2u8; KEY_LEN], vec![PairScope::View]);
        devices.insert_direct([3u8; KEY_LEN], PairScope::ALL.to_vec());
        devices.register_host(h4rn_protocol::HostId::new("b"), [4u8; KEY_LEN], None);
        let removed = devices.reconcile_connect(&[ConnectEntry {
            device_id: "kept".into(),
            key: [2u8; KEY_LEN],
            scopes: vec![PairScope::View, PairScope::Approve],
        }]);
        assert_eq!(removed, vec![[1u8; KEY_LEN]]);
        assert!(devices.authorization(&[1u8; KEY_LEN]).is_none());
        assert_eq!(
            devices.scopes_for_key(&[2u8; KEY_LEN]),
            Some(vec![PairScope::View, PairScope::Approve])
        );
        assert!(devices.authorization(&[3u8; KEY_LEN]).is_some());
        assert!(devices.authorization(&[4u8; KEY_LEN]).is_some());
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
