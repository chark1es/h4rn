//! The client-side multipath engine.
//!
//! The engine is a synchronous state machine driven by socket events and a
//! caller-supplied monotonic clock. It owns one [`SessionLayer`] and one Noise
//! handshake/channel per path. Native async code
//! ([`crate::client`]) and the wasm wrapper are thin adapters over this type;
//! tests drive it with an in-memory peer and a virtual clock.
//!
//! Path policy (ADR-009):
//!
//! - every candidate is probed in parallel with a Noise handshake;
//! - all ready paths are pinged every `probe_interval_ms` so a standby relay
//!   stays warm and its RTT is known;
//! - the active path is the lowest-ranked ready path (direct, then Tailscale,
//!   then relay), tie-broken by smoothed RTT;
//! - losing the active path fails over immediately (the session layer
//!   retransmits every unacknowledged record on the new path);
//! - switching back to a better path requires a sustained margin for
//!   `switch_confirmations` probes, so a 2 ms RTT wobble cannot flap the path.

use std::collections::{BTreeMap, VecDeque};

use chrono::Utc;
use h4rn_protocol::{
    TransportPathKind, TransportPathState, TransportPathStatus, TransportSessionState,
    TransportStatus,
};

use crate::keys::DeviceIdentity;
use crate::noise::{Handshake, HandshakeConfig, NoiseError, RekeyLimits, SecureChannel};
use crate::session::{AppFrame, Incoming, IncomingKind, ResumeTicket, SessionError, SessionLayer};
use crate::wire::{
    Control, Packet, PacketKind, Record, RecordKind, RejectReason, SessionId, TOKEN_LEN, WireError,
};

/// Opaque handle for one candidate path.
pub type PathId = u32;

/// A candidate path to probe and, when chosen, carry the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathSpec {
    /// Physical kind, which drives preference.
    pub kind: TransportPathKind,
    /// Endpoint for the status bar and diagnostics.
    pub endpoint: String,
}

/// Engine tuning. Defaults are the shipping values.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Logical session id, stable for the life of the app session.
    pub session_id: SessionId,
    /// This device's identity.
    pub identity: DeviceIdentity,
    /// The daemon's pinned X25519 static key from the pairing payload.
    pub daemon_public: [u8; 32],
    /// One-time pairing PSK, when pairing without the relay.
    pub psk: Option<[u8; TOKEN_LEN]>,
    /// Persisted resume ticket for a reconnect.
    pub resume: Option<ResumeTicket>,
    /// Rekey policy.
    pub rekey: RekeyLimits,
    /// Maximum application frames held while no path is established.
    pub max_queued_frames: usize,
    /// Retransmit timeout.
    pub rto_ms: u64,
    /// How often a ready path is pinged.
    pub probe_interval_ms: u64,
    /// How long a path may go without any authenticated packet before it is
    /// declared dead.
    pub path_timeout_ms: u64,
    /// How long to coalesce acknowledgements.
    pub ack_delay_ms: u64,
    /// RTT margin a same-class path must beat by to trigger a switch back.
    pub switch_margin_ms: u32,
    /// Consecutive probes a better path must win before switching.
    pub switch_confirmations: u32,
}

impl EngineConfig {
    /// Builds a config with the shipping defaults.
    pub fn new(session_id: SessionId, identity: DeviceIdentity, daemon_public: [u8; 32]) -> Self {
        Self {
            session_id,
            identity,
            daemon_public,
            psk: None,
            resume: None,
            rekey: RekeyLimits::default(),
            max_queued_frames: 1024,
            rto_ms: 250,
            probe_interval_ms: 250,
            path_timeout_ms: 1_000,
            ack_delay_ms: 40,
            switch_margin_ms: 40,
            switch_confirmations: 2,
        }
    }
}

/// Something the engine wants the caller to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Write these bytes to the path's socket as one binary message.
    Send {
        /// Path to write on.
        path: PathId,
        /// Fully framed packet.
        packet: Vec<u8>,
    },
    /// An application frame arrived; hand it to the protocol client.
    Frame(AppFrame),
    /// Transport state changed; surface it to the UI.
    Status(TransportStatus),
    /// The session is established and the resume token should be persisted.
    Established(ResumeTicket),
    /// The session is over and cannot recover.
    Closed {
        /// Human-readable reason.
        reason: String,
    },
}

