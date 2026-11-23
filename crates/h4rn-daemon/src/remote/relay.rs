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

/// How long a guest body may take to finish before it is dropped.
const GUEST_UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// How many guest exchanges may use the surface at once.
///
/// A guest upload, a snapshot and a blob read are all bounded work, but they
/// must never queue behind each other in the relay loop. The cap also bounds
/// how many uploads may hold buffered bytes, so the relay's memory follows the
/// same limit as its concurrent work.
const GUEST_EXCHANGE_CONCURRENCY: usize = 4;

/// How much of an upload may be buffered before its link is authorized.
///
/// A chunked submission's body starts arriving while the token and link check
/// runs. Only a couple of chunks can precede a completed check, so this caps
/// what an unauthorized or abandoned upload can pin before it is refused;
/// after authorization the exchange's own 21 MB limit applies.
const GUEST_PREAUTH_BYTES: usize = 4 * h4rn_protocol::GUEST_CHUNK_BYTES;

/// One guest body that is still arriving in chunks.
struct PendingGuest {
    conn_id: String,
    request: h4rn_protocol::GuestHttpRequest,
    body: Vec<u8>,
    next_seq: u64,
    /// When the first frame arrived, so an abandoned upload cannot pin memory.
    started: std::time::Instant,
    /// A submission may only be buffered once `check_submission` accepted its
    /// link; until then chunks wait here.
    checking: bool,
    /// The last chunk arrived while the check was still running.
    complete: bool,
}

#[derive(Default)]
struct GuestState {
    pending: HashMap<String, PendingGuest>,
    /// Request ids refused before their body was read, with the time they were
    /// refused, so later chunks are dropped and the set cannot grow forever.
    rejected: HashMap<String, std::time::Instant>,
}

/// A guest HTTP surface, separate from `AppState` so the relay framing can be
/// tested with a fake surface and no daemon.
#[async_trait]
trait GuestSurface: Send + Sync {
    /// Refuses a submission before its body is buffered, when it can.
    async fn check_submission(
        &self,
        input: &crate::guest::GuestHttpInput,
    ) -> Result<(), crate::guest::GuestHttpOutput>;
    /// Answers one exchange.
    async fn exchange(&self, input: crate::guest::GuestHttpInput) -> crate::guest::GuestHttpOutput;
}

#[async_trait]
impl GuestSurface for AppState {
    async fn check_submission(
        &self,
        input: &crate::guest::GuestHttpInput,
    ) -> Result<(), crate::guest::GuestHttpOutput> {
        self.guest.check_submission(input).await
    }

    async fn exchange(&self, input: crate::guest::GuestHttpInput) -> crate::guest::GuestHttpOutput {
        self.guest.handle(input).await
    }
}

/// Answers guest frames for one relay connection.
///
/// Every exchange runs in its own task with its own `out_tx` clone and a
/// concurrency permit, so a large chunked answer, a snapshot or a 10 MB fsync
/// can never block the loop that drains the shared relay socket.
#[derive(Clone)]
struct GuestRelay {
    surface: Arc<dyn GuestSurface>,
    out_tx: mpsc::Sender<RelayFrame>,
    state: Arc<Mutex<GuestState>>,
    permits: Arc<tokio::sync::Semaphore>,
}

impl GuestRelay {
    fn new(surface: Arc<dyn GuestSurface>, out_tx: mpsc::Sender<RelayFrame>) -> Self {
        Self {
            surface,
            out_tx,
            state: Arc::new(Mutex::new(GuestState::default())),
            permits: Arc::new(tokio::sync::Semaphore::new(GUEST_EXCHANGE_CONCURRENCY)),
        }
    }

