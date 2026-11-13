//! The daemon-side secure session service.
//!
//! A client dials `/secure` on whichever path it likes. This service runs the
//! Noise handshake on that socket, binds the path to the logical session named
//! in the packet header, and then exposes the decrypted application frames as a
//! pair of channels. The daemon's protocol loop is unchanged: it sees the same
//! JSON and binary frames it sees over `/ws`.
//!
//! The service is transport-agnostic. [`Link`] is the only socket-shaped type,
//! so the same code runs over axum WebSockets, the mock relay in tests, or a
//! future QUIC adapter.
//!
//! Path policy on the daemon is deliberately simple (ADR-009): it does not
//! choose a path. It tracks every attached path and uses the most recently
//! attached one for outbound frames, which is the path the client just proved
//! alive. Losing that path leaves the session resumable, not dead.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::DEFAULT_MAX_FRAME_BYTES;
use crate::engine::PathId;
use crate::keys::{DeviceIdentity, KEY_LEN};
use crate::noise::{NoiseError, RekeyLimits};
use crate::server_session::{PairingPolicy, ServerHandshakeStep, ServerPath, ServerSession};
use crate::session::{AppFrame, Incoming, IncomingKind, Outgoing, ResumeTicket, SessionError};
use crate::wire::{
    Control, Packet, PacketKind, Record, RecordKind, RejectReason, SessionId, TOKEN_LEN, WireError,
};

/// One packet-oriented connection to a peer.
#[async_trait]
pub trait Link: Send {
    /// Sends one complete binary transport packet.
    async fn send_packet(&mut self, packet: Vec<u8>) -> Result<(), LinkError>;
    /// Receives the next packet, or `None` when the peer closed the socket.
    async fn recv_packet(&mut self) -> Result<Option<Vec<u8>>, LinkError>;
}

/// A link failure.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// The socket failed.
    #[error("transport link failed: {0}")]
    Transport(String),
    /// The peer closed the connection.
    #[error("the transport link closed")]
    Closed,
}

/// Daemon policy and tuning for the secure transport.
#[derive(Clone, Debug)]
pub struct SecureOptions {
    /// The daemon's long-lived identity.
    pub identity: DeviceIdentity,
    /// Which device keys may attach.
    pub policy: PairingPolicy,
    /// One-time PSK for pairing a device that is not on the allowlist yet.
    pub psk: Option<[u8; TOKEN_LEN]>,
    /// Rekey policy for each path.
    pub rekey: RekeyLimits,
    /// How long a silent path is kept before it is closed.
    pub path_timeout: Duration,
    /// How long acknowledgements are coalesced.
    pub ack_delay_ms: u64,
    /// How often retransmits and acknowledgements run.
    pub tick: Duration,
    /// Largest application frame accepted from a client.
    pub max_frame_bytes: usize,
}

impl SecureOptions {
    /// Shipping defaults: pairing closed, a one-second path timeout and a
    /// 25 ms tick (matching the client's failover budget).
    pub fn new(identity: DeviceIdentity) -> Self {
        Self {
            identity,
            policy: PairingPolicy::default(),
            psk: None,
            rekey: RekeyLimits::default(),
            path_timeout: Duration::from_secs(2),
            ack_delay_ms: 40,
            tick: Duration::from_millis(25),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
        }
    }

    /// Enables PSK pairing with `psk`.
    pub fn with_psk(mut self, psk: [u8; TOKEN_LEN]) -> Self {
        self.psk = Some(psk);
        self.policy = self.policy.clone().allow_psk_pairing(true);
        self
    }

    /// Replaces the allowlist policy.
    pub fn with_policy(mut self, policy: PairingPolicy) -> Self {
        self.policy = policy;
        self
    }
}

/// One accepted secure session.
pub struct SessionHandle {
    /// Logical session id.
    pub session_id: SessionId,
    /// Authenticated device X25519 static key.
    pub device: [u8; KEY_LEN],
    /// Resume ticket the client should persist.
    pub ticket: ResumeTicket,
    /// Path handle for diagnostics.
    pub path: PathId,
    /// Decrypted frames from the client.
    pub frames: mpsc::Receiver<AppFrame>,
    commands: mpsc::UnboundedSender<SessionCommand>,
}

