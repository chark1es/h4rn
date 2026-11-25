//! The relay: authenticated WebSockets, opaque frame routing and presence.
//!
//! `h4rn-connect` never inspects a data payload. It authorises a session when a
//! client sends an [`RelayFrameKind::Open`] frame, remembers which daemon owns
//! the target machine, and from then on forwards bytes by `conn_id` (ADR-010).
//! The only structured frame is the open, whose payload names the machine; the
//! relay fills in the authenticated device id before forwarding, so a client
//! cannot impersonate another device.
//!
//! Logging is metadata-only: conn ids and machine ids are fine, payloads are
//! never formatted.

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::Query;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use chrono::Utc;
use futures::{SinkExt as _, StreamExt as _};
use h4rn_protocol::{
    ConnectDevice, ConnectErrorBody, PresenceUpdate, RelayFrame, RelayFrameKind, RelayOpen,
    RelayServerEvent, RelayService,
};
use serde::Deserialize;
use tokio::sync::{RwLock, mpsc, oneshot};
use uuid::Uuid;

use crate::api::{AppState, Authed, ClientIp};
use crate::error::{AppError, Result};

/// One live peer on the relay.
#[derive(Clone, Debug)]
pub struct Peer {
    /// Device id.
    pub device_id: Uuid,
    /// Owner.
    pub user_id: Uuid,
    /// Machine, for a daemon.
    pub machine_id: Option<Uuid>,
    /// `daemon|desktop|mobile|web`.
    pub kind: String,
    /// Outbound queue.
    pub tx: mpsc::Sender<RelayServerEvent>,
}

/// One client/daemon session.
#[derive(Clone, Debug)]
pub struct Session {
    /// Routing key.
    pub conn_id: String,
    /// Client device.
    pub client_device_id: Uuid,
    /// Daemon device.
    pub daemon_device_id: Uuid,
    /// Machine.
    pub machine_id: Uuid,
    /// Owner.
    pub user_id: Uuid,
}

/// Peers and sessions. All methods are async and lock-free at the await points
/// that send to a peer: the maps are only read for a target id.
#[derive(Debug, Default)]
pub struct RelayHub {
    peers: RwLock<HashMap<Uuid, Peer>>,
    sessions: RwLock<HashMap<String, Session>>,
    /// Guest HTTP exchanges: the browser request waiting on a daemon's answer
    /// and any answer whose body is still arriving in chunks.
    ///
    /// Both live under one lock because a cleanup that removed the waiter but
    /// missed its body would leave a chunked answer assembling forever. One
    /// lock makes a forgotten exchange either wholly present or wholly gone.
    guests: RwLock<GuestBook>,
}

/// A browser request and the chunked answers being assembled for it.
#[derive(Debug, Default)]
struct GuestBook {
    /// Exchanges waiting for a daemon's answer, by request id.
    waiters: HashMap<String, GuestWaiter>,
    /// Answers whose body is still arriving in chunks, by request id.
    bodies: HashMap<String, GuestBody>,
}

/// One browser request waiting for a specific daemon's answer.
#[derive(Debug)]
struct GuestWaiter {
    /// The daemon the exchange was sent to. An answer from any other device is
    /// rejected, so a second daemon cannot answer another machine's guest.
    daemon_device_id: Uuid,
    tx: oneshot::Sender<h4rn_protocol::GuestHttpResponse>,
}

/// A chunked guest answer in progress.
#[derive(Debug)]
struct GuestBody {
    daemon_device_id: Uuid,
    response: h4rn_protocol::GuestHttpResponse,
    next_seq: u64,
}

impl RelayHub {
    /// Creates an empty hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a peer, replacing a stale connection for the same device.
    pub async fn register(&self, peer: Peer) {
        self.peers.write().await.insert(peer.device_id, peer);
    }

    /// Removes a peer and every session it took part in. Returns the sessions
    /// so the caller can tell the other end that it went away.
    pub async fn unregister(&self, device_id: Uuid) -> Vec<Session> {
        self.peers.write().await.remove(&device_id);
        let mut sessions = self.sessions.write().await;
        let dead: Vec<String> = sessions
            .iter()
            .filter(|(_, session)| {
                session.client_device_id == device_id || session.daemon_device_id == device_id
            })
            .map(|(conn_id, _)| conn_id.clone())
            .collect();
        dead.into_iter()
            .filter_map(|conn_id| sessions.remove(&conn_id))
            .collect()
    }

    /// Whether a daemon is connected.
    pub async fn online(&self, device_id: Uuid) -> bool {
        self.peers.read().await.contains_key(&device_id)
    }