    /// Handles one `GuestHttp` frame without waiting on anything.
    fn dispatch(&self, frame: &RelayFrame) {
        let request: h4rn_protocol::GuestHttpRequest = match serde_json::from_slice(&frame.payload)
        {
            Ok(request) => request,
            Err(_) => {
                tracing::warn!("relay sent a guest HTTP frame that is not JSON");
                return;
            }
        };
        let request_id = request.request_id.clone();
        if self.lock().rejected.contains_key(&request_id) {
            return;
        }
        if request.chunked {
            let checking = guest_method(&request.method) == crate::guest::Method::Post;
            let mut state = self.lock();
            if checking && state.pending.len() >= GUEST_EXCHANGE_CONCURRENCY {
                // Every buffer slot is taken, so accepting another upload would
                // grow memory past the cap the permits already promise.
                drop(state);
                self.refuse_busy(frame.conn_id.clone(), request_id);
                return;
            }
            state.pending.insert(
                request_id.clone(),
                PendingGuest {
                    conn_id: frame.conn_id.clone(),
                    request,
                    body: Vec::new(),
                    next_seq: 0,
                    started: std::time::Instant::now(),
                    checking,
                    complete: false,
                },
            );
            drop(state);
            if checking {
                self.spawn_check(request_id);
            }
            return;
        }
        // Check the link before any part of a body is buffered.
        self.spawn_exchange(frame.conn_id.clone(), request, None);
    }

    /// Handles one `GuestHttpChunk` frame without waiting on anything.
    fn dispatch_chunk(&self, frame: &RelayFrame) {
        let chunk: h4rn_protocol::GuestHttpChunk = match serde_json::from_slice(&frame.payload) {
            Ok(chunk) => chunk,
            Err(_) => {
                tracing::warn!("relay sent a guest chunk that is not JSON");
                return;
            }
        };
        let mut state = self.lock();
        if state.rejected.contains_key(&chunk.request_id) {
            if chunk.last {
                state.rejected.remove(&chunk.request_id);
            }
            return;
        }
        let Some(pending) = state.pending.get_mut(&chunk.request_id) else {
            return;
        };
        if chunk.seq != pending.next_seq {
            tracing::warn!("relay sent a guest body out of order");
            state.pending.remove(&chunk.request_id);
            state
                .rejected
                .insert(chunk.request_id.clone(), std::time::Instant::now());
            return;
        }
        pending.next_seq += 1;
        // While the link check runs, only a bounded amount may accumulate: an
        // unauthorized upload must not be able to buffer its whole body before
        // it is refused. Once authorized, the exchange limit applies instead.
        let limit = if pending.checking {
            GUEST_PREAUTH_BYTES
        } else {
            h4rn_protocol::GUEST_MAX_EXCHANGE_BYTES
        };
        if pending.body.len().saturating_add(chunk.body.len()) > limit {
            state.pending.remove(&chunk.request_id);
            state
                .rejected
                .insert(chunk.request_id.clone(), std::time::Instant::now());
            self.reply_now(
                frame.conn_id.clone(),
                chunk.request_id.clone(),
                413,
                vec![(
                    "content-type".to_owned(),
                    "text/plain; charset=utf-8".to_owned(),
                )],
                b"That message is over the 20 MB guest limit.".to_vec(),
            );
            return;
        }
        pending.body.extend_from_slice(&chunk.body);
        if !chunk.last {
            return;
        }
        if pending.checking {
            pending.complete = true;
            return;
        }
        let Some(pending) = state.pending.remove(&chunk.request_id) else {
            return;
        };
        drop(state);
        self.spawn_exchange(pending.conn_id, pending.request, Some(pending.body));
    }

