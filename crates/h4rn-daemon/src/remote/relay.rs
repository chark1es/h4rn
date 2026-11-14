//! The authenticated relay connector and its transport `Link` adapter.
//!
//! The daemon holds one WebSocket to `h4rn-connect` for its whole lifetime. The
//! relay speaks JSON `RelayFrame`s with base64 payloads while the transport
//! speaks binary `HR` packets, so this module is the adapter: an `open` frame
//! starts a [`crate::server::serve_secure_link`] task, `data` frames are pushed
//! into that task's inbound queue, and the task's outgoing packets become
//! `data` frames with the same `conn_id`.
//!
//! The relay is untrusted: it routes ciphertext and can lie about a device id,
//! but Noise still authenticates the client's static key. Device events are
//! used only to maintain the allowlist, never to bypass a handshake.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt as _, StreamExt as _};
use h4rn_protocol::{ConnectDevice, RelayFrame, RelayFrameKind, RelayOpen, RelayServerEvent};
use h4rn_transport::keys::KEY_LEN;
use h4rn_transport::server::{Link, LinkError, SecureService};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::registry::{ConnectEntry, DeviceRegistry};
use super::{ConnectClient, SessionOrigin};
use crate::server::{AppState, serve_secure_link};

/// How often the daemon pings the relay to keep presence warm.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Reconnect backoff ceiling.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// In-flight relay connections, keyed by `conn_id`.
type Connections = Arc<Mutex<HashMap<String, mpsc::Sender<Vec<u8>>>>>;

