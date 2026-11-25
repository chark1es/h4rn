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
use std::sync::Arc;
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
use tokio::sync::{Notify, RwLock, mpsc, oneshot};
use uuid::Uuid;

use crate::api::{AppState, Authed, ClientIp};
use crate::error::{AppError, Result};

/// One live peer on the relay.
#[derive(Clone, Debug)]
pub struct Peer {
    /// Device id.
    pub device_id: Uuid,
    /// Identity of this WebSocket. A hello or close from an older socket
    /// cannot speak for the connection that replaced it.
    pub connection_id: Uuid,
    /// Whether this socket promised a relay hello in its connect query. A
    /// connection that does not is a build that predates hellos.
    pub announces_hello: bool,
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
    /// The client's socket when the session opened.
    ///
    /// A session belongs to the two sockets that opened it, not only to their
    /// devices: a reconnect replaces the socket, and a session left behind must
    /// not route to the replacement under its old `conn_id`.
    pub client_connection: Uuid,
    /// The daemon's socket when the session opened.
    pub daemon_connection: Uuid,
}

/// What closing one socket did to the relay.
pub struct Unregistered {
    /// Whether this socket was still the device's live peer. Only a live
    /// socket's close says anything about the device's presence.
    pub live: bool,
    /// Sessions opened on this socket. The caller tells each partner they are
    /// gone; the device's own presence is untouched when `live` is false.
    pub sessions: Vec<Session>,
}

/// Peers and sessions. All methods are async and lock-free at the await points
/// that send to a peer: the maps are only read for a target id.
#[derive(Debug, Default)]
pub struct RelayHub {
    peers: RwLock<HashMap<Uuid, Peer>>,
    sessions: RwLock<HashMap<String, Session>>,
    /// The generation of each device's current connection, bumped on every
    /// register. A presence write reads it under the device's lock, so a socket
    /// a reconnect already replaced cannot report the device offline after the
    /// replacement published it online.
    generations: RwLock<HashMap<Uuid, u64>>,
    /// One presence lock per device: presence is read, checked and written as
    /// one step, so online and offline updates cannot pass each other.
    presence_locks: std::sync::Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>,
    /// Guest HTTP exchanges: the browser request waiting on a daemon's answer
    /// and any answer whose body is still arriving in chunks.
    ///
    /// Both live under one lock because a cleanup that removed the waiter but
    /// missed its body would leave a chunked answer assembling forever. One
    /// lock makes a forgotten exchange either wholly present or wholly gone.
    guests: RwLock<GuestBook>,
    /// What each connected peer said, or promised to say, on its relay hello.
    ///
    /// The state belongs to one WebSocket, not to the device: a reconnecting
    /// daemon starts again from what its own handshake promised, so a stale
    /// hello from the previous socket can neither announce for it nor hold an
    /// upload open. A daemon that did not announce a feature keeps the older
    /// path for it; nothing here is inferred from how long a peer takes to
    /// answer.
    features: RwLock<HashMap<Uuid, PeerHello>>,
    /// Wakes every guest exchange that is waiting for a peer's hello.
    greeted_now: Notify,
}

/// The relay's hello state for one peer connection.
#[derive(Clone, Debug)]
struct PeerHello {
    /// The socket this state belongs to.
    connection: Uuid,
    state: HelloState,
}

/// What a peer connection has said about its hello.
#[derive(Clone, Debug)]
enum HelloState {
    /// The connect query promised a hello on this socket.
    Expected,
    /// The hello arrived, with the features it named.
    Greeted(Vec<String>),
    /// The socket was opened by a build that predates hellos.
    PreHello,
}

/// What a connected daemon can do about a chunked guest upload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestAdmission {
    /// This connection's hello names the admission feature.
    Announced,
    /// This connection predates hellos, or its hello omitted the feature.
    Legacy,
    /// This connection promised a hello that has not arrived (or went away
    /// before it did). An upload must not stream in this state.
    Unknown,
}

/// One daemon connection a guest exchange is pinned to.
///
/// A device may reconnect while a browser request is in flight, and the new
/// socket starts with its own hello and no admission. An exchange therefore
/// names the exact connection it began with: capability, admission, its request
/// frames and every body chunk travel to that socket or the exchange fails as
/// retryable, never to a socket the daemon has only just opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DaemonLink {
    /// The daemon device.
    pub device_id: Uuid,
    /// The WebSocket this exchange belongs to.
    pub connection_id: Uuid,
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
    /// The daemon connection the exchange was sent to. An answer from another
    /// device, or from a later socket of the same daemon, is rejected: neither
    /// can describe an exchange it never received.
    daemon: DaemonLink,
    tx: oneshot::Sender<h4rn_protocol::GuestHttpResponse>,
    /// Sent once the daemon accepts a chunked body; dropped untouched when the
    /// daemon refuses, so the proxy never uploads a byte it was not asked for.
    admission: Option<oneshot::Sender<()>>,
}

/// One guest exchange as the proxy sees it: the daemon's go-ahead and its
/// answer.
pub struct GuestExchange {
    /// Resolves when the daemon accepts the upload, or closes when it answers
    /// first (a refusal) or forgets the exchange.
    pub admission: oneshot::Receiver<()>,
    /// The daemon's final answer.
    pub response: oneshot::Receiver<h4rn_protocol::GuestHttpResponse>,
}

/// A chunked guest answer in progress.
#[derive(Debug)]
struct GuestBody {
    daemon: DaemonLink,
    response: h4rn_protocol::GuestHttpResponse,
    next_seq: u64,
}

impl RelayHub {
    /// Creates an empty hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a peer, replacing a stale connection for the same device.
    ///
    /// The replacement also resets what the relay knows about this peer's
    /// hello: capability is a property of the socket, and the new socket has
    /// not spoken yet. Returns the generation this connection now owns; a
    /// presence write checks it so an older socket cannot publish after it.
    pub async fn register(&self, peer: Peer) -> u64 {
        let device_id = peer.device_id;
        let connection = peer.connection_id;
        let state = if peer.announces_hello {
            HelloState::Expected
        } else {
            HelloState::PreHello
        };
        // Publish the new capability before the peer becomes routable. A
        // guest request in this gap must see "unknown", never the previous
        // socket's hello.
        self.features
            .write()
            .await
            .insert(device_id, PeerHello { connection, state });
        self.peers.write().await.insert(device_id, peer);
        let generation = {
            let mut generations = self.generations.write().await;
            let generation = generations.get(&device_id).copied().unwrap_or(0) + 1;
            generations.insert(device_id, generation);
            generation
        };
        // A waiter for the previous connection re-reads the new one, which
        // starts from what its own handshake promised.
        self.greeted_now.notify_waiters();
        generation
    }