    /// Checks a chunked submission's link before its body is trusted.
    fn spawn_check(&self, request_id: String) {
        let relay = self.clone();
        let surface = Arc::clone(&self.surface);
        tokio::spawn(async move {
            let _permit = relay
                .permits
                .acquire()
                .await
                .expect("the guest exchange semaphore is never closed");
            let input = {
                let state = relay.lock();
                let Some(pending) = state.pending.get(&request_id) else {
                    return;
                };
                crate::server::guest_input(
                    guest_method(&pending.request.method),
                    pending.request.path.clone(),
                    pending.request.public_prefix.clone(),
                    &guest_headers(&pending.request),
                    Vec::new(),
                )
            };
            match surface.check_submission(&input).await {
                Ok(()) => {
                    let started = {
                        let mut state = relay.lock();
                        let Some(pending) = state.pending.get_mut(&request_id) else {
                            return;
                        };
                        pending.checking = false;
                        if !pending.complete {
                            return;
                        }
                        let Some(pending) = state.pending.remove(&request_id) else {
                            return;
                        };
                        Some((pending.conn_id, pending.request, pending.body))
                    };
                    if let Some((conn_id, request, body)) = started {
                        relay.spawn_exchange(conn_id, request, Some(body));
                    }
                }
                Err(output) => {
                    let conn_id = {
                        let mut state = relay.lock();
                        let conn_id = state
                            .pending
                            .remove(&request_id)
                            .map(|pending| pending.conn_id)
                            .unwrap_or_else(|| format!("guest-{request_id}"));
                        state
                            .rejected
                            .insert(request_id.clone(), std::time::Instant::now());
                        conn_id
                    };
                    relay.send_output(conn_id, request_id, &output).await;
                }
            }
        });
    }

    /// Runs one exchange in its own task, capped by the semaphore.
    fn spawn_exchange(
        &self,
        conn_id: String,
        request: h4rn_protocol::GuestHttpRequest,
        body: Option<Vec<u8>>,
    ) {
        let relay = self.clone();
        let surface = Arc::clone(&self.surface);
        let request_id = request.request_id.clone();
        tokio::spawn(async move {
            let _permit = relay
                .permits
                .acquire()
                .await
                .expect("the guest exchange semaphore is never closed");
            let headers = guest_headers(&request);
            let input = crate::server::guest_input(
                guest_method(&request.method),
                request.path,
                request.public_prefix,
                &headers,
                body.unwrap_or(request.body),
            );
            let output = surface.exchange(input).await;
            // The permit is held until the last frame is queued: a large
            // answer is not free simply because the surface call returned.
            relay.send_output(conn_id, request_id, &output).await;
        });
    }

    /// Sends a prepared answer as one frame, plus chunks when it is large.
    async fn send_output(
        &self,
        conn_id: String,
        request_id: String,
        output: &crate::guest::GuestHttpOutput,
    ) {
        let mut headers = vec![("content-type".to_owned(), output.content_type.to_owned())];
        headers.extend(output.headers.clone());
        // The cookie also rides as a header for a relay built before the reply
        // frame had its own field; a new relay prefers the field.
        if let Some(cookie) = &output.set_cookie {
            headers.push(("set-cookie".to_owned(), cookie.clone()));
        }
        self.queue_frames(
            conn_id,
            request_id,
            output.status,
            headers,
            output.body.clone(),
            output.set_cookie.clone(),
        )
        .await;
    }

    /// Refuses an upload that would overfill the buffer slots.
    fn refuse_busy(&self, conn_id: String, request_id: String) {
        self.lock()
            .rejected
            .insert(request_id.clone(), std::time::Instant::now());
        self.reply_now(
            conn_id,
            request_id,
            503,
            vec![(
                "content-type".to_owned(),
                "text/plain; charset=utf-8".to_owned(),
            )],
            b"This machine is busy with other guest uploads. Try again.".to_vec(),
        );
    }

    /// Queues one answer from the relay loop without waiting on the queue.
    ///
    /// The relay loop drains this same bounded channel, so it must never wait
    /// on it; the frames are produced in their own task.
    fn reply_now(
        &self,
        conn_id: String,
        request_id: String,
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) {
        let relay = self.clone();
        tokio::spawn(async move {
            relay
                .queue_frames(conn_id, request_id, status, headers, body, None)
                .await;
        });
    }

