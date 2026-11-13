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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header::SEC_WEBSOCKET_PROTOCOL};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, serve};
use futures::{SinkExt, StreamExt};
use h4rn_protocol::{
    BinaryFrame, ClientFrame, ClientRequest, Event, PROTOCOL_VERSION, ProtoError, ProtoErrorCode,
    ResponseBody, ServerFrame, Topic,
};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

use crate::auth::{TokenStore, authorize};
use crate::hosts::HostInventory;
use crate::threads::ThreadManager;

/// Capacity of one connection's outbound queue.
pub const OUTBOUND_CAPACITY: usize = 128;

/// How long one frame may take to reach a client before it is considered dead.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

/// Shared state for the axum router.
#[derive(Clone)]
pub struct AppState {
    /// Thread manager and live sessions.
    pub manager: Arc<ThreadManager>,
    /// Host inventory.
    pub hosts: Arc<HostInventory>,
    /// Local access token.
    pub tokens: TokenStore,
    /// Server boot time, for `/healthz`.
    pub started: Instant,
}

/// Builds the daemon router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_handler))
        .with_state(state)
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
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
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
    upgrade.on_upgrade(move |socket| handle_socket(socket, state))
}

#[derive(Debug)]
enum Outbound {
    Text(String),
}

struct Connection {
    state: AppState,
    hello_done: bool,
    topics: Arc<RwLock<Vec<Topic>>>,
    ready: Arc<AtomicBool>,
    last_seq: Arc<AtomicU64>,
    tx: mpsc::Sender<Outbound>,
}

async fn handle_socket(socket: WebSocket, state: AppState) {
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

    let subscription = state.manager.subscribe();
    let forward = tokio::spawn(forward_loop(
        subscription,
        tx.clone(),
        Arc::clone(&topics),
        Arc::clone(&ready),
        Arc::clone(&overflowed),
        Arc::clone(&notify),
        Arc::clone(&last_seq),
    ));

    let mut connection = Connection {
        state,
        hello_done: false,
        topics,
        ready,
        last_seq,
        tx: tx.clone(),
    };

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
                let reply = match BinaryFrame::decode(&bytes) {
                    Ok(frame) => ServerFrame::Event {
                        seq: connection.last_seq.load(Ordering::Relaxed),
                        event: Event::Error {
                            thread_id: None,
                            message: format!(
                                "binary {:?} frames are reserved for PTY and blob streams",
                                frame.kind
                            ),
                            hint: Some("Terminal support arrives in phase 2".into()),
                            retryable: false,
                        },
                    },
                    Err(error) => ServerFrame::Event {
                        seq: connection.last_seq.load(Ordering::Relaxed),
                        event: Event::Error {
                            thread_id: None,
                            message: format!("malformed binary frame: {error}"),
                            hint: Some("The binary header is 16 bytes; see protocol docs".into()),
                            retryable: false,
                        },
                    },
                };
                if let Ok(text) = serde_json::to_string(&reply)
                    && connection.send(Outbound::Text(text)).await.is_err()
                {
                    break;
                }
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    forward.abort();
    drop(connection);
    drop(tx);
    let _ = writer.await;
}

impl Connection {
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