impl SessionHandle {
    /// Queues one application frame for the client.
    pub fn send(&self, frame: AppFrame) -> Result<(), SecureError> {
        self.commands
            .send(SessionCommand::Frame(frame))
            .map_err(|_| SecureError::Closed)
    }

    /// Waits for the next decrypted frame.
    pub async fn recv(&mut self) -> Option<AppFrame> {
        self.frames.recv().await
    }

    /// Returns the next frame without waiting.
    pub fn try_recv(&mut self) -> Result<AppFrame, mpsc::error::TryRecvError> {
        self.frames.try_recv()
    }

    /// Closes the session and lets the peer reconcile.
    pub async fn close(self) {
        let _ = self.commands.send(SessionCommand::Close);
    }

    /// Splits the handle into a clonable sender and the received-frame stream.
    ///
    /// The daemon uses this to adapt the session to its existing socket loop:
    /// the sender stands in for the WebSocket sink and the receiver for the
    /// WebSocket stream.
    pub fn into_parts(self) -> (SessionSender, mpsc::Receiver<AppFrame>) {
        (
            SessionSender {
                commands: self.commands,
            },
            self.frames,
        )
    }
}

/// A clonable, synchronous handle for queueing frames to a secure session.
#[derive(Clone)]
pub struct SessionSender {
    commands: mpsc::UnboundedSender<SessionCommand>,
}

impl SessionSender {
    /// Queues one application frame for the client.
    pub fn send(&self, frame: AppFrame) -> Result<(), SecureError> {
        self.commands
            .send(SessionCommand::Frame(frame))
            .map_err(|_| SecureError::Closed)
    }

    /// Closes the session and lets the peer reconcile.
    pub fn close(&self) {
        let _ = self.commands.send(SessionCommand::Close);
    }
}

enum SessionCommand {
    Frame(AppFrame),
    Close,
}

/// The daemon's secure transport service.
pub struct SecureService {
    options: SecureOptions,
    policy: RwLock<PairingPolicy>,
    sessions: Mutex<HashMap<SessionId, Arc<Mutex<ServerSession>>>>,
    next_path: AtomicU32,
    started: Instant,
}

impl SecureService {
    /// Creates a service from `options`.
    pub fn new(options: SecureOptions) -> Self {
        let policy = RwLock::new(options.policy.clone());
        Self {
            options,
            policy,
            sessions: Mutex::new(HashMap::new()),
            next_path: AtomicU32::new(1),
            started: Instant::now(),
        }
    }

    /// The effective options.
    pub fn options(&self) -> &SecureOptions {
        &self.options
    }

    /// The daemon's public identity, for a pairing payload.
    pub fn public_identity(&self) -> crate::keys::DevicePublicIdentity {
        self.options.identity.public()
    }

    /// Adds a device key to the allowlist (after an out-of-band pairing).
    pub fn allow(&self, key: [u8; KEY_LEN]) {
        self.policy
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .allow(key);
    }

    /// Revokes a device key immediately.
    pub fn revoke(&self, key: [u8; KEY_LEN]) {
        self.policy
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .revoke(key);
    }

    /// Whether `key` may attach right now.
    pub fn is_allowed(&self, key: &[u8; KEY_LEN]) -> bool {
        self.policy
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_allowed(key)
    }