    /// Queues one answer's frames, in order.
    ///
    /// Awaiting this is safe from a spawned exchange: it waits on the shared
    /// outbound queue, never on the task that drains it.
    async fn queue_frames(
        &self,
        conn_id: String,
        request_id: String,
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        set_cookie: Option<String>,
    ) {
        let chunked = body.len() > h4rn_protocol::GUEST_CHUNK_BYTES;
        let reply = h4rn_protocol::GuestHttpResponse {
            request_id: request_id.clone(),
            status,
            chunked,
            headers,
            body: if chunked { Vec::new() } else { body.clone() },
            set_cookie,
        };
        let Ok(payload) = serde_json::to_vec(&reply) else {
            tracing::warn!("could not serialise a guest answer");
            return;
        };
        let _ = self
            .out_tx
            .send(RelayFrame {
                conn_id: conn_id.clone(),
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
                request_id: request_id.clone(),
                seq: index as u64,
                last: index + 1 == total,
                body: chunk.to_vec(),
            };
            let Ok(payload) = serde_json::to_vec(&frame) else {
                continue;
            };
            let _ = self
                .out_tx
                .send(RelayFrame {
                    conn_id: conn_id.clone(),
                    kind: RelayFrameKind::GuestHttpChunk,
                    payload,
                })
                .await;
        }
    }

    /// Drops uploads whose browser went away and old refusal markers.
    fn expire(&self) {
        let now = std::time::Instant::now();
        let mut state = self.lock();
        state
            .pending
            .retain(|_, guest| now.duration_since(guest.started) < GUEST_UPLOAD_TIMEOUT);
        state
            .rejected
            .retain(|_, at| now.duration_since(*at) < GUEST_UPLOAD_TIMEOUT);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GuestState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    // Guest frames never run on this loop: each exchange is a task with its
    // own queue clone and a concurrency permit.
    let guests = GuestRelay::new(Arc::new(state.clone()), out_tx.clone());

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
                            handle_event(event, state, service, client, registry, &connections, &out_tx, &guests).await?;
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
    guests: &GuestRelay,
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
            RelayFrameKind::GuestHttp | RelayFrameKind::GuestHttpChunk | RelayFrameKind::Ping => {
                handle_unblocked_frame(&frame, guests);
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

/// Handles the relay frames that must never wait on the relay loop's own queue.
///
/// `handle_event` routes every guest frame and every echoed ping here; the
/// tests below feed real wire frames through the same function.
fn handle_unblocked_frame(frame: &RelayFrame, guests: &GuestRelay) {
    match frame.kind {
        RelayFrameKind::GuestHttp => guests.dispatch(frame),
        RelayFrameKind::GuestHttpChunk => guests.dispatch_chunk(frame),
        RelayFrameKind::Ping => {
            // The relay echoes the daemon's ping; use the quiet moment to
            // drop uploads a browser abandoned.
            guests.expire();
        }
        _ => {}
    }
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use h4rn_protocol::PairScope;

    /// A guest surface with no daemon behind it.
    struct FakeSurface {
        refuse: bool,
        body: Vec<u8>,
        exchanges: Arc<AtomicUsize>,
        /// When set, the link check waits for it, so a test can fill an
        /// upload's pre-authorization buffer.
        check_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
        check_arrived: tokio::sync::Notify,
    }

    impl FakeSurface {
        fn new(body: Vec<u8>) -> Arc<Self> {
            Arc::new(Self {
                refuse: false,
                body,
                exchanges: Arc::new(AtomicUsize::new(0)),
                check_gate: Mutex::new(None),
                check_arrived: tokio::sync::Notify::new(),
            })
        }

        fn refusing() -> Arc<Self> {
            Arc::new(Self {
                refuse: true,
                body: Vec::new(),
                exchanges: Arc::new(AtomicUsize::new(0)),
                check_gate: Mutex::new(None),
                check_arrived: tokio::sync::Notify::new(),
            })
        }

        /// An unauthorized surface whose check waits at `gate`.
        fn refusing_at(gate: Arc<tokio::sync::Notify>) -> Arc<Self> {
            let surface = Self::refusing();
            *surface
                .check_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(gate);
            surface
        }
    }

    #[async_trait]
    impl GuestSurface for FakeSurface {
        async fn check_submission(
            &self,
            _input: &crate::guest::GuestHttpInput,
        ) -> Result<(), crate::guest::GuestHttpOutput> {
            let gate = self
                .check_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            if let Some(gate) = gate {
                self.check_arrived.notify_one();
                gate.notified().await;
            }
            if self.refuse {
                Err(crate::guest::GuestHttpOutput::text(410, "gone"))
            } else {
                Ok(())
            }
        }

        async fn exchange(
            &self,
            input: crate::guest::GuestHttpInput,
        ) -> crate::guest::GuestHttpOutput {
            self.exchanges.fetch_add(1, Ordering::SeqCst);
            let body = if self.body.is_empty() {
                input.body
            } else {
                self.body.clone()
            };
            crate::guest::GuestHttpOutput {
                status: 200,
                content_type: "application/octet-stream",
                body: body.clone(),
                set_cookie: None,
                headers: vec![("content-length".to_owned(), body.len().to_string())],
            }
        }
    }

    fn guest_frame(request_id: &str, chunked: bool) -> RelayFrame {
        let request = h4rn_protocol::GuestHttpRequest {
            request_id: request_id.to_owned(),
            method: "POST".to_owned(),
            path: "/guest/tok/messages".to_owned(),
            public_prefix: Some("/guest/m/tok".to_owned()),
            chunked,
            headers: vec![
                ("content-type".to_owned(), "application/json".to_owned()),
                ("origin".to_owned(), "https://relay.test".to_owned()),
                ("host".to_owned(), "relay.test".to_owned()),
            ],
            body: Vec::new(),
        };
        RelayFrame {
            conn_id: format!("guest-{request_id}"),
            kind: RelayFrameKind::GuestHttp,
            payload: serde_json::to_vec(&request).expect("request json"),
        }
    }

    fn chunk_frame(request_id: &str, seq: u64, last: bool, body: Vec<u8>) -> RelayFrame {
        let chunk = h4rn_protocol::GuestHttpChunk {
            request_id: request_id.to_owned(),
            seq,
            last,
            body,
        };
        RelayFrame {
            conn_id: format!("guest-{request_id}"),
            kind: RelayFrameKind::GuestHttpChunk,
            payload: serde_json::to_vec(&chunk).expect("chunk json"),
        }
    }

    fn ping_frame() -> RelayFrame {
        RelayFrame {
            conn_id: String::new(),
            kind: RelayFrameKind::Ping,
            payload: Vec::new(),
        }
    }

    /// Waits for a non-chunk guest answer frame.
    async fn next_answer(rx: &mut mpsc::Receiver<RelayFrame>) -> RelayFrame {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let frame = rx.recv().await.expect("relay queue open");
                if frame.kind == RelayFrameKind::GuestHttp {
                    return frame;
                }
            }
        })
        .await
        .expect("a guest answer in time")
    }