    /// The generation of one device's current connection.
    pub async fn generation(&self, device_id: Uuid) -> u64 {
        self.generations
            .read()
            .await
            .get(&device_id)
            .copied()
            .unwrap_or(0)
    }

    /// Whether `generation` still owns this device's presence.
    ///
    /// A presence write asks this while holding the device's presence lock: a
    /// connection that has been replaced must not publish online or offline
    /// after the replacement did.
    pub async fn owns_presence(&self, device_id: Uuid, generation: u64) -> bool {
        self.generation(device_id).await == generation
    }

    /// Waits for the device's presence lock.
    ///
    /// The caller holds it across the presence read, its generation check and
    /// the write, so a closing socket and its replacement cannot interleave
    /// their online/offline updates.
    pub async fn presence_guard(&self, device_id: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self
                .presence_locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(
                locks
                    .entry(device_id)
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        lock.lock_owned().await
    }

    /// Removes a peer and the sessions opened on its socket. Returns what that
    /// close means for the caller: whether the device went away, and the
    /// sessions whose partners must be told.
    ///
    /// Only the socket that is still the registered one reports the device
    /// offline: an old connection closing after a reconnect must not erase the
    /// new connection's capability or presence. Its sessions still go, though:
    /// they were opened on a socket that is gone, and leaving them would route
    /// frames to the replacement under a `conn_id` it never opened.
    pub async fn unregister(&self, device_id: Uuid, connection: Uuid) -> Unregistered {
        // The check and the removal are one critical section, so a socket that
        // reconnects in this instant cannot be erased by the old one closing.
        let live = {
            let mut peers = self.peers.write().await;
            match peers.get(&device_id) {
                Some(peer) if peer.connection_id != connection => false,
                Some(_) => {
                    peers.remove(&device_id);
                    true
                }
                None => false,
            }
        };
        if live {
            let mut features = self.features.write().await;
            // A newer connection may have registered since the peers lock was
            // released; only its own hello state may be removed.
            if features
                .get(&device_id)
                .is_some_and(|hello| hello.connection == connection)
            {
                features.remove(&device_id);
            }
        }
        // A waiter that was still hoping for this peer's hello re-checks and
        // keeps waiting for the grace it was given; no one is left asleep.
        self.greeted_now.notify_waiters();
        let mut sessions = self.sessions.write().await;
        let dead: Vec<String> = sessions
            .iter()
            .filter(|(_, session)| {
                session.client_connection == connection || session.daemon_connection == connection
            })
            .map(|(conn_id, _)| conn_id.clone())
            .collect();
        Unregistered {
            live,
            sessions: dead
                .into_iter()
                .filter_map(|conn_id| sessions.remove(&conn_id))
                .collect(),
        }
    }

    /// Tells each session's other end that the session is gone.
    ///
    /// `closed_connection` names the socket that went away, so the partner is
    /// the other party, never the device that is closing.
    pub async fn close_partners(&self, closed_connection: Uuid, sessions: Vec<Session>) {
        for session in sessions {
            let other = if session.client_connection == closed_connection {
                session.daemon_device_id
            } else {
                session.client_device_id
            };
            let _ = self
                .send(
                    other,
                    RelayServerEvent::Frame {
                        frame: RelayFrame::close(session.conn_id.clone()),
                    },
                )
                .await;
        }
    }

    /// Whether a daemon is connected.
    pub async fn online(&self, device_id: Uuid) -> bool {
        self.peers.read().await.contains_key(&device_id)
    }

    /// Records the features a peer announced on its relay hello.
    ///
    /// The hello names the socket that sent it, so a late hello from a socket
    /// that was replaced cannot announce for the live one. Every waiter is
    /// woken, not just one: two guest requests may be waiting on the same
    /// peer.
    pub async fn announce(
        &self,
        device_id: Uuid,
        connection: Uuid,
        hello: h4rn_protocol::RelayHello,
    ) {
        let mut features = self.features.write().await;
        let Some(peer) = features.get_mut(&device_id) else {
            return;
        };
        if peer.connection != connection {
            return;
        }
        peer.state = HelloState::Greeted(hello.features);
        drop(features);
        self.greeted_now.notify_waiters();
    }

    /// Whether one connected peer announced a feature.
    ///
    /// A peer that is gone, that has not sent its hello yet, or whose socket
    /// predates hellos advertises nothing.
    pub async fn advertises(&self, device_id: Uuid, feature: &str) -> bool {
        self.features
            .read()
            .await
            .get(&device_id)
            .is_some_and(|peer| match &peer.state {
                HelloState::Greeted(features) => features.iter().any(|known| known == feature),
                HelloState::Expected | HelloState::PreHello => false,
            })
    }

    /// The daemon's live connection, if it is online.
    ///
    /// A guest exchange names this link, so every frame it sends afterwards is
    /// addressed to the socket that existed when it began instead of whichever
    /// one happens to be current.
    pub async fn link(&self, device_id: Uuid) -> Option<DaemonLink> {
        self.peers
            .read()
            .await
            .get(&device_id)
            .map(|peer| DaemonLink {
                device_id,
                connection_id: peer.connection_id,
            })
    }

    /// What one daemon connection can do about a chunked guest upload, waiting
    /// briefly for a hello it promised.
    ///
    /// A daemon that has just opened its socket may not have sent its hello
    /// yet, and treating that instant as an older build would let an upload
    /// start before a current daemon could admit it. A connection that
    /// promised a hello and has not sent one is [`GuestAdmission::Unknown`]:
    /// the caller must not stream a body, and may ask the browser to retry.
    /// Only a socket that predates hellos, or whose hello omitted the feature,
    /// keeps the bounded legacy path.
    ///
    /// The answer is about `link` alone. A socket that replaced it has said
    /// nothing, so the exchange is unknown (`retryable`), never legacy: a chunk
    /// must never reach a connection whose hello and admission it never had.
    pub async fn guest_admission(&self, link: DaemonLink, grace: Duration) -> GuestAdmission {
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            // Register this waiter before reading the state. `notify_waiters`
            // keeps no permit for a later waiter, so a hello published between
            // the read and the wait would otherwise be missed by this one and
            // by every other waiter that happened to check first.
            let notified = self.greeted_now.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(admission) = self.admission_of(link).await {
                return admission;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                // One last read at the deadline: a hello that arrived in this
                // final instant is the answer, not a timeout.
                return self
                    .admission_of(link)
                    .await
                    .unwrap_or(GuestAdmission::Unknown);
            }
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return self
                    .admission_of(link)
                    .await
                    .unwrap_or(GuestAdmission::Unknown);
            }
        }
    }

    /// The admission answer for one connection right now, or `None` while its
    /// promised hello has still not arrived.
    async fn admission_of(&self, link: DaemonLink) -> Option<GuestAdmission> {
        let features = self.features.read().await;
        let Some(peer) = features.get(&link.device_id) else {
            return Some(GuestAdmission::Unknown);
        };
        if peer.connection != link.connection_id {
            // This socket was replaced (or is gone); silence from the new one
            // is not this exchange's hello.
            return Some(GuestAdmission::Unknown);
        }
        match &peer.state {
            HelloState::Greeted(features)
                if features
                    .iter()
                    .any(|known| known == h4rn_protocol::RELAY_FEATURE_GUEST_ADMISSION) =>
            {
                Some(GuestAdmission::Announced)
            }
            HelloState::Greeted(_) => Some(GuestAdmission::Legacy),
            HelloState::PreHello => Some(GuestAdmission::Legacy),
            HelloState::Expected => None,
        }
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
        wait_for_capacity(tx, event, wait).await
    }

    /// Sends an event to one pinned daemon connection.
    ///
    /// The write fails retryably once that socket is no longer the live peer,
    /// so an exchange whose connection was replaced never delivers a chunk (or
    /// a cancel) to the socket that replaced it. The check is made again at the
    /// moment the enqueue commits: a frame that waited for queue capacity may
    /// have waited long enough for the connection to be replaced, and entering
    /// the old queue then would deliver it to a socket the daemon has dropped.
    pub async fn send_wait_to(
        &self,
        link: DaemonLink,
        event: RelayServerEvent,
        wait: Duration,
    ) -> Result<()> {
        let tx = self.link_sender(link).await?;
        let permit = match tokio::time::timeout(wait, tx.reserve()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                return Err(AppError::unavailable(
                    "peer_offline",
                    "the other device is not connected",
                ));
            }
            Err(_) => {
                return Err(AppError::unavailable(
                    "relay_backpressure",
                    "the other device is too slow",
                ));
            }
        };
        // No await between this check and the send: the replacement either
        // committed before the check (and this frame is refused) or after
        // (and it was already committed to the old queue, which holds it).
        if !self.is_live(link).await {
            drop(permit);
            return Err(AppError::unavailable(
                "guest_connection_replaced",
                "the machine reconnected; try again",
            ));
        }
        permit.send(event);
        Ok(())
    }

    /// Whether `link` is still the device's live connection.
    async fn is_live(&self, link: DaemonLink) -> bool {
        self.peers
            .read()
            .await
            .get(&link.device_id)
            .is_some_and(|peer| peer.connection_id == link.connection_id)
    }

    /// The outbound queue of one exact connection, while it is still live.
    async fn link_sender(&self, link: DaemonLink) -> Result<mpsc::Sender<RelayServerEvent>> {
        let peers = self.peers.read().await;
        match peers.get(&link.device_id) {
            Some(peer) if peer.connection_id == link.connection_id => Ok(peer.tx.clone()),
            _ => Err(AppError::unavailable(
                "guest_connection_replaced",
                "the machine reconnected; try again",
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
    /// The waiter is bound to that exact connection; [`Self::complete_guest`]
    /// ignores an answer from any other device or socket.
    pub async fn expect_guest(
        &self,
        request_id: &str,
        link: DaemonLink,
    ) -> oneshot::Receiver<h4rn_protocol::GuestHttpResponse> {
        self.expect_guest_exchange(request_id, link).await.response
    }

    /// Registers one guest exchange that may carry an upload body.
    ///
    /// The returned admission resolves when the daemon accepts the link, the
    /// browser and the budget. A refusal answers the exchange instead, which
    /// closes the admission without a go-ahead. Everything this exchange sends
    /// and receives is addressed to `link`, so a socket the daemon replaced
    /// mid-upload can neither receive its chunks nor answer it.
    pub async fn expect_guest_exchange(&self, request_id: &str, link: DaemonLink) -> GuestExchange {
        let (tx, response) = oneshot::channel();
        let (admit_tx, admission) = oneshot::channel();
        self.guests.write().await.waiters.insert(
            request_id.to_owned(),
            GuestWaiter {
                daemon: link,
                tx,
                admission: Some(admit_tx),
            },
        );
        GuestExchange {
            admission,
            response,
        }
    }

    /// Delivers one daemon's go-ahead for a chunked guest body.
    ///
    /// Returns false when there is no waiter, the answer already arrived, or
    /// the go-ahead came from another device or connection than the one asked.
    pub async fn admit_guest(&self, from: DaemonLink, request_id: &str) -> bool {
        let mut book = self.guests.write().await;
        let Some(waiter) = book.waiters.get_mut(request_id) else {
            return false;
        };
        if waiter.daemon != from {
            return false;
        }
        // One admission per exchange; a second frame cannot restart an upload.
        let Some(admission) = waiter.admission.take() else {
            return false;
        };
        admission.send(()).is_ok()
    }

    /// Delivers a daemon's guest answer to the waiting browser request.
    ///
    /// Returns false when there is no waiter, or when the answer came from a
    /// device other than the one the request was sent to.
    pub async fn complete_guest(
        &self,
        from: DaemonLink,
        response: h4rn_protocol::GuestHttpResponse,
    ) -> bool {
        let mut book = self.guests.write().await;
        if !book
            .waiters
            .get(&response.request_id)
            .is_some_and(|waiter| waiter.daemon == from)
        {
            tracing::warn!("ignored a guest answer from a connection that was not asked");
            return false;
        }
        if response.chunked {
            book.bodies.insert(
                response.request_id.clone(),
                GuestBody {
                    daemon: from,
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
        from: DaemonLink,
        chunk: h4rn_protocol::GuestHttpChunk,
    ) -> bool {
        let mut book = self.guests.write().await;
        let Some(body) = book.bodies.get_mut(&chunk.request_id) else {
            return false;
        };
        if body.daemon != from {
            tracing::warn!("ignored a guest chunk from the wrong connection");
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

    /// How many chunked answers are still assembling.
    ///
    /// A failed exchange must leave no body behind either, not just no waiter.
    #[doc(hidden)]
    pub async fn pending_guest_bodies(&self) -> usize {
        self.guests.read().await.bodies.len()
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
    /// `1` when the daemon promises a relay hello on this socket.
    #[serde(default)]
    hello: Option<String>,
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
    // A current daemon promises a hello on this socket. The marker is part of
    // the connection, so a build that predates hellos (no marker) keeps the
    // legacy upload path while a current one can never be mistaken for it.
    let announces_hello = query.hello.as_deref() == Some("1");

    Ok(ws.on_upgrade(move |socket| async move {
        handle_socket(state, socket, authed, announces_hello).await;
    }))
}

async fn handle_socket(state: AppState, socket: WebSocket, authed: Authed, announces_hello: bool) {
    let device = authed.device.clone();
    let user_id = authed.user.id;
    let machine_id = device.machine_id;
    let kind = device.kind.clone();
    let connection = Uuid::now_v7();

    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel(state.config.relay_queue_depth.max(1));
    let generation = state
        .relay
        .register(Peer {
            device_id: device.id,
            connection_id: connection,
            announces_hello,
            user_id,
            machine_id,
            kind: kind.clone(),
            tx,
        })
        .await;

    let _ = state.store.touch_device(device.id).await;
    if kind == "daemon" {
        // The presence write is serialized per device and checks the
        // connection's generation, so a socket a reconnect already replaced
        // cannot publish "online" after the replacement's own write.
        let _presence = state.relay.presence_guard(device.id).await;
        if !state.relay.owns_presence(device.id, generation).await {
            return;
        }
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
                    if let Err(error) =
                        dispatch(&state_for_loop, &device_for_loop, frame, connection).await
                    {
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

    // A socket that a reconnect already replaced must not report the device
    // offline: the new connection is the device's presence now. Its sessions
    // still go, though, so frames stop routing to the replacement under a
    // `conn_id` the replacement never opened.
    let closed = state.relay.unregister(device.id, connection).await;
    state
        .relay
        .close_partners(connection, closed.sessions)
        .await;
    writer.abort();
    if !closed.live {
        return;
    }

    if kind == "daemon" {
        // The offline write holds the device's presence lock and re-checks the
        // connection generation: a replacement that registered (and published
        // online) since `unregister` must not be overwritten by this socket's
        // close, and one that registers now publishes after this write.
        let _presence = state.relay.presence_guard(device.id).await;
        if !state.relay.owns_presence(device.id, generation).await {
            return;
        }
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

/// Reads one activity frame, or decides it must publish nothing.
///
/// Only a machine's daemon observes a thread, so a phone or desktop frame is
/// ignored rather than turned into an alert for an account it may share. A
/// daemon's payload must be the fixed [`h4rn_protocol::AgentActivity`] shape;
/// anything else is a bad request, not a partial alert.
fn activity_payload(
    device_kind: &str,
    payload: &[u8],
) -> Result<Option<h4rn_protocol::AgentActivity>> {
    if device_kind != "daemon" {
        return Ok(None);
    }
    serde_json::from_slice(payload).map(Some).map_err(|_| {
        AppError::bad_request(
            "activity_invalid",
            "the activity payload must be AgentActivity JSON",
        )
    })
}

async fn dispatch(
    state: &AppState,
    authed: &Authed,
    mut frame: RelayFrame,
    connection: Uuid,
) -> Result<()> {
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
            let Some(daemon_link) = state.relay.link(daemon_device_id).await else {
                return Err(AppError::unavailable(
                    "daemon_offline",
                    "the machine's daemon is not connected",
                ));
            };

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
                        client_connection: connection,
                        daemon_connection: daemon_link.connection_id,
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
            if let Some(activity) = activity_payload(&authed.device.kind, &frame.payload)? {
                state.alerts.ingest(authed.user.id, activity).await;
            }
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
                state
                    .relay
                    .complete_guest(
                        DaemonLink {
                            device_id: authed.device.id,
                            connection_id: connection,
                        },
                        response,
                    )
                    .await;
            }
            Ok(())
        }
        RelayFrameKind::GuestHttpAdmit => {
            // Only a daemon admits an upload, and only for the exchange it was
            // asked to handle.
            if authed.device.kind != "daemon" {
                return Ok(());
            }
            if let Ok(admission) =
                serde_json::from_slice::<h4rn_protocol::GuestHttpAdmission>(&frame.payload)
            {
                state
                    .relay
                    .admit_guest(
                        DaemonLink {
                            device_id: authed.device.id,
                            connection_id: connection,
                        },
                        &admission.request_id,
                    )
                    .await;
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
                    .complete_guest_chunk(
                        DaemonLink {
                            device_id: authed.device.id,
                            connection_id: connection,
                        },
                        chunk,
                    )
                    .await;
            }
            Ok(())
        }
        RelayFrameKind::Hello => {
            // A hello tells the relay which guest protocol the peer speaks.
            // Only a daemon uses it; a client has no guest exchange to offer.
            if authed.device.kind != "daemon" {
                return Ok(());
            }
            if let Ok(hello) = serde_json::from_slice::<h4rn_protocol::RelayHello>(&frame.payload) {
                state
                    .relay
                    .announce(authed.device.id, connection, hello)
                    .await;
            }
            Ok(())
        }
        RelayFrameKind::GuestHttpCancel => {
            // Only the relay sends this, to a daemon whose admission it gave
            // up on. A device sending one is ignored: it has no slot here.
            Ok(())
        }
    }
}

/// Waits for room in one peer's outbound queue, bounded by `wait`.
async fn wait_for_capacity(
    tx: mpsc::Sender<RelayServerEvent>,
    event: RelayServerEvent,
    wait: Duration,
) -> Result<()> {
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
    use std::sync::Arc;

    use sqlx::postgres::PgPoolOptions;

    use super::*;
    use crate::alerts::AlertService;
    use crate::config::Config;
    use crate::crypto::{Envelope, Keystore, b64, random_bytes};
    use crate::dpop::ReplayCache;
    use crate::jwt::AccessClaims;
    use crate::oidc::OidcRegistry;
    use crate::push::FcmClient;
    use crate::rate_limit::RateLimiter;
    use crate::store::{DeviceRow, SessionRow, Store, UserRow};

    /// An `AppState` whose database is never dialled.
    ///
    /// The Activity arm answers before it touches storage, so a lazy pool is
    /// enough to drive the real `dispatch` path without Postgres.
    fn inert_state() -> AppState {
        let config = Arc::new(Config {
            public_base_url: "http://localhost:8787".to_string(),
            database_url: "postgres://inert:inert@127.0.0.1:1/h4rn".to_string(),
            kek: b64(&random_bytes(32)),
            signing_key: Some(b64(&random_bytes(32))),
            allow_insecure: true,
            ..Config::default()
        });
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy(&config.database_url)
            .expect("a well-formed lazy pool URL");
        let store = Store::from_pool(pool);
        let http = reqwest::Client::new();
        let push = Arc::new(FcmClient::new(&config, http.clone()));
        AppState {
            keystore: Keystore::from_config(&config).expect("signing key"),
            envelope: Envelope::from_config(&config).expect("kek"),
            oidc: Arc::new(OidcRegistry::new(Arc::clone(&config)).expect("oidc")),
            relay: Arc::new(RelayHub::new()),
            rate: Arc::new(RateLimiter::new()),
            replay: Arc::new(ReplayCache::default()),
            alerts: Arc::new(AlertService::new(
                store.clone(),
                Arc::clone(&push),
                Duration::from_millis(1),
                Duration::from_secs(120),
            )),
            store,
            config,
            http,
            push,
            started: std::time::Instant::now(),
        }
    }

    fn authed(kind: &str) -> Authed {
        let user = UserRow {
            id: Uuid::now_v7(),
            email: None,
            display_name: None,
            avatar_url: None,
            created_at: Utc::now(),
        };
        let device = DeviceRow {
            id: Uuid::now_v7(),
            user_id: user.id,
            machine_id: (kind == "daemon").then(Uuid::now_v7),
            kind: kind.to_string(),
            name: "test device".to_string(),
            proof_public_key: vec![0; 32],
            noise_static_public_key: None,
            platform: None,
            last_seen: None,
            revoked_at: None,
            created_at: Utc::now(),
        };
        let session = SessionRow {
            id: Uuid::now_v7(),
            family_id: Uuid::now_v7(),
            user_id: user.id,
            device_id: device.id,
            refresh_hash: "inert".to_string(),
            access_jti: Uuid::now_v7(),
            issued_at: Utc::now(),
            expires_at: Utc::now(),
            rotated_at: None,
            replaced_by: None,
            successor_secret: None,
            revoked_at: None,
            revoke_reason: None,
        };
        let claims = AccessClaims {
            iss: "h4rn-connect".to_string(),
            sub: user.id.to_string(),
            aud: "h4rn".to_string(),
            iat: Utc::now().timestamp(),
            exp: Utc::now().timestamp() + 60,
            jti: Uuid::now_v7().to_string(),
            device_id: device.id.to_string(),
            session_id: session.id.to_string(),
            family_id: session.family_id.to_string(),
            typ: "access".to_string(),
            scp: Vec::new(),
        };
        Authed {
            claims,
            user,
            device,
            session,
            scopes: Vec::new(),
        }
    }

    fn frame(payload: Vec<u8>) -> RelayFrame {
        RelayFrame {
            conn_id: String::new(),
            kind: RelayFrameKind::Activity,
            payload,
        }
    }

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
                connection_id: Uuid::now_v7(),
                announces_hello: false,
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
        let daemon_connection = daemon_peer.connection_id;
        let client_connection = client_peer.connection_id;
        hub.register(daemon_peer).await;
        hub.register(client_peer).await;
        assert!(hub.machine_online(machine).await);

        let session = Session {
            conn_id: "c1".to_string(),
            client_device_id: client,
            daemon_device_id: daemon,
            machine_id: machine,
            user_id: user,
            client_connection,
            daemon_connection,
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
        let daemon_connection = daemon_peer.connection_id;
        let (client_peer, _client_rx) = peer(client, user, None, 8);
        let client_connection = client_peer.connection_id;
        hub.register(daemon_peer).await;
        hub.register(client_peer).await;
        hub.open(
            Session {
                conn_id: "c1".to_string(),
                client_device_id: client,
                daemon_device_id: daemon,
                machine_id: machine,
                user_id: user,
                client_connection,
                daemon_connection,
            },
            RelayFrame::open("c1", machine.to_string()),
        )
        .await
        .unwrap();
        let closed = hub.unregister(daemon, daemon_connection).await;
        assert!(closed.live, "the live socket reported its sessions");
        let sessions = closed.sessions;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].conn_id, "c1");
        assert!(!hub.machine_online(machine).await);
        assert!(hub.online_machines(user).await.is_empty());
    }

    #[tokio::test]
    async fn a_replaced_sockets_session_is_closed_for_its_partner() {
        // A session belongs to the socket that opened it. When the daemon
        // reconnects, the old socket's session must go: its partner is told,
        // and the old conn_id can no longer reach the replacement.
        let hub = RelayHub::new();
        let machine = Uuid::now_v7();
        let daemon = Uuid::now_v7();
        let client = Uuid::now_v7();
        let user = Uuid::now_v7();
        let (daemon_peer, mut daemon_rx) = peer(daemon, user, Some(machine), 8);
        let old_connection = daemon_peer.connection_id;
        let (client_peer, mut client_rx) = peer(client, user, None, 8);
        let client_connection = client_peer.connection_id;
        hub.register(daemon_peer).await;
        hub.register(client_peer).await;
        hub.open(
            Session {
                conn_id: "c1".to_string(),
                client_device_id: client,
                daemon_device_id: daemon,
                machine_id: machine,
                user_id: user,
                client_connection,
                daemon_connection: old_connection,
            },
            RelayFrame::open("c1", machine.to_string()),
        )
        .await
        .unwrap();
        match daemon_rx.recv().await.expect("open") {
            RelayServerEvent::Frame { frame } => assert_eq!(frame.kind, RelayFrameKind::Open),
            other => panic!("unexpected event: {other:?}"),
        }

        // The daemon reconnects; its new socket is the device's live peer.
        let (replacement, mut replacement_rx) = peer(daemon, user, Some(machine), 8);
        hub.register(replacement).await;

        // The old socket closes. It did not own the device any more, but its
        // own session still has to go.
        let closed = hub.unregister(daemon, old_connection).await;
        assert!(
            !closed.live,
            "a replaced socket claimed the device went away"
        );
        assert_eq!(
            closed.sessions.len(),
            1,
            "a replaced socket's session was left behind"
        );
        assert_eq!(closed.sessions[0].conn_id, "c1");
        hub.close_partners(old_connection, closed.sessions).await;
        match client_rx.recv().await.expect("the partner is connected") {
            RelayServerEvent::Frame { frame } => {
                assert_eq!(frame.kind, RelayFrameKind::Close);
                assert_eq!(frame.conn_id, "c1");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // The removal is also what stops the old conn_id from routing to the
        // replacement.
        let error = hub
            .route(client, RelayFrame::data("c1", b"late".to_vec()))
            .await
            .unwrap_err();
        assert_eq!(error.body().code, "session_not_found");
        assert!(
            replacement_rx.try_recv().is_err(),
            "a frame reached the replacement under the old conn_id"
        );
        assert!(
            hub.online(daemon).await,
            "closing the replaced socket marked the live one offline"
        );
    }

    #[tokio::test]
    async fn a_reconnect_before_the_offline_write_keeps_the_new_presence() {
        // A daemon's socket unregisters, and the reconnect lands before the
        // offline write runs. The offline write's generation check must see
        // that the new connection owns presence, so it is never overwritten.
        let hub = RelayHub::new();
        let machine = Uuid::now_v7();
        let daemon = Uuid::now_v7();
        let user = Uuid::now_v7();
        let (old_peer, _old_rx) = peer(daemon, user, Some(machine), 8);
        let old_connection = old_peer.connection_id;
        let old_generation = hub.register(old_peer).await;
        assert!(hub.owns_presence(daemon, old_generation).await);

        let closed = hub.unregister(daemon, old_connection).await;
        assert!(closed.live);

        // The reconnect happens between `unregister` and the offline update.
        let (new_peer, _new_rx) = peer(daemon, user, Some(machine), 8);
        let new_generation = hub.register(new_peer).await;
        assert_ne!(old_generation, new_generation);

        assert!(
            !hub.owns_presence(daemon, old_generation).await,
            "the closing socket still believed it owned the device's presence"
        );
        assert!(hub.owns_presence(daemon, new_generation).await);
        assert!(
            hub.online(daemon).await,
            "the reconnected socket was reported offline"
        );
    }

    #[tokio::test]
    async fn a_frame_that_waited_for_capacity_never_enters_a_replaced_queue() {
        let hub = Arc::new(RelayHub::new());
        let machine = Uuid::now_v7();
        let daemon = Uuid::now_v7();
        let user = Uuid::now_v7();
        let (daemon_peer, mut old_rx) = peer(daemon, user, Some(machine), 1);
        let old_connection = daemon_peer.connection_id;
        hub.register(daemon_peer).await;
        // The old socket's queue is full, so the upload's frame must wait.
        hub.send(
            daemon,
            RelayServerEvent::Frame {
                frame: RelayFrame::data("fill", b"fill".to_vec()),
            },
        )
        .await
        .unwrap();

        let link = link_for(daemon, old_connection);
        let waiting = {
            let hub = Arc::clone(&hub);
            tokio::spawn(async move {
                hub.send_wait_to(
                    link,
                    RelayServerEvent::Frame {
                        frame: RelayFrame::data("c1", b"chunk".to_vec()),
                    },
                    Duration::from_secs(5),
                )
                .await
            })
        };
        // The task's only await before the capacity wait is the peer lookup, so
        // a short pause leaves it parked on the full queue.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The daemon reconnects while the frame is still waiting.
        let (replacement, mut replacement_rx) = peer(daemon, user, Some(machine), 8);
        hub.register(replacement).await;

        // Room appears on the old queue: the frame must be refused instead of
        // committed to a socket the daemon has replaced.
        match old_rx.recv().await.expect("the old queue") {
            RelayServerEvent::Frame { frame } => assert_eq!(frame.conn_id, "fill"),
            other => panic!("unexpected event: {other:?}"),
        }
        let error = waiting
            .await
            .expect("the send task")
            .expect_err("a frame waiting on a replaced socket was accepted");
        assert_eq!(error.body().code, "guest_connection_replaced");
        assert!(
            old_rx.try_recv().is_err(),
            "the chunk entered the replaced socket's queue"
        );
        assert!(
            replacement_rx.try_recv().is_err(),
            "the chunk reached the socket that replaced the one it began on"
        );
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
        let daemon = DaemonLink {
            device_id: Uuid::now_v7(),
            connection_id: Uuid::now_v7(),
        };
        let other = DaemonLink {
            device_id: Uuid::now_v7(),
            connection_id: Uuid::now_v7(),
        };
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

    /// Registers one daemon connection and returns the socket's id.
    async fn register_daemon(hub: &RelayHub, device: Uuid, announces_hello: bool) -> Uuid {
        let connection = Uuid::now_v7();
        let (tx, _rx) = mpsc::channel(1);
        hub.register(Peer {
            device_id: device,
            connection_id: connection,
            announces_hello,
            user_id: Uuid::now_v7(),
            machine_id: Some(Uuid::now_v7()),
            kind: "daemon".to_owned(),
            tx,
        })
        .await;
        connection
    }

    /// The link one registered daemon socket represents.
    fn link_for(device: Uuid, connection: Uuid) -> DaemonLink {
        DaemonLink {
            device_id: device,
            connection_id: connection,
        }
    }

    fn admission_hello() -> h4rn_protocol::RelayHello {
        h4rn_protocol::RelayHello {
            features: vec![h4rn_protocol::RELAY_FEATURE_GUEST_ADMISSION.to_owned()],
        }
    }

    #[tokio::test]
    async fn a_hello_that_arrives_during_the_grace_still_negotiates() {
        // A daemon that just opened its socket may not have sent its hello
        // when the browser's request arrives. The window must not read as an
        // older build.
        let hub = Arc::new(RelayHub::new());
        let daemon = Uuid::now_v7();
        let connection = register_daemon(&hub, daemon, true).await;
        let announcing = Arc::clone(&hub);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            announcing
                .announce(daemon, connection, admission_hello())
                .await;
        });
        assert_eq!(
            hub.guest_admission(link_for(daemon, connection), Duration::from_secs(2))
                .await,
            GuestAdmission::Announced,
            "a hello inside the grace was treated as an older daemon"
        );
    }

    #[tokio::test]
    async fn a_connection_that_promised_a_hello_fails_closed_until_it_arrives() {
        // A socket that marked itself as current must never be mistaken for an
        // older daemon, however late its hello is: no body may stream before
        // it says what it supports.
        let hub = RelayHub::new();
        let daemon = Uuid::now_v7();
        let connection = register_daemon(&hub, daemon, true).await;
        assert_eq!(
            hub.guest_admission(link_for(daemon, connection), Duration::from_millis(50))
                .await,
            GuestAdmission::Unknown,
            "an unannounced current connection kept a legacy decision"
        );
    }

    #[tokio::test]
    async fn two_waiters_both_see_one_hello() {
        // A waking primitive that keeps one permit, or wakes one waiter, loses
        // this: the second guest request would sit out the whole grace and
        // report an older daemon even though the hello arrived.
        let hub = Arc::new(RelayHub::new());
        let daemon = Uuid::now_v7();
        let connection = register_daemon(&hub, daemon, true).await;
        let link = link_for(daemon, connection);
        let waiter = |hub: Arc<RelayHub>| {
            tokio::spawn(async move { hub.guest_admission(link, Duration::from_secs(5)).await })
        };
        let first = waiter(Arc::clone(&hub));
        let second = waiter(Arc::clone(&hub));
        // Let both register before the single hello.
        tokio::time::sleep(Duration::from_millis(50)).await;
        hub.announce(daemon, connection, admission_hello()).await;
        // Both must wake on the hello itself, not sit out the grace and only
        // learn the answer from the timeout re-check.
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), first)
                .await
                .expect("the first waiter was not woken by the hello")
                .expect("first waiter"),
            GuestAdmission::Announced,
            "the first waiter missed the hello"
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), second)
                .await
                .expect("the second waiter was not woken by the hello")
                .expect("second waiter"),
            GuestAdmission::Announced,
            "the second waiter missed the hello"
        );
    }

    #[tokio::test]
    async fn a_connection_that_predates_hellos_keeps_the_legacy_path() {
        let hub = RelayHub::new();
        let daemon = Uuid::now_v7();
        let connection = register_daemon(&hub, daemon, false).await;
        let started = std::time::Instant::now();
        assert_eq!(
            hub.guest_admission(link_for(daemon, connection), Duration::from_secs(5))
                .await,
            GuestAdmission::Legacy
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a pre-hello connection waited for a hello it cannot send"
        );
    }

    #[tokio::test]
    async fn a_hello_without_the_feature_is_an_answer_not_a_timeout() {
        let hub = RelayHub::new();
        let daemon = Uuid::now_v7();
        let connection = register_daemon(&hub, daemon, true).await;
        hub.announce(
            daemon,
            connection,
            h4rn_protocol::RelayHello { features: vec![] },
        )
        .await;
        let started = std::time::Instant::now();
        assert_eq!(
            hub.guest_admission(link_for(daemon, connection), Duration::from_secs(5))
                .await,
            GuestAdmission::Legacy
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a hello without the feature waited for the grace to expire"
        );
    }

    #[tokio::test]
    async fn a_close_from_a_replaced_socket_leaves_the_new_ones_hello() {
        // The old socket closing after a reconnect must not erase the new
        // socket's state; the new one is still waiting for its own hello.
        let hub = RelayHub::new();
        let daemon = Uuid::now_v7();
        let previous = register_daemon(&hub, daemon, true).await;
        hub.announce(daemon, previous, admission_hello()).await;
        let current = register_daemon(&hub, daemon, true).await;
        let closed = hub.unregister(daemon, previous).await;
        assert!(
            !closed.live,
            "a replaced socket reported the device as present"
        );
        assert!(
            closed.sessions.is_empty(),
            "a replaced socket reported sessions it no longer owns"
        );
        // The old socket closing must not report the device offline: the new
        // connection is the device's presence now.
        assert!(
            hub.online(daemon).await,
            "a replaced socket's close marked the live connection offline"
        );
        assert_eq!(
            hub.guest_admission(link_for(daemon, current), Duration::from_millis(50))
                .await,
            GuestAdmission::Unknown,
            "a replaced socket's close erased the new socket's hello state"
        );
        hub.announce(daemon, current, admission_hello()).await;
        assert_eq!(
            hub.guest_admission(link_for(daemon, current), Duration::from_millis(50))
                .await,
            GuestAdmission::Announced
        );
    }

    #[tokio::test]
    async fn a_late_hello_from_a_replaced_socket_is_ignored() {
        // Capability belongs to the connection: a new socket starts unknown,
        // and the previous socket's hello must not announce for it.
        let hub = RelayHub::new();
        let daemon = Uuid::now_v7();
        let previous = register_daemon(&hub, daemon, true).await;
        let current = register_daemon(&hub, daemon, true).await;
        hub.announce(daemon, previous, admission_hello()).await;
        assert_eq!(
            hub.guest_admission(link_for(daemon, current), Duration::from_millis(50))
                .await,
            GuestAdmission::Unknown,
            "a replaced socket's hello spoke for the new one"
        );
        hub.announce(daemon, current, admission_hello()).await;
        assert_eq!(
            hub.guest_admission(link_for(daemon, current), Duration::from_millis(50))
                .await,
            GuestAdmission::Announced
        );
    }

    #[tokio::test]
    async fn an_upload_starts_only_after_the_daemon_admits_it() {
        let hub = RelayHub::new();
        let daemon = DaemonLink {
            device_id: Uuid::now_v7(),
            connection_id: Uuid::now_v7(),
        };
        let other = DaemonLink {
            device_id: Uuid::now_v7(),
            connection_id: Uuid::now_v7(),
        };
        let exchange = hub.expect_guest_exchange("g1", daemon).await;
        // A different device cannot admit another machine's upload.
        assert!(
            !hub.admit_guest(other, "g1").await,
            "another device admitted the upload"
        );
        assert!(hub.admit_guest(daemon, "g1").await);
        exchange
            .admission
            .await
            .expect("the admission never reached the browser");
        // One admission per exchange: a repeat cannot restart the upload.
        assert!(!hub.admit_guest(daemon, "g1").await);
    }

    #[tokio::test]
    async fn a_refused_upload_never_gets_an_admission() {
        let hub = RelayHub::new();
        let daemon = DaemonLink {
            device_id: Uuid::now_v7(),
            connection_id: Uuid::now_v7(),
        };
        let exchange = hub.expect_guest_exchange("g1", daemon).await;
        assert!(
            hub.complete_guest(
                daemon,
                h4rn_protocol::GuestHttpResponse {
                    request_id: "g1".into(),
                    status: 410,
                    chunked: false,
                    headers: Vec::new(),
                    body: b"gone".to_vec(),
                    set_cookie: None,
                }
            )
            .await
        );
        assert!(
            exchange.admission.await.is_err(),
            "a refused upload was told to send its body"
        );
        assert_eq!(hub.pending_guests().await, 0);
    }

    #[tokio::test]
    async fn a_chunked_guest_answer_is_reassembled_in_order() {
        let hub = RelayHub::new();
        let daemon = DaemonLink {
            device_id: Uuid::now_v7(),
            connection_id: Uuid::now_v7(),
        };
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
        let daemon = DaemonLink {
            device_id: Uuid::now_v7(),
            connection_id: Uuid::now_v7(),
        };
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
        let daemon = DaemonLink {
            device_id: Uuid::now_v7(),
            connection_id: Uuid::now_v7(),
        };
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
        let daemon = DaemonLink {
            device_id: Uuid::now_v7(),
            connection_id: Uuid::now_v7(),
        };
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

    #[tokio::test]
    async fn a_cleanup_cancelled_while_it_runs_still_forgets_the_exchange() {
        // A browser that disconnects inside the cleanup drops the handler
        // future at an await. The guard must still be armed there, or the
        // waiter (and any half-arrived body) stays in the relay forever.
        let hub = Arc::new(RelayHub::new());
        let daemon = DaemonLink {
            device_id: Uuid::now_v7(),
            connection_id: Uuid::now_v7(),
        };
        let _waiter = hub.expect_guest("g1", daemon).await;
        // Hold the book so the cleanup is genuinely mid-await when cancelled.
        let book = hub.guests.write().await;
        let task = {
            let hub = Arc::clone(&hub);
            tokio::spawn(async move {
                let mut guard =
                    crate::api::GuestExchangeGuard::new(Arc::clone(&hub), "g1".to_owned());
                let outcome: crate::error::Result<()> =
                    Err(crate::error::AppError::internal("the daemon went away"));
                crate::api::settle_exchange(&mut guard, outcome).await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        task.abort();
        let _ = task.await;
        drop(book);
        tokio::time::timeout(Duration::from_secs(5), async {
            while hub.pending_guests().await > 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a cancelled cleanup left its waiter behind");
    }

    #[tokio::test]
    async fn a_hello_seen_only_at_the_deadline_is_still_an_answer() {
        // The wakeup primitive cannot keep a permit for a waiter that has not
        // registered yet, so a hello can land between the last read and the
        // wait. The deadline reads once more instead of calling that a timeout.
        let hub = Arc::new(RelayHub::new());
        let daemon = Uuid::now_v7();
        let connection = register_daemon(&hub, daemon, true).await;
        let link = link_for(daemon, connection);
        let waiting = Arc::clone(&hub);
        let waiter = tokio::spawn(async move {
            waiting
                .guest_admission(link, Duration::from_millis(120))
                .await
        });
        tokio::time::sleep(Duration::from_millis(40)).await;
        // Published without waking anyone, exactly the missed-notification
        // window the final read exists for.
        hub.features.write().await.insert(
            daemon,
            PeerHello {
                connection,
                state: HelloState::Greeted(vec![
                    h4rn_protocol::RELAY_FEATURE_GUEST_ADMISSION.to_owned(),
                ]),
            },
        );
        assert_eq!(
            waiter.await.expect("waiter"),
            GuestAdmission::Announced,
            "a hello visible at the deadline was reported as a timeout"
        );
    }

    #[test]
    fn a_client_activity_frame_publishes_nothing_and_a_bad_payload_is_refused() {
        let activity = h4rn_protocol::AgentActivity {
            thread_id: h4rn_protocol::ThreadId::new("th_test"),
            thread_title: "Fix the thing".into(),
            project_name: "h4rn".into(),
            state: h4rn_protocol::AgentAlertState::Approval,
            at: chrono::Utc::now(),
            transition: "t1".into(),
        };
        let payload = serde_json::to_vec(&activity).expect("serialise activity");

        // A phone's Activity frame is gated before anything can be alerted.
        assert_eq!(
            activity_payload("mobile", &payload).expect("a client frame is not an error"),
            None
        );

        // A daemon's malformed frame is refused with the wire error.
        let error = activity_payload("daemon", b"{ not json").unwrap_err();
        assert_eq!(error.body().code, "activity_invalid");

        // A daemon's valid frame is the one transition handed to the alerts.
        assert_eq!(
            activity_payload("daemon", &payload).expect("a daemon frame is read"),
            Some(activity)
        );
    }

    #[tokio::test]
    async fn dispatch_ignores_a_phones_activity_frame_and_refuses_a_malformed_daemon_one() {
        // The gate must hold on the real frame path, not just in the payload
        // parser: a phone must never alert the account, and a daemon's broken
        // payload must be a wire error.
        let state = inert_state();
        let activity = h4rn_protocol::AgentActivity {
            thread_id: h4rn_protocol::ThreadId::new("th_test"),
            thread_title: "Fix the thing".into(),
            project_name: "h4rn".into(),
            state: h4rn_protocol::AgentAlertState::Approval,
            at: Utc::now(),
            transition: "t1".into(),
        };
        let payload = serde_json::to_vec(&activity).expect("serialise activity");

        dispatch(&state, &authed("mobile"), frame(payload), Uuid::now_v7())
            .await
            .expect("a phone's activity frame is ignored, not an error");

        let error = dispatch(
            &state,
            &authed("daemon"),
            frame(b"{ not json".to_vec()),
            Uuid::now_v7(),
        )
        .await
        .expect_err("a malformed daemon payload is refused");
        assert_eq!(error.body().code, "activity_invalid");
    }
}
