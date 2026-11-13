//! Daemon-side session state.
//!
//! The daemon does not choose paths; it accepts a Noise handshake on whichever
//! path a client dials and attaches it to the logical session named in the
//! packet header. The session layer, not the socket, owns sequence numbers, so
//! a client that switches from LAN to relay resumes the same stream instead of
//! starting a new one.
//!
//! This module is pure (no async, no clock reads) so it compiles for wasm and
//! can be exercised without sockets; [`crate::server`] wraps it in the axum
//! connection loop.

use std::collections::BTreeSet;

use subtle::ConstantTimeEq;

use crate::engine::PathId;
use crate::keys::{DeviceIdentity, KEY_LEN};
use crate::noise::{Handshake, HandshakeConfig, NoiseError, RekeyLimits, SecureChannel};
use crate::session::{AppFrame, Incoming, Outgoing, SessionError, SessionLayer};
use crate::wire::{Control, RejectReason, SessionId, TOKEN_LEN};

/// Daemon policy for accepting a device key.
#[derive(Clone, Debug, Default)]
pub struct PairingPolicy {
    allowed: BTreeSet<[u8; KEY_LEN]>,
    revoked: BTreeSet<[u8; KEY_LEN]>,
    allow_psk_pairing: bool,
}

impl PairingPolicy {
    /// Builds a policy from the persisted allowlist.
    pub fn new(allowed: impl IntoIterator<Item = [u8; KEY_LEN]>) -> Self {
        Self {
            allowed: allowed.into_iter().collect(),
            revoked: BTreeSet::new(),
            allow_psk_pairing: false,
        }
    }

    /// Allows an unknown device to pair when it proves possession of the
    /// one-time PSK from the pairing payload.
    pub fn allow_psk_pairing(mut self, allow: bool) -> Self {
        self.allow_psk_pairing = allow;
        self
    }

    /// Permanently rejects a device key.
    pub fn revoke(&mut self, key: [u8; KEY_LEN]) {
        self.revoked.insert(key);
        self.allowed.remove(&key);
    }

    /// Adds a device key to the allowlist.
    pub fn allow(&mut self, key: [u8; KEY_LEN]) {
        self.revoked.remove(&key);
        self.allowed.insert(key);
    }

    /// Whether the key is currently allowed.
    pub fn is_allowed(&self, key: &[u8; KEY_LEN]) -> bool {
        !self.revoked.contains(key) && self.allowed.contains(key)
    }

    /// Decides whether a completed handshake may attach.
    ///
    /// Returns `Ok(true)` when the device is newly paired through the PSK and
    /// the caller must persist it, `Ok(false)` when it was already known.
    pub fn authorize(&self, key: &[u8; KEY_LEN], used_psk: bool) -> Result<bool, RejectReason> {
        if self.revoked.contains(key) {
            return Err(RejectReason::Revoked);
        }
        if self.allowed.contains(key) {
            return Ok(false);
        }
        if used_psk && self.allow_psk_pairing {
            return Ok(true);
        }
        Err(RejectReason::NotPaired)
    }
}

/// One path's daemon-side handshake and cipher.
pub struct ServerPath {
    session_id: SessionId,
    identity: DeviceIdentity,
    psk: Option<[u8; TOKEN_LEN]>,
    rekey: RekeyLimits,
    handshake: Option<Handshake>,
    channel: Option<SecureChannel>,
    device: Option<[u8; KEY_LEN]>,
}

impl ServerPath {
    /// Creates a path acceptor for `session_id`.
    pub fn new(
        session_id: SessionId,
        identity: DeviceIdentity,
        psk: Option<[u8; TOKEN_LEN]>,
        rekey: RekeyLimits,
    ) -> Self {
        Self {
            session_id,
            identity,
            psk,
            rekey,
            handshake: None,
            channel: None,
            device: None,
        }
    }