    #[tokio::test]
    async fn a_chunked_guest_body_is_answered_in_order_and_off_the_queue() {
        let surface = FakeSurface::new(Vec::new());
        let exchanges = Arc::clone(&surface.exchanges);
        let (tx, mut rx) = mpsc::channel(8);
        let relay = GuestRelay::new(surface, tx);

        handle_unblocked_frame(&guest_frame("g1", true), &relay);
        handle_unblocked_frame(&chunk_frame("g1", 0, false, b"{\"text\":".to_vec()), &relay);
        handle_unblocked_frame(&chunk_frame("g1", 1, true, b"\"hi\"}".to_vec()), &relay);

        let answer = next_answer(&mut rx).await;
        let response: h4rn_protocol::GuestHttpResponse =
            serde_json::from_slice(&answer.payload).expect("answer json");
        assert_eq!(response.status, 200);
        assert_eq!(exchanges.load(Ordering::SeqCst), 1);
        assert_eq!(
            response.body, b"{\"text\":\"hi\"}",
            "the chunks were not reassembled in order"
        );
    }

    #[tokio::test]
    async fn an_out_of_order_body_is_dropped_without_running_the_turn() {
        let surface = FakeSurface::new(b"ok".to_vec());
        let exchanges = Arc::clone(&surface.exchanges);
        let (tx, _rx) = mpsc::channel(8);
        let relay = GuestRelay::new(surface, tx);

        handle_unblocked_frame(&guest_frame("g1", true), &relay);
        handle_unblocked_frame(&chunk_frame("g1", 1, true, b"\"hi\"}".to_vec()), &relay);
        handle_unblocked_frame(&chunk_frame("g1", 0, false, b"{\"text\":".to_vec()), &relay);
        // The missing first chunk never arrives.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(exchanges.load(Ordering::SeqCst), 0);
        assert!(relay.lock().pending.is_empty());
        assert!(relay.lock().rejected.contains_key("g1"));
    }