        let result: Result<ResponseBody, ProtoError> = match body {
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
                    *self
                        .topics
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = vec![Topic::All];
                    Ok(ResponseBody::Hello {
                        protocol_version: PROTOCOL_VERSION,
                        server_version: crate::version().to_owned(),
                        device_id: device.device_id,
                        hosts: self.state.hosts.summaries(),
                    })
                }
            }
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
                        .map(|event| event.event)
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
                attachments: _,
            } => {
                let request = crate::threads::NewThread {
                    host,
                    harness,
                    workspace_path,
                    model,
                    prompt,
                };
                // The protocol has no dedicated CreateThread response yet, so
                // the new thread's summary is returned in `ResponseBody::Thread`.
                self.state
                    .manager
                    .create_thread(request)
                    .await
                    .map(|summary| ResponseBody::Thread {
                        summary,
                        events: Vec::new(),
                        next_seq: 0,
                    })
                    .map_err(crate::threads::ManagerError::into_proto)
            }
            ClientRequest::SendMessage {
                thread_id,
                text,
                attachments: _,
            } => self
                .state
                .manager
                .send_message(&thread_id, text)
                .await
                .map(|()| ResponseBody::Ack)
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
            } => self
                .state
                .manager
                .respond_approval(&thread_id, &approval_id, decision)
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
            ClientRequest::Subscribe { topics } => {
                *self
                    .topics
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = topics;
                Ok(ResponseBody::Ack)
            }
            other => Err(not_implemented(&other)),
        };

        Some(match result {
            Ok(body) => response_frame(id, body),
            Err(error) => error_frame(id, error),
        })
    }

    async fn send(&self, message: Outbound) -> Result<(), mpsc::error::SendError<Outbound>> {
        self.tx.send(message).await
    }
}

async fn forward_loop(
    mut subscription: tokio::sync::broadcast::Receiver<crate::journal::WireEvent>,
    tx: mpsc::Sender<Outbound>,
    topics: Arc<RwLock<Vec<Topic>>>,
    ready: Arc<AtomicBool>,
    overflowed: Arc<AtomicBool>,
    notify: Arc<Notify>,
    last_seq: Arc<AtomicU64>,
) {
    loop {
        let event = match subscription.recv().await {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "client fell behind the event broadcast");
                overflowed.store(true, Ordering::Relaxed);
                notify.notify_waiters();
                return;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };

        if !ready.load(Ordering::Relaxed) {
            continue;
        }
        let subscribed = {
            let topics = topics
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            topics
                .iter()
                .any(|topic| matches!(topic, Topic::All | Topic::Threads))
        };
        if !subscribed {
            continue;
        }

        last_seq.store(event.seq, Ordering::Relaxed);
        let frame = ServerFrame::Event {
            seq: event.seq,
            event: event.event,
        };
        let Ok(text) = serde_json::to_string(&frame) else {
            continue;
        };
        match tx.try_send(Outbound::Text(text)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("client outbound queue is full; dropping the connection");
                overflowed.store(true, Ordering::Relaxed);
                notify.notify_waiters();
                return;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return,
        }
    }
}

async fn writer_loop(
    mut sink: futures::stream::SplitSink<WebSocket, Message>,
    mut rx: mpsc::Receiver<Outbound>,
    overflowed: Arc<AtomicBool>,
    notify: Arc<Notify>,
    last_seq: Arc<AtomicU64>,
) {
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

/// Names a request that this phase intentionally does not implement.
///
/// The protocol error enum has no `not_implemented` code yet, so these use
/// `Unsupported` with a `not_implemented:` message prefix (see the hand-off
/// notes); unknown later-phase requests never reach a handler.
fn not_implemented(request: &ClientRequest) -> ProtoError {
    let name = request_name(request);
    ProtoError::new(
        ProtoErrorCode::Unsupported,
        format!("not_implemented: {name} is not implemented in this daemon"),
        format!("Upgrade the daemon when {name} ships; no state was changed"),
    )
}

fn request_name(request: &ClientRequest) -> &'static str {
    match request {
        ClientRequest::TerminalOpen { .. } => "terminal_open",
        ClientRequest::TerminalInput { .. } => "terminal_input",
        ClientRequest::TerminalResize { .. } => "terminal_resize",
        ClientRequest::TerminalClose { .. } => "terminal_close",
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
        ClientRequest::ForgeListPrs { .. } => "forge_list_prs",
        ClientRequest::ForgeCreatePr { .. } => "forge_create_pr",
        _ => "request",
    }
}

/// Serves the router on one listener until `shutdown` resolves.
pub async fn serve_listener(
    listener: TcpListener,
    app: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    serve(listener, app).with_graceful_shutdown(shutdown).await
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
        assert_eq!(error.code, ProtoErrorCode::Unsupported);
        assert!(error.message.contains("not_implemented"));
        assert!(error.hint.unwrap().contains("mcp_list"));
    }
}