    /// Feeds one handshake message.
    pub fn on_handshake(
        &mut self,
        message: &[u8],
        policy: &PairingPolicy,
    ) -> Result<ServerHandshakeStep, NoiseError> {
        if self.handshake.is_none() {
            self.handshake = Some(Handshake::start_responder(&HandshakeConfig {
                session_id: &self.session_id,
                local_static: self.identity.noise_secret(),
                remote_static: None,
                psk: self.psk.as_ref(),
            })?);
        }
        let handshake = self.handshake.as_mut().expect("handshake exists");
        let reply = handshake.read(message)?;
        let used_psk = handshake.used_psk();
        let mut newly_paired = false;
        if let Some(device) = handshake.remote_static() {
            match policy.authorize(&device, used_psk) {
                Ok(paired) => {
                    self.device = Some(device);
                    newly_paired = paired;
                }
                Err(reason) => {
                    self.handshake = None;
                    return Ok(ServerHandshakeStep::Rejected(reason));
                }
            }
        }
        if handshake.is_finished() {
            let handshake = self.handshake.take().expect("handshake exists");
            let channel = handshake.into_channel(self.rekey)?;
            let device = self.device.unwrap_or([0u8; KEY_LEN]);
            self.channel = Some(channel);
            return Ok(ServerHandshakeStep::Complete {
                device,
                newly_paired,
                reply,
            });
        }
        if newly_paired {
            let device = self.device.expect("authorised device exists");
            return Ok(ServerHandshakeStep::PairedThroughPsk { device, reply });
        }
        Ok(match reply {
            Some(reply) => ServerHandshakeStep::Reply(reply),
            None => ServerHandshakeStep::Waiting,
        })
    }

    /// Decrypts one record.
    pub fn open(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let channel = self.channel.as_mut().ok_or(NoiseError::NotFinished)?;
        channel.open(ciphertext)
    }

    /// Seals one record.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let channel = self.channel.as_mut().ok_or(NoiseError::NotFinished)?;
        channel.seal(plaintext)
    }

    /// The authenticated device key, once known.
    pub fn device(&self) -> Option<[u8; KEY_LEN]> {
        self.device
    }

    /// Whether the handshake finished.
    pub fn is_ready(&self) -> bool {
        self.channel.is_some()
    }

    /// Messages sent on this path, for tests.
    pub fn sent_messages(&self) -> u64 {
        self.channel.as_ref().map_or(0, SecureChannel::send_count)
    }
}

/// Outcome of one handshake message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerHandshakeStep {
    /// Reply with this handshake message.
    Reply(Vec<u8>),
    /// A device paired with the one-time PSK; persist it, then continue.
    PairedThroughPsk {
        /// The new device's X25519 static public key.
        device: [u8; KEY_LEN],
        /// Handshake message to send to the peer, when this step produced one.
        reply: Option<Vec<u8>>,
    },
    /// The handshake is complete.
    Complete {
        /// The authenticated device key.
        device: [u8; KEY_LEN],
        /// True when the device just paired through the one-time PSK and the
        /// caller must persist it.
        newly_paired: bool,
        /// The final handshake message to send to the peer, when there is one.
        reply: Option<Vec<u8>>,
    },
    /// Nothing to send yet.
    Waiting,
    /// The device is not allowed.
    Rejected(RejectReason),
}

/// One logical session as the daemon tracks it.
pub struct ServerSession {
    /// Session id from the packet header.
    pub session_id: SessionId,
    /// Resume token. Compared in constant time.
    pub token: [u8; TOKEN_LEN],
    /// Reliability state shared by every attached path.
    pub layer: SessionLayer,
    /// Path currently used for outbound frames.
    pub active_path: Option<PathId>,
    /// Every attached path.
    pub attached: BTreeSet<PathId>,
    /// Creation time on the daemon's monotonic clock.
    pub created_ms: u64,
    /// Last time the session carried traffic.
    pub last_seen_ms: u64,
}

impl ServerSession {
    /// Creates a session with a fresh resume token.
    pub fn new(session_id: SessionId, now_ms: u64) -> Result<Self, SessionError> {
        let mut token = [0u8; TOKEN_LEN];
        getrandom::fill(&mut token).map_err(|_| SessionError::BadTicket)?;
        Ok(Self {
            session_id,
            token,
            layer: SessionLayer::new(),
            active_path: None,
            attached: BTreeSet::new(),
            created_ms: now_ms,
            last_seen_ms: now_ms,
        })
    }