    #[tokio::test]
    async fn a_body_over_the_exchange_cap_is_refused() {
        let surface = FakeSurface::new(b"ok".to_vec());
        let exchanges = Arc::clone(&surface.exchanges);
        let (tx, mut rx) = mpsc::channel(8);
        let relay = GuestRelay::new(surface, tx);

        handle_unblocked_frame(&guest_frame("g1", true), &relay);
        handle_unblocked_frame(
            &chunk_frame(
                "g1",
                0,
                true,
                vec![b'x'; h4rn_protocol::GUEST_MAX_EXCHANGE_BYTES + 1],
            ),
            &relay,
        );

        let answer = next_answer(&mut rx).await;
        let response: h4rn_protocol::GuestHttpResponse =
            serde_json::from_slice(&answer.payload).expect("answer json");
        assert_eq!(response.status, 413);
        assert_eq!(exchanges.load(Ordering::SeqCst), 0);
        // Later chunks for the refused request are ignored.
        handle_unblocked_frame(&chunk_frame("g1", 1, true, b"x".to_vec()), &relay);
        assert!(!relay.lock().pending.contains_key("g1"));
    }

    #[tokio::test]
    async fn a_rejected_upload_never_reaches_the_surface() {
        let surface = FakeSurface::refusing();
        let exchanges = Arc::clone(&surface.exchanges);
        let (tx, mut rx) = mpsc::channel(8);
        let relay = GuestRelay::new(surface, tx);

        handle_unblocked_frame(&guest_frame("g1", true), &relay);
        handle_unblocked_frame(&chunk_frame("g1", 0, true, b"{}".to_vec()), &relay);

        let answer = next_answer(&mut rx).await;
        let response: h4rn_protocol::GuestHttpResponse =
            serde_json::from_slice(&answer.payload).expect("answer json");
        assert_eq!(response.status, 410);
        assert_eq!(exchanges.load(Ordering::SeqCst), 0);
        // Late chunks are dropped, and the marker is trimmed by age.
        handle_unblocked_frame(&chunk_frame("g1", 1, false, b"x".to_vec()), &relay);
        {
            let mut state = relay.lock();
            let old = std::time::Instant::now() - GUEST_UPLOAD_TIMEOUT - Duration::from_secs(1);
            state.rejected.insert("g1".to_owned(), old);
        }
        handle_unblocked_frame(&ping_frame(), &relay);
        assert!(!relay.lock().rejected.contains_key("g1"));
    }

    #[tokio::test]
    async fn an_unauthorized_upload_is_refused_before_its_body_is_read() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let surface = FakeSurface::refusing_at(Arc::clone(&gate));
        let exchanges = Arc::clone(&surface.exchanges);
        let arrived = {
            // The surface announces when the check is running.
            let surface = Arc::clone(&surface);
            async move { surface.check_arrived.notified().await }
        };
        let (tx, mut rx) = mpsc::channel(8);
        let relay = GuestRelay::new(surface, tx);
        handle_unblocked_frame(&guest_frame("g1", true), &relay);
        arrived.await;

        // Fill up to the pre-authorization bound; not one byte more may be kept
        // while the link is still unproven.
        let chunk = vec![b'x'; h4rn_protocol::GUEST_CHUNK_BYTES];
        let mut seq = 0;
        for _ in 0..(GUEST_PREAUTH_BYTES / h4rn_protocol::GUEST_CHUNK_BYTES) {
            handle_unblocked_frame(&chunk_frame("g1", seq, false, chunk.clone()), &relay);
            seq += 1;
        }
        assert!(
            relay
                .lock()
                .pending
                .get("g1")
                .is_some_and(|pending| pending.body.len() <= GUEST_PREAUTH_BYTES),
            "the pre-authorization buffer exceeded its bound"
        );
        // One chunk past the bound is refused and the buffer is released.
        handle_unblocked_frame(&chunk_frame("g1", seq, false, chunk), &relay);
        let answer = next_answer(&mut rx).await;
        let response: h4rn_protocol::GuestHttpResponse =
            serde_json::from_slice(&answer.payload).expect("answer json");
        assert_eq!(response.status, 413);
        assert!(!relay.lock().pending.contains_key("g1"));