    /// Whether the daemon for a machine is connected.
    pub async fn machine_online(&self, machine_id: Uuid) -> bool {
        self.peers
            .read()
            .await
            .values()
            .any(|peer| peer.machine_id == Some(machine_id))
    }

    /// Snapshot of the caller's connected machines.
    pub async fn online_machines(&self, user_id: Uuid) -> std::collections::HashSet<Uuid> {
        self.peers
            .read()
            .await
            .values()
            .filter(|peer| peer.user_id == user_id)
            .filter_map(|peer| peer.machine_id)
            .collect()
    }

    /// Registers a session and forwards its open frame to the daemon.
    pub async fn open(&self, session: Session, frame: RelayFrame) -> Result<()> {
        let conn_id = session.conn_id.clone();
        if self.sessions.read().await.contains_key(&conn_id) {
            return Err(AppError::conflict(
                "conn_id_in_use",
                "a session with this conn_id already exists",
            ));
        }
        self.sessions
            .write()
            .await
            .insert(conn_id.clone(), session.clone());
        if let Err(error) = self
            .send(session.daemon_device_id, RelayServerEvent::Frame { frame })
            .await
        {
            self.sessions.write().await.remove(&conn_id);
            return Err(error);
        }
        Ok(())
    }

    /// Routes a data or close frame to the other end of a session.
    pub async fn route(&self, from_device: Uuid, frame: RelayFrame) -> Result<()> {
        let conn_id = frame.conn_id.clone();
        let session = self
            .sessions
            .read()
            .await
            .get(&conn_id)
            .cloned()
            .ok_or_else(|| {
                AppError::not_found("session_not_found", "no relay session with this conn_id")
            })?;
        let target = if session.client_device_id == from_device {
            session.daemon_device_id
        } else if session.daemon_device_id == from_device {
            session.client_device_id
        } else {
            return Err(AppError::forbidden(
                "session_forbidden",
                "device is not part of this relay session",
            ));
        };
        if frame.kind == RelayFrameKind::Close {
            self.sessions.write().await.remove(&conn_id);
        }
        self.send(target, RelayServerEvent::Frame { frame }).await
    }

