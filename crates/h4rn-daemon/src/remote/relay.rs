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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt as _, StreamExt as _};
use h4rn_protocol::{
    ConnectDevice, RelayFrame, RelayFrameKind, RelayOpen, RelayServerEvent, RelayService,
};
use h4rn_transport::keys::KEY_LEN;
use h4rn_transport::relay::{
    RelayError, RelayFrame as LinkFrame, RelayLink as TransportRelayLink, RelayRequest, RelayRole,
};
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

/// What a relay session's inbound queue carries.
enum RelayInbound {
    /// Ciphertext for the Noise session.
    Data(Vec<u8>),
    /// The relay rejected the session (for example `machine_forbidden`).
    Error(String),
}

/// Guest exchanges whose body is still arriving in chunks.
#[derive(Default)]
struct GuestExchanges {
    pending: HashMap<String, PendingGuest>,
    /// Requests refused before their body was read; later chunks are dropped.
    rejected: std::collections::HashSet<String>,
}

struct PendingGuest {
    request: h4rn_protocol::GuestHttpRequest,
    body: Vec<u8>,
    next_seq: u64,
    /// When the first frame arrived, so an abandoned upload cannot pin memory.
    started: std::time::Instant,
}

/// How long a guest body may take to finish before it is dropped.
const GUEST_UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

impl GuestExchanges {
    /// Drops uploads whose browser went away before the last frame.
    fn expire(&mut self) {
        self.pending
            .retain(|_, guest| guest.started.elapsed() < GUEST_UPLOAD_TIMEOUT);
    }
}

/// In-flight relay connections, keyed by `conn_id`. It holds both sessions
/// this daemon serves (inbound) and sessions it dialled through the same
/// authenticated relay socket (outbound).
type Connections = Arc<Mutex<HashMap<String, mpsc::Sender<RelayInbound>>>>;

/// A handle for opening peer MCP sessions over the daemon's one relay socket.
///
/// h4rn-connect keys a relay peer by device id, so a second WebSocket
/// authenticated as this daemon would evict its control connection. Peer
/// dialling therefore multiplexes new `conn_id`s through the socket that
/// [`run_relay`] already owns. When the relay is offline the dialer reports
/// unreachable and the multipath engine retries later.
#[derive(Default)]
pub struct RelayDialer {
    state: Mutex<Option<DialerState>>,
    next: AtomicU64,
}

impl std::fmt::Debug for RelayDialer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RelayDialer")
            .field("connected", &self.is_connected())
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct DialerState {
    outbound: mpsc::Sender<RelayFrame>,
    connections: Connections,
}

impl RelayDialer {
    /// Creates a dialer with no socket yet.
    pub fn new() -> Self {
        Self::default()
    }

    fn attach(&self, outbound: mpsc::Sender<RelayFrame>, connections: Connections) {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(DialerState {
            outbound,
            connections,
        });
    }

    /// Clears the dialer only when it still points at this socket.
    fn detach(&self, connections: &Connections) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let same = state
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(&current.connections, connections));
        if same {
            *state = None;
        }
    }

    /// Whether the daemon's relay socket is currently attached.
    pub fn is_connected(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// Opens one peer MCP session to `machine_id` over the shared socket.
    pub async fn dial(
        &self,
        machine_id: &str,
        service: RelayService,
        request: RelayRequest,
    ) -> Result<Box<dyn TransportRelayLink>, RelayError> {
        let Some(state) = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return Err(RelayError::Unreachable(
                "the daemon's relay socket is not connected".into(),
            ));
        };
        let session_id = request.session_id;
        let conn_id = format!(
            "peer-{}-{}",
            hex::encode(session_id),
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let (in_tx, in_rx) = mpsc::channel(256);
        state
            .connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(conn_id.clone(), in_tx);
        let frame = RelayFrame::open_service(conn_id.clone(), machine_id, service);
        if state.outbound.send(frame).await.is_err() {
            state
                .connections
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&conn_id);
            return Err(RelayError::Closed);
        }
        Ok(Box::new(RelayPathLink {
            conn_id,
            session_id,
            inbound: in_rx,
            outbound: state.outbound,
            connections: state.connections,
            _role: request.role,
        }))
    }
}