        // The check finishes, but the surface never sees the body.
        gate.notify_waiters();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(exchanges.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn uploads_past_the_cap_are_refused_instead_of_buffered() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let surface = FakeSurface::refusing_at(Arc::clone(&gate));
        let (tx, mut rx) = mpsc::channel(16);
        let relay = GuestRelay::new(surface, tx);
        for index in 0..GUEST_EXCHANGE_CONCURRENCY {
            handle_unblocked_frame(&guest_frame(&format!("g{index}"), true), &relay);
        }
        // Every buffer slot is taken by a check in flight.
        handle_unblocked_frame(&guest_frame("over", true), &relay);
        assert!(
            relay.lock().pending.len() <= GUEST_EXCHANGE_CONCURRENCY,
            "an upload past the cap was buffered"
        );
        let answer = next_answer(&mut rx).await;
        let response: h4rn_protocol::GuestHttpResponse =
            serde_json::from_slice(&answer.payload).expect("answer json");
        assert_eq!(response.status, 503);
        assert!(!relay.lock().pending.contains_key("over"));
        gate.notify_waiters();
    }

    #[tokio::test]
    async fn a_full_channel_does_not_block_a_large_guest_answer() {
        let surface = FakeSurface::new(vec![b'a'; h4rn_protocol::GUEST_CHUNK_BYTES * 2]);
        let (tx, mut rx) = mpsc::channel(1);
        let relay = GuestRelay::new(surface, tx.clone());
        // The loop's own queue is full: a synchronous send would deadlock the
        // relay, because the same task drains this channel.
        tx.send(RelayFrame {
            conn_id: "existing".to_owned(),
            kind: RelayFrameKind::Ping,
            payload: Vec::new(),
        })
        .await
        .expect("fill queue");

        // Neither a request nor another frame waits on the full channel.
        tokio::time::timeout(Duration::from_millis(200), async {
            handle_unblocked_frame(
                &RelayFrame {
                    conn_id: "guest-g1".to_owned(),
                    kind: RelayFrameKind::GuestHttp,
                    payload: serde_json::to_vec(&h4rn_protocol::GuestHttpRequest {
                        request_id: "g1".to_owned(),
                        method: "GET".to_owned(),
                        path: "/guest/tok/blob/abc".to_owned(),
                        public_prefix: None,
                        chunked: false,
                        headers: Vec::new(),
                        body: Vec::new(),
                    })
                    .expect("request"),
                },
                &relay,
            );
            handle_unblocked_frame(&ping_frame(), &relay);
        })
        .await
        .expect("dispatching a guest frame must not wait on the relay queue");

        // Drain: the spawned answer and its chunks arrive once there is room.
        let mut seen = 0;
        let mut chunked = 0;
        tokio::time::timeout(Duration::from_secs(5), async {
            while seen == 0 || chunked == 0 {
                let frame = rx.recv().await.expect("relay queue open");
                match frame.kind {
                    RelayFrameKind::GuestHttp => seen += 1,
                    RelayFrameKind::GuestHttpChunk => chunked += 1,
                    _ => {}
                }
            }
        })
        .await
        .expect("the answer arrives after the queue drains");
    }

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

    /// Builds the real daemon state so this test drives `handle_event`, the
    /// function the relay loop actually awaits, and not a stand-in for it.
    async fn real_daemon_state() -> (crate::server::AppState, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("daemon dir");
        let config = crate::config::Config {
            data_dir: dir.path().to_path_buf(),
            bind: "127.0.0.1:0".parse().expect("bind"),
            extra_bind: None,
            tailscale: crate::remote::TailscaleMode::Off,
            network_access: None,
            network_port: 0,
            log_level: "off".to_owned(),
        };
        let daemon = crate::daemon::DaemonBuilder::new(config)
            .with_local_host(
                h4rn_protocol::HostId::new("host_relay_queue_test"),
                "queue test",
            )
            .without_title_agents()
            .with_harness(Arc::new(h4rn_harness::FakeHarness::new()))
            .build()
            .await
            .expect("build daemon");
        let bound = daemon.bind().await.expect("bind daemon");
        (bound.app_state().clone(), dir)
    }

