//! The local WebSocket server.
//!
//! `GET /healthz` is unauthenticated liveness for supervisors and `doctor`.
//! `GET /ws` requires the local access token and then carries the typed
//! protocol of `h4rn-protocol` (ADR-001). JSON frames are requests, responses
//! and ordered events; binary frames are reserved for PTY output and blobs.
//!
//! Resilience rules:
//! - every connection has its own bounded outbound queue;
//! - a caller that falls behind is disconnected with an event carrying the last
//!   sequence it saw, so it can reconnect and `GetThread` from there;
//! - a slow writer never blocks the thread manager or another client.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode, header::SEC_WEBSOCKET_PROTOCOL};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, serve};
use futures::{Sink, SinkExt, Stream, StreamExt};
use h4rn_protocol::{
    BINARY_HEADER_LEN, BinaryFrame, BinaryKind, ClientFrame, ClientRequest, DeviceId, DeviceInfo,
    Event, PROTOCOL_VERSION, PairScope, ProtoError, ProtoErrorCode, ResponseBody, SeqEvent,
    ServerFrame, Topic,
};
use h4rn_transport::server::{AcceptOutcome, Link, LinkError, SecureService, SessionSender};
use h4rn_transport::session::AppFrame;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

use crate::auth::{TokenStore, authorize};
use crate::blobs::{BlobError, BlobStore};
use crate::forge::ForgeService;
use crate::fs::{FsError, FsService};
use crate::hosts::HostInventory;
use crate::mcp::McpService;
use crate::remote::{RemoteAudit, SessionOrigin};
use crate::terminal::{Attached, OpenSpec, TerminalManager, run_viewer};
use crate::threads::ThreadManager;

mod forward;
mod handlers;

/// Capacity of one connection's outbound queue.
pub const OUTBOUND_CAPACITY: usize = 128;

/// How long one frame may take to reach a client before it is considered dead.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

/// Shared state for the axum router.
#[derive(Clone)]
pub struct AppState {
    /// Thread manager and live sessions.
    pub manager: Arc<ThreadManager>,
    /// Delegated child threads and their budgets (ADR-038).
    pub subagents: Arc<crate::subagents::SubagentManager>,
    /// Host inventory.
    pub hosts: Arc<HostInventory>,
    /// Daemon-owned PTY terminals.
    pub terminals: Arc<TerminalManager>,
    /// Content-addressed attachment blobs (ADR-008).
    pub blobs: Arc<BlobStore>,
    /// Local access token.
    pub tokens: TokenStore,
    /// Workspace-scoped filesystem and git service for IDE mode.
    pub fs: Arc<FsService>,
    /// Forge accounts, clients and watched-PR checks polling.
    pub forge: Arc<ForgeService>,
    /// Skill registry, installers and connect sync (ADR-034, ADR-035).
    pub skills: Arc<h4rn_skills::SkillsService>,
    /// Noise-encrypted multipath transport, when enabled.
    pub secure: Option<Arc<SecureService>>,
    /// Remote-action audit trail, when the daemon is signed in to connect.
    pub remote: Option<Arc<RemoteAudit>>,
    /// Persisted device registry: the only source of remote authorisation.
    pub devices: Arc<crate::remote::DeviceRegistry>,
    /// CLIProxyAPI profiles and routing (ADR-015).
    pub proxy: Arc<h4rn_proxy::ProxyService>,
    /// The daemon-owned MCP hub (ADR-006).
    pub mcp: Arc<McpService>,
    /// Durable metering, limits and auto-resume (ADR-024).
    pub meter: Arc<h4rn_meter::MeterHub>,
    /// Server boot time, for `/healthz`.
    pub started: Instant,
}

/// Builds the daemon router.
///
/// `/secure` is always registered so a remote client gets a clear "not enabled"
/// response instead of a 404, while plain WebSocket access stays loopback-only
/// (ADR-009).
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_handler))
        .route("/secure", get(secure_handler))
        .route("/secure-mcp", get(secure_mcp_handler))
        .route(
            "/mcp",
            get(mcp_http_get)
                .post(mcp_http_post)
                .delete(mcp_http_delete),
        )
        .with_state(state)
}

/// Streamable-HTTP MCP endpoint for harness sessions (ADR-006).
async fn mcp_http_post(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    crate::mcp::http_post(state.mcp.http_state(), headers, body).await
}

async fn mcp_http_get(State(state): State<AppState>) -> Response {
    crate::mcp::http_get(state.mcp.http_state()).await
}

async fn mcp_http_delete(State(state): State<AppState>, headers: HeaderMap) -> Response {
    crate::mcp::http_delete(state.mcp.http_state(), headers).await
}

/// Owner half of cross-machine MCP sharing (ADR-006).
///
/// The peer authenticates with the same Noise identity it uses for `/secure`;
/// the hub then applies the `server -> machine` grant and audits each call.
async fn secure_mcp_handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    let Some(service) = state.secure.clone() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "the secure transport is not enabled on this daemon",
            })),
        )
            .into_response();
    };
    let mcp = Arc::clone(&state.mcp);
    ws.on_upgrade(move |socket| async move {
        let link = Box::new(WsLink { socket });
        mcp.serve_secure_mcp(service, link).await;
    })
}

/// Unauthenticated liveness response.
pub async fn healthz(State(state): State<AppState>) -> Json<serde_json::Value> {
    let threads = state.manager.list_threads(&Default::default()).await.len();
    Json(json!({
        "status": "ok",
        "version": crate::version(),
        "threads": threads,
        "uptime_ms": state.started.elapsed().as_millis() as u64,
    }))
}