    /// Number of live logical sessions.
    pub fn session_count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Runs the handshake and the first `Open`/`Welcome` exchange, then returns
    /// a handle whose channels are serviced by a background task.
    pub async fn accept(
        self: &Arc<Self>,
        mut link: Box<dyn Link>,
    ) -> Result<SessionHandle, SecureError> {
        let Some(first) = link
            .recv_packet()
            .await
            .map_err(|error| SecureError::Link(error.to_string()))?
        else {
            return Err(SecureError::Closed);
        };
        let packet = Packet::decode(&first)?;
        if packet.kind != PacketKind::Handshake {
            let _ = reject(&mut *link, packet.session_id, RejectReason::BadHandshake).await;
            return Err(SecureError::Protocol("first packet was not a handshake"));
        }
        let session_id = packet.session_id;
        let (mut path, device) = self.handshake(&mut *link, session_id, packet.body).await?;

        // The first encrypted record must be `Open`: it binds this fresh Noise
        // channel to the logical session and carries the resume token.
        let Some(bytes) = link
            .recv_packet()
            .await
            .map_err(|error| SecureError::Link(error.to_string()))?
        else {
            return Err(SecureError::Closed);
        };
        let packet = Packet::decode(&bytes)?;
        if packet.kind != PacketKind::Record {
            return Err(SecureError::Protocol("expected an encrypted Open record"));
        }
        let plaintext = path.open(&packet.body)?;
        let record = Record::decode_plaintext(&plaintext)?;
        if record.kind != RecordKind::Control {
            return Err(SecureError::Protocol("expected an Open control record"));
        }
        let Control::Open {
            token,
            recv_next,
            send_next,
        } = Control::decode(&record.payload)?
        else {
            return Err(SecureError::Protocol("expected an Open control record"));
        };

        let now = self.now_ms();
        let path_id = self.next_path.fetch_add(1, Ordering::Relaxed);
        let (session, welcome, pending, ticket) = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.evict_stale(&mut sessions, now);
            let session = match token {
                Some(token) => {
                    let session = sessions
                        .get(&session_id)
                        .cloned()
                        .ok_or(SecureError::NotPaired)?;
                    if !lock(&session).token_matches(&token) {
                        return Err(SecureError::NotPaired);
                    }
                    session
                }
                None => {
                    if sessions.contains_key(&session_id) {
                        return Err(SecureError::Protocol("session id is already in use"));
                    }
                    let session = Arc::new(Mutex::new(ServerSession::new(session_id, now)?));
                    sessions.insert(session_id, Arc::clone(&session));
                    session
                }
            };
            let mut guard = lock(&session);
            let welcome = guard.attach(path_id, recv_next, send_next, now)?;
            let (ack, ack_bits) = guard.ack_state();
            let pending = guard.sendable(now, true);
            let ticket = ResumeTicket {
                session_id,
                token: guard.token,
            };
            drop(guard);
            (
                Arc::clone(&session),
                (welcome, ack, ack_bits),
                pending,
                ticket,
            )
        };

        let (welcome, ack, ack_bits) = welcome;
        let mut welcome_record = Record::control(&welcome);
        welcome_record.ack = ack;
        welcome_record.ack_bits = ack_bits;
        send_record(&mut path, &mut *link, session_id, welcome_record).await?;
        for outgoing in pending {
            send_outgoing(&mut path, &mut *link, session_id, outgoing, ack, ack_bits).await?;
        }

        let (frames_tx, frames_rx) = mpsc::channel(256);
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let service = Arc::clone(self);
        tokio::spawn(async move {
            service
                .serve_path(session, path_id, path, link, frames_tx, commands_rx)
                .await;
        });

