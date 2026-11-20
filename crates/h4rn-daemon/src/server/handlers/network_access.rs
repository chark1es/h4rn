//! Network access and direct pairing requests.
//!
//! The daemon owns the listener, so these requests are the only supported way
//! to turn it on and open a pairing window. Turning it off drops the listener
//! without a restart; the persisted setting is what a headless `h4rnd serve`
//! and the desktop both read.

use h4rn_protocol::{ClientRequest, ProtoError, ResponseBody};
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

fn snapshot(state: NetworkAccessSnapshot) -> ResponseBody {
    ResponseBody::NetworkAccess {
        enabled: state.enabled,
        listening: state.listening,
        port: state.port,
        addresses: state.addresses,
        error: state.error,
    }
}