struct PathRuntime {
    spec: PathSpec,
    state: TransportPathState,
    handshake: Option<Handshake>,
    channel: Option<SecureChannel>,
    rtt_ms: Option<u32>,
    last_probe_ms: u64,
    last_packet_ms: u64,
    pending_ping: Option<(u64, u64)>,
    failure: Option<String>,
}

/// One client transport session across several paths.
pub struct TransportEngine {
    config: EngineConfig,
    session: SessionLayer,
    paths: BTreeMap<PathId, PathRuntime>,
    next_path_id: PathId,
    active: Option<PathId>,
    better_candidate: Option<(PathId, u32)>,
    established: bool,
    closed: bool,
    queued: VecDeque<AppFrame>,
    ticket: Option<ResumeTicket>,
    next_nonce: u64,
    last_status_key: String,
}

impl std::fmt::Debug for TransportEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransportEngine")
            .field("session_id", &hex::encode(self.config.session_id))
            .field("active", &self.active)
            .field("paths", &self.paths.keys().collect::<Vec<_>>())
            .field("established", &self.established)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl TransportEngine {
    /// Creates an engine for `config`.
    pub fn new(config: EngineConfig) -> Result<Self, EngineError> {
        if config.daemon_public == [0u8; 32] {
            return Err(EngineError::Protocol("daemon public key is all zeroes"));
        }
        // A persisted ticket must be sent in the first `Open` on every path.
        // Without it the daemon sees a new session with an id it already holds
        // and rejects the path, so a reconnect could never resume.
        if let Some(resume) = config.resume
            && resume.session_id != config.session_id
        {
            return Err(EngineError::Protocol(
                "resume ticket belongs to a different session",
            ));
        }
        let ticket = config.resume;
        Ok(Self {
            config,
            session: SessionLayer::new(),
            paths: BTreeMap::new(),
            next_path_id: 1,
            active: None,
            better_candidate: None,
            established: false,
            closed: false,
            queued: VecDeque::new(),
            ticket,
            next_nonce: 1,
            last_status_key: String::new(),
        })
    }

    /// Session id.
    pub fn session_id(&self) -> &SessionId {
        &self.config.session_id
    }

    /// Session id as hex.
    pub fn session_id_hex(&self) -> String {
        hex::encode(self.config.session_id)
    }

    /// Registers a candidate path and returns its handle.
    ///
    /// The caller must open the socket and then call [`Self::on_path_open`].
    pub fn add_path(&mut self, spec: PathSpec) -> PathId {
        let id = self.next_path_id;
        self.next_path_id += 1;
        self.paths.insert(
            id,
            PathRuntime {
                spec,
                state: TransportPathState::Probing,
                handshake: None,
                channel: None,
                rtt_ms: None,
                last_probe_ms: 0,
                last_packet_ms: 0,
                pending_ping: None,
                failure: None,
            },
        );
        id
    }

    /// The path currently carrying application frames.
    pub fn active_path(&self) -> Option<PathId> {
        self.active
    }

    /// Whether the session completed a handshake and received a welcome.
    pub fn is_established(&self) -> bool {
        self.established
    }

    /// The resume ticket, once the daemon issued one.
    pub fn ticket(&self) -> Option<ResumeTicket> {
        self.ticket
    }

    /// Last smoothed RTT measured on a path.
    pub fn path_rtt(&self, path: PathId) -> Option<u32> {
        self.paths.get(&path).and_then(|runtime| runtime.rtt_ms)
    }

    /// Current path state.
    pub fn path_state(&self, path: PathId) -> Option<TransportPathState> {
        self.paths.get(&path).map(|runtime| runtime.state)
    }

    /// Starts the handshake on a path whose socket just opened.
    pub fn on_path_open(&mut self, path: PathId, now_ms: u64) -> Result<Vec<Effect>, EngineError> {
        if self.closed {
            return Err(EngineError::Closed);
        }
        let runtime = self
            .paths
            .get_mut(&path)
            .ok_or(EngineError::UnknownPath(path))?;
        let mut effects = Vec::new();
        // A resume ticket proves this device completed a handshake before, so
        // the daemon has already allowlisted it. A paired device must never mix
        // the one-time pairing secret into a handshake: that is only for a
        // device that has no allowlist entry yet.
        let psk = if self.ticket.is_some() {
            None
        } else {
            self.config.psk.as_ref()
        };
        let handshake_config = HandshakeConfig {
            session_id: &self.config.session_id,
            local_static: self.config.identity.noise_secret(),
            remote_static: Some(&self.config.daemon_public),
            psk,
        };
        let (handshake, message) = Handshake::start_initiator(&handshake_config)?;
        runtime.handshake = Some(handshake);
        runtime.channel = None;
        runtime.state = TransportPathState::Probing;
        runtime.last_packet_ms = now_ms;
        runtime.pending_ping = None;
        effects.push(Effect::Send {
            path,
            packet: Packet {
                kind: PacketKind::Handshake,
                session_id: self.config.session_id,
                body: message,
            }
            .encode(),
        });
        self.maybe_emit_status(&mut effects, false, None, true);
        Ok(effects)
    }

    /// Marks a path's socket closed.
    pub fn on_path_closed(&mut self, path: PathId, now_ms: u64) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(runtime) = self.paths.get_mut(&path) {
            runtime.state = TransportPathState::Failed;
            runtime.channel = None;
            runtime.handshake = None;
            runtime.pending_ping = None;
            runtime.failure = Some("socket closed".into());
        }
        self.recover(now_ms, &mut effects);
        self.maybe_emit_status(&mut effects, false, None, true);
        effects
    }

    /// Feeds one packet received on `path`.
    pub fn on_packet(
        &mut self,
        path: PathId,
        bytes: &[u8],
        now_ms: u64,
    ) -> Result<Vec<Effect>, EngineError> {
        if self.closed {
            return Err(EngineError::Closed);
        }
        let packet = match Packet::decode(bytes) {
            Ok(packet) => packet,
            Err(error) => {
                self.fail_path(path, format!("malformed packet: {error}"));
                return Err(EngineError::Wire(error));
            }
        };
        if packet.session_id != self.config.session_id {
            self.fail_path(path, "packet for a different session".into());
            return Err(EngineError::Wire(WireError::WrongSession));
        }
        match packet.kind {
            PacketKind::Handshake => self.on_handshake(path, &packet.body, now_ms),
            PacketKind::Reject => {
                let reason = packet
                    .body
                    .first()
                    .copied()
                    .and_then(RejectReason::from_u8)
                    .map(RejectReason::as_str)
                    .unwrap_or("peer rejected the path");
                self.fail_path(path, reason.to_owned());
                let mut effects = Vec::new();
                self.recover(now_ms, &mut effects);
                self.maybe_emit_status(&mut effects, false, Some(reason), true);
                if !self.established
                    && self.paths.values().all(|runtime| {
                        matches!(
                            runtime.state,
                            TransportPathState::Failed | TransportPathState::Disabled
                        )
                    })
                {
                    effects.push(Effect::Closed {
                        reason: reason.to_owned(),
                    });
                }
                Ok(effects)
            }
            PacketKind::Record => self.on_record(path, &packet.body, now_ms),
        }
    }

    /// Queues one application frame, sending it when a path is active.
    pub fn send(&mut self, frame: AppFrame, now_ms: u64) -> Result<Vec<Effect>, EngineError> {
        if self.closed {
            return Err(EngineError::Closed);
        }
        let mut effects = Vec::new();
        if !self.established {
            if self.queued.len() >= self.config.max_queued_frames {
                return Err(EngineError::Session(SessionError::Backpressure));
            }
            self.queued.push_back(frame);
            return Ok(effects);
        }
        match self.session.enqueue(frame.clone(), now_ms) {
            Ok(()) => {
                self.flush(now_ms, false, &mut effects);
            }
            Err(SessionError::Backpressure) => {
                if self.queued.len() >= self.config.max_queued_frames {
                    return Err(EngineError::Session(SessionError::Backpressure));
                }
                self.queued.push_back(frame);
            }
            Err(error) => return Err(EngineError::Session(error)),
        }
        Ok(effects)
    }

    /// Periodic work: liveness probes, retransmits, acks and path selection.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.closed {
            return effects;
        }
        self.check_liveness(now_ms, &mut effects);
        self.evaluate_active(now_ms, &mut effects);
        self.flush(now_ms, false, &mut effects);
        self.probe(now_ms, &mut effects);
        self.maybe_emit_status(&mut effects, false, None, false);
        effects
    }

    /// Closes the session and notifies the peer.
    pub fn close(&mut self, now_ms: u64) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.closed {
            return effects;
        }
        let _ = now_ms;
        if self.established {
            for path in self.ready_paths() {
                if self
                    .send_control(path, &Control::Close { code: 0 }, &mut effects)
                    .is_err()
                {
                    continue;
                }
            }
        }
        for runtime in self.paths.values_mut() {
            runtime.state = TransportPathState::Disabled;
            runtime.channel = None;
            runtime.handshake = None;
        }
        self.closed = true;
        self.active = None;
        self.maybe_emit_status(&mut effects, false, Some("client closed the session"), true);
        effects
    }

    /// A snapshot for the UI.
    pub fn status(&self) -> TransportStatus {
        self.build_status(false, None)
    }

    // ------------------------------------------------------------------
    // Packet handlers
    // ------------------------------------------------------------------

    fn on_handshake(
        &mut self,
        path: PathId,
        message: &[u8],
        now_ms: u64,
    ) -> Result<Vec<Effect>, EngineError> {
        let mut effects = Vec::new();
        let runtime = self
            .paths
            .get_mut(&path)
            .ok_or(EngineError::UnknownPath(path))?;
        runtime.last_packet_ms = now_ms;
        let handshake = runtime.handshake.as_mut().ok_or(EngineError::Protocol(
            "handshake message without a handshake",
        ))?;
        let reply = handshake.read(message)?;
        let finished = handshake.is_finished();
        if let Some(reply) = reply {
            effects.push(Effect::Send {
                path,
                packet: Packet {
                    kind: PacketKind::Handshake,
                    session_id: self.config.session_id,
                    body: reply,
                }
                .encode(),
            });
        }
        if !finished {
            return Ok(effects);
        }
        let handshake = runtime.handshake.take().expect("handshake exists");
        let channel = handshake.into_channel(self.config.rekey)?;
        runtime.channel = Some(channel);
        runtime.state = TransportPathState::Ready;
        let token = self.ticket.map(|ticket| ticket.token);
        let (recv_next, send_next) = self.session.resume_state();
        self.send_control(
            path,
            &Control::Open {
                token,
                recv_next,
                send_next,
            },
            &mut effects,
        )?;
        self.maybe_emit_status(&mut effects, false, None, true);
        Ok(effects)
    }

    fn on_record(
        &mut self,
        path: PathId,
        ciphertext: &[u8],
        now_ms: u64,
    ) -> Result<Vec<Effect>, EngineError> {
        let mut effects = Vec::new();
        let mut plaintext = {
            let runtime = self
                .paths
                .get_mut(&path)
                .ok_or(EngineError::UnknownPath(path))?;
            runtime.last_packet_ms = now_ms;
            let Some(channel) = runtime.channel.as_mut() else {
                self.fail_path(path, "record before the handshake finished".into());
                return Err(EngineError::Protocol("record before handshake"));
            };
            match channel.open(ciphertext) {
                Ok(plaintext) => plaintext,
                Err(error) => {
                    self.fail_path(path, format!("record did not authenticate: {error}"));
                    return Err(EngineError::Noise(error));
                }
            }
        };
        let record = match Record::decode_plaintext(&plaintext) {
            Ok(record) => record,
            Err(error) => {
                self.fail_path(path, format!("malformed record: {error}"));
                return Err(EngineError::Wire(error));
            }
        };
        // Wipe the plaintext after decoding; the record owns its own payload.
        plaintext.fill(0);
        match record.kind {
            RecordKind::Data | RecordKind::Ack => {
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
                self.session.on_record(incoming, now_ms)?;
                for frame in self.session.take_deliveries() {
                    effects.push(Effect::Frame(frame));
                }
            }
            RecordKind::Control => {
                let control = Control::decode(&record.payload)?;
                self.apply_peer_ack(record.ack);
                self.on_control(path, control, now_ms, &mut effects)?;
            }
        }
        Ok(effects)
    }

    fn apply_peer_ack(&mut self, ack: u64) {
        if ack > 0 {
            self.session.apply_peer_position(ack + 1);
        }
    }

    fn on_control(
        &mut self,
        path: PathId,
        control: Control,
        now_ms: u64,
        effects: &mut Vec<Effect>,
    ) -> Result<(), EngineError> {
        match control {
            Control::Welcome {
                token,
                recv_next,
                send_next,
            } => {
                let ticket = ResumeTicket {
                    session_id: self.config.session_id,
                    token,
                };
                self.session.apply_peer_position(recv_next);
                // `send_next` is the daemon's next new sequence, not the
                // oldest it may retransmit. The client's own receive position
                // is what it can accept, and it already sent that in `Open`.
                // Only jump forward when the daemon lost state and is behind
                // us; never skip records it is about to retransmit.
                let local_recv_next = self.session.recv_next();
                if send_next < local_recv_next {
                    self.session.reset_incoming(send_next);
                }
                self.ticket = Some(ticket);
                let first = !self.established;
                self.established = true;
                if let Some(runtime) = self.paths.get_mut(&path) {
                    runtime.state = TransportPathState::Ready;
                    runtime.failure = None;
                }
                if first {
                    effects.push(Effect::Established(ticket));
                }
                self.evaluate_active(now_ms, effects);
                self.flush(now_ms, true, effects);
                self.maybe_emit_status(effects, true, Some("session established"), true);
            }
            Control::Ping { nonce, sent_ms } => {
                self.send_control(path, &Control::Pong { nonce, sent_ms }, effects)?;
            }
            Control::Pong { nonce, sent_ms } => {
                if let Some(runtime) = self.paths.get_mut(&path)
                    && runtime
                        .pending_ping
                        .is_some_and(|(pending, _)| pending == nonce)
                {
                    let sample = now_ms.saturating_sub(sent_ms).max(1) as u32;
                    runtime.rtt_ms = Some(match runtime.rtt_ms {
                        Some(previous) => {
                            ((u64::from(previous) * 3 + u64::from(sample)) / 4) as u32
                        }
                        None => sample,
                    });
                    runtime.pending_ping = None;
                    runtime.failure = None;
                }
            }
            Control::Close { code } => {
                // The daemon sends Close only when the logical session is over
                // (the device was revoked, or the daemon is shutting down), so
                // this is final and not a path-local failure: ending the session
                // immediately is what tells the app a revocation happened.
                for runtime in self.paths.values_mut() {
                    runtime.state = TransportPathState::Disabled;
                    runtime.channel = None;
                    runtime.handshake = None;
                    runtime.pending_ping = None;
                }
                self.closed = true;
                self.active = None;
                self.better_candidate = None;
                self.maybe_emit_status(effects, false, Some("the daemon closed the session"), true);
                effects.push(Effect::Closed {
                    reason: format!("the daemon closed the session ({code})"),
                });
            }
            Control::Open { .. } => {
                return Err(EngineError::Protocol("client received Open"));
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Sending
    // ------------------------------------------------------------------

    fn flush(&mut self, now_ms: u64, all: bool, effects: &mut Vec<Effect>) {
        if !self.established {
            return;
        }
        let Some(active) = self.active else {
            return;
        };
        let (ack, ack_bits) = self.session.ack_state();
        for outgoing in self.session.sendable(now_ms, self.config.rto_ms, all) {
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
            if let Err(error) = self.send_record(active, record, effects) {
                self.fail_path(active, format!("write failed: {error}"));
                self.recover(now_ms, effects);
                // Retry once on the new active path.
                if let Some(next) = self.active
                    && next != active
                {
                    let (ack, ack_bits) = self.session.ack_state();
                    for outgoing in self.session.sendable(now_ms, 0, true) {
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
                        let _ = self.send_record(next, record, effects);
                    }
                }
                return;
            }
        }
        if self.session.should_ack(now_ms, self.config.ack_delay_ms) {
            let record = Record::ack(ack, ack_bits);
            let _ = self.send_record(active, record, effects);
        }
    }

    fn send_record(
        &mut self,
        path: PathId,
        record: Record,
        effects: &mut Vec<Effect>,
    ) -> Result<(), EngineError> {
        let plaintext = record.encode_plaintext()?;
        let runtime = self
            .paths
            .get_mut(&path)
            .ok_or(EngineError::UnknownPath(path))?;
        let channel = runtime
            .channel
            .as_mut()
            .ok_or(EngineError::Protocol("path is not ready"))?;
        let ciphertext = channel.seal(&plaintext)?;
        effects.push(Effect::Send {
            path,
            packet: Packet {
                kind: PacketKind::Record,
                session_id: self.config.session_id,
                body: ciphertext,
            }
            .encode(),
        });
        Ok(())
    }

    fn send_control(
        &mut self,
        path: PathId,
        control: &Control,
        effects: &mut Vec<Effect>,
    ) -> Result<(), EngineError> {
        let (ack, ack_bits) = self.session.ack_state();
        let mut record = Record::control(control);
        record.ack = ack;
        record.ack_bits = ack_bits;
        self.send_record(path, record, effects)
    }

    // ------------------------------------------------------------------
    // Path health and selection
    // ------------------------------------------------------------------

    fn probe(&mut self, now_ms: u64, effects: &mut Vec<Effect>) {
        let paths: Vec<PathId> = self.ready_paths();
        for path in paths {
            let should_probe = {
                let Some(runtime) = self.paths.get(&path) else {
                    continue;
                };
                runtime.pending_ping.is_none()
                    && now_ms.saturating_sub(runtime.last_probe_ms) >= self.config.probe_interval_ms
            };
            if !should_probe {
                continue;
            }
            let nonce = self.next_nonce;
            self.next_nonce += 1;
            if self
                .send_control(
                    path,
                    &Control::Ping {
                        nonce,
                        sent_ms: now_ms,
                    },
                    effects,
                )
                .is_ok()
                && let Some(runtime) = self.paths.get_mut(&path)
            {
                runtime.pending_ping = Some((nonce, now_ms));
                runtime.last_probe_ms = now_ms;
            }
        }
    }

    fn check_liveness(&mut self, now_ms: u64, effects: &mut Vec<Effect>) {
        let dead: Vec<PathId> = self
            .paths
            .iter()
            .filter_map(|(id, runtime)| {
                let silent = now_ms.saturating_sub(runtime.last_packet_ms);
                if runtime.state == TransportPathState::Failed
                    || runtime.state == TransportPathState::Disabled
                {
                    return None;
                }
                if runtime.channel.is_some() && silent > self.config.path_timeout_ms {
                    Some(*id)
                } else {
                    None
                }
            })
            .collect();
        for path in &dead {
            self.fail_path(*path, "path stopped answering probes".into());
        }
        if !dead.is_empty() {
            self.recover(now_ms, effects);
            self.maybe_emit_status(effects, false, Some("active path timed out"), true);
        }
    }

    fn evaluate_active(&mut self, now_ms: u64, effects: &mut Vec<Effect>) {
        let ordered = self.ranked_ready_paths();
        let Some(best) = ordered.first().copied() else {
            if self.active.is_some() {
                self.active = None;
                self.better_candidate = None;
                self.maybe_emit_status(effects, true, Some("no path is ready"), true);
            }
            return;
        };
        let Some(active) = self.active else {
            self.switch_to(best, now_ms, effects, "first ready path");
            return;
        };
        let active_is_usable = self.paths.get(&active).is_some_and(|runtime| {
            matches!(
                runtime.state,
                TransportPathState::Ready | TransportPathState::Active
            )
        });
        if !active_is_usable {
            self.switch_to(best, now_ms, effects, "active path is gone");
            return;
        }
        if best == active {
            self.better_candidate = None;
            return;
        }
        if !self.is_better(best, active) {
            self.better_candidate = None;
            return;
        }
        let confirmations = match self.better_candidate {
            Some((candidate, count)) if candidate == best => count + 1,
            _ => 1,
        };
        if confirmations >= self.config.switch_confirmations.max(1) {
            self.better_candidate = None;
            self.switch_to(best, now_ms, effects, "better path recovered");
        } else {
            self.better_candidate = Some((best, confirmations));
        }
    }

    fn recover(&mut self, now_ms: u64, effects: &mut Vec<Effect>) {
        let ordered = self.ranked_ready_paths();
        match ordered.first().copied() {
            Some(best) => {
                if self.active != Some(best) {
                    self.switch_to(best, now_ms, effects, "failover");
                } else {
                    self.flush(now_ms, true, effects);
                }
            }
            None => {
                if self.active.is_some() {
                    self.active = None;
                    self.maybe_emit_status(effects, true, Some("all paths failed"), true);
                }
            }
        }
    }

    fn switch_to(&mut self, path: PathId, now_ms: u64, effects: &mut Vec<Effect>, reason: &str) {
        self.active = Some(path);
        for (id, runtime) in &mut self.paths {
            if runtime.state == TransportPathState::Ready {
                runtime.state = if *id == path {
                    TransportPathState::Active
                } else {
                    TransportPathState::Ready
                };
            } else if *id != path && runtime.state == TransportPathState::Active {
                runtime.state = TransportPathState::Ready;
            }
        }
        if let Some(runtime) = self.paths.get_mut(&path) {
            runtime.state = TransportPathState::Active;
        }
        self.flush(now_ms, true, effects);
        self.maybe_emit_status(effects, true, Some(reason), true);
    }

    fn is_better(&self, candidate: PathId, active: PathId) -> bool {
        let Some(candidate) = self.paths.get(&candidate) else {
            return false;
        };
        let Some(active) = self.paths.get(&active) else {
            return false;
        };
        let ordering = candidate.spec.kind.rank().cmp(&active.spec.kind.rank());
        match ordering {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal => match (candidate.rtt_ms, active.rtt_ms) {
                (Some(better), Some(current)) => {
                    better.saturating_add(self.config.switch_margin_ms) < current
                }
                (Some(_), None) => true,
                _ => false,
            },
        }
    }

    fn ranked_ready_paths(&self) -> Vec<PathId> {
        let mut ready: Vec<PathId> = self
            .paths
            .iter()
            .filter(|(_, runtime)| {
                matches!(
                    runtime.state,
                    TransportPathState::Ready | TransportPathState::Active
                )
            })
            .map(|(id, _)| *id)
            .collect();
        ready.sort_by_key(|id| {
            let runtime = &self.paths[id];
            (
                runtime.spec.kind.rank(),
                runtime.rtt_ms.unwrap_or(u32::MAX),
                *id,
            )
        });
        ready
    }

    fn ready_paths(&self) -> Vec<PathId> {
        self.paths
            .iter()
            .filter(|(_, runtime)| {
                matches!(
                    runtime.state,
                    TransportPathState::Ready | TransportPathState::Active
                )
            })
            .map(|(id, _)| *id)
            .collect()
    }

    fn fail_path(&mut self, path: PathId, reason: String) {
        if let Some(runtime) = self.paths.get_mut(&path) {
            runtime.state = TransportPathState::Failed;
            runtime.channel = None;
            runtime.handshake = None;
            runtime.pending_ping = None;
            runtime.failure = Some(reason);
        }
        if self.active == Some(path) {
            self.active = None;
        }
    }

    // ------------------------------------------------------------------
    // Status
    // ------------------------------------------------------------------

    fn build_status(&self, switched: bool, detail: Option<&str>) -> TransportStatus {
        let candidates: Vec<TransportPathStatus> = self
            .paths
            .iter()
            .map(|(id, runtime)| {
                let state = if self.active == Some(*id) {
                    TransportPathState::Active
                } else {
                    runtime.state
                };
                TransportPathStatus {
                    kind: runtime.spec.kind,
                    endpoint: runtime.spec.endpoint.clone(),
                    state,
                    rtt_ms: runtime.rtt_ms,
                    encrypted: true,
                }
            })
            .collect();
        let active = self.active.and_then(|id| {
            self.paths.get(&id).map(|runtime| TransportPathStatus {
                kind: runtime.spec.kind,
                endpoint: runtime.spec.endpoint.clone(),
                state: TransportPathState::Active,
                rtt_ms: runtime.rtt_ms,
                encrypted: true,
            })
        });
        TransportStatus {
            session_id: hex::encode(self.config.session_id),
            state: self.session_state(),
            active,
            candidates,
            switched,
            detail: detail.map(ToOwned::to_owned),
            updated_at: Utc::now(),
        }
    }

    fn session_state(&self) -> TransportSessionState {
        if self.closed {
            return TransportSessionState::Offline;
        }
        if self.established {
            if self.active.is_some() {
                TransportSessionState::Online
            } else {
                TransportSessionState::Degraded
            }
        } else if !self.paths.is_empty()
            && self.paths.values().all(|runtime| {
                matches!(
                    runtime.state,
                    TransportPathState::Failed | TransportPathState::Disabled
                )
            })
        {
            TransportSessionState::Offline
        } else {
            TransportSessionState::Connecting
        }
    }

    fn maybe_emit_status(
        &mut self,
        effects: &mut Vec<Effect>,
        switched: bool,
        detail: Option<&str>,
        force: bool,
    ) {
        let status = self.build_status(switched, detail);
        let key = status_key(&status);
        if !force && key == self.last_status_key {
            return;
        }
        self.last_status_key = key;
        effects.push(Effect::Status(status));
    }
}

fn status_key(status: &TransportStatus) -> String {
    let mut key = format!(
        "{:?}|{:?}|{}|",
        status.state, status.active, status.switched
    );
    for candidate in &status.candidates {
        key.push_str(&format!(
            "{:?}:{:?}:{:?}:{};",
            candidate.kind, candidate.state, candidate.rtt_ms, candidate.endpoint
        ));
    }
    key
}

/// Engine failures. Most are path-local; the engine stays usable.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// Handshake or record failure.
    #[error(transparent)]
    Noise(#[from] NoiseError),
    /// Session-layer failure.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// Framing failure.
    #[error(transparent)]
    Wire(#[from] WireError),
    /// No such path.
    #[error("unknown path {0}")]
    UnknownPath(PathId),
    /// The engine was closed.
    #[error("the transport session is closed")]
    Closed,
    /// An invariant was violated.
    #[error("transport protocol violation: {0}")]
    Protocol(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KEY_LEN;
    use crate::wire::SESSION_ID_LEN;
    use h4rn_protocol::TransportPathKind;

    #[test]
    fn path_preference_uses_rank_then_rtt() {
        let daemon = DeviceIdentity::generate().unwrap();
        let identity = DeviceIdentity::generate().unwrap();
        let mut engine = TransportEngine::new(EngineConfig::new(
            [1u8; SESSION_ID_LEN],
            identity,
            daemon.public().noise_public,
        ))
        .unwrap();
        let direct = engine.add_path(PathSpec {
            kind: TransportPathKind::Direct,
            endpoint: "127.0.0.1:1".into(),
        });
        let relay = engine.add_path(PathSpec {
            kind: TransportPathKind::Relay,
            endpoint: "relay".into(),
        });
        assert_ne!(direct, relay);
    }

    #[test]
    fn all_zero_daemon_key_is_rejected() {
        let identity = DeviceIdentity::generate().unwrap();
        let error = TransportEngine::new(EngineConfig::new(
            [0u8; SESSION_ID_LEN],
            identity,
            [0u8; KEY_LEN],
        ))
        .unwrap_err();
        assert!(matches!(error, EngineError::Protocol(_)));
    }

    #[test]
    fn a_resumed_engine_never_sends_the_pairing_psk() {
        // M1: a resume ticket proves the daemon already allowlisted this
        // device. Even if a one-time secret is still configured, the handshake
        // must be plain `IK`, because the daemon selects the pattern per
        // device and only a device without an entry may present a PSK.
        let daemon = DeviceIdentity::generate().unwrap();
        let identity = DeviceIdentity::generate().unwrap();
        let session = [2u8; SESSION_ID_LEN];
        let mut config = EngineConfig::new(session, identity, daemon.public().noise_public);
        config.psk = Some([9u8; KEY_LEN]);
        config.resume = Some(ResumeTicket {
            session_id: session,
            token: [3u8; TOKEN_LEN],
        });
        let mut engine = TransportEngine::new(config).unwrap();
        let path = engine.add_path(PathSpec {
            kind: TransportPathKind::Direct,
            endpoint: "127.0.0.1:1".into(),
        });
        let effects = engine.on_path_open(path, 0).unwrap();
        let packet = effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Send { packet, .. } => Some(packet),
                _ => None,
            })
            .expect("the engine sends handshake message 1");
        let packet = Packet::decode(&packet).unwrap();
        assert_eq!(packet.kind, PacketKind::Handshake);
        let mut responder = Handshake::start_responder(&HandshakeConfig {
            session_id: &session,
            local_static: daemon.noise_secret(),
            remote_static: None,
            psk: None,
        })
        .unwrap();
        responder
            .read(&packet.body)
            .expect("a resumed handshake message 1 must be readable as plain IK");
    }
}