    #[tokio::test]
    async fn a_saturated_queue_still_delivers_another_session_frame() {
        let (state, _dir) = real_daemon_state().await;
        let surface = FakeSurface::new(vec![b'a'; h4rn_protocol::GUEST_CHUNK_BYTES * 2]);
        let (out_tx, mut out_rx) = mpsc::channel(1);
        let guests = GuestRelay::new(surface, out_tx.clone());
        // The shared relay queue is saturated: a synchronous send from the loop
        // would deadlock it, because the same task drains this channel.
        out_tx
            .send(RelayFrame {
                conn_id: "existing".to_owned(),
                kind: RelayFrameKind::Ping,
                payload: Vec::new(),
            })
            .await
            .expect("fill the relay queue");

        // An independent session has its own inbound queue; its frame must be
        // routed even while the guest answer is waiting for queue space.
        let connections: Connections = Arc::new(Mutex::new(HashMap::new()));
        let (session_tx, mut session_rx) = mpsc::channel(4);
        connections
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert("other".to_owned(), session_tx);

        let service = Arc::new(SecureService::new(
            h4rn_transport::server::SecureOptions::new(
                h4rn_transport::keys::DeviceIdentity::generate().expect("identity"),
            ),
        ));
        let client = Arc::new(
            ConnectClient::new(
                _dir.path(),
                h4rn_transport::keys::DeviceIdentity::generate().expect("identity"),
            )
            .expect("connect client"),
        );
        let registry = Arc::new(DeviceRegistry::in_memory());

        let request = h4rn_protocol::GuestHttpRequest {
            request_id: "g1".to_owned(),
            method: "GET".to_owned(),
            path: "/guest/tok/blob/abc".to_owned(),
            public_prefix: None,
            chunked: false,
            headers: Vec::new(),
            body: Vec::new(),
        };
        let frame = RelayFrame {
            conn_id: "guest-g1".to_owned(),
            kind: RelayFrameKind::GuestHttp,
            payload: serde_json::to_vec(&request).expect("request json"),
        };
        tokio::time::timeout(
            Duration::from_millis(500),
            handle_event(
                RelayServerEvent::Frame { frame },
                &state,
                &service,
                &client,
                &registry,
                &connections,
                &out_tx,
                &guests,
            ),
        )
        .await
        .expect("the relay loop must not wait for a guest answer")
        .expect("guest frame handled");

        // The independent session's frame makes progress while the answer is
        // blocked on the saturated queue.
        tokio::time::timeout(
            Duration::from_millis(500),
            handle_event(
                RelayServerEvent::Frame {
                    frame: RelayFrame::data("other", b"progress".to_vec()),
                },
                &state,
                &service,
                &client,
                &registry,
                &connections,
                &out_tx,
                &guests,
            ),
        )
        .await
        .expect("another session must make progress")
        .expect("data frame handled");
        match session_rx.recv().await.expect("session frame") {
            RelayInbound::Data(payload) => assert_eq!(payload, b"progress"),
            RelayInbound::Error(message) => panic!("unexpected session error: {message}"),
        }

        // Drain the queue: the guest answer and its chunks arrive once there is
        // room, so nothing was silently dropped.
        let mut answer = false;
        let mut chunked = false;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !(answer && chunked) {
                let frame = out_rx.recv().await.expect("relay queue open");
                match frame.kind {
                    RelayFrameKind::GuestHttp => answer = true,
                    RelayFrameKind::GuestHttpChunk => chunked = true,
                    _ => {}
                }
            }
        })
        .await
        .expect("the guest answer arrives after the queue drains");
    }
}