    /// Creates a session with a known token (tests and persistence).
    pub fn with_token(session_id: SessionId, token: [u8; TOKEN_LEN], now_ms: u64) -> Self {
        Self {
            session_id,
            token,
            layer: SessionLayer::new(),
            active_path: None,
            attached: BTreeSet::new(),
            created_ms: now_ms,
            last_seen_ms: now_ms,
        }
    }

    /// Constant-time token check.
    pub fn token_matches(&self, candidate: &[u8; TOKEN_LEN]) -> bool {
        self.token.ct_eq(candidate).into()
    }

    /// Attaches a new path and reconciles the two sequence spaces.
    pub fn attach(
        &mut self,
        path: PathId,
        client_recv_next: u64,
        client_send_next: u64,
        now_ms: u64,
    ) -> Result<Control, SessionError> {
        self.attached.insert(path);
        self.active_path = Some(path);
        self.last_seen_ms = now_ms;
        // The client's receive position proves which of our records it has.
        self.layer.apply_peer_position(client_recv_next);
        if self.layer.next_seq() > client_recv_next
            && self
                .layer
                .oldest_unacked()
                .is_none_or(|oldest| oldest > client_recv_next)
        {
            // The retransmit buffer no longer covers the client's position
            // (a long outage or a client restart). Continue from where the
            // client actually is; the application protocol replays its own gap.
            self.layer.reset_outgoing(client_recv_next);
        }
        if client_send_next < self.layer.recv_next() {
            self.layer.reset_incoming(client_send_next);
        }
        let (recv_next, send_next) = self.layer.resume_state();
        Ok(Control::Welcome {
            token: self.token,
            recv_next,
            send_next,
        })
    }

    /// Detaches a path, promoting another attachment if it was active.
    pub fn detach(&mut self, path: PathId) -> Option<PathId> {
        self.attached.remove(&path);
        if self.active_path == Some(path) {
            self.active_path = self.attached.iter().next().copied();
        }
        self.active_path
    }

    /// Applies one received record.
    pub fn on_record(
        &mut self,
        _path: PathId,
        record: Incoming,
        now_ms: u64,
    ) -> Result<Vec<AppFrame>, SessionError> {
        self.last_seen_ms = now_ms;
        self.layer.on_record(record, now_ms)?;
        Ok(self.layer.take_deliveries())
    }

    /// Queues one outgoing frame.
    pub fn send(&mut self, frame: AppFrame, now_ms: u64) -> Result<(), SessionError> {
        self.layer.enqueue(frame, now_ms)
    }

    /// Records to send on the active path now.
    pub fn sendable(&mut self, now_ms: u64, all: bool) -> Vec<Outgoing> {
        self.layer.sendable(now_ms, 250, all)
    }

    /// Whether a bare acknowledgement is due.
    pub fn take_ack(&mut self, now_ms: u64, delay_ms: u64) -> Option<(u64, u32)> {
        if self.layer.should_ack(now_ms, delay_ms) {
            Some(self.layer.ack_state())
        } else {
            None
        }
    }

    /// Current ack fields to piggyback on an outgoing record.
    pub fn ack_state(&self) -> (u64, u32) {
        self.layer.ack_state()
    }
}

