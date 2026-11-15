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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

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
    closed: Arc<AtomicBool>,
    liveness: SessionLiveness,
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

    /// Observes whether the peer still has a path attached.
    ///
    /// A peer closing one path is not the end of the logical session: the
    /// session stays resumable and a failover path may attach a moment later.
    /// The application loop uses this to notice when every path is gone, which
    /// is the point at which the peer can no longer hear a response.
    pub fn liveness(&self) -> SessionLiveness {
        self.liveness.clone()
    }

    /// Closes the session and lets the peer reconcile.
    pub async fn close(self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.commands.send(SessionCommand::Close);
    }

    /// Splits the handle into a clonable sender and the received-frame stream.
    ///
    /// The daemon uses this to adapt the session to its existing socket loop:
    /// the sender stands in for the WebSocket sink and the receiver for the
    /// WebSocket stream. Both stay valid across a path switch because they are
    /// owned by the logical session, not by the socket that created them.
    pub fn into_parts(self) -> (SessionSender, mpsc::Receiver<AppFrame>) {
        (
            SessionSender {
                commands: self.commands,
                closed: self.closed,
            },
            self.frames,
        )
    }
}

/// A clonable, synchronous handle for queueing frames to a secure session.
#[derive(Clone)]
pub struct SessionSender {
    commands: mpsc::UnboundedSender<SessionCommand>,
    closed: Arc<AtomicBool>,
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
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.commands.send(SessionCommand::Close);
    }
}

/// Observes whether a logical session still has a live path.
///
/// A peer that closes one path leaves the session resumable: a failover path
/// may attach before the old one is gone. [`Self::gone`] resolves only when
/// the session has had a path and then lost every one, which is the point at
/// which the peer can no longer receive a response.
#[derive(Clone)]
pub struct SessionLiveness {
    attached: watch::Receiver<bool>,
    ever_live: Arc<AtomicBool>,
}

impl SessionLiveness {
    /// Whether at least one path is attached right now.
    pub fn attached(&self) -> bool {
        *self.attached.borrow()
    }