        Ok(SessionHandle {
            session_id,
            device,
            ticket,
            path: path_id,
            frames: frames_rx,
            commands: commands_tx,
        })
    }

    async fn handshake(
        &self,
        link: &mut dyn Link,
        session_id: SessionId,
        mut message: Vec<u8>,
    ) -> Result<(ServerPath, [u8; KEY_LEN]), SecureError> {
        let mut path = ServerPath::new(
            session_id,
            self.options.identity.clone(),
            self.options.psk,
            self.options.rekey,
        );
        loop {
            let step = {
                let policy = self
                    .policy
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                path.on_handshake(&message, &policy)?
            };
            match step {
                ServerHandshakeStep::Rejected(reason) => {
                    let _ = reject(link, session_id, reason).await;
                    return Err(SecureError::Rejected(reason));
                }
                ServerHandshakeStep::Reply(reply) => {
                    send_handshake(link, session_id, reply).await?;
                }
                ServerHandshakeStep::PairedThroughPsk { device, reply } => {
                    self.allow(device);
                    if let Some(reply) = reply {
                        send_handshake(link, session_id, reply).await?;
                    }
                }
                ServerHandshakeStep::Complete {
                    device,
                    newly_paired,
                    reply,
                } => {
                    if newly_paired {
                        self.allow(device);
                    }
                    if let Some(reply) = reply {
                        send_handshake(link, session_id, reply).await?;
                    }
                    return Ok((path, device));
                }
                ServerHandshakeStep::Waiting => {}
            }
            let Some(next) = link
                .recv_packet()
                .await
                .map_err(|error| SecureError::Link(error.to_string()))?
            else {
                return Err(SecureError::Closed);
            };
            let packet = Packet::decode(&next)?;
            if packet.kind != PacketKind::Handshake || packet.session_id != session_id {
                let _ = reject(link, session_id, RejectReason::BadHandshake).await;
                return Err(SecureError::Protocol("expected the next handshake message"));
            }
            message = packet.body;
        }
    }

    async fn serve_path(
        &self,
        session: Arc<Mutex<ServerSession>>,
        path_id: PathId,
        mut path: ServerPath,
        mut link: Box<dyn Link>,
        frames: mpsc::Sender<AppFrame>,
        mut commands: mpsc::UnboundedReceiver<SessionCommand>,
    ) {
        let mut ticker = tokio::time::interval(self.options.tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut idle_since = self.now_ms();
        let session_id = session_id_of(&session);

        loop {
            let keep_going = tokio::select! {
                command = commands.recv() => {
                    match command {
                        Some(SessionCommand::Frame(frame)) => {
                            let now = self.now_ms();
                            let queued = lock(&session).send(frame, now);
                            idle_since = now;
                            queued.is_ok()
                        }
                        Some(SessionCommand::Close) | None => {
                            let (ack, ack_bits) = lock(&session).ack_state();
                            let mut record = Record::control(&Control::Close { code: 0 });
                            record.ack = ack;
                            record.ack_bits = ack_bits;
                            let _ = send_record(&mut path, &mut *link, session_id, record).await;
                            false
                        }
                    }
                }
                packet = link.recv_packet() => {
                    self.on_packet(
                        &session,
                        path_id,
                        &mut path,
                        &mut *link,
                        &frames,
                        &mut idle_since,
                        packet,
                    )
                    .await
                }
                _ = ticker.tick() => {
                    let now = self.now_ms();
                    now.saturating_sub(idle_since)
                        <= self.options.path_timeout.as_millis() as u64
                }
            };

            if !keep_going {
                break;
            }
            if !flush(
                &session,
                path_id,
                &mut path,
                &mut *link,
                self.now_ms(),
                self.options.ack_delay_ms,
                false,
            )
            .await
            {
                break;
            }
        }

        lock(&session).detach(path_id);
    }

    #[allow(clippy::too_many_arguments)]
    async fn on_packet(
        &self,
        session: &Arc<Mutex<ServerSession>>,
        path_id: PathId,
        path: &mut ServerPath,
        link: &mut dyn Link,
        frames: &mpsc::Sender<AppFrame>,
        idle_since: &mut u64,
        packet: Result<Option<Vec<u8>>, LinkError>,
    ) -> bool {
        let bytes = match packet {
            Ok(Some(bytes)) => bytes,
            Ok(None) | Err(_) => return false,
        };
        let Ok(packet) = Packet::decode(&bytes) else {
            return false;
        };
        match packet.kind {
            PacketKind::Record => {
                let Ok(plaintext) = path.open(&packet.body) else {
                    return false;
                };
                let Ok(record) = Record::decode_plaintext(&plaintext) else {
                    return false;
                };
                match record.kind {
                    RecordKind::Data | RecordKind::Ack => {
                        let now = self.now_ms();
                        let deliveries = {
                            let mut guard = lock(session);
                            let incoming = Incoming {
                                kind: if record.kind == RecordKind::Ack {
                                    IncomingKind::Ack
                                } else {
                                    IncomingKind::Data
                                },
                                binary: record.binary,
                                msg_id: record.msg_id,
                                frag_index: record.frag_index,
                                frag_last: record.frag_last,
                                seq: record.seq,
                                ack: record.ack,
                                ack_bits: record.ack_bits,
                                payload: record.payload,
                            };
                            match guard.on_record(path_id, incoming, now) {
                                Ok(deliveries) => deliveries,
                                Err(_) => return false,
                            }
                        };
                        for frame in deliveries {
                            if frames.send(frame).await.is_err() {
                                return false;
                            }
                        }
                        *idle_since = now;
                        true
                    }
                    RecordKind::Control => match Control::decode(&record.payload) {
                        Ok(Control::Ping { nonce, sent_ms }) => {
                            let (ack, ack_bits) = lock(session).ack_state();
                            let mut reply = Record::control(&Control::Pong { nonce, sent_ms });
                            reply.ack = ack;
                            reply.ack_bits = ack_bits;
                            send_record(path, link, packet.session_id, reply)
                                .await
                                .is_ok()
                        }
                        Ok(Control::Pong { .. }) => true,
                        Ok(Control::Close { .. })
                        | Ok(Control::Open { .. })
                        | Ok(Control::Welcome { .. })
                        | Err(_) => false,
                    },
                }
            }
            // A second handshake on a live path is ignored; the peer opens a
            // new socket for a new path instead.
            PacketKind::Handshake => true,
            PacketKind::Reject => false,
        }
    }

    fn evict_stale(&self, sessions: &mut HashMap<SessionId, Arc<Mutex<ServerSession>>>, now: u64) {
        // Keep a detached session long enough for a client to resume it, then
        // reclaim it. The application protocol replays its own gap if the
        // resume arrives too late.
        let grace = self.options.path_timeout.as_millis() as u64 * 4;
        sessions.retain(|_, session| {
            let guard = lock(session);
            !guard.attached.is_empty() || now.saturating_sub(guard.last_seen_ms) < grace
        });
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

/// Secure connection failures.
#[derive(Debug, thiserror::Error)]
pub enum SecureError {
    /// Framing failure.
    #[error(transparent)]
    Wire(#[from] WireError),
    /// Noise failure.
    #[error(transparent)]
    Noise(#[from] NoiseError),
    /// Session-layer failure.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// The link failed.
    #[error("{0}")]
    Link(String),
    /// The peer rejected the path.
    #[error("the peer rejected the path: {0:?}")]
    Rejected(RejectReason),
    /// The resume token was absent, unknown or wrong.
    #[error("the device is not paired")]
    NotPaired,
    /// The peer closed before the session was established.
    #[error("the connection closed before the session was established")]
    Closed,
    /// The peer violated the protocol.
    #[error("secure transport protocol violation: {0}")]
    Protocol(&'static str),
}

async fn send_handshake(
    link: &mut dyn Link,
    session_id: SessionId,
    body: Vec<u8>,
) -> Result<(), SecureError> {
    link.send_packet(
        Packet {
            kind: PacketKind::Handshake,
            session_id,
            body,
        }
        .encode(),
    )
    .await
    .map_err(|error| SecureError::Link(error.to_string()))
}

async fn reject(
    link: &mut dyn Link,
    session_id: SessionId,
    reason: RejectReason,
) -> Result<(), SecureError> {
    link.send_packet(
        Packet {
            kind: PacketKind::Reject,
            session_id,
            body: vec![reason as u8],
        }
        .encode(),
    )
    .await
    .map_err(|error| SecureError::Link(error.to_string()))
}

async fn send_record(
    path: &mut ServerPath,
    link: &mut dyn Link,
    session_id: SessionId,
    record: Record,
) -> Result<(), SecureError> {
    let plaintext = record.encode_plaintext()?;
    let ciphertext = path.seal(&plaintext)?;
    link.send_packet(
        Packet {
            kind: PacketKind::Record,
            session_id,
            body: ciphertext,
        }
        .encode(),
    )
    .await
    .map_err(|error| SecureError::Link(error.to_string()))
}

async fn send_outgoing(
    path: &mut ServerPath,
    link: &mut dyn Link,
    session_id: SessionId,
    outgoing: Outgoing,
    ack: u64,
    ack_bits: u32,
) -> Result<(), SecureError> {
    let record = Record {
        kind: RecordKind::Data,
        binary: outgoing.binary,
        msg_id: outgoing.msg_id,
        frag_index: outgoing.frag_index,
        frag_last: outgoing.frag_last,
        seq: outgoing.seq,
        ack,
        ack_bits,
        payload: outgoing.payload.to_vec(),
    };
    send_record(path, link, session_id, record).await
}

async fn flush(
    session: &Arc<Mutex<ServerSession>>,
    _path_id: PathId,
    path: &mut ServerPath,
    link: &mut dyn Link,
    now: u64,
    ack_delay_ms: u64,
    all: bool,
) -> bool {
    let (outgoing, ack, ack_bits, bare_ack) = {
        let mut guard = lock(session);
        let (ack, ack_bits) = guard.ack_state();
        let outgoing = guard.sendable(now, all);
        let bare_ack = guard.take_ack(now, ack_delay_ms);
        (outgoing, ack, ack_bits, bare_ack)
    };
    let session_id = session_id_of(session);
    for record in outgoing {
        if send_outgoing(path, link, session_id, record, ack, ack_bits)
            .await
            .is_err()
        {
            return false;
        }
    }
    if let Some((ack, ack_bits)) = bare_ack {
        let record = Record::ack(ack, ack_bits);
        if send_record(path, link, session_id, record).await.is_err() {
            return false;
        }
    }
    true
}

fn session_id_of(session: &Arc<Mutex<ServerSession>>) -> SessionId {
    lock(session).session_id
}

fn lock(session: &Arc<Mutex<ServerSession>>) -> std::sync::MutexGuard<'_, ServerSession> {
    session
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KEY_LEN;
    use crate::noise::{Handshake, HandshakeConfig};
    use crate::wire::SESSION_ID_LEN;

    #[test]
    fn default_policy_rejects_unknown_devices() {
        let service = SecureService::new(SecureOptions::new(DeviceIdentity::generate().unwrap()));
        assert!(!service.is_allowed(&[1u8; KEY_LEN]));
        service.allow([1u8; KEY_LEN]);
        assert!(service.is_allowed(&[1u8; KEY_LEN]));
        service.revoke([1u8; KEY_LEN]);
        assert!(!service.is_allowed(&[1u8; KEY_LEN]));
    }

    #[test]
    fn psk_options_enable_pairing() {
        let service = SecureService::new(
            SecureOptions::new(DeviceIdentity::generate().unwrap()).with_psk([9u8; TOKEN_LEN]),
        );
        assert_eq!(service.options().psk, Some([9u8; TOKEN_LEN]));
        assert!(
            service
                .options()
                .policy
                .authorize(&[1u8; KEY_LEN], true)
                .is_ok()
        );
    }

    #[test]
    fn ticket_is_stable_for_a_session() {
        let session = ServerSession::new([3u8; SESSION_ID_LEN], 0).unwrap();
        let ticket = ResumeTicket {
            session_id: session.session_id,
            token: session.token,
        };
        assert_eq!(
            ResumeTicket::decode(&ticket.encode()).unwrap().token,
            ticket.token
        );
    }

    #[test]
    fn handshake_starts_with_the_pinned_daemon_key() {
        let daemon = DeviceIdentity::generate().unwrap();
        let client = DeviceIdentity::generate().unwrap();
        let session = [4u8; SESSION_ID_LEN];
        let (_handshake, message) = Handshake::start_initiator(&HandshakeConfig {
            session_id: &session,
            local_static: client.noise_secret(),
            remote_static: Some(&daemon.public().noise_public),
            psk: None,
        })
        .unwrap();
        assert!(!message.is_empty());
    }
}