    /// Sends an event to one device, with backpressure instead of unbounded
    /// buffering.
    pub async fn send(&self, device_id: Uuid, event: RelayServerEvent) -> Result<()> {
        let tx = self
            .peers
            .read()
            .await
            .get(&device_id)
            .map(|peer| peer.tx.clone())
            .ok_or_else(|| {
                AppError::unavailable("peer_offline", "the other device is not connected")
            })?;
        tx.try_send(event).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => {
                AppError::unavailable("relay_backpressure", "the other device is too slow")
            }
            mpsc::error::TrySendError::Closed(_) => {
                AppError::unavailable("peer_offline", "the other device is not connected")
            }
        })
    }

    /// Sends an event to one device, waiting for queue capacity instead of
    /// failing the moment the peer's queue is momentarily full.
    ///
    /// Guest chunk streams can produce dozens of frames in a burst; a short
    /// backpressure wait keeps an otherwise healthy daemon from seeing a
    /// spurious 503.
    pub async fn send_wait(
        &self,
        device_id: Uuid,
        event: RelayServerEvent,
        wait: Duration,
    ) -> Result<()> {
        let tx = self
            .peers
            .read()
            .await
            .get(&device_id)
            .map(|peer| peer.tx.clone())
            .ok_or_else(|| {
                AppError::unavailable("peer_offline", "the other device is not connected")
            })?;
        match tokio::time::timeout(wait, tx.send(event)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(AppError::unavailable(
                "peer_offline",
                "the other device is not connected",
            )),
            Err(_) => Err(AppError::unavailable(
                "relay_backpressure",
                "the other device is too slow",
            )),
        }
    }

    /// Sends a relay error scoped to a session, ignoring failures.
    pub async fn send_error(
        &self,
        device_id: Uuid,
        conn_id: Option<String>,
        code: &str,
        message: &str,
    ) {
        let _ = self
            .send(
                device_id,
                RelayServerEvent::Error {
                    conn_id,
                    error: ConnectErrorBody::new(code, message, None),
                },
            )
            .await;
    }

    /// Broadcasts an event to every connection of one user.
    pub async fn broadcast_user(&self, user_id: Uuid, event: RelayServerEvent) {
        let targets: Vec<Uuid> = self
            .peers
            .read()
            .await
            .values()
            .filter(|peer| peer.user_id == user_id)
            .map(|peer| peer.device_id)
            .collect();
        for device_id in targets {
            let _ = self.send(device_id, event.clone()).await;
        }
    }

    /// Tells a daemon that a device was paired with its machine.
    pub async fn notify_paired(&self, machine_id: Uuid, device: ConnectDevice) {
        let target = self.daemon_for_machine(machine_id).await;
        if let Some(device_id) = target {
            let _ = self
                .send(
                    device_id,
                    RelayServerEvent::DevicePaired {
                        machine_id: machine_id.to_string(),
                        device,
                    },
                )
                .await;
        }
    }

    /// Tells every daemon paired with a machine that a device was revoked.
    pub async fn notify_revoked(&self, machine_id: Uuid, device_id: Uuid) {
        if let Some(daemon) = self.daemon_for_machine(machine_id).await {
            let _ = self
                .send(
                    daemon,
                    RelayServerEvent::DeviceRevoked {
                        machine_id: machine_id.to_string(),
                        device_id: device_id.to_string(),
                    },
                )
                .await;
        }
    }

    /// Broadcasts presence for a machine to the owner's devices.
    pub async fn broadcast_presence(&self, user_id: Uuid, update: PresenceUpdate) {
        self.broadcast_user(user_id, RelayServerEvent::Presence { presence: update })
            .await;
    }

    async fn daemon_for_machine(&self, machine_id: Uuid) -> Option<Uuid> {
        self.peers
            .read()
            .await
            .values()
            .find(|peer| peer.machine_id == Some(machine_id))
            .map(|peer| peer.device_id)
    }

    /// Registers one guest HTTP exchange before it is sent to a daemon.
    ///
    /// The waiter is bound to that daemon's device id; [`Self::complete_guest`]
    /// ignores an answer from any other device.
    pub async fn expect_guest(
        &self,
        request_id: &str,
        daemon_device_id: Uuid,
    ) -> oneshot::Receiver<h4rn_protocol::GuestHttpResponse> {
        let (tx, rx) = oneshot::channel();
        self.guests.write().await.waiters.insert(
            request_id.to_owned(),
            GuestWaiter {
                daemon_device_id,
                tx,
            },
        );
        rx
    }

    /// Delivers a daemon's guest answer to the waiting browser request.
    ///
    /// Returns false when there is no waiter, or when the answer came from a
    /// device other than the one the request was sent to.
    pub async fn complete_guest(
        &self,
        from_device: Uuid,
        response: h4rn_protocol::GuestHttpResponse,
    ) -> bool {
        let mut book = self.guests.write().await;
        if !book
            .waiters
            .get(&response.request_id)
            .is_some_and(|waiter| waiter.daemon_device_id == from_device)
        {
            tracing::warn!("ignored a guest answer from a device that was not asked");
            return false;
        }
        if response.chunked {
            book.bodies.insert(
                response.request_id.clone(),
                GuestBody {
                    daemon_device_id: from_device,
                    response,
                    next_seq: 0,
                },
            );
            return true;
        }
        let Some(waiter) = book.waiters.remove(&response.request_id) else {
            return false;
        };
        drop(book);
        let _ = waiter.tx.send(response);
        true
    }

    /// Adds one slice to a chunked guest answer, completing it on the last.
    pub async fn complete_guest_chunk(
        &self,
        from_device: Uuid,
        chunk: h4rn_protocol::GuestHttpChunk,
    ) -> bool {
        let mut book = self.guests.write().await;
        let Some(body) = book.bodies.get_mut(&chunk.request_id) else {
            return false;
        };
        if body.daemon_device_id != from_device {
            tracing::warn!("ignored a guest chunk from the wrong device");
            return false;
        }
        if chunk.seq != body.next_seq {
            book.bodies.remove(&chunk.request_id);
            return false;
        }
        body.next_seq += 1;
        if body.response.body.len().saturating_add(chunk.body.len())
            > h4rn_protocol::GUEST_MAX_EXCHANGE_BYTES
        {
            // The answer is unusable; drop both halves together so no chunk can
            // keep assembling and no waiter holds a body that will never finish.
            book.bodies.remove(&chunk.request_id);
            book.waiters.remove(&chunk.request_id);
            return false;
        }
        body.response.body.extend_from_slice(&chunk.body);
        if !chunk.last {
            return true;
        }
        let Some(body) = book.bodies.remove(&chunk.request_id) else {
            return false;
        };
        let Some(waiter) = book.waiters.remove(&chunk.request_id) else {
            return false;
        };
        drop(book);
        let _ = waiter.tx.send(body.response);
        true
    }

    /// Drops a waiter and any half-arrived body that will never be answered.
    pub async fn forget_guest(&self, request_id: &str) {
        let mut book = self.guests.write().await;
        book.waiters.remove(request_id);
        book.bodies.remove(request_id);
    }

    /// How many guest exchanges are waiting for a daemon answer.
    ///
    /// A health check and its tests use this: a failed exchange must leave no
    /// waiter behind.
    pub async fn pending_guests(&self) -> usize {
        self.guests.read().await.waiters.len()
    }

    /// Whether a daemon for this machine is connected right now.
    pub async fn daemon_for(&self, machine_id: Uuid) -> Option<Uuid> {
        self.daemon_for_machine(machine_id).await
    }
}