/// The client side of one multiplexed peer session.
struct RelayPathLink {
    conn_id: String,
    session_id: [u8; h4rn_transport::wire::SESSION_ID_LEN],
    inbound: mpsc::Receiver<RelayInbound>,
    outbound: mpsc::Sender<RelayFrame>,
    connections: Connections,
    _role: RelayRole,
}

impl std::fmt::Debug for RelayPathLink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RelayPathLink")
            .field("conn_id", &self.conn_id)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl TransportRelayLink for RelayPathLink {
    async fn send(&mut self, frame: LinkFrame) -> Result<(), RelayError> {
        let frame = match frame {
            LinkFrame::Register { .. } => return Ok(()),
            LinkFrame::Data { packet, .. } => RelayFrame::data(self.conn_id.clone(), packet),
            LinkFrame::Close { .. } => RelayFrame::close(self.conn_id.clone()),
        };
        self.outbound
            .send(frame)
            .await
            .map_err(|_| RelayError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<LinkFrame>, RelayError> {
        match self.inbound.recv().await {
            Some(RelayInbound::Data(packet)) => Ok(Some(LinkFrame::Data {
                session_id: self.session_id,
                packet,
            })),
            Some(RelayInbound::Error(message)) => Err(RelayError::Rejected(message)),
            None => Ok(None),
        }
    }

    async fn close(&mut self) -> Result<(), RelayError> {
        let _ = self
            .outbound
            .try_send(RelayFrame::close(self.conn_id.clone()));
        Ok(())
    }
}

impl Drop for RelayPathLink {
    fn drop(&mut self) {
        self.connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.conn_id);
        let _ = self
            .outbound
            .try_send(RelayFrame::close(self.conn_id.clone()));
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
    dialer: Arc<RelayDialer>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match run_once(&state, &service, &client, &registry, &dialer).await {
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
    dialer: &Arc<RelayDialer>,
) -> anyhow::Result<()> {
    let url = client.relay_websocket_url().await?;
    let (socket, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .map_err(|error| anyhow::anyhow!("connect to the relay: {error}"))?;
    tracing::info!(machine = ?client.machine_id(), "relay connected");
    let (mut sink, mut stream) = socket.split();
    let connections: Connections = Arc::new(Mutex::new(HashMap::new()));
    let (out_tx, mut out_rx) = mpsc::channel::<RelayFrame>(256);
    dialer.attach(out_tx.clone(), Arc::clone(&connections));
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut guests = GuestExchanges::default();

    let outcome = loop {
        let step: anyhow::Result<bool> = async {
            tokio::select! {
                outgoing = out_rx.recv() => {
                    let Some(frame) = outgoing else { return Ok(false); };
                    let text = serde_json::to_string(&frame)?;
                    sink.send(Message::Text(text.into()))
                        .await
                        .map_err(|error| anyhow::anyhow!("relay write failed: {error}"))?;
                }
                incoming = stream.next() => {
                    let Some(message) = incoming else { return Ok(false); };
                    match message {
                        Ok(Message::Text(text)) => {
                            let event: RelayServerEvent = match serde_json::from_str(&text) {
                                Ok(event) => event,
                                Err(error) => {
                                    tracing::warn!(%error, "ignoring a malformed relay event");
                                    return Ok(true);
                                }
                            };
                            handle_event(event, state, service, client, registry, &connections, &out_tx, &mut guests).await?;
                        }
                        Ok(Message::Close(_)) | Err(_) => return Ok(false),
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
            Ok(true)
        }
        .await;
        match step {
            Ok(true) => {}
            Ok(false) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    dialer.detach(&connections);
    outcome
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
    guests: &mut GuestExchanges,
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
                let (in_tx, in_rx) = mpsc::channel::<RelayInbound>(256);
                connections
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(frame.conn_id.clone(), in_tx);
                // The relay's device id is a claim. The session's identity is
                // the Noise key authenticated by the handshake; a disagreeing
                // claim is discarded by the registry check.
                let origin = SessionOrigin::relay(open.device_id, None);
                let link = RelayLink {
                    conn_id: frame.conn_id,
                    inbound: in_rx,
                    outbound: out_tx.clone(),
                    connections: Arc::clone(connections),
                };
                let service = Arc::clone(service);
                let state = state.clone();
                match open.service {
                    RelayService::Secure => {
                        tokio::spawn(async move {
                            serve_secure_link(service, Box::new(link), state, origin).await;
                        });
                    }
                    RelayService::SecureMcp => {
                        // Peer-only: `serve_secure_mcp` resolves the caller's
                        // machine from the device registry and refuses a user
                        // device key before any tunnel frame is read.
                        let mcp = Arc::clone(&state.mcp);
                        tokio::spawn(async move {
                            mcp.serve_secure_mcp(service, Box::new(link)).await;
                        });
                    }
                }
            }
            RelayFrameKind::Data => {
                let sender = connections
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&frame.conn_id)
                    .cloned();
                if let Some(sender) = sender
                    && sender
                        .send(RelayInbound::Data(frame.payload))
                        .await
                        .is_err()
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
            RelayFrameKind::GuestHttp => {
                let request: h4rn_protocol::GuestHttpRequest =
                    match serde_json::from_slice(&frame.payload) {
                        Ok(request) => request,
                        Err(_) => {
                            tracing::warn!("relay sent a guest HTTP frame that is not JSON");
                            return Ok(());
                        }
                    };
                let request_id = request.request_id.clone();
                if guests.rejected.contains(&request_id) {
                    return Ok(());
                }
                if request.chunked {
                    // Check the link before any chunk of the body arrives, so
                    // an unknown token never makes the daemon buffer a file.
                    let method = guest_method(&request.method);
                    if method == crate::guest::Method::Post {
                        let input = crate::server::guest_input(
                            method,
                            request.path.clone(),
                            request.public_prefix.clone(),
                            &guest_headers(&request),
                            Vec::new(),
                        );
                        if let Err(output) = state.guest.check_submission(&input).await {
                            guests.rejected.insert(request_id.clone());
                            send_guest_answer(out_tx, &frame.conn_id, &request_id, output).await;
                            return Ok(());
                        }
                    }
                    guests.pending.insert(
                        request_id,
                        PendingGuest {
                            request,
                            body: Vec::new(),
                            next_seq: 0,
                            started: std::time::Instant::now(),
                        },
                    );
                    return Ok(());
                }
                let (status, headers, body) = guest_answer(
                    state,
                    &request.method,
                    request.path.clone(),
                    request.public_prefix.clone(),
                    &guest_headers(&request),
                    request.body,
                )
                .await;
                send_guest_reply(out_tx, &frame.conn_id, &request_id, status, headers, body).await;
            }
            RelayFrameKind::GuestHttpChunk => {
                let chunk: h4rn_protocol::GuestHttpChunk =
                    match serde_json::from_slice(&frame.payload) {
                        Ok(chunk) => chunk,
                        Err(_) => {
                            tracing::warn!("relay sent a guest chunk that is not JSON");
                            return Ok(());
                        }
                    };
                if guests.rejected.contains(&chunk.request_id) {
                    if chunk.last {
                        guests.rejected.remove(&chunk.request_id);
                    }
                    return Ok(());
                }
                let Some(pending) = guests.pending.get_mut(&chunk.request_id) else {
                    return Ok(());
                };
                if chunk.seq != pending.next_seq {
                    tracing::warn!("relay sent a guest body out of order");
                    guests.pending.remove(&chunk.request_id);
                    return Ok(());
                }
                pending.next_seq += 1;
                if pending.body.len().saturating_add(chunk.body.len())
                    > h4rn_protocol::GUEST_MAX_EXCHANGE_BYTES
                {
                    let request_id = chunk.request_id.clone();
                    guests.pending.remove(&request_id);
                    guests.rejected.insert(request_id.clone());
                    let output = crate::guest::GuestHttpOutput::text(
                        413,
                        "That message is over the 20 MB guest limit.",
                    );
                    send_guest_answer(out_tx, &frame.conn_id, &request_id, output).await;
                    return Ok(());
                }
                pending.body.extend_from_slice(&chunk.body);
                if !chunk.last {
                    return Ok(());
                }
                let Some(pending) = guests.pending.remove(&chunk.request_id) else {
                    return Ok(());
                };
                let method = pending.request.method.clone();
                let headers = guest_headers(&pending.request);
                let path = pending.request.path;
                let public_prefix = pending.request.public_prefix;
                let (status, headers, body) =
                    guest_answer(state, &method, path, public_prefix, &headers, pending.body).await;
                send_guest_reply(
                    out_tx,
                    &frame.conn_id,
                    &chunk.request_id,
                    status,
                    headers,
                    body,
                )
                .await;
            }
            RelayFrameKind::Ping => {
                // The relay echoes the daemon's ping; use the quiet moment to
                // drop uploads a browser abandoned.
                guests.expire();
            }
        },
        RelayServerEvent::Snapshot { devices, .. } => {
            apply_snapshot(service, registry, &devices);
            registry.topology().notify_one();
        }
        RelayServerEvent::DevicePaired { device, .. } => {
            register_device(service, registry, &device);
            registry.topology().notify_one();
        }
        RelayServerEvent::DeviceRevoked { device_id, .. } => {
            let (key, persisted) = registry.remove(&device_id);
            if let Some(key) = key {
                service.revoke(key);
                tracing::info!(device = %device_id, "revoked a paired device");
            }
            if let Err(error) = persisted {
                tracing::warn!(device = %device_id, %error, "the device revocation could not be saved");
            }
            registry.topology().notify_one();
        }
        RelayServerEvent::Presence { presence } => {
            registry.topology().notify_one();
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
            // A dialled peer session is waiting on this error: surface it as a
            // rejected path so the multipath engine can settle the attempt.
            if let Some(conn_id) = conn_id {
                let sender = connections
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&conn_id)
                    .cloned();
                if let Some(sender) = sender {
                    let _ = sender
                        .send(RelayInbound::Error(format!(
                            "{}: {}",
                            error.code, error.message
                        )))
                        .await;
                }
            }
        }
    }
    Ok(())
}

/// Maps the wire method to the guest surface's method.
fn guest_method(method: &str) -> crate::guest::Method {
    if method.eq_ignore_ascii_case("post") {
        crate::guest::Method::Post
    } else {
        crate::guest::Method::Get
    }
}

/// Rebuilds the forwarded request headers for the guest surface.
fn guest_headers(request: &h4rn_protocol::GuestHttpRequest) -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    for (name, value) in &request.headers {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(name.as_str()),
            axum::http::HeaderValue::try_from(value.as_str()),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

/// Runs one guest exchange and returns its parts.
async fn guest_answer(
    state: &AppState,
    method: &str,
    path: String,
    public_prefix: Option<String>,
    headers: &axum::http::HeaderMap,
    body: Vec<u8>,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let response = crate::server::guest_exchange(
        state,
        guest_method(method),
        path,
        public_prefix,
        headers,
        body,
    )
    .await;
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, h4rn_protocol::GUEST_MAX_EXCHANGE_BYTES)
        .await
        .map(|bytes| bytes.to_vec())
        .unwrap_or_default();
    let response_headers = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect();
    (parts.status.as_u16(), response_headers, bytes)
}

/// Sends a pre-built guest error answer as one frame.
async fn send_guest_answer(
    out_tx: &mpsc::Sender<RelayFrame>,
    conn_id: &str,
    request_id: &str,
    output: crate::guest::GuestHttpOutput,
) {
    let mut headers = vec![("content-type".to_owned(), output.content_type.to_owned())];
    headers.extend(output.headers);
    send_guest_reply(
        out_tx,
        conn_id,
        request_id,
        output.status,
        headers,
        output.body,
    )
    .await;
}

/// Sends one guest answer, chunked when the body is large.
async fn send_guest_reply(
    out_tx: &mpsc::Sender<RelayFrame>,
    conn_id: &str,
    request_id: &str,
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) {
    let chunked = body.len() > h4rn_protocol::GUEST_CHUNK_BYTES;
    let reply = h4rn_protocol::GuestHttpResponse {
        request_id: request_id.to_owned(),
        status,
        chunked,
        headers,
        body: if chunked { Vec::new() } else { body.clone() },
    };
    let Ok(payload) = serde_json::to_vec(&reply) else {
        tracing::warn!("could not serialise a guest answer");
        return;
    };
    let _ = out_tx
        .send(RelayFrame {
            conn_id: conn_id.to_owned(),
            kind: RelayFrameKind::GuestHttp,
            payload,
        })
        .await;
    if !chunked {
        return;
    }
    let chunks: Vec<&[u8]> = body.chunks(h4rn_protocol::GUEST_CHUNK_BYTES).collect();
    let total = chunks.len();
    for (index, chunk) in chunks.into_iter().enumerate() {
        let frame = h4rn_protocol::GuestHttpChunk {
            request_id: request_id.to_owned(),
            seq: index as u64,
            last: index + 1 == total,
            body: chunk.to_vec(),
        };
        let Ok(payload) = serde_json::to_vec(&frame) else {
            continue;
        };
        let _ = out_tx
            .send(RelayFrame {
                conn_id: conn_id.to_owned(),
                kind: RelayFrameKind::GuestHttpChunk,
                payload,
            })
            .await;
    }
}

/// Makes a connect snapshot authoritative for connect-managed devices.
///
/// A snapshot is the full list of devices paired with this machine, so any
/// connect-managed key missing from it was revoked while the daemon was
/// offline: it leaves the registry, the transport allowlist and every live
/// session. Direct pairings and peer daemons are not connect's to revoke.
fn apply_snapshot(service: &SecureService, registry: &DeviceRegistry, devices: &[ConnectDevice]) {
    let entries: Vec<ConnectEntry> = devices.iter().filter_map(connect_entry).collect();
    let (removed, persisted) = registry.reconcile_connect(&entries);
    if let Err(error) = persisted {
        tracing::warn!(%error, "the connect snapshot could not be persisted");
    }
    for entry in &entries {
        // A snapshot is authoritative for scopes, never for admission: a
        // pending device stays off the allowlist until the host confirms it.
        if registry.is_admitted(&entry.key) {
            service.allow(entry.key);
        }
    }
    let daemon_key = service.public_identity().noise_public;
    for device in devices {
        let Some(entry) = connect_entry(device) else {
            continue;
        };
        if registry.is_pending(&entry.key) {
            // Rebuild the host's confirmation card after a restart: the key is
            // persisted but the in-memory SAS list is not.
            registry.note_confirmation(&entry.device_id, &device.name, &entry.key, &daemon_key);
        }
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

/// Records a newly paired device from the relay.
///
/// The key is stored but *not* admitted: a relay that substituted a device key
/// must not receive the scopes before a person compares the short
/// authentication string and confirms the pairing on the host. Only
/// [`DeviceRegistry::decide_pending`] may promote it.
fn register_device(service: &SecureService, registry: &DeviceRegistry, device: &ConnectDevice) {
    let Some(entry) = connect_entry(device) else {
        return;
    };
    if let Err(error) = registry.provision_scoped(entry.device_id.clone(), entry.key, entry.scopes)
    {
        // The key was not recorded, so it cannot be confirmed or admitted. A
        // later snapshot retries the write.
        tracing::warn!(device = %device.name, %error, "could not record a relay pairing");
        return;
    }
    if registry.is_admitted(&entry.key) {
        // An already-confirmed device reappearing (a re-pair event) is admitted
        // as before; only a pending one waits.
        service.allow(entry.key);
        return;
    }
    let daemon_key = service.public_identity().noise_public;
    registry.note_confirmation(&entry.device_id, &device.name, &entry.key, &daemon_key);
    tracing::info!(
        device = %device.name,
        sas = %h4rn_transport::pairing::short_auth_string(&entry.key, &daemon_key),
        "new device paired: waiting for the host to confirm the code"
    );
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
    inbound: mpsc::Receiver<RelayInbound>,
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
        match self.inbound.recv().await {
            Some(RelayInbound::Data(packet)) => Ok(Some(packet)),
            Some(RelayInbound::Error(message)) => {
                tracing::debug!(conn = %self.conn_id, %message, "relay rejected a session");
                Ok(None)
            }
            None => Ok(None),
        }
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
        // The host confirmed `kept` while both were present.
        let kept = registry.key_for_device("kept").expect("kept key");
        registry.decide_pending("kept", true).1.expect("persist");
        service.allow(kept);
        registry
            .insert_direct([3u8; KEY_LEN], PairScope::ALL.to_vec())
            .expect("persist");
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
    fn a_new_device_stays_pending_until_the_host_confirms() {
        // Regression for E4: registering a relay key must not grant scopes or
        // admit it on the transport before a person compares the SAS.
        let identity = h4rn_transport::keys::DeviceIdentity::generate().unwrap();
        let daemon_key = identity.public().noise_public;
        let service = SecureService::new(h4rn_transport::server::SecureOptions::new(identity));
        let registry = DeviceRegistry::in_memory();
        register_device(&service, &registry, &device("d1", 7, vec![PairScope::View]));
        assert!(!service.is_allowed(&[7u8; KEY_LEN]));
        assert!(registry.is_pending(&[7u8; KEY_LEN]));
        assert!(registry.authorization(&[7u8; KEY_LEN]).is_none());
        assert!(registry.scopes_for_key(&[7u8; KEY_LEN]).is_none());
        let pending = registry.pending_confirmations();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].sas,
            h4rn_transport::pairing::short_auth_string(&[7u8; KEY_LEN], &daemon_key)
        );

        // A snapshot that keeps listing it must never promote it.
        apply_snapshot(
            &service,
            &registry,
            &[device("d1", 7, vec![PairScope::View])],
        );
        assert!(registry.is_pending(&[7u8; KEY_LEN]));
        assert!(!service.is_allowed(&[7u8; KEY_LEN]));

        // Only the host's decision admits it.
        let (key, confirm) = registry
            .decide_pending("d1", true)
            .0
            .expect("the pending pairing");
        assert_eq!(key, [7u8; KEY_LEN]);
        assert!(confirm);
        service.allow(key);
        assert!(service.is_allowed(&[7u8; KEY_LEN]));
        assert_eq!(
            registry.scopes_for_key(&[7u8; KEY_LEN]),
            Some(vec![PairScope::View])
        );
        assert!(registry.pending_confirmations().is_empty());

        // A revoked device is no longer listed.
        registry.remove("d1").1.expect("persist");
        assert!(registry.pending_confirmations().is_empty());
    }

    #[test]
    fn rejecting_a_pending_device_removes_and_revokes_it() {
        let service = SecureService::new(h4rn_transport::server::SecureOptions::new(
            h4rn_transport::keys::DeviceIdentity::generate().unwrap(),
        ));
        let registry = DeviceRegistry::in_memory();
        register_device(&service, &registry, &device("d1", 3, vec![PairScope::View]));
        let (key, confirm) = registry
            .decide_pending("d1", false)
            .0
            .expect("the pending pairing");
        assert!(!confirm);
        service.revoke(key);
        assert!(registry.authorization(&[3u8; KEY_LEN]).is_none());
        assert!(!service.is_allowed(&[3u8; KEY_LEN]));
        assert!(registry.pending_confirmations().is_empty());
        assert!(
            registry.decide_pending("d1", true).0.is_none(),
            "already decided"
        );
    }
}