impl std::fmt::Debug for ServerSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerSession")
            .field("session_id", &hex::encode(self.session_id))
            .field("active_path", &self.active_path)
            .field("attached", &self.attached)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::IncomingKind;
    use crate::wire::SESSION_ID_LEN;

    fn daemon_identity() -> DeviceIdentity {
        DeviceIdentity::generate().unwrap()
    }

    #[test]
    fn pairing_policy_allows_known_and_psk_devices_only() {
        let known = [1u8; KEY_LEN];
        let unknown = [2u8; KEY_LEN];
        let policy = PairingPolicy::new([known]).allow_psk_pairing(true);
        assert_eq!(policy.authorize(&known, false), Ok(false));
        assert_eq!(policy.authorize(&unknown, true), Ok(true));
        assert_eq!(
            policy.authorize(&unknown, false),
            Err(RejectReason::NotPaired)
        );
        let mut revoked = policy.clone();
        revoked.revoke(known);
        assert_eq!(revoked.authorize(&known, true), Err(RejectReason::Revoked));
    }

    #[test]
    fn psk_pairing_completes_and_reports_the_device() {
        let daemon = daemon_identity();
        let client = DeviceIdentity::generate().unwrap();
        let session = [4u8; SESSION_ID_LEN];
        let psk = [9u8; TOKEN_LEN];
        let policy = PairingPolicy::new(Vec::new()).allow_psk_pairing(true);
        let mut path = ServerPath::new(session, daemon.clone(), Some(psk), RekeyLimits::default());

        let (mut initiator, message1) = Handshake::start_initiator(&HandshakeConfig {
            session_id: &session,
            local_static: client.noise_secret(),
            remote_static: Some(&daemon.public().noise_public),
            psk: Some(&psk),
        })
        .unwrap();
        let reply = path.on_handshake(&message1, &policy).unwrap();
        let ServerHandshakeStep::Complete {
            device,
            newly_paired: true,
            reply: Some(message2),
        } = reply
        else {
            panic!("expected a completed pairing with the handshake reply, got {reply:?}");
        };
        assert_eq!(device, client.public().noise_public);
        initiator.read(&message2).unwrap();
        assert!(path.is_ready());
        assert_eq!(path.device().unwrap(), client.public().noise_public);
    }

    #[test]
    fn an_unpaired_device_is_rejected_after_message_one() {
        let daemon = daemon_identity();
        let client = DeviceIdentity::generate().unwrap();
        let session = [5u8; SESSION_ID_LEN];
        let policy = PairingPolicy::new(Vec::new());
        let mut path = ServerPath::new(session, daemon.clone(), None, RekeyLimits::default());
        let (_initiator, message1) = Handshake::start_initiator(&HandshakeConfig {
            session_id: &session,
            local_static: client.noise_secret(),
            remote_static: Some(&daemon.public().noise_public),
            psk: None,
        })
        .unwrap();
        assert_eq!(
            path.on_handshake(&message1, &policy).unwrap(),
            ServerHandshakeStep::Rejected(RejectReason::NotPaired)
        );
    }

    #[test]
    fn resume_preserves_sequence_numbers_across_paths() {
        let mut session = ServerSession::with_token([6u8; SESSION_ID_LEN], [7u8; TOKEN_LEN], 0);
        session.send(AppFrame::text("a"), 0).unwrap();
        session.send(AppFrame::text("b"), 0).unwrap();
        assert_eq!(session.sendable(0, false).len(), 2);
        // Client only received the first record, so its next expected sequence
        // is 2; both directions have sent one record (next sends are 2).
        let reply = session.attach(2, 2, 2, 10).unwrap();
        let Control::Welcome { recv_next, .. } = reply else {
            panic!("expected welcome");
        };
        assert_eq!(recv_next, 1);
        assert_eq!(session.active_path, Some(2));
        // The unacked second record is retransmitted on the new path.
        assert_eq!(session.sendable(10, true).len(), 1);
    }

    #[test]
    fn token_comparison_rejects_a_wrong_token() {
        let session = ServerSession::with_token([8u8; SESSION_ID_LEN], [1u8; TOKEN_LEN], 0);
        assert!(session.token_matches(&[1u8; TOKEN_LEN]));
        assert!(!session.token_matches(&[2u8; TOKEN_LEN]));
    }

    #[test]
    fn detaching_the_active_path_promotes_another() {
        let mut session = ServerSession::with_token([9u8; SESSION_ID_LEN], [1u8; TOKEN_LEN], 0);
        let _ = session.attach(1, 1, 1, 0).unwrap();
        let _ = session.attach(2, 1, 1, 0).unwrap();
        assert_eq!(session.active_path, Some(2));
        assert_eq!(session.detach(2), Some(1));
    }

    #[test]
    fn delivery_of_a_data_record() {
        let mut session = ServerSession::with_token([3u8; SESSION_ID_LEN], [1u8; TOKEN_LEN], 0);
        let record = Incoming {
            kind: IncomingKind::Data,
            binary: false,
            msg_id: 1,
            frag_index: 0,
            frag_last: true,
            seq: 1,
            ack: 0,
            ack_bits: 0,
            payload: b"hello".to_vec(),
        };
        let frames = session.on_record(1, record, 0).unwrap();
        assert_eq!(frames, vec![AppFrame::text("hello")]);
    }
}