/// Runs the relay connector forever, reconnecting with exponential backoff.
///
/// The daemon aborts this task on shutdown. A failed reconnect is expected on a
/// laptop that sleeps or changes networks; sessions stay resumable in
/// [`SecureService`] while the relay is away.
pub async fn run_relay(
    state: AppState,
    service: Arc<SecureService>,
    client: Arc<ConnectClient>,
    registry: Arc<DeviceRegistry>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match run_once(&state, &service, &client, &registry).await {
            Ok(()) => {
                tracing::info!("relay connection closed; reconnecting");
                backoff = Duration::from_secs(1);
            }
            Err(error) => {
                tracing::warn!(%error, "relay connection failed; backing off");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn run_once(
    state: &AppState,
    service: &Arc<SecureService>,
    client: &Arc<ConnectClient>,
    registry: &Arc<DeviceRegistry>,
) -> anyhow::Result<()> {
    let url = client.relay_websocket_url().await?;
    let (socket, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .map_err(|error| anyhow::anyhow!("connect to the relay: {error}"))?;
    tracing::info!(machine = ?client.machine_id(), "relay connected");
    let (mut sink, mut stream) = socket.split();
    let connections: Connections = Arc::new(Mutex::new(HashMap::new()));
    let (out_tx, mut out_rx) = mpsc::channel::<RelayFrame>(256);
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            outgoing = out_rx.recv() => {
                let Some(frame) = outgoing else { return Ok(()); };
                let text = serde_json::to_string(&frame)?;
                sink.send(Message::Text(text.into()))
                    .await
                    .map_err(|error| anyhow::anyhow!("relay write failed: {error}"))?;
            }
            incoming = stream.next() => {
                let Some(message) = incoming else { return Ok(()); };
                match message {
                    Ok(Message::Text(text)) => {
                        let event: RelayServerEvent = match serde_json::from_str(&text) {
                            Ok(event) => event,
                            Err(error) => {
                                tracing::warn!(%error, "ignoring a malformed relay event");
                                continue;
                            }
                        };
                        handle_event(event, state, service, client, registry, &connections, &out_tx).await?;
                    }
                    Ok(Message::Close(_)) | Err(_) => return Ok(()),
                    // Control frames are handled by the WebSocket layer.
                    Ok(_) => {}
                }
            }
            _ = ping.tick() => {
                let frame = RelayFrame {
                    conn_id: String::new(),
                    kind: RelayFrameKind::Ping,
                    payload: Vec::new(),
                };
                let text = serde_json::to_string(&frame)?;
                sink.send(Message::Text(text.into()))
                    .await
                    .map_err(|error| anyhow::anyhow!("relay ping failed: {error}"))?;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_event(
    event: RelayServerEvent,
    state: &AppState,
    service: &Arc<SecureService>,
    client: &Arc<ConnectClient>,
    registry: &Arc<DeviceRegistry>,
    connections: &Connections,
    out_tx: &mpsc::Sender<RelayFrame>,
) -> anyhow::Result<()> {
    match event {
        RelayServerEvent::Frame { frame } => match frame.kind {
            RelayFrameKind::Open => {
                let open: RelayOpen = serde_json::from_slice(&frame.payload)
                    .map_err(|_| anyhow::anyhow!("relay open payload is not RelayOpen JSON"))?;
                if Some(open.machine_id.as_str()) != client.machine_id().as_deref() {
                    tracing::warn!(
                        machine = %open.machine_id,
                        "relay opened a session for a machine this daemon does not own"
                    );
                    return Ok(());
                }
                let (in_tx, in_rx) = mpsc::channel::<Vec<u8>>(256);
                connections
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(frame.conn_id.clone(), in_tx);
                // The relay's device id is a claim. The session's identity is
                // the Noise key authenticated by the handshake, resolved in
                // `serve_secure_link`; a disagreeing claim is discarded there.
                let origin = SessionOrigin::relay(open.device_id, None);
                let link = RelayLink {
                    conn_id: frame.conn_id,
                    inbound: in_rx,
                    outbound: out_tx.clone(),
                    connections: Arc::clone(connections),
                };
                let service = Arc::clone(service);
                let state = state.clone();
                tokio::spawn(async move {
                    serve_secure_link(service, Box::new(link), state, origin).await;
                });
            }
            RelayFrameKind::Data => {
                let sender = connections
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&frame.conn_id)
                    .cloned();
                if let Some(sender) = sender
                    && sender.send(frame.payload).await.is_err()
                {
                    tracing::debug!(conn = %frame.conn_id, "relay session already closed");
                }
            }
            RelayFrameKind::Close => {
                connections
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&frame.conn_id);
            }
            RelayFrameKind::Ping => {}
        },
        RelayServerEvent::Snapshot { devices, .. } => {
            apply_snapshot(service, registry, &devices);
        }
        RelayServerEvent::DevicePaired { device, .. } => {
            register_device(service, registry, &device);
        }
        RelayServerEvent::DeviceRevoked { device_id, .. } => {
            if let Some(key) = registry.remove(&device_id) {
                service.revoke(key);
                tracing::info!(device = %device_id, "revoked a paired device");
            }
        }
        RelayServerEvent::Presence { presence } => {
            tracing::debug!(
                machine = %presence.machine_id,
                online = presence.online,
                "relay presence update"
            );
        }
        RelayServerEvent::Error { conn_id, error } => {
            tracing::warn!(
                conn = ?conn_id,
                code = %error.code,
                message = %error.message,
                "relay reported an error"
            );
        }
    }
    Ok(())
}

/// Makes a connect snapshot authoritative for connect-managed devices.
///
/// A snapshot is the full list of devices paired with this machine, so any
/// connect-managed key missing from it was revoked while the daemon was
/// offline: it leaves the registry, the transport allowlist and every live
/// session. Direct pairings and peer daemons are not connect's to revoke.
fn apply_snapshot(service: &SecureService, registry: &DeviceRegistry, devices: &[ConnectDevice]) {
    let entries: Vec<ConnectEntry> = devices.iter().filter_map(connect_entry).collect();
    let removed = registry.reconcile_connect(&entries);
    for entry in &entries {
        service.allow(entry.key);
    }
    for key in removed {
        service.revoke(key);
        tracing::info!(key = %hex::encode(key), "revoked a device missing from the connect snapshot");
    }
}

/// Parses a connect device into a registry entry, skipping malformed keys.
fn connect_entry(device: &ConnectDevice) -> Option<ConnectEntry> {
    let encoded = device.noise_static_public_key.as_deref()?;
    let Some(key) = decode_key(encoded) else {
        tracing::warn!(device = %device.id, "paired device has a malformed Noise key");
        return None;
    };
    Some(ConnectEntry {
        device_id: device.id.clone(),
        key,
        scopes: device.scopes.clone(),
    })
}

/// Adds a paired device's Noise key to the allowlist and the registry.
///
/// A device the registry has not seen before is also queued for short
/// authentication string confirmation: a typed-code pairing hands the phone its
/// daemon key through the relay, and this is how the person detects a swap.
fn register_device(service: &SecureService, registry: &DeviceRegistry, device: &ConnectDevice) {
    let Some(entry) = connect_entry(device) else {
        return;
    };
    let is_new = registry.authorization(&entry.key).is_none();
    registry.insert_scoped(entry.device_id.clone(), entry.key, entry.scopes);
    service.allow(entry.key);
    if is_new {
        let daemon_key = service.public_identity().noise_public;
        registry.note_confirmation(&entry.device_id, &device.name, &entry.key, &daemon_key);
        tracing::info!(
            device = %device.name,
            sas = %h4rn_transport::pairing::short_auth_string(&entry.key, &daemon_key),
            "new device paired: confirm this code matches the one on its screen"
        );
    }
}

fn decode_key(value: &str) -> Option<[u8; KEY_LEN]> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value.trim())
        .ok()?;
    bytes.as_slice().try_into().ok()
}

/// One relay connection's view as a transport [`Link`].
struct RelayLink {
    conn_id: String,
    inbound: mpsc::Receiver<Vec<u8>>,
    outbound: mpsc::Sender<RelayFrame>,
    connections: Connections,
}

#[async_trait]
impl Link for RelayLink {
    async fn send_packet(&mut self, packet: Vec<u8>) -> Result<(), LinkError> {
        self.outbound
            .send(RelayFrame::data(self.conn_id.clone(), packet))
            .await
            .map_err(|_| LinkError::Closed)
    }

    async fn recv_packet(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
        Ok(self.inbound.recv().await)
    }
}

impl Drop for RelayLink {
    fn drop(&mut self) {
        self.connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.conn_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use h4rn_protocol::PairScope;

    fn device(id: &str, key: u8, scopes: Vec<PairScope>) -> ConnectDevice {
        use base64::Engine as _;
        ConnectDevice {
            id: id.into(),
            kind: h4rn_protocol::ConnectDeviceKind::Mobile,
            name: id.into(),
            machine_id: None,
            proof_public_key: String::new(),
            noise_static_public_key: Some(
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([key; KEY_LEN]),
            ),
            platform: None,
            created_at: chrono::Utc::now(),
            last_seen: None,
            revoked_at: None,
            scopes,
        }
    }

    #[test]
    fn a_reconnect_snapshot_revokes_devices_missing_from_it() {
        let service = SecureService::new(h4rn_transport::server::SecureOptions::new(
            h4rn_transport::keys::DeviceIdentity::generate().unwrap(),
        ));
        let registry = DeviceRegistry::in_memory();
        // Both were paired before the daemon went offline; `gone` was then
        // revoked, and a direct pairing exists alongside.
        apply_snapshot(
            &service,
            &registry,
            &[
                device("kept", 1, vec![PairScope::View]),
                device("gone", 2, vec![PairScope::View]),
            ],
        );
        registry.insert_direct([3u8; KEY_LEN], PairScope::ALL.to_vec());
        service.allow([3u8; KEY_LEN]);
        apply_snapshot(
            &service,
            &registry,
            &[device("kept", 1, vec![PairScope::View, PairScope::Approve])],
        );
        assert!(
            !service.is_allowed(&[2u8; KEY_LEN]),
            "revoked key still allowed"
        );
        assert!(registry.authorization(&[2u8; KEY_LEN]).is_none());
        assert!(service.is_allowed(&[1u8; KEY_LEN]));
        assert_eq!(
            registry.scopes_for_key(&[1u8; KEY_LEN]),
            Some(vec![PairScope::View, PairScope::Approve]),
            "scopes follow the latest snapshot"
        );
        assert!(
            service.is_allowed(&[3u8; KEY_LEN]),
            "direct pairing was dropped"
        );
        assert!(registry.authorization(&[3u8; KEY_LEN]).is_some());
    }

    #[test]
    fn a_new_device_is_queued_for_sas_confirmation_with_the_daemons_own_key() {
        let identity = h4rn_transport::keys::DeviceIdentity::generate().unwrap();
        let daemon_key = identity.public().noise_public;
        let service = SecureService::new(h4rn_transport::server::SecureOptions::new(identity));
        let registry = DeviceRegistry::in_memory();
        register_device(&service, &registry, &device("d1", 7, vec![PairScope::View]));
        let pending = registry.pending_confirmations();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].sas,
            h4rn_transport::pairing::short_auth_string(&[7u8; KEY_LEN], &daemon_key)
        );
        // Seeing the same device again (a snapshot) does not re-queue it.
        register_device(&service, &registry, &device("d1", 7, vec![PairScope::View]));
        assert_eq!(registry.pending_confirmations().len(), 1);
        // A revoked device is no longer listed.
        registry.remove("d1");
        assert!(registry.pending_confirmations().is_empty());
    }

    #[test]
    fn register_device_records_scopes_and_allows_the_key() {
        let service = SecureService::new(h4rn_transport::server::SecureOptions::new(
            h4rn_transport::keys::DeviceIdentity::generate().unwrap(),
        ));
        let registry = DeviceRegistry::in_memory();
        register_device(&service, &registry, &device("d1", 3, vec![PairScope::View]));
        assert!(service.is_allowed(&[3u8; KEY_LEN]));
        assert_eq!(
            registry.scopes_for_key(&[3u8; KEY_LEN]),
            Some(vec![PairScope::View])
        );
    }
}