#[derive(Debug, Deserialize)]
pub struct RelayAuthQuery {
    access_token: Option<String>,
    proof: Option<String>,
    timestamp: Option<i64>,
    nonce: Option<String>,
}

/// `GET /relay`: authenticate, then upgrade.
pub async fn upgrade(
    state: AppState,
    client: ClientIp,
    Query(query): Query<RelayAuthQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    match upgrade_inner(state, client, query, headers, ws).await {
        Ok(response) => Ok(response),
        Err(error) => {
            // Metadata only: the code is stable and contains no token or proof.
            tracing::warn!(
                code = %error.body().code,
                message = %error.body().message,
                "relay connection rejected"
            );
            Err(error)
        }
    }
}

async fn upgrade_inner(
    state: AppState,
    client: ClientIp,
    query: RelayAuthQuery,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    state.rate.check(
        "relay_connect",
        &client.0,
        state.config.rate_limits.relay_connects_per_minute,
        Duration::from_secs(60),
    )?;

    let token = query
        .access_token
        .clone()
        .or_else(|| bearer(&headers))
        .ok_or_else(|| {
            AppError::unauthorized("token_required", "the relay needs an access token")
        })?;
    let proof = query
        .proof
        .clone()
        .or_else(|| header(&headers, "x-h4rn-proof"))
        .ok_or_else(|| AppError::unauthorized("proof_required", "the relay needs a DPoP proof"))?;
    let timestamp = match query.timestamp {
        Some(timestamp) => timestamp,
        None => header(&headers, "x-h4rn-timestamp")
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| {
                AppError::unauthorized("proof_required", "the relay needs a proof timestamp")
            })?,
    };
    let nonce = query
        .nonce
        .clone()
        .or_else(|| header(&headers, "x-h4rn-nonce"))
        .ok_or_else(|| AppError::unauthorized("proof_required", "the relay needs a proof nonce"))?;

    let claims = crate::jwt::verify_access_token(&state.keystore, &token, Utc::now())?;
    let crate::store::Authenticated {
        user,
        device,
        session,
    } = state.store.authenticate(&claims).await?;
    let public_key = crate::crypto::b64(&device.proof_public_key);
    crate::dpop::verify_proof(
        &crate::dpop::Proof {
            public_key: &public_key,
            method: "GET",
            path: "/relay",
            timestamp,
            nonce: &nonce,
            signature: &proof,
        },
        Utc::now(),
        state.config.dpop_max_skew_secs,
    )?;
    if !state
        .replay
        .first_use(&proof, timestamp, Utc::now().timestamp())
    {
        return Err(AppError::unauthorized(
            "proof_replayed",
            "this proof was already used",
        ));
    }
    let scopes = state.store.device_scopes(device.id).await?;
    let authed = Authed {
        claims,
        user,
        device,
        session,
        scopes,
    };

    Ok(ws.on_upgrade(move |socket| async move {
        handle_socket(state, socket, authed).await;
    }))
}

