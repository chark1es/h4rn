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
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt as _, StreamExt as _};
use h4rn_protocol::{
    ConnectDevice, PairScope, RelayFrame, RelayFrameKind, RelayOpen, RelayServerEvent,
};
use h4rn_transport::keys::KEY_LEN;
use h4rn_transport::server::{Link, LinkError, SecureService};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::{ConnectClient, SessionOrigin};
use crate::server::{AppState, serve_secure_link};

/// How often the daemon pings the relay to keep presence warm.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Reconnect backoff ceiling.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// In-flight relay connections, keyed by `conn_id`.
type Connections = Arc<Mutex<HashMap<String, mpsc::Sender<Vec<u8>>>>>;

/// Paired devices learned from the relay, keyed by connect device id.
///
/// The Noise key is what the transport allowlist and audit trail actually use;
/// the device id is the human-facing connect id.
#[derive(Debug, Default)]
pub struct DeviceRegistry {
    devices: RwLock<HashMap<String, RegisteredDevice>>,
}

#[derive(Debug, Clone)]
struct RegisteredDevice {
    key: [u8; KEY_LEN],
    /// Empty means unrestricted (see `ConnectDevice::scopes`).
    scopes: Vec<PairScope>,
}

impl DeviceRegistry {
    /// Registers a device and its Noise key with no scope restriction.
    pub fn insert(&self, device_id: impl Into<String>, noise_key: [u8; KEY_LEN]) {
        self.insert_scoped(device_id, noise_key, Vec::new());
    }

    /// Registers a device with the scopes its pairing granted. Pairing again
    /// replaces the previous scopes.
    pub fn insert_scoped(
        &self,
        device_id: impl Into<String>,
        noise_key: [u8; KEY_LEN],
        scopes: Vec<PairScope>,
    ) {
        self.devices
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                device_id.into(),
                RegisteredDevice {
                    key: noise_key,
                    scopes,
                },
            );
    }

    /// The scopes restricting a device, or `None` when it is unrestricted or
    /// unknown (a direct pairing never passes through connect).
    pub fn scopes_for(&self, device_id: &str) -> Option<Vec<PairScope>> {
        self.devices
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(device_id)
            .filter(|device| !device.scopes.is_empty())
            .map(|device| device.scopes.clone())
    }

    /// Removes a device, returning the Noise key that can be revoked.
    pub fn remove(&self, device_id: &str) -> Option<[u8; KEY_LEN]> {
        self.devices
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(device_id)
            .map(|device| device.key)
    }

    /// The device id owning `key`, for audit attribution.
    pub fn device_for_key(&self, key: &[u8; KEY_LEN]) -> Option<String> {
        self.devices
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|(_, candidate)| candidate.key == *key)
            .map(|(device_id, _)| device_id.clone())
    }

    /// The Noise key for a device id.
    pub fn key_for_device(&self, device_id: &str) -> Option<[u8; KEY_LEN]> {
        self.devices
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(device_id)
            .map(|device| device.key)
    }

    /// Number of known devices.
    pub fn len(&self) -> usize {
        self.devices
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

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
                let device_key = open
                    .device_id
                    .as_deref()
                    .and_then(|device_id| registry.key_for_device(device_id));
                let origin = SessionOrigin::relay(open.device_id, device_key);
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
            for device in devices {
                register_device(service, registry, &device);
            }
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

/// Adds a paired device's Noise key to the allowlist.
fn register_device(service: &SecureService, registry: &DeviceRegistry, device: &ConnectDevice) {
    let Some(encoded) = device.noise_static_public_key.as_deref() else {
        return;
    };
    let Some(key) = decode_key(encoded) else {
        tracing::warn!(device = %device.id, "paired device has a malformed Noise key");
        return;
    };
    registry.insert_scoped(device.id.clone(), key, device.scopes.clone());
    service.allow(key);
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

    #[test]
    fn the_registry_maps_both_directions() {
        let registry = DeviceRegistry::default();
        registry.insert("device-1", [3u8; KEY_LEN]);
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.key_for_device("device-1"), Some([3u8; KEY_LEN]));
        assert_eq!(
            registry.device_for_key(&[3u8; KEY_LEN]),
            Some("device-1".into())
        );
        assert_eq!(registry.remove("device-1"), Some([3u8; KEY_LEN]));
        assert!(registry.is_empty());
    }
}