async fn ws_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Plain WebSocket is unencrypted. It is only allowed on loopback; every
    // remote path must use `/secure`, where Noise authenticates and encrypts.
    if !peer.ip().is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            Json(ProtoError::new(
                ProtoErrorCode::Unauthorized,
                "the plain WebSocket endpoint is loopback-only",
                "Connect remotely over /secure, which requires the Noise handshake",
            )),
        )
            .into_response();
    }

    let protocol_header = headers
        .get(SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok());

    let grant = match authorize(
        &state.tokens,
        query.get("token").map(String::as_str),
        protocol_header,
    ) {
        Ok(grant) => grant,
        Err(error) => return (StatusCode::UNAUTHORIZED, Json(error)).into_response(),
    };

    let upgrade = match grant.protocol {
        // Echo one of the offered subprotocols, which browsers require before
        // they accept the socket. `bearer, <token>` echoes `bearer`; the
        // `bearer.<token>` shape echoes the exact offered value.
        Some(protocol) => ws.protocols([protocol]),
        None => ws,
    };
    upgrade.on_upgrade(move |socket| handle_socket(socket, state, SessionOrigin::local()))
}

/// Upgrades to the Noise-encrypted multipath transport.
///
/// A client dials this route on every candidate path; each socket gets a fresh
/// handshake (ADR-010) and is bound to the logical session named in the packet
/// header. The decrypted application frames then flow through the same
/// [`handle_socket`] loop as plain WebSocket traffic.
async fn secure_handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    let Some(service) = state.secure.clone() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "the secure transport is not enabled on this daemon",
            })),
        )
            .into_response();
    };
    ws.on_upgrade(move |socket| async move {
        let link = Box::new(WsLink { socket });
        match service.accept(link).await {
            Ok(AcceptOutcome::New(handle)) => {
                if refuse_peer_daemon(&state, &handle.device) {
                    handle.into_parts().0.close();
                    return;
                }
                let mut origin = SessionOrigin::secure(handle.device);
                origin.device_id = state.devices.device_for_key(&handle.device);
                let (sender, frames) = handle.into_parts();
                handle_socket(SecureSocket { sender, frames }, state, origin).await;
            }
            // The path attached to a live session. Its application loop is
            // already running, so there is nothing to start here: returning
            // drops the raw socket and leaves the path task to drive it.
            Ok(AcceptOutcome::Resumed { session_id }) => {
                tracing::debug!(
                    session = %hex::encode(session_id),
                    "secure session resumed on a new path"
                );
            }
            Err(error) => tracing::warn!(%error, "secure transport handshake failed"),
        }
    })
}

/// Whether `key` is a registered peer daemon. Peer daemons authenticate on
/// `/secure-mcp` only: on the client protocol route they get no session at all,
/// whatever the allowlist says.
fn refuse_peer_daemon(state: &AppState, key: &[u8; h4rn_transport::keys::KEY_LEN]) -> bool {
    let peer = state
        .devices
        .authorization(key)
        .is_some_and(|auth| auth.role == crate::remote::Role::Host);
    if peer {
        tracing::warn!(
            key = %hex::encode(key),
            "refused a peer daemon key on the client route; peers may only use /secure-mcp"
        );
    }
    peer
}

/// Runs one accepted secure session through the normal protocol loop.
///
/// Both `/secure` and the relay connector call this, so a relayed session is
/// indistinguishable from a direct one after the Noise handshake. `origin`
/// carries the authenticated device identity for the audit trail.
pub(crate) async fn serve_secure_link(
    service: Arc<SecureService>,
    link: Box<dyn h4rn_transport::server::Link>,
    state: AppState,
    origin: SessionOrigin,
) {
    match service.accept(link).await {
        Ok(AcceptOutcome::New(handle)) => {
            if refuse_peer_daemon(&state, &handle.device) {
                handle.into_parts().0.close();
                return;
            }
            // Identity is the key the Noise handshake authenticated. A device id
            // the relay put in its open frame is only a claim: it is replaced by
            // the registry's answer for that key, even when they disagree.
            let mut origin = origin;
            origin.device_key = Some(handle.device);
            origin.device_id = state.devices.device_for_key(&handle.device);
            let (sender, frames) = handle.into_parts();
            handle_socket(SecureSocket { sender, frames }, state, origin).await;
        }
        Ok(AcceptOutcome::Resumed { session_id }) => {
            tracing::debug!(
                session = %hex::encode(session_id),
                "secure session resumed on a new path"
            );
        }
        Err(error) => tracing::warn!(%error, "secure transport handshake failed"),
    }
}

/// Adapts an axum WebSocket to the transport's packet-oriented [`Link`].
struct WsLink {
    socket: WebSocket,
}

#[async_trait::async_trait]
impl Link for WsLink {
    async fn send_packet(&mut self, packet: Vec<u8>) -> Result<(), LinkError> {
        self.socket
            .send(Message::Binary(packet.into()))
            .await
            .map_err(|error| LinkError::Transport(error.to_string()))
    }

    async fn recv_packet(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
        loop {
            match self.socket.recv().await {
                Some(Ok(Message::Binary(bytes))) => return Ok(Some(bytes.to_vec())),
                Some(Ok(Message::Close(_))) | None => return Ok(None),
                // Text/ping frames are not transport packets; keep reading.
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(LinkError::Transport(error.to_string())),
            }
        }
    }
}

/// Presents an established secure session as a `Sink`/`Stream` of WebSocket
/// messages, so [`handle_socket`] is reused unchanged.
struct SecureSocket {
    sender: SessionSender,
    frames: mpsc::Receiver<AppFrame>,
}