async fn handle_socket(state: AppState, socket: WebSocket, authed: Authed) {
    let device = authed.device.clone();
    let user_id = authed.user.id;
    let machine_id = device.machine_id;
    let kind = device.kind.clone();

    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel(state.config.relay_queue_depth.max(1));
    state
        .relay
        .register(Peer {
            device_id: device.id,
            user_id,
            machine_id,
            kind: kind.clone(),
            tx,
        })
        .await;

    let _ = state.store.touch_device(device.id).await;
    if kind == "daemon" {
        let _ = state
            .store
            .set_presence(device.id, user_id, machine_id, true)
            .await;
        if let Some(machine_id) = machine_id {
            if let Ok(devices) = state.store.paired_device_wires(machine_id).await {
                let _ = state
                    .relay
                    .send(
                        device.id,
                        RelayServerEvent::Snapshot {
                            machine_id: machine_id.to_string(),
                            devices,
                        },
                    )
                    .await;
            }
            let last_seen = Some(Utc::now());
            state
                .relay
                .broadcast_presence(
                    user_id,
                    crate::store::presence_update(machine_id, true, last_seen),
                )
                .await;
        }
        state
            .store
            .audit(
                Some(user_id),
                Some(device.id),
                "relay.connected",
                Some("device"),
                Some(&device.id.to_string()),
                serde_json::json!({"kind": kind, "machine_id": machine_id.map(|id| id.to_string())}),
            )
            .await
            .ok();
    }

    let writer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let Ok(text) = serde_json::to_string(&event) else {
                continue;
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let state_for_loop = state.clone();
    let device_for_loop = authed.clone();
    while let Some(message) = stream.next().await {
        match message {
            Ok(Message::Text(text)) => match serde_json::from_str::<RelayFrame>(&text) {
                Ok(frame) => {
                    if let Err(error) = dispatch(&state_for_loop, &device_for_loop, frame).await {
                        state_for_loop
                            .relay
                            .send_error(device.id, None, &error.body().code, &error.body().message)
                            .await;
                    }
                }
                Err(_) => {
                    state_for_loop
                        .relay
                        .send_error(
                            device.id,
                            None,
                            "invalid_frame",
                            "relay frames must be JSON",
                        )
                        .await;
                }
            },
            Ok(Message::Binary(_)) => {
                state_for_loop
                    .relay
                    .send_error(device.id, None, "invalid_frame", "send text JSON frames")
                    .await;
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }

    let closed = state.relay.unregister(device.id).await;
    for session in closed {
        let other = if session.client_device_id == device.id {
            session.daemon_device_id
        } else {
            session.client_device_id
        };
        let _ = state
            .relay
            .send(
                other,
                RelayServerEvent::Frame {
                    frame: RelayFrame::close(session.conn_id.clone()),
                },
            )
            .await;
    }
    writer.abort();

    if kind == "daemon" {
        let _ = state
            .store
            .set_presence(device.id, user_id, machine_id, false)
            .await;
        if let Some(machine_id) = machine_id {
            state
                .relay
                .broadcast_presence(
                    user_id,
                    crate::store::presence_update(machine_id, false, Some(Utc::now())),
                )
                .await;
        }
        state
            .store
            .audit(
                Some(user_id),
                Some(device.id),
                "relay.disconnected",
                Some("device"),
                Some(&device.id.to_string()),
                serde_json::json!({"machine_id": machine_id.map(|id| id.to_string())}),
            )
            .await
            .ok();
    }
}

async fn dispatch(state: &AppState, authed: &Authed, mut frame: RelayFrame) -> Result<()> {
    match frame.kind {
        RelayFrameKind::Ping => {
            state
                .relay
                .send(
                    authed.device.id,
                    RelayServerEvent::Frame {
                        frame: RelayFrame {
                            conn_id: frame.conn_id,
                            kind: RelayFrameKind::Ping,
                            payload: frame.payload,
                        },
                    },
                )
                .await
        }
        RelayFrameKind::Open => {
            let open: RelayOpen = serde_json::from_slice(&frame.payload).map_err(|_| {
                AppError::bad_request("open_invalid", "the open payload must be RelayOpen JSON")
            })?;
            let machine_id = Uuid::parse_str(&open.machine_id).map_err(|_| {
                AppError::bad_request("machine_invalid", "machine_id is not a UUID")
            })?;
            let is_daemon = authed.device.kind == "daemon";
            match (is_daemon, open.service) {
                (true, RelayService::Secure) => {
                    return Err(AppError::forbidden(
                        "open_forbidden",
                        "a daemon may only open a peer MCP relay session",
                    ));
                }
                (false, RelayService::SecureMcp) => {
                    // `/secure-mcp` admits peer daemon keys only. Letting a
                    // user device open it would still fail its handshake, but
                    // rejecting the route here keeps the relay's guarantees
                    // explicit.
                    return Err(AppError::forbidden(
                        "open_forbidden",
                        "only a daemon may open the peer MCP relay service",
                    ));
                }
                _ => {}
            }
            let machine = state.store.machine(machine_id).await?;
            if is_daemon {
                // Cross-machine sharing is between two machines of the same
                // account. The owner's `/secure-mcp` still authorises the
                // caller by its registered peer key, so this relay check only
                // narrows who may ask.
                if machine.user_id != authed.user.id {
                    return Err(AppError::forbidden(
                        "machine_forbidden",
                        "the machine belongs to another account",
                    ));
                }
                if authed.device.machine_id == Some(machine_id) {
                    return Err(AppError::forbidden(
                        "machine_forbidden",
                        "a daemon may not open a peer session to its own machine",
                    ));
                }
            } else {
                let allowed = state
                    .store
                    .device_may_reach(authed.user.id, authed.device.id, machine_id)
                    .await?;
                if !allowed {
                    return Err(AppError::forbidden(
                        "machine_forbidden",
                        "this device is not paired with the machine",
                    ));
                }
            }
            let daemon_device_id = machine.daemon_device_id.ok_or_else(|| {
                AppError::unavailable("daemon_missing", "the machine has no daemon")
            })?;
            if !state.relay.online(daemon_device_id).await {
                return Err(AppError::unavailable(
                    "daemon_offline",
                    "the machine's daemon is not connected",
                ));
            }

            let forwarded = RelayOpen {
                machine_id: machine_id.to_string(),
                device_id: Some(authed.device.id.to_string()),
                service: open.service,
            };
            frame.payload = serde_json::to_vec(&forwarded)
                .map_err(|_| AppError::internal("serialise RelayOpen"))?;
            let conn_id = frame.conn_id.clone();
            state
                .relay
                .open(
                    Session {
                        conn_id: conn_id.clone(),
                        client_device_id: authed.device.id,
                        daemon_device_id,
                        machine_id,
                        user_id: authed.user.id,
                    },
                    frame,
                )
                .await?;
            state
                .store
                .audit(
                    Some(authed.user.id),
                    Some(authed.device.id),
                    "relay.opened",
                    Some("machine"),
                    Some(&machine_id.to_string()),
                    serde_json::json!({
                        "conn_id": conn_id,
                        "service": open.service.as_str(),
                    }),
                )
                .await
                .ok();
            Ok(())
        }
        RelayFrameKind::Data | RelayFrameKind::Close => {
            let conn_id = frame.conn_id.clone();
            match state.relay.route(authed.device.id, frame).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    state
                        .relay
                        .send_error(
                            authed.device.id,
                            Some(conn_id),
                            &error.body().code,
                            &error.body().message,
                        )
                        .await;
                    Ok(())
                }
            }
        }
        RelayFrameKind::Activity => {
            // Only the machine's daemon observes a thread, and only a daemon
            // may wake a phone. A client device's frame is ignored rather than
            // treated as an alert for an account it may share.
            if authed.device.kind != "daemon" {
                return Ok(());
            }
            let activity: h4rn_protocol::AgentActivity = serde_json::from_slice(&frame.payload)
                .map_err(|_| {
                    AppError::bad_request(
                        "activity_invalid",
                        "the activity payload must be AgentActivity JSON",
                    )
                })?;
            state.alerts.ingest(authed.user.id, activity).await;
            Ok(())
        }
        RelayFrameKind::GuestHttp => {
            // Only a daemon answers a guest exchange; a client device has no
            // waiting request and its frame is ignored. The waiter is bound to
            // this device id, so another daemon cannot answer it either.
            if authed.device.kind != "daemon" {
                return Ok(());
            }
            if let Ok(response) =
                serde_json::from_slice::<h4rn_protocol::GuestHttpResponse>(&frame.payload)
            {
                state.relay.complete_guest(authed.device.id, response).await;
            }
            Ok(())
        }
        RelayFrameKind::GuestHttpChunk => {
            if authed.device.kind != "daemon" {
                return Ok(());
            }
            if let Ok(chunk) =
                serde_json::from_slice::<h4rn_protocol::GuestHttpChunk>(&frame.payload)
            {
                state
                    .relay
                    .complete_guest_chunk(authed.device.id, chunk)
                    .await;
            }
            Ok(())
        }
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(ToOwned::to_owned)
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(
        device_id: Uuid,
        user_id: Uuid,
        machine_id: Option<Uuid>,
        depth: usize,
    ) -> (Peer, mpsc::Receiver<RelayServerEvent>) {
        let (tx, rx) = mpsc::channel(depth);
        (
            Peer {
                device_id,
                user_id,
                machine_id,
                kind: if machine_id.is_some() {
                    "daemon"
                } else {
                    "desktop"
                }
                .to_string(),
                tx,
            },
            rx,
        )
    }

    #[tokio::test]
    async fn open_and_data_route_between_peers() {
        let hub = RelayHub::new();
        let machine = Uuid::now_v7();
        let daemon = Uuid::now_v7();
        let client = Uuid::now_v7();
        let user = Uuid::now_v7();
        let (daemon_peer, mut daemon_rx) = peer(daemon, user, Some(machine), 8);
        let (client_peer, mut client_rx) = peer(client, user, None, 8);
        hub.register(daemon_peer).await;
        hub.register(client_peer).await;
        assert!(hub.machine_online(machine).await);

        let session = Session {
            conn_id: "c1".to_string(),
            client_device_id: client,
            daemon_device_id: daemon,
            machine_id: machine,
            user_id: user,
        };
        hub.open(session, RelayFrame::open("c1", machine.to_string()))
            .await
            .unwrap();
        let open = daemon_rx.recv().await.unwrap();
        assert!(
            matches!(open, RelayServerEvent::Frame { frame } if frame.kind == RelayFrameKind::Open)
        );

        hub.route(client, RelayFrame::data("c1", b"ping".to_vec()))
            .await
            .unwrap();
        let forwarded = daemon_rx.recv().await.unwrap();
        match forwarded {
            RelayServerEvent::Frame { frame } => {
                assert_eq!(frame.kind, RelayFrameKind::Data);
                assert_eq!(frame.payload, b"ping");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        hub.route(daemon, RelayFrame::data("c1", b"pong".to_vec()))
            .await
            .unwrap();
        let back = client_rx.recv().await.unwrap();
        assert!(matches!(back, RelayServerEvent::Frame { frame } if frame.payload == b"pong"));

        // A third device cannot inject into someone else's session.
        let outsider = Uuid::now_v7();
        let (outsider_peer, _) = peer(outsider, user, None, 2);
        hub.register(outsider_peer).await;
        let error = hub
            .route(outsider, RelayFrame::data("c1", b"x".to_vec()))
            .await
            .unwrap_err();
        assert_eq!(error.body().code, "session_forbidden");

        // Closing removes the session.
        hub.route(client, RelayFrame::close("c1")).await.unwrap();
        let closed = daemon_rx.recv().await.unwrap();
        assert!(
            matches!(closed, RelayServerEvent::Frame { frame } if frame.kind == RelayFrameKind::Close)
        );
        assert_eq!(
            hub.route(client, RelayFrame::data("c1", b"x".to_vec()))
                .await
                .unwrap_err()
                .body()
                .code,
            "session_not_found"
        );
    }

    #[tokio::test]
    async fn unregister_reports_sessions_and_clears_presence() {
        let hub = RelayHub::new();
        let machine = Uuid::now_v7();
        let daemon = Uuid::now_v7();
        let client = Uuid::now_v7();
        let user = Uuid::now_v7();
        let (daemon_peer, _daemon_rx) = peer(daemon, user, Some(machine), 8);
        let (client_peer, _client_rx) = peer(client, user, None, 8);
        hub.register(daemon_peer).await;
        hub.register(client_peer).await;
        hub.open(
            Session {
                conn_id: "c1".to_string(),
                client_device_id: client,
                daemon_device_id: daemon,
                machine_id: machine,
                user_id: user,
            },
            RelayFrame::open("c1", machine.to_string()),
        )
        .await
        .unwrap();
        let sessions = hub.unregister(daemon).await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].conn_id, "c1");
        assert!(!hub.machine_online(machine).await);
        assert!(hub.online_machines(user).await.is_empty());
    }

    #[tokio::test]
    async fn a_full_queue_is_backpressure_not_an_unbounded_buffer() {
        let hub = RelayHub::new();
        let device = Uuid::now_v7();
        let (slow_peer, _rx) = peer(device, Uuid::now_v7(), None, 1);
        hub.register(slow_peer).await;
        hub.send(
            device,
            RelayServerEvent::Presence {
                presence: PresenceUpdate {
                    machine_id: Uuid::now_v7().to_string(),
                    online: true,
                    last_seen: None,
                },
            },
        )
        .await
        .unwrap();
        let error = hub
            .send(
                device,
                RelayServerEvent::Presence {
                    presence: PresenceUpdate {
                        machine_id: Uuid::now_v7().to_string(),
                        online: false,
                        last_seen: None,
                    },
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.body().code, "relay_backpressure");
    }

    #[tokio::test]
    async fn a_guest_exchange_is_bound_to_the_daemon_it_was_sent_to() {
        let hub = RelayHub::new();
        let daemon = Uuid::now_v7();
        let other = Uuid::now_v7();
        let waiter = hub.expect_guest("g1", daemon).await;
        // A different daemon claiming the same request id cannot answer it.
        assert!(
            !hub.complete_guest(
                other,
                h4rn_protocol::GuestHttpResponse {
                    request_id: "g1".into(),
                    status: 200,
                    chunked: false,
                    headers: Vec::new(),
                    body: b"stolen".to_vec(),
                    set_cookie: None,
                }
            )
            .await
        );
        // Nor can a stray chunk.
        assert!(
            !hub.complete_guest_chunk(
                other,
                h4rn_protocol::GuestHttpChunk {
                    request_id: "g1".into(),
                    seq: 0,
                    last: true,
                    body: b"stolen".to_vec(),
                }
            )
            .await
        );
        assert!(
            hub.complete_guest(
                daemon,
                h4rn_protocol::GuestHttpResponse {
                    request_id: "g1".into(),
                    status: 200,
                    chunked: false,
                    headers: Vec::new(),
                    body: b"hello".to_vec(),
                    set_cookie: None,
                }
            )
            .await
        );
        let answer = waiter.await.expect("answer");
        assert_eq!(answer.status, 200);
        assert_eq!(answer.body, b"hello");
    }

    #[tokio::test]
    async fn a_chunked_guest_answer_is_reassembled_in_order() {
        let hub = RelayHub::new();
        let daemon = Uuid::now_v7();
        let waiter = hub.expect_guest("g1", daemon).await;
        assert!(
            hub.complete_guest(
                daemon,
                h4rn_protocol::GuestHttpResponse {
                    request_id: "g1".into(),
                    status: 200,
                    chunked: true,
                    headers: Vec::new(),
                    body: Vec::new(),
                    set_cookie: None,
                }
            )
            .await
        );
        for (seq, last, text) in [(0, false, "hel"), (1, true, "lo")] {
            assert!(
                hub.complete_guest_chunk(
                    daemon,
                    h4rn_protocol::GuestHttpChunk {
                        request_id: "g1".into(),
                        seq,
                        last,
                        body: text.as_bytes().to_vec(),
                    }
                )
                .await
            );
        }
        let answer = waiter.await.expect("answer");
        assert_eq!(answer.body, b"hello");
    }

    #[tokio::test]
    async fn a_forgotten_guest_waiter_never_receives_an_answer() {
        let hub = RelayHub::new();
        let daemon = Uuid::now_v7();
        let dropped = hub.expect_guest("g2", daemon).await;
        hub.forget_guest("g2").await;
        assert!(
            !hub.complete_guest(
                daemon,
                h4rn_protocol::GuestHttpResponse {
                    request_id: "g2".into(),
                    status: 204,
                    chunked: false,
                    headers: Vec::new(),
                    body: Vec::new(),
                    set_cookie: None,
                }
            )
            .await
        );
        assert!(dropped.await.is_err(), "the forgotten waiter got an answer");
    }

    fn chunked_answer(request_id: &str) -> h4rn_protocol::GuestHttpResponse {
        h4rn_protocol::GuestHttpResponse {
            request_id: request_id.into(),
            status: 200,
            chunked: true,
            headers: Vec::new(),
            body: Vec::new(),
            set_cookie: None,
        }
    }

    #[tokio::test]
    async fn forgetting_a_half_arrived_chunked_answer_leaves_no_body() {
        let hub = RelayHub::new();
        let daemon = Uuid::now_v7();
        let waiter = hub.expect_guest("g1", daemon).await;
        assert!(
            hub.complete_guest(daemon, chunked_answer("g1")).await,
            "the chunked answer did not start"
        );
        assert!(
            hub.complete_guest_chunk(
                daemon,
                h4rn_protocol::GuestHttpChunk {
                    request_id: "g1".into(),
                    seq: 0,
                    last: false,
                    body: b"half".to_vec(),
                }
            )
            .await
        );

        // Both proxy error paths call `forget_guest`; the second call is the
        // other path racing the same exchange.
        hub.forget_guest("g1").await;
        hub.forget_guest("g1").await;
        assert_eq!(hub.pending_guests().await, 0);
        assert!(
            hub.guests.read().await.bodies.is_empty(),
            "a forgotten answer kept its half-arrived body"
        );
        // A late chunk cannot resurrect the exchange.
        assert!(
            !hub.complete_guest_chunk(
                daemon,
                h4rn_protocol::GuestHttpChunk {
                    request_id: "g1".into(),
                    seq: 1,
                    last: true,
                    body: b"rest".to_vec(),
                }
            )
            .await
        );
        assert!(waiter.await.is_err(), "a forgotten waiter got an answer");
    }

    #[tokio::test]
    async fn an_answer_arriving_after_the_cleanup_is_dropped_whole() {
        let hub = RelayHub::new();
        let daemon = Uuid::now_v7();
        let waiter = hub.expect_guest("g3", daemon).await;
        // The cleanup runs first: the error path that failed before the daemon
        // could answer wins the race.
        hub.forget_guest("g3").await;
        assert!(
            !hub.complete_guest(daemon, chunked_answer("g3")).await,
            "an answer started after its exchange was forgotten"
        );
        assert!(
            hub.guests.read().await.bodies.is_empty(),
            "the forgotten exchange still holds a body"
        );
        assert!(waiter.await.is_err(), "the forgotten waiter got an answer");
    }
}