    /// Resolves once the session had a path and every path has since detached.
    ///
    /// Waiting for the first attachment matters because the handle is built
    /// just before the path task starts; without it a caller that checks
    /// immediately would race the first attach and see a session that was
    /// never alive. A session whose only path detached before the caller
    /// subscribed has already published the change it would wait for, so the
    /// flag makes that case return instead of blocking forever. Cancel-safe.
    pub async fn gone(&mut self) {
        if !*self.attached.borrow() && self.ever_live.load(Ordering::SeqCst) {
            return;
        }
        while !*self.attached.borrow_and_update() {
            if self.attached.changed().await.is_err() {
                return;
            }
        }
        loop {
            if !*self.attached.borrow_and_update() {
                return;
            }
            if self.attached.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Outcome of dialling `/secure` on one path.
pub enum AcceptOutcome {
    /// A brand-new logical session. The caller owns its application loop.
    New(SessionHandle),
    /// A new path attached to a session whose application loop is already
    /// running. Nothing else must be started: the existing loop survives a
    /// direct-to-relay failover.
    Resumed {
        /// Logical session id.
        session_id: SessionId,
    },
}

enum SessionCommand {
    Frame(AppFrame),
    Close,
}

/// One logical session and the channels its application loop owns.
struct SessionEndpoint {
    session: Arc<Mutex<ServerSession>>,
    frames: mpsc::Sender<AppFrame>,
    commands: mpsc::UnboundedSender<SessionCommand>,
    closed: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
    /// How many path tasks are currently attached. Failover may briefly hold
    /// two; a drop to zero means the peer has no way back to this session.
    live_paths: AtomicUsize,
    liveness: watch::Sender<bool>,
    /// Set by the first attached path; see [`SessionLiveness::gone`].
    ever_live: Arc<AtomicBool>,
}

/// Keeps one live-path count in the session for as long as its task runs.
///
/// Counted when the path is attached, not when its task first polls, so a
/// caller that subscribes to [`SessionLiveness`] can never race the first
/// attachment and wait forever for a "live" transition it missed.
struct LivePath {
    endpoint: Arc<SessionEndpoint>,
}

impl LivePath {
    fn new(endpoint: Arc<SessionEndpoint>) -> Self {
        endpoint.ever_live.store(true, Ordering::SeqCst);
        if endpoint.live_paths.fetch_add(1, Ordering::SeqCst) == 0 {
            endpoint.liveness.send_replace(true);
        }
        Self { endpoint }
    }
}

impl Drop for LivePath {
    fn drop(&mut self) {
        if self.endpoint.live_paths.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.endpoint.liveness.send_replace(false);
        }
    }
}

type PairingHook = Arc<dyn Fn([u8; KEY_LEN]) + Send + Sync>;
type PskConsumedHook = Arc<dyn Fn([u8; TOKEN_LEN]) + Send + Sync>;

/// The daemon's secure transport service.
pub struct SecureService {
    options: SecureOptions,
    policy: RwLock<PairingPolicy>,
    /// Current one-time pairing secret, consumed by the first successful pair.
    psk: RwLock<Option<[u8; TOKEN_LEN]>>,
    /// Optional file the allowlist is mirrored to, so pairings survive restart.
    allowlist_path: RwLock<Option<std::path::PathBuf>>,
    pairing_hook: RwLock<Option<PairingHook>>,
    psk_consumed_hook: RwLock<Option<PskConsumedHook>>,
    sessions: Mutex<HashMap<SessionId, Arc<SessionEndpoint>>>,
    next_path: AtomicU32,
    started: Instant,
}

impl SecureService {
    /// Creates a service from `options`.
    pub fn new(options: SecureOptions) -> Self {
        let policy = RwLock::new(options.policy.clone());
        let psk = RwLock::new(options.psk);
        Self {
            options,
            policy,
            psk,
            allowlist_path: RwLock::new(None),
            pairing_hook: RwLock::new(None),
            psk_consumed_hook: RwLock::new(None),
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
        self.persist_allowlist();
    }

    /// Revokes a device key immediately and drops its live sessions.
    ///
    /// This is what a `DeviceRevoked` control frame from connect calls. It
    /// removes the key from the allowlist and marks every session owned by it
    /// closed, so path tasks stop retransmitting and the application loop ends.
    pub fn revoke(&self, key: [u8; KEY_LEN]) {
        self.policy
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .revoke(key);
        self.persist_allowlist();
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sessions.retain(|_, endpoint| {
            let matches = lock(&endpoint.session).device == Some(key);
            if matches {
                endpoint.closed.store(true, Ordering::SeqCst);
                endpoint.notify.notify_waiters();
            }
            !matches
        });
    }

    /// Whether `key` may attach right now.
    pub fn is_allowed(&self, key: &[u8; KEY_LEN]) -> bool {
        self.policy
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_allowed(key)
    }

    /// Every currently allowed device key, for diagnostics and persistence.
    pub fn allowed_devices(&self) -> Vec<[u8; KEY_LEN]> {
        self.policy
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .allowed_keys()
            .collect()
    }

    /// Installs a one-time pairing secret.
    ///
    /// A live secret opens PSK pairing: an unknown device that proves
    /// possession of it may pair. [`Self::clear_psk`] and a successful
    /// [`Self::consume_psk`] close the window again, so a stale secret cannot
    /// keep admitting devices after it is gone.
    pub fn set_psk(&self, psk: [u8; TOKEN_LEN]) {
        *self
            .psk
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(psk);
        self.policy
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .set_psk_pairing(true);
    }

    /// Removes the current pairing secret (rotation or expiry) and closes PSK
    /// pairing.
    pub fn clear_psk(&self) {
        *self
            .psk
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.policy
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .set_psk_pairing(false);
    }

    /// Atomically consumes the pairing secret if it is still `used`.
    ///
    /// Returns `true` for exactly one caller: the first to present the live
    /// secret. A second handshake that started with the same secret (it took a
    /// snapshot before the first finished) gets `false` and must be rejected.
    /// The consumed hook runs before `true` is returned, so a durable record of
    /// the secret can be invalidated before the device is authorised.
    pub fn consume_psk(&self, used: &[u8; TOKEN_LEN]) -> bool {
        use subtle::ConstantTimeEq as _;
        let consumed = {
            let mut guard = self
                .psk
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match guard.as_ref() {
                Some(current) if bool::from(current.ct_eq(used)) => {
                    *guard = None;
                    true
                }
                _ => false,
            }
        };
        if !consumed {
            return false;
        }
        // The window closes with the secret, even while a durable invalidation
        // is in flight.
        self.policy
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .set_psk_pairing(false);
        let hook = self
            .psk_consumed_hook
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook(*used);
        }
        true
    }

    /// Registers a hook run with the used secret when the one-time secret is
    /// consumed, before the device is allowed.
    ///
    /// The daemon uses it to delete the `direct-pairing.json` that carried the
    /// secret, so a restart cannot re-arm a used QR.
    pub fn set_psk_consumed_hook(&self, hook: Arc<dyn Fn([u8; TOKEN_LEN]) + Send + Sync>) {
        *self
            .psk_consumed_hook
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hook);
    }

    /// Registers a hook run when a device pairs through the one-time secret,
    /// after the secret is consumed and before the device is allowed.
    ///
    /// The daemon uses it to record the new device's scopes and provenance.
    pub fn set_pairing_hook(&self, hook: Arc<dyn Fn([u8; KEY_LEN]) + Send + Sync>) {
        *self
            .pairing_hook
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hook);
    }