impl Stream for SecureSocket {
    type Item = Result<Message, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.frames.poll_recv(cx) {
            Poll::Ready(Some(frame)) => Poll::Ready(Some(Ok(if frame.binary {
                Message::Binary(frame.payload.into())
            } else {
                Message::Text(String::from_utf8_lossy(&frame.payload).into_owned().into())
            }))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Sink<Message> for SecureSocket {
    type Error = std::io::Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        let this = self.get_mut();
        let frame = match item {
            Message::Text(text) => AppFrame::text(text.as_str()),
            Message::Binary(bytes) => AppFrame::binary(bytes.to_vec()),
            // The secure transport carries its own liveness pings; a WebSocket
            // ping from the application layer has no equivalent here.
            Message::Ping(_) | Message::Pong(_) => return Ok(()),
            Message::Close(_) => {
                this.sender.close();
                return Ok(());
            }
        };
        this.sender
            .send(frame)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::BrokenPipe, error.to_string()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().sender.close();
        Poll::Ready(Ok(()))
    }
}

/// One WebSocket message queued for a connection.
///
/// The writer task owns the socket and turns these into text or binary
/// messages, so terminal viewers can enqueue output without touching the sink.
#[derive(Debug)]
pub enum Outbound {
    /// A JSON protocol frame.
    Text(String),
    /// A framed binary message (PTY output, snapshot, blob chunk or ack).
    Binary(Vec<u8>),
}

struct Connection {
    state: AppState,
    hello_done: bool,
    topics: Arc<RwLock<Vec<Topic>>>,
    ready: Arc<AtomicBool>,
    last_seq: Arc<AtomicU64>,
    device: Option<DeviceInfo>,
    viewer_id: crate::terminal::ViewerId,
    tx: mpsc::Sender<Outbound>,
    /// Who opened this session, for the remote audit trail.
    origin: SessionOrigin,
    /// One filesystem watcher subscription per requested workspace.
    watches: HashMap<String, JoinHandle<()>>,
    /// Set when this connection must be dropped with a resume hint.
    overflowed: Arc<AtomicBool>,
    /// Wakes the writer loop when a forwarder overflows this connection.
    notify: Arc<Notify>,
}

impl Connection {
    fn device_id(&self) -> Option<DeviceId> {
        self.device.as_ref().map(|device| device.device_id.clone())
    }

    /// Records one remote action, if this session came from another device.
    fn audit(
        &self,
        action: &str,
        target_type: Option<&str>,
        target_id: Option<&str>,
        metadata: serde_json::Value,
    ) {
        if let Some(audit) = &self.state.remote {
            audit.record(&self.origin, action, target_type, target_id, metadata);
        }
    }
}

async fn handle_socket<S, E>(socket: S, state: AppState, origin: SessionOrigin)
where
    S: Sink<Message, Error = E> + Stream<Item = Result<Message, E>> + Unpin + Send + 'static,
    E: std::fmt::Debug + Send + 'static,
{
    let (sink, mut stream) = socket.split();
    let (tx, rx) = mpsc::channel::<Outbound>(OUTBOUND_CAPACITY);
    let overflowed = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let last_seq = Arc::new(AtomicU64::new(0));
    let topics = Arc::new(RwLock::new(vec![Topic::All]));
    let ready = Arc::new(AtomicBool::new(false));

    let writer = tokio::spawn(writer_loop(
        sink,
        rx,
        Arc::clone(&overflowed),
        Arc::clone(&notify),
        Arc::clone(&last_seq),
    ));

    let viewer_id = state.terminals.next_viewer_id();
    let mut connection = Connection {
        state,
        hello_done: false,
        topics,
        ready,
        last_seq,
        device: None,
        viewer_id,
        tx: tx.clone(),
        origin,
        watches: HashMap::new(),
        overflowed: Arc::clone(&overflowed),
        notify: Arc::clone(&notify),
    };
    let journal_forward = tokio::spawn(forward::forward_loop(
        connection.state.manager.subscribe(),
        forward::ForwardContext::for_connection(&connection),
        forward::ForwardConfig::JOURNAL,
        |event: crate::journal::WireEvent, _last| {
            Some(forward::ForwardFrame::durable(
                ServerFrame::Event {
                    seq: event.seq,
                    event: event.event,
                },
                event.seq,
            ))
        },
    ));
    let service_forwards = handlers::spawn_forwarders(&connection);

    while let Some(message) = stream.next().await {
        let Ok(message) = message else {
            break;
        };
        match message {
            Message::Text(text) => {
                let Some(reply) = connection.handle_text(&text).await else {
                    continue;
                };
                if connection.send(Outbound::Text(reply)).await.is_err() {
                    break;
                }
            }
            Message::Binary(bytes) => {
                if let Some(reply) = connection.handle_binary(&bytes) {
                    if connection.send(reply).await.is_err() {
                        break;
                    }
                }
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    journal_forward.abort();
    for handle in service_forwards {
        handle.abort();
    }
    // Detaching a socket never kills a terminal; it only removes this
    // connection's viewers so the PTY keeps running for the next attach.
    connection
        .state
        .terminals
        .detach_viewer(connection.viewer_id);
    connection.stop_watches();
    drop(connection);
    drop(tx);
    let _ = writer.await;
}

impl Connection {
    /// Aborts every filesystem watcher task owned by this connection.
    ///
    /// Watcher tasks hold a sender clone, so they would otherwise outlive the
    /// socket and keep pushing into a queue nobody reads.
    fn stop_watches(&mut self) {
        for (_, handle) in self.watches.drain() {
            handle.abort();
        }
    }

    /// Returns an error when the request names a host this daemon does not run.
    fn local_host_error(&self, host: &h4rn_protocol::HostId) -> Option<ProtoError> {
        if self.state.hosts.local_id() == *host {
            None
        } else {
            Some(ProtoError::new(
                ProtoErrorCode::Unsupported,
                "that host is not this daemon",
                "Open the IDE on the host that owns the workspace",
            ))
        }
    }
    async fn handle_text(&mut self, text: &str) -> Option<String> {
        let frame: ClientFrame = match serde_json::from_str(text) {
            Ok(frame) => frame,
            Err(error) => {
                return Some(error_frame(
                    0,
                    ProtoError::new(
                        ProtoErrorCode::InvalidRequest,
                        format!("frame is not valid client JSON: {error}"),
                        "Send a ClientFrame with an integer id and a tagged body",
                    ),
                ));
            }
        };
        let id = frame.id;
        let reply_id = id;
        let body = frame.body;

        if !self.hello_done && !matches!(&body, ClientRequest::Hello { .. }) {
            return Some(error_frame(
                id,
                ProtoError::new(
                    ProtoErrorCode::Unauthorized,
                    "the connection has not completed the Hello handshake",
                    "Send Hello with the device identity before other requests",
                ),
            ));
        }

        if let Err(error) =
            crate::remote::authorize_scopes(&self.state.devices, &self.origin, &body)
        {
            tracing::warn!(
                origin = %self.origin.label(),
                needs = body.required_scope().as_str(),
                "refused a request outside the device's scopes"
            );
            self.audit(
                "scope.denied",
                None,
                None,
                serde_json::json!({"needs": body.required_scope().as_str()}),
            );
            return Some(error_frame(id, error));
        }

        let result: Result<ResponseBody, ProtoError> = match handlers::Service::of(&body) {
            Some(service) => service.handle(self, body).await,
            None => match body {
                ClientRequest::Hello {
                    protocol_version,
                    device,
                } => {
                    if protocol_version != PROTOCOL_VERSION {
                        Err(ProtoError::new(
                            ProtoErrorCode::Unsupported,
                            format!(
                                "protocol version {protocol_version} is not supported (daemon speaks {PROTOCOL_VERSION})"
                            ),
                            "Update the client or the daemon so both speak the same protocol major",
                        ))
                    } else {
                        self.hello_done = true;
                        self.ready.store(true, Ordering::Relaxed);
                        self.device = Some(device.clone());
                        *self
                            .topics
                            .write()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = vec![Topic::All];
                        Ok(ResponseBody::Hello {
                            protocol_version: PROTOCOL_VERSION,
                            server_version: crate::version().to_owned(),
                            device_id: device.device_id,
                            local_host: self.state.hosts.local_id(),
                            hosts: self.state.hosts.summaries(),
                        })
                    }
                }
                ClientRequest::PairingConfirmations => Ok(ResponseBody::PairingConfirmations {
                    confirmations: self.state.devices.pending_confirmations(),
                }),
                ClientRequest::ListHosts => Ok(ResponseBody::Hosts {
                    hosts: self.state.hosts.summaries(),
                }),
                ClientRequest::ListThreads { filter } => Ok(ResponseBody::Threads {
                    threads: self.state.manager.list_threads(&filter).await,
                }),
                ClientRequest::GetThread {
                    thread_id,
                    since_seq,
                } => self
                    .state
                    .manager
                    .get_thread(&thread_id, since_seq)
                    .await
                    .map(|snapshot| ResponseBody::Thread {
                        summary: snapshot.summary,
                        events: snapshot
                            .events
                            .into_iter()
                            .map(|event| SeqEvent {
                                seq: event.seq,
                                event: event.event,
                            })
                            .collect(),
                        next_seq: snapshot.next_seq,
                    })
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::CreateThread {
                    host,
                    harness,
                    workspace_path,
                    model,
                    prompt,
                    attachments,
                    runtime_mode,
                    worktree,
                } => {
                    let request = crate::threads::NewThread {
                        thread_id: None,
                        parent_thread_id: None,
                        subagent_id: None,
                        host,
                        harness,
                        workspace_path: workspace_path.clone(),
                        model,
                        prompt,
                        attachments,
                        runtime_mode,
                        worktree,
                    };
                    match self.state.manager.create_thread(request).await {
                        Ok(created) => {
                            // A thread's workspace becomes an authorised IDE root.
                            // This is the only way a workspace is added at runtime,
                            // so IDE mode cannot reach a path the user never opened.
                            if let Err(error) = self.state.fs.allow_root(&workspace_path) {
                                tracing::warn!(%workspace_path, %error, "IDE root not authorised");
                            }
                            self.audit(
                                "thread.created",
                                Some("thread"),
                                Some(created.thread_id.as_ref()),
                                json!({"workspace": workspace_path}),
                            );
                            Ok(ResponseBody::ThreadCreated {
                                thread_id: created.thread_id,
                                turn_id: created.turn_id,
                                summary: created.summary,
                            })
                        }
                        Err(error) => Err(error.into_proto()),
                    }
                }
                ClientRequest::SendMessage {
                    thread_id,
                    text,
                    attachments,
                } => self
                    .state
                    .manager
                    .send_message(&thread_id, text, attachments)
                    .await
                    .map(|accepted| ResponseBody::MessageAccepted {
                        thread_id: accepted.thread_id,
                        turn_id: accepted.turn_id,
                        seq: accepted.seq,
                    })
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::Interrupt { thread_id } => self
                    .state
                    .manager
                    .interrupt(&thread_id)
                    .await
                    .map(|()| ResponseBody::Ack)
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::RespondApproval {
                    thread_id,
                    approval_id,
                    decision,
                    option_id,
                } => self
                    .state
                    .manager
                    .respond_approval(
                        &thread_id,
                        &approval_id,
                        decision,
                        option_id,
                        self.device_id(),
                    )
                    .await
                    .map(|()| ResponseBody::Ack)
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::SetThreadTab {
                    thread_id,
                    tab,
                    pinned,
                } => self
                    .state
                    .manager
                    .set_thread_tab(&thread_id, tab, pinned)
                    .await
                    .map(|_| ResponseBody::Ack)
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::Snooze { thread_id, until } => self
                    .state
                    .manager
                    .snooze(&thread_id, until)
                    .await
                    .map(|_| ResponseBody::Ack)
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::Subscribe { topics, since_seq } => {
                    *self
                        .topics
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = topics;
                    match since_seq {
                        None => {
                            self.ready.store(true, Ordering::Relaxed);
                            Ok(ResponseBody::Ack)
                        }
                        Some(since) => {
                            // Hold the manager's append lock across read + enqueue
                            // + ready so no event falls between replay and live.
                            let guard = self.state.manager.lock_append().await;
                            let replay = self.state.manager.replay_global_since(since).await;
                            if let Ok(events) = &replay {
                                for event in events {
                                    let frame = ServerFrame::Event {
                                        seq: event.seq,
                                        event: event.event.clone(),
                                    };
                                    if let Ok(text) = serde_json::to_string(&frame)
                                        && self.send(Outbound::Text(text)).await.is_err()
                                    {
                                        break;
                                    }
                                }
                                if let Some(last) = events.last() {
                                    self.last_seq.store(last.seq, Ordering::Relaxed);
                                }
                                self.ready.store(true, Ordering::Relaxed);
                            }
                            drop(guard);
                            replay
                                .map(|_| ResponseBody::Ack)
                                .map_err(crate::threads::ManagerError::into_proto)
                        }
                    }
                }
                ClientRequest::TerminalOpen {
                    thread_id,
                    host,
                    cwd,
                    shell,
                    args,
                    cols,
                    rows,
                } => match self.require_device() {
                    Ok(device) => {
                        let opened = self
                            .state
                            .terminals
                            .open(
                                OpenSpec {
                                    thread_id,
                                    host,
                                    cwd,
                                    shell,
                                    args,
                                    cols,
                                    rows,
                                },
                                &device,
                            )
                            .await;
                        if let Ok(terminal) = &opened {
                            self.audit(
                            "terminal.opened",
                            Some("terminal"),
                            Some(terminal.terminal_id.as_ref()),
                            json!({"thread_id": terminal.thread_id.as_ref().map(|id| id.as_ref())}),
                        );
                        }
                        opened
                            .map(|terminal| ResponseBody::TerminalOpened { terminal })
                            .map_err(crate::terminal::TerminalError::into_proto)
                    }
                    Err(error) => Err(error),
                },
                ClientRequest::TerminalAttach {
                    terminal_id,
                    cols,
                    rows,
                } => self
                    .state
                    .terminals
                    .attach(&terminal_id, self.viewer_id, self.tx.clone(), cols, rows)
                    .map(|attached| {
                        let Attached {
                            terminal,
                            snapshot,
                            output,
                            viewer,
                            session,
                        } = attached;
                        let start_seq = snapshot.seq;
                        let ansi = snapshot.ansi;
                        // The viewer task frames output and applies backpressure
                        // until this connection closes or the terminal exits.
                        tokio::spawn(run_viewer(
                            session,
                            self.viewer_id,
                            viewer,
                            output,
                            start_seq,
                        ));
                        ResponseBody::TerminalAttached {
                            terminal,
                            snapshot: ansi,
                            start_seq,
                        }
                    })
                    .map_err(crate::terminal::TerminalError::into_proto),
                ClientRequest::TerminalList { thread_id } => Ok(ResponseBody::Terminals {
                    terminals: self.state.terminals.list(thread_id.as_ref()),
                }),
                ClientRequest::TerminalResize {
                    terminal_id,
                    cols,
                    rows,
                } => self
                    .state
                    .terminals
                    .resize(&terminal_id, self.viewer_id, cols, rows)
                    .map(|()| ResponseBody::Ack)
                    .map_err(crate::terminal::TerminalError::into_proto),
                ClientRequest::TerminalAck {
                    terminal_id,
                    up_to_chunk,
                } => self
                    .state
                    .terminals
                    .ack(&terminal_id, self.viewer_id, up_to_chunk)
                    .map(|()| ResponseBody::Ack)
                    .map_err(crate::terminal::TerminalError::into_proto),
                ClientRequest::TerminalClose { terminal_id } => self
                    .state
                    .terminals
                    .close(&terminal_id)
                    .await
                    .map(|()| ResponseBody::Ack)
                    .map_err(crate::terminal::TerminalError::into_proto),
                // ------------------------------------------------------------------
                // IDE mode: workspace-scoped filesystem, search, watch and git.
                // Every path is resolved daemon-side; the client never sends an
                // absolute target.
                // ------------------------------------------------------------------
                ClientRequest::FsList {
                    host,
                    workspace,
                    path,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    fs_block(fs, move |fs| fs.list(&workspace, &path))
                        .await
                        .map(|listing| ResponseBody::FsListing { listing })
                }
                ClientRequest::FsRead {
                    host,
                    workspace,
                    path,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    fs_block(fs, move |fs| fs.read(&workspace, &path))
                        .await
                        .map(|file| ResponseBody::FsFile { file })
                }
                ClientRequest::FsWrite {
                    host,
                    workspace,
                    path,
                    content,
                    base_etag,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let device = self.device_id().map(|id| id.as_str().to_owned());
                    let audit_path = path.clone();
                    let audit_workspace = workspace.clone();
                    let written = fs_block(fs, move |fs| {
                        fs.write(
                            &workspace,
                            &path,
                            &content,
                            base_etag.as_deref(),
                            device.as_deref(),
                        )
                    })
                    .await;
                    if written.is_ok() {
                        self.audit(
                            "file.written",
                            Some("file"),
                            Some(&audit_path),
                            json!({"workspace": audit_workspace}),
                        );
                    }
                    written.map(|result| ResponseBody::FsWritten { result })
                }
                ClientRequest::FsCreate {
                    host,
                    workspace,
                    path,
                    kind,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let device = self.device_id().map(|id| id.as_str().to_owned());
                    fs_block(fs, move |fs| {
                        fs.create(&workspace, &path, kind, device.as_deref())
                    })
                    .await
                    .map(|entry| ResponseBody::FsEntryCreated { entry })
                }
                ClientRequest::FsRename {
                    host,
                    workspace,
                    from,
                    to,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let device = self.device_id().map(|id| id.as_str().to_owned());
                    fs_block(fs, move |fs| {
                        fs.rename(&workspace, &from, &to, device.as_deref())
                    })
                    .await
                    .map(|entry| ResponseBody::FsEntryCreated { entry })
                }
                ClientRequest::FsDelete {
                    host,
                    workspace,
                    path,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let device = self.device_id().map(|id| id.as_str().to_owned());
                    fs_block(fs, move |fs| {
                        fs.delete(&workspace, &path, device.as_deref())
                    })
                    .await
                    .map(|result| ResponseBody::FsDeleted { result })
                }
                ClientRequest::FsRestore {
                    host,
                    workspace,
                    trash_id,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let device = self.device_id().map(|id| id.as_str().to_owned());
                    fs_block(fs, move |fs| {
                        fs.restore(&workspace, &trash_id, device.as_deref())
                    })
                    .await
                    .map(|path| ResponseBody::FsRestored { path })
                }
                ClientRequest::FsSearch {
                    host,
                    workspace,
                    query,
                    regex,
                    case_sensitive,
                    glob,
                    max_results,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let searched = tokio::task::spawn_blocking(move || {
                        fs.search(
                            &workspace,
                            &query,
                            regex,
                            case_sensitive,
                            glob.as_deref(),
                            max_results,
                        )
                    })
                    .await;
                    let (search_id, mut rx) = match searched {
                        Ok(Ok(value)) => value,
                        Ok(Err(error)) => return Some(error_frame(id, error.into_proto())),
                        Err(error) => {
                            return Some(error_frame(
                                id,
                                ProtoError::plain(
                                    ProtoErrorCode::Internal,
                                    format!("search task failed: {error}"),
                                ),
                            ));
                        }
                    };

                    let tx = self.tx.clone();
                    let seq = self.last_seq.load(Ordering::Relaxed);
                    let response_id = search_id.clone();
                    tokio::spawn(async move {
                        while let Some(batch) = rx.recv().await {
                            let frame = ServerFrame::Event {
                                seq,
                                event: Event::FsSearchBatch {
                                    search_id: batch.search_id,
                                    results: batch.results,
                                    done: batch.done,
                                    truncated: batch.truncated,
                                },
                            };
                            let Ok(text) = serde_json::to_string(&frame) else {
                                continue;
                            };
                            if tx.send(Outbound::Text(text)).await.is_err() {
                                break;
                            }
                        }
                    });
                    Ok(ResponseBody::FsSearchStarted {
                        search_id: response_id,
                    })
                }
                ClientRequest::FsWatch {
                    host,
                    workspace,
                    paths,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let requested = workspace.clone();
                    let watched = tokio::task::spawn_blocking(move || fs.watch(&requested)).await;
                    let root = match watched {
                        Ok(Ok(value)) => value,
                        Ok(Err(error)) => return Some(error_frame(id, error.into_proto())),
                        Err(error) => {
                            return Some(error_frame(
                                id,
                                ProtoError::plain(
                                    ProtoErrorCode::Internal,
                                    format!("watcher task failed: {error}"),
                                ),
                            ));
                        }
                    };
                    let canonical = root.display().to_string();
                    let mut rx = self.state.fs.subscribe();
                    let tx = self.tx.clone();
                    let seq = self.last_seq.load(Ordering::Relaxed);
                    let filter = paths.clone();
                    let event_workspace = canonical.clone();
                    if let Some(previous) = self.watches.remove(&workspace) {
                        previous.abort();
                    }
                    let handle = tokio::spawn(async move {
                        while let Ok(change) = rx.recv().await {
                            if change.workspace != root {
                                continue;
                            }
                            let inside = filter.is_empty()
                                || filter.iter().any(|path| {
                                    change.path == *path
                                        || change.path.starts_with(&format!("{path}/"))
                                });
                            if !inside {
                                continue;
                            }
                            let frame = ServerFrame::Event {
                                seq,
                                event: Event::FsChanged {
                                    workspace: event_workspace.clone(),
                                    path: change.path,
                                    change: change.change,
                                    at: chrono::Utc::now(),
                                },
                            };
                            let Ok(text) = serde_json::to_string(&frame) else {
                                continue;
                            };
                            if tx.send(Outbound::Text(text)).await.is_err() {
                                break;
                            }
                        }
                    });
                    self.watches.insert(workspace, handle);
                    Ok(ResponseBody::FsWatching {
                        workspace: canonical,
                        paths,
                    })
                }
                ClientRequest::FsUnwatch { workspace } => {
                    if let Some(handle) = self.watches.remove(&workspace) {
                        handle.abort();
                    }
                    Ok(ResponseBody::Ack)
                }
                ClientRequest::FsDiff {
                    host,
                    workspace,
                    path,
                    against,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    fs_block(fs, move |fs| fs.diff(&workspace, &path, &against))
                        .await
                        .map(|diff| ResponseBody::FsDiff { diff })
                }
                ClientRequest::FsTurnChanges {
                    host,
                    workspace,
                    thread_id,
                    turn_id,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    fs_block(fs, move |fs| {
                        fs.turn_changes(&workspace, &thread_id, &turn_id)
                    })
                    .await
                    .map(|changes| ResponseBody::FsTurnChanges { changes })
                }
                ClientRequest::BeginUpload {
                    name,
                    size,
                    mime_hint,
                } => self
                    .state
                    .blobs
                    .begin_upload(&name, size, mime_hint)
                    .map(|upload| ResponseBody::UploadStarted { upload })
                    .map_err(BlobError::into_proto),
                ClientRequest::CommitUpload { upload_id, blake3 } => self
                    .state
                    .blobs
                    .commit_upload(&upload_id, &blake3)
                    .await
                    .map(|blob| ResponseBody::UploadCommitted { blob })
                    .map_err(BlobError::into_proto),
                ClientRequest::AbortUpload { upload_id } => self
                    .state
                    .blobs
                    .abort_upload(&upload_id)
                    .map(|()| ResponseBody::Ack)
                    .map_err(BlobError::into_proto),
                ClientRequest::QueryUpload { upload_id } => self
                    .state
                    .blobs
                    .query_upload(&upload_id)
                    .map(|upload| ResponseBody::UploadStatus { upload })
                    .map_err(BlobError::into_proto),
                ClientRequest::GetBlob {
                    id,
                    range,
                    thumbnail,
                } => match self.state.blobs.download(&id, range, thumbnail).await {
                    Ok((download, bytes)) => {
                        let reply = response_frame(
                            reply_id,
                            ResponseBody::Blob {
                                download: download.clone(),
                            },
                        );
                        if self.send(Outbound::Text(reply)).await.is_err() {
                            return None;
                        }
                        for (seq, chunk) in
                            bytes.chunks(crate::blobs::DOWNLOAD_CHUNK_SIZE).enumerate()
                        {
                            let frame = BinaryFrame {
                                kind: BinaryKind::BlobChunk,
                                stream_id: download.stream_id,
                                seq: seq as u64,
                                payload: chunk.to_vec(),
                            }
                            .encode();
                            if self.send(Outbound::Binary(frame)).await.is_err() {
                                return None;
                            }
                        }
                        return None;
                    }
                    Err(error) => Err(error.into_proto()),
                },
                other => Err(not_implemented(&other)),
            },
        };

        Some(match result {
            Ok(body) => response_frame(id, body),
            Err(error) => error_frame(id, error),
        })
    }

    fn require_device(&self) -> Result<DeviceInfo, ProtoError> {
        self.device.clone().ok_or_else(|| {
            ProtoError::new(
                ProtoErrorCode::Unauthorized,
                "the connection has not completed the Hello handshake",
                "Send Hello with the device identity before opening a terminal",
            )
        })
    }

    /// Handles one client-to-daemon binary frame.
    ///
    /// PTY input is the only binary frame a client sends today. It is applied
    /// directly and errors are reported as events because the frame carries no
    /// request id to answer.
    fn handle_binary(&mut self, bytes: &[u8]) -> Option<Outbound> {
        let seq = self.last_seq.load(Ordering::Relaxed);
        if !self.hello_done {
            return Some(Outbound::Text(binary_error_frame(
                seq,
                ProtoError::new(
                    ProtoErrorCode::Unauthorized,
                    "the connection has not completed the Hello handshake",
                    "Send Hello with the device identity before binary frames",
                ),
            )));
        }
        let frame = match BinaryFrame::decode(bytes) {
            Ok(frame) => frame,
            Err(error) => {
                return Some(Outbound::Text(binary_error_frame(
                    seq,
                    ProtoError::new(
                        ProtoErrorCode::InvalidRequest,
                        format!("malformed binary frame: {error}"),
                        format!(
                            "The binary header is {BINARY_HEADER_LEN} bytes; see the protocol docs"
                        ),
                    ),
                )));
            }
        };
        match frame.kind {
            // A viewer typing into a PTY.
            BinaryKind::PtyInput => {
                // Stream ids are small sequential integers, so possession of
                // one proves nothing: the same scope a TerminalAttach needs,
                // and a viewer this very connection attached, are required.
                if let Err(error) = crate::remote::authorize_scope(
                    &self.state.devices,
                    &self.origin,
                    PairScope::Terminal,
                ) {
                    return Some(Outbound::Text(binary_error_frame(seq, error)));
                }
                match self.state.terminals.input_by_stream(
                    frame.stream_id,
                    self.viewer_id,
                    &frame.payload,
                ) {
                    Ok(()) => None,
                    Err(error) => Some(Outbound::Text(binary_error_frame(seq, error.into_proto()))),
                }
            }
            // An attachment uploader sending the next chunk. Acknowledge on the
            // same stream so the sender can advance without a round trip.
            BinaryKind::BlobChunk => {
                if let Err(error) = crate::remote::authorize_scope(
                    &self.state.devices,
                    &self.origin,
                    PairScope::Approve,
                ) {
                    return Some(Outbound::Text(binary_error_frame(seq, error)));
                }
                match self
                    .state
                    .blobs
                    .append_chunk(frame.stream_id, frame.seq, &frame.payload)
                {
                    Ok(_) => Some(Outbound::Binary(
                        BinaryFrame::new(BinaryKind::Ack, frame.stream_id, frame.seq).encode(),
                    )),
                    Err(error) => Some(Outbound::Text(binary_error_frame(seq, error.into_proto()))),
                }
            }
            other => Some(Outbound::Text(binary_error_frame(
                seq,
                ProtoError::new(
                    ProtoErrorCode::InvalidRequest,
                    format!("binary {other:?} frames are not accepted from clients"),
                    "Clients send only pty_input and blob_chunk; output is daemon-to-client",
                ),
            ))),
        }
    }

    async fn send(&self, message: Outbound) -> Result<(), mpsc::error::SendError<Outbound>> {
        self.tx.send(message).await
    }
}

async fn writer_loop<S>(
    mut sink: futures::stream::SplitSink<S, Message>,
    mut rx: mpsc::Receiver<Outbound>,
    overflowed: Arc<AtomicBool>,
    notify: Arc<Notify>,
    last_seq: Arc<AtomicU64>,
) where
    S: Sink<Message> + Unpin + Send + 'static,
{
    loop {
        tokio::select! {
            biased;
            _ = notify.notified(), if overflowed.load(Ordering::Relaxed) => {
                let hint = resume_hint(last_seq.load(Ordering::Relaxed));
                let _ = tokio::time::timeout(WRITE_TIMEOUT, sink.send(Message::Text(hint.into()))).await;
                break;
            }
            message = rx.recv() => {
                let Some(message) = message else { break; };
                match message {
                    Outbound::Text(text) => {
                        let sent = tokio::time::timeout(
                            WRITE_TIMEOUT,
                            sink.send(Message::Text(text.into())),
                        )
                        .await;
                        if sent.is_err() {
                            tracing::warn!("client write timed out; closing the connection");
                            break;
                        }
                    }
                    Outbound::Binary(bytes) => {
                        let sent = tokio::time::timeout(
                            WRITE_TIMEOUT,
                            sink.send(Message::Binary(bytes.into())),
                        )
                        .await;
                        if sent.is_err() {
                            tracing::warn!("client write timed out; closing the connection");
                            break;
                        }
                    }
                }
            }
        }
    }
    let _ = sink.send(Message::Close(None)).await;
}

fn resume_hint(last_seq: u64) -> String {
    let frame = ServerFrame::Event {
        seq: last_seq,
        event: Event::Error {
            thread_id: None,
            message: "this client fell behind the live event stream".into(),
            hint: Some(format!(
                "Reconnect, then call GetThread with since_seq {last_seq} to replay without gaps"
            )),
            retryable: true,
        },
    };
    serde_json::to_string(&frame).unwrap_or_else(|_| {
        r#"{"kind":"event","seq":0,"event":{"type":"error","thread_id":null,"message":"fell behind","hint":null,"retryable":true}}"#
            .to_owned()
    })
}

/// Runs one blocking filesystem operation off the async runtime.
async fn fs_block<T, F>(fs: Arc<FsService>, work: F) -> Result<T, ProtoError>
where
    T: Send + 'static,
    F: FnOnce(&FsService) -> Result<T, FsError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || work(&fs))
        .await
        .map_err(|error| {
            ProtoError::plain(
                ProtoErrorCode::Internal,
                format!("filesystem task failed: {error}"),
            )
        })?
        .map_err(FsError::into_proto)
}

fn response_frame(id: u64, body: ResponseBody) -> String {
    serde_json::to_string(&ServerFrame::Response {
        id,
        result: Ok(body),
    })
    .unwrap_or_default()
}

fn error_frame(id: u64, error: ProtoError) -> String {
    serde_json::to_string(&ServerFrame::Response {
        id,
        result: Err(error),
    })
    .unwrap_or_default()
}

fn binary_error_frame(seq: u64, error: ProtoError) -> String {
    serde_json::to_string(&ServerFrame::Event {
        seq,
        event: Event::Error {
            thread_id: None,
            message: error.message,
            hint: error.hint,
            retryable: false,
        },
    })
    .unwrap_or_default()
}

/// Names a request that this phase intentionally does not implement.
///
/// These are part of the wire contract but no code path serves them yet, so
/// they report [`ProtoErrorCode::NotImplemented`] rather than `Unsupported`
/// (which is reserved for unknown protocol versions and capabilities).
fn not_implemented(request: &ClientRequest) -> ProtoError {
    ProtoError::not_implemented(request_name(request))
}

fn request_name(request: &ClientRequest) -> &'static str {
    match request {
        ClientRequest::FsList { .. } => "fs_list",
        ClientRequest::FsRead { .. } => "fs_read",
        ClientRequest::FsWrite { .. } => "fs_write",
        ClientRequest::McpList => "mcp_list",
        ClientRequest::McpUpsert { .. } => "mcp_upsert",
        ClientRequest::McpRemove { .. } => "mcp_remove",
        ClientRequest::SkillsList { .. } => "skills_list",
        ClientRequest::SkillsSetEnabled { .. } => "skills_set_enabled",
        ClientRequest::ProxyProbe { .. } => "proxy_probe",
        ClientRequest::ProxyConfigure { .. } => "proxy_configure",
        ClientRequest::UsageQuery { .. } => "usage_query",
        ClientRequest::ForgeListAccounts => "forge_list_accounts",
        ClientRequest::ForgeAddAccount { .. } => "forge_add_account",
        ClientRequest::ForgeRemoveAccount { .. } => "forge_remove_account",
        ClientRequest::ForgeDetectRepo { .. } => "forge_detect_repo",
        ClientRequest::ForgeListPrs { .. } => "forge_list_prs",
        ClientRequest::ForgeGetPr { .. } => "forge_get_pr",
        ClientRequest::ForgeGetPrDiff { .. } => "forge_get_pr_diff",
        ClientRequest::ForgeListPrComments { .. } => "forge_list_pr_comments",
        ClientRequest::ForgeListChecks { .. } => "forge_list_checks",
        ClientRequest::ForgeListPrCommits { .. } => "forge_list_pr_commits",
        ClientRequest::ForgeRerunChecks { .. } => "forge_rerun_checks",
        ClientRequest::ListFailoverOptions { .. } => "list_failover_options",
        ClientRequest::FailOver { .. } => "fail_over",
        ClientRequest::ForgeReply { .. } => "forge_reply",
        ClientRequest::ForgeComment { .. } => "forge_comment",
        ClientRequest::ForgeOpenPr { .. } => "forge_open_pr",
        ClientRequest::ForgeHandPrToAgent { .. } => "forge_hand_pr_to_agent",
        _ => "request",
    }
}

/// Serves the router on one listener until `shutdown` resolves.
pub async fn serve_listener(
    listener: TcpListener,
    app: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    // `ConnectInfo` is what lets `/ws` reject non-loopback peers and force
    // them onto the Noise transport.
    serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

/// Wraps a listener address for diagnostics.
pub fn bound_addr(listener: &TcpListener) -> Option<SocketAddr> {
    listener.local_addr().ok()
}

/// Spawns one axum server per listener and waits for a shutdown signal.
pub(crate) async fn serve_all(
    listeners: Vec<TcpListener>,
    app: Router,
    shutdown: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    let (tx, rx) = watch::channel(false);
    let mut handles: Vec<JoinHandle<std::io::Result<()>>> = Vec::new();
    for listener in listeners {
        let app = app.clone();
        let mut rx = rx.clone();
        handles.push(tokio::spawn(async move {
            serve_listener(listener, app, async move {
                let _ = rx.changed().await;
            })
            .await
        }));
    }

    shutdown.await;
    let _ = tx.send(true);
    for handle in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "listener stopped with an error"),
            Err(error) => tracing::warn!(%error, "listener task failed"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_hint_names_the_exact_sequence() {
        let hint = resume_hint(42);
        assert!(hint.contains("since_seq 42"));
        let frame: ServerFrame = serde_json::from_str(&hint).unwrap();
        assert!(matches!(frame, ServerFrame::Event { seq: 42, .. }));
    }

    #[test]
    fn later_phase_requests_are_named() {
        let request = ClientRequest::McpList;
        assert_eq!(request_name(&request), "mcp_list");
        let error = not_implemented(&request);
        assert_eq!(error.code, ProtoErrorCode::NotImplemented);
        assert!(error.message.contains("not implemented"));
        assert!(error.hint.unwrap().contains("mcp_list"));
    }
}
