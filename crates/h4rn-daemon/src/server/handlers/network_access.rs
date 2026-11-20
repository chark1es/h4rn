//! Network access and direct pairing requests.
//!
//! The daemon owns the listener, so these requests are the only supported way
//! to turn it on and open a pairing window. Turning it off drops the listener
//! without a restart; the persisted setting is what a headless `h4rnd serve`
//! and the desktop both read.

use h4rn_protocol::{ClientRequest, DirectDevice, ProtoError, ProtoErrorCode, ResponseBody};
use h4rn_transport::keys::KEY_LEN;
use tokio::task::JoinHandle;

use crate::network_access::NetworkAccessSnapshot;
use crate::server::Connection;

/// Requests owned by this service.
pub(super) fn matches(request: &ClientRequest) -> bool {
    matches!(
        request,
        ClientRequest::GetNetworkAccess
            | ClientRequest::SetNetworkAccess { .. }
            | ClientRequest::CreateDirectPairing { .. }
            | ClientRequest::CancelDirectPairing
            | ClientRequest::ListDirectDevices
            | ClientRequest::RevokeDirectDevice { .. }
    )
}

/// Handles one network access or direct pairing request.
pub(super) async fn handle(
    conn: &Connection,
    request: ClientRequest,
) -> Result<ResponseBody, ProtoError> {
    match request {
        ClientRequest::GetNetworkAccess => Ok(snapshot(conn.state.network.snapshot())),
        ClientRequest::SetNetworkAccess { enabled } => {
            let state = conn.state.network.set_enabled(enabled).await?;
            conn.audit(
                "network_access.changed",
                Some("setting"),
                Some("network_access"),
                serde_json::json!({
                    "enabled": state.enabled,
                    "listening": state.listening,
                    "port": state.port,
                }),
            );
            Ok(snapshot(state))
        }
        ClientRequest::CreateDirectPairing { scopes, label } => {
            let offer = conn.state.network.create_direct_pairing(scopes, label)?;
            // Never audit the URI: it carries the one-time secret.
            conn.audit(
                "pairing.direct_created",
                Some("device"),
                None,
                serde_json::json!({
                    "scopes": offer.scopes.iter().map(|scope| scope.as_str()).collect::<Vec<_>>(),
                    "expires_at": offer.expires_at,
                    "addresses": offer.addresses,
                }),
            );
            Ok(ResponseBody::DirectPairingOffer {
                uri: offer.uri,
                expires_at: offer.expires_at,
                addresses: offer.addresses,
                scopes: offer.scopes,
            })
        }
        ClientRequest::CancelDirectPairing => {
            conn.state.network.cancel_direct_pairing();
            conn.audit(
                "pairing.direct_cancelled",
                Some("device"),
                None,
                serde_json::json!({}),
            );
            Ok(ResponseBody::Ack)
        }
        ClientRequest::ListDirectDevices => Ok(direct_devices(conn)),
        ClientRequest::RevokeDirectDevice { key } => {
            let bytes: [u8; KEY_LEN] = hex::decode(key.trim())
                .ok()
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or_else(|| {
                    ProtoError::new(
                        ProtoErrorCode::InvalidRequest,
                        "the device key is not a 32-byte hex key",
                        "Refresh the paired devices list and try again",
                    )
                })?;
            let (removed, persisted) = conn.state.devices.remove_direct(&bytes);
            if !removed {
                return Err(ProtoError::new(
                    ProtoErrorCode::NotFound,
                    "no device paired directly with that key",
                    "Refresh the paired devices list; it may already be removed",
                ));
            }
            // Drop it from the transport allowlist and end its live sessions
            // even when the registry write failed: the removal is in memory.
            if let Some(service) = &conn.state.secure {
                service.revoke(bytes);
            }
            conn.audit(
                "pairing.direct_revoked",
                Some("device"),
                None,
                serde_json::json!({"key": hex::encode(bytes)}),
            );
            persisted.map_err(|error| {
                ProtoError::new(
                    ProtoErrorCode::Internal,
                    format!("the removal could not be saved: {error}"),
                    "Free disk space or fix the data directory, then try again. The device stays blocked until the daemon restarts.",
                )
            })?;
            Ok(direct_devices(conn))
        }
        other => Err(crate::server::not_implemented(&other)),
    }
}

/// Spawns this service's transient-event forwarder.
///
/// Network access changes are request/response only: there is no background
/// stream a client has to follow, so this connection has nothing to forward.
pub(super) fn spawn(_conn: &Connection) -> JoinHandle<()> {
    tokio::spawn(async {})
}

/// The directly paired devices, with whether each is connected now.
fn direct_devices(conn: &Connection) -> ResponseBody {
    let connected = conn
        .state
        .secure
        .as_ref()
        .map(|service| service.connected_devices())
        .unwrap_or_default();
    let devices = conn
        .state
        .devices
        .direct_devices()
        .into_iter()
        .map(|entry| {
            let is_connected = hex::decode(&entry.key)
                .ok()
                .is_some_and(|bytes| connected.iter().any(|key| key.as_slice() == bytes));
            DirectDevice {
                key: entry.key,
                scopes: entry.scopes,
                paired_at: entry.paired_at,
                connected: is_connected,
            }
        })
        .collect();
    ResponseBody::DirectDevices { devices }
}

fn snapshot(state: NetworkAccessSnapshot) -> ResponseBody {
    ResponseBody::NetworkAccess {
        enabled: state.enabled,
        listening: state.listening,
        port: state.port,
        addresses: state.addresses,
        error: state.error,
    }
}