    fn paired_through_psk(&self, used: Option<[u8; TOKEN_LEN]>, device: [u8; KEY_LEN]) -> bool {
        let Some(used) = used else { return false };
        if !self.consume_psk(&used) {
            return false;
        }
        let hook = self
            .pairing_hook
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(hook) = hook {
            hook(device);
        }
        self.allow(device);
        true
    }

    /// The current pairing secret, when pairing is open.
    pub fn psk(&self) -> Option<[u8; TOKEN_LEN]> {
        *self
            .psk
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Mirrors the allowlist to `path` on every change.
    pub fn set_allowlist_path(&self, path: impl Into<std::path::PathBuf>) {
        *self
            .allowlist_path
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path.into());
        self.persist_allowlist();
    }

    fn persist_allowlist(&self) {
        let path = self
            .allowlist_path
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(path) = path else { return };
        let body = self
            .allowed_devices()
            .into_iter()
            .map(hex::encode)
            .collect::<Vec<_>>()
            .join("\n");
        if let Err(error) = crate::keys::write_secret_file(&path, body.as_bytes()) {
            tracing::warn!(%error, "could not persist the paired-device allowlist");
        }
    }

    /// Number of live logical sessions.
    pub fn session_count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Runs the handshake and the first `Open`/`Welcome` exchange, then either
    /// returns a handle for a new logical session or attaches another path to
    /// an existing one.
    ///
    /// A resumed path deliberately returns no handle: the application loop that
    /// owns the original session keeps running, so a direct-to-relay failover
    /// never restarts `Hello` or replays application state.
    pub async fn accept(
        self: &Arc<Self>,
        mut link: Box<dyn Link>,
    ) -> Result<AcceptOutcome, SecureError> {
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
        // The single-use pairing secret is consumed inside `handshake`, at the
        // moment the device is authorised, so two racing handshakes cannot
        // both pair with the same QR.
        let (mut path, device, _newly_paired) =
            self.handshake(&mut *link, session_id, packet.body).await?;

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
        // The channels only exist for a brand-new logical session. A resumed
        // path reuses the ones the existing application loop already owns.
        type AppChannels = (
            mpsc::Receiver<AppFrame>,
            mpsc::UnboundedReceiver<SessionCommand>,
        );
        let (endpoint, app_channels, welcome, pending, ticket) = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.evict_stale(&mut sessions, now);
            let (endpoint, app_channels): (Arc<SessionEndpoint>, Option<AppChannels>) = match token
            {
                Some(token) => {
                    let endpoint = sessions
                        .get(&session_id)
                        .cloned()
                        .ok_or(SecureError::NotPaired)?;
                    {
                        let mut guard = lock(&endpoint.session);
                        if !guard.token_matches(&token) {
                            return Err(SecureError::NotPaired);
                        }
                        match guard.device {
                            Some(existing) if existing != device => {
                                return Err(SecureError::Rejected(RejectReason::NotPaired));
                            }
                            Some(_) => {}
                            None => guard.set_device(device),
                        }
                    }
                    (endpoint, None)
                }
                None => {
                    if sessions.contains_key(&session_id) {
                        return Err(SecureError::Protocol("session id is already in use"));
                    }
                    let mut session = ServerSession::new(session_id, now)?;
                    session.set_device(device);
                    let session = Arc::new(Mutex::new(session));
                    let (frames, frames_rx) = mpsc::channel(256);
                    let (commands, commands_rx) = mpsc::unbounded_channel();
                    let endpoint = Arc::new(SessionEndpoint {
                        session: Arc::clone(&session),
                        frames,
                        commands,
                        closed: Arc::new(AtomicBool::new(false)),
                        notify: Arc::new(tokio::sync::Notify::new()),
                        live_paths: AtomicUsize::new(0),
                        liveness: watch::channel(false).0,
                        ever_live: Arc::new(AtomicBool::new(false)),
                    });
                    sessions.insert(session_id, Arc::clone(&endpoint));
                    (endpoint, Some((frames_rx, commands_rx)))
                }
            };
            let mut guard = lock(&endpoint.session);
            let welcome = guard.attach(path_id, recv_next, send_next, now)?;
            let (ack, ack_bits) = guard.ack_state();
            let pending = guard.sendable(now, true);
            let ticket = ResumeTicket {
                session_id,
                token: guard.token,
            };
            drop(guard);
            (
                Arc::clone(&endpoint),
                app_channels,
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

        let service = Arc::clone(self);
        let path_endpoint = Arc::clone(&endpoint);
        // Counted before the task starts, so a caller never misses the first
        // attach, and released when the task ends, whatever ended it.
        let live_path = LivePath::new(Arc::clone(&endpoint));
        tokio::spawn(async move {
            let _live = live_path;
            service.serve_path(path_endpoint, path_id, path, link).await;
        });

        // A resumed path attaches to a session whose application loop is
        // already running. Starting another one here would fork the outbound
        // stream, so the caller gets no handle.
        let Some((frames_rx, commands_rx)) = app_channels else {
            return Ok(AcceptOutcome::Resumed { session_id });
        };

        // The command loop is session-scoped, not path-scoped. It survives a
        // path dying because the failover path only needs the shared channel.
        let loop_service = Arc::clone(self);
        let loop_endpoint = Arc::clone(&endpoint);
        tokio::spawn(async move {
            loop_service.run_commands(loop_endpoint, commands_rx).await;
        });

        Ok(AcceptOutcome::New(SessionHandle {
            session_id,
            device,
            ticket,
            path: path_id,
            frames: frames_rx,
            commands: endpoint.commands.clone(),
            closed: Arc::clone(&endpoint.closed),
            liveness: SessionLiveness {
                attached: endpoint.liveness.subscribe(),
                ever_live: Arc::clone(&endpoint.ever_live),
            },
        }))
    }

    /// Drains application frames into the logical session and wakes the path
    /// tasks that carry them.
    ///
    /// Exactly one of these runs per logical session. A resumed path attaches
    /// to the same session and never starts a second loop, so a direct-to-relay
    /// failover cannot reorder or duplicate the outbound stream.
    async fn run_commands(
        &self,
        endpoint: Arc<SessionEndpoint>,
        mut commands: mpsc::UnboundedReceiver<SessionCommand>,
    ) {
        while let Some(command) = commands.recv().await {
            match command {
                SessionCommand::Frame(frame) => {
                    let now = self.now_ms();
                    if lock(&endpoint.session).send(frame, now).is_err() {
                        // The retransmit window is full because the peer is not
                        // acknowledging. Stop pretending the session is healthy;
                        // the path tasks send `Close` and detach.
                        endpoint.closed.store(true, Ordering::SeqCst);
                    }
                    endpoint.notify.notify_waiters();
                }
                SessionCommand::Close => {
                    endpoint.closed.store(true, Ordering::SeqCst);
                    endpoint.notify.notify_waiters();
                    break;
                }
            }
        }
    }

    async fn handshake(
        &self,
        link: &mut dyn Link,
        session_id: SessionId,
        mut message: Vec<u8>,
    ) -> Result<(ServerPath, [u8; KEY_LEN], bool), SecureError> {
        // Read the live secret: a pairing payload installed after the daemon
        // started must be usable on the next handshake. The snapshot is only
        // a candidate; `consume_psk` decides who actually owns it.
        let psk_snapshot = self.psk();
        let mut path = ServerPath::new(
            session_id,
            self.options.identity.clone(),
            psk_snapshot,
            self.options.rekey,
        );
        let mut paired_through_psk = false;
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
                    if !self.paired_through_psk(psk_snapshot, device) {
                        let _ = reject(link, session_id, RejectReason::NotPaired).await;
                        return Err(SecureError::Rejected(RejectReason::NotPaired));
                    }
                    paired_through_psk = true;
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
                        // A path that authorised in an earlier step already
                        // consumed the secret and allowed the device.
                        if !self.is_allowed(&device)
                            && !self.paired_through_psk(psk_snapshot, device)
                        {
                            let _ = reject(link, session_id, RejectReason::NotPaired).await;
                            return Err(SecureError::Rejected(RejectReason::NotPaired));
                        }
                        paired_through_psk = true;
                    }
                    if let Some(reply) = reply {
                        send_handshake(link, session_id, reply).await?;
                    }
                    return Ok((path, device, paired_through_psk));
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
        endpoint: Arc<SessionEndpoint>,
        path_id: PathId,
        mut path: ServerPath,
        mut link: Box<dyn Link>,
    ) {
        let mut ticker = tokio::time::interval(self.options.tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut idle_since = self.now_ms();
        let session_id = session_id_of(&endpoint.session);

        loop {
            if endpoint.closed.load(Ordering::SeqCst) {
                break;
            }
            let keep_going = tokio::select! {
                // The session command loop queued a frame, or the session was
                // closed/revoked. Either way, wake up and flush promptly.
                _ = endpoint.notify.notified() => true,
                packet = link.recv_packet() => {
                    self.on_packet(
                        &endpoint.session,
                        path_id,
                        &mut path,
                        &mut *link,
                        &endpoint.frames,
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
                &endpoint.session,
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

        if endpoint.closed.load(Ordering::SeqCst) {
            // Tell the peer this path is finished so it drops the socket now
            // rather than waiting for the timeout. Every attached path sends a
            // Close; the peer treats the first one as final.
            let (ack, ack_bits) = lock(&endpoint.session).ack_state();
            let mut record = Record::control(&Control::Close { code: 0 });
            record.ack = ack;
            record.ack_bits = ack_bits;
            let _ = send_record(&mut path, &mut *link, session_id, record).await;
        }

        lock(&endpoint.session).detach(path_id);
        endpoint.notify.notify_waiters();
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

    fn evict_stale(&self, sessions: &mut HashMap<SessionId, Arc<SessionEndpoint>>, now: u64) {
        // Keep a detached session long enough for a client to resume it, then
        // reclaim it. The application protocol replays its own gap if the
        // resume arrives too late.
        let grace = self.options.path_timeout.as_millis() as u64 * 4;
        sessions.retain(|_, endpoint| {
            if endpoint.closed.load(Ordering::SeqCst) {
                return false;
            }
            let guard = lock(&endpoint.session);
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
    fn the_pairing_secret_is_consumed_exactly_once() {
        let service = SecureService::new(
            SecureOptions::new(DeviceIdentity::generate().unwrap()).with_psk([9u8; TOKEN_LEN]),
        );
        // Two handshakes both took a snapshot of the live secret.
        assert!(service.consume_psk(&[9u8; TOKEN_LEN]));
        assert!(!service.consume_psk(&[9u8; TOKEN_LEN]));
        assert_eq!(service.psk(), None);
    }

    #[test]
    fn a_wrong_secret_does_not_consume_the_live_one() {
        let service = SecureService::new(
            SecureOptions::new(DeviceIdentity::generate().unwrap()).with_psk([9u8; TOKEN_LEN]),
        );
        assert!(!service.consume_psk(&[1u8; TOKEN_LEN]));
        assert_eq!(service.psk(), Some([9u8; TOKEN_LEN]));
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
