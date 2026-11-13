//! Noise handshakes and encrypted records (ADR-010).
//!
//! The pattern is `Noise_IK_25519_ChaChaPoly_BLAKE2s`. The client is the
//! initiator and already knows the daemon's static public key from the pairing
//! QR, so `IK` authenticates the daemon on the first message. The daemon learns
//! the client's static key from the pairing claim pushed by h4rn-connect, or
//! from a one-time pairing secret mixed in as a PSK (`IKpsk2`) for a LAN or
//! Tailscale pairing made without the relay.
//!
//! Every path gets a **fresh** handshake. Noise transport state is never
//! carried across sockets: a path switch establishes new keys bound to the
//! logical session id through the prologue, and the resume token travels
//! inside the encrypted payload ([`crate::wire::Control::Open`]).
//!
//! The prologue is `b"h4rn-transport/1" || session_id`, identical on both
//! sides, so a handshake cannot be replayed onto a different logical session.

use snow::{Builder, HandshakeState, TransportState};

use crate::keys::KEY_LEN;
use crate::wire::{SESSION_ID_LEN, SessionId};

/// Noise pattern for an already-paired device.
pub const PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

/// Noise pattern for a LAN/Tailscale pairing that has no prior allowlist entry.
pub const PATTERN_PSK: &str = "Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s";

/// Domain separator for the handshake prologue.
pub const PROLOGUE_PREFIX: &[u8] = b"h4rn-transport/1";

/// Location of the pairing PSK in `IKpsk2`.
const PSK_LOCATION: u8 = 2;

/// Rekey policy for one direction of a secure channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RekeyLimits {
    /// Rotate after this many messages. Zero disables the message trigger.
    pub messages: u64,
    /// Rotate after this many plaintext bytes. Zero disables the byte trigger.
    pub bytes: u64,
}

impl Default for RekeyLimits {
    fn default() -> Self {
        Self {
            messages: 1 << 20,
            bytes: 128 * 1024 * 1024,
        }
    }
}

/// Inputs for one path's handshake.
#[derive(Clone, Copy, Debug)]
pub struct HandshakeConfig<'a> {
    /// Logical session the path belongs to.
    pub session_id: &'a SessionId,
    /// This device's X25519 static secret.
    pub local_static: &'a [u8; KEY_LEN],
    /// The daemon's X25519 static public key (initiator only).
    pub remote_static: Option<&'a [u8; KEY_LEN]>,
    /// One-time pairing secret, when pairing without a prior allowlist entry.
    pub psk: Option<&'a [u8; KEY_LEN]>,
}

/// One in-flight Noise handshake.
pub struct Handshake {
    state: HandshakeState,
    initiator: bool,
    expected_remote: Option<[u8; KEY_LEN]>,
    used_psk: bool,
}

impl Handshake {
    /// Builds an initiator and produces handshake message 1.
    pub fn start_initiator(config: &HandshakeConfig<'_>) -> Result<(Self, Vec<u8>), NoiseError> {
        let expected = *config
            .remote_static
            .ok_or(NoiseError::MissingRemoteStatic)?;
        if expected == [0u8; KEY_LEN] {
            return Err(NoiseError::MissingRemoteStatic);
        }
        let prologue = prologue(config.session_id);
        let pattern = if config.psk.is_some() {
            PATTERN_PSK
        } else {
            PATTERN
        };
        let params = pattern
            .parse()
            .map_err(|error| NoiseError::Handshake(format!("{error}")))?;
        let mut builder = Builder::new(params)
            .local_private_key(config.local_static)
            .map_err(noise_error)?
            .remote_public_key(&expected)
            .map_err(noise_error)?
            .prologue(&prologue)
            .map_err(noise_error)?;
        if let Some(psk) = config.psk {
            builder = builder.psk(PSK_LOCATION, psk).map_err(noise_error)?;
        }
        let mut state = builder.build_initiator().map_err(noise_error)?;
        let mut message = vec![0u8; 65_535];
        let written = state
            .write_message(&[], &mut message)
            .map_err(noise_error)?;
        message.truncate(written);
        Ok((
            Self {
                state,
                initiator: true,
                expected_remote: Some(expected),
                used_psk: config.psk.is_some(),
            },
            message,
        ))
    }

    /// Builds a responder. It waits for message 1 via [`Self::read`].
    pub fn start_responder(config: &HandshakeConfig<'_>) -> Result<Self, NoiseError> {
        let prologue = prologue(config.session_id);
        let pattern = if config.psk.is_some() {
            PATTERN_PSK
        } else {
            PATTERN
        };
        let params = pattern
            .parse()
            .map_err(|error| NoiseError::Handshake(format!("{error}")))?;
        let mut builder = Builder::new(params)
            .local_private_key(config.local_static)
            .map_err(noise_error)?
            .prologue(&prologue)
            .map_err(noise_error)?;
        if let Some(psk) = config.psk {
            builder = builder.psk(PSK_LOCATION, psk).map_err(noise_error)?;
        }
        let state = builder.build_responder().map_err(noise_error)?;
        Ok(Self {
            state,
            initiator: false,
            expected_remote: None,
            used_psk: config.psk.is_some(),
        })
    }

    /// Consumes one handshake message and returns the reply to send, if any.
    pub fn read(&mut self, message: &[u8]) -> Result<Option<Vec<u8>>, NoiseError> {
        let mut payload = vec![0u8; 65_535];
        self.state
            .read_message(message, &mut payload)
            .map_err(|error| NoiseError::Handshake(format!("{error}")))?;
        if !self.initiator && self.expected_remote.is_none() {
            let remote = self
                .state
                .get_remote_static()
                .ok_or(NoiseError::MissingRemoteStatic)?;
            if remote.len() != KEY_LEN {
                return Err(NoiseError::MissingRemoteStatic);
            }
            let mut key = [0u8; KEY_LEN];
            key.copy_from_slice(remote);
            self.expected_remote = Some(key);
        }
        if !self.state.is_handshake_finished() && self.state.is_my_turn() {
            let mut reply = vec![0u8; 65_535];
            let written = self
                .state
                .write_message(&[], &mut reply)
                .map_err(noise_error)?;
            reply.truncate(written);
            Ok(Some(reply))
        } else {
            Ok(None)
        }
    }

    /// Whether both sides have completed the handshake.
    pub fn is_finished(&self) -> bool {
        self.state.is_handshake_finished()
    }

    /// The peer's authenticated X25519 static public key.
    ///
    /// On the responder this is available after message 1, which is what lets
    /// the daemon enforce its allowlist before completing the handshake.
    pub fn remote_static(&self) -> Option<[u8; KEY_LEN]> {
        self.expected_remote
    }

    /// Whether this handshake mixed in the pairing PSK.
    pub fn used_psk(&self) -> bool {
        self.used_psk
    }

    /// Whether this side initiated the handshake.
    pub fn is_initiator(&self) -> bool {
        self.initiator
    }

    /// Converts a finished handshake into a transport cipher.
    pub fn into_channel(self, limits: RekeyLimits) -> Result<SecureChannel, NoiseError> {
        if !self.state.is_handshake_finished() {
            return Err(NoiseError::NotFinished);
        }
        let state = self.state.into_transport_mode().map_err(noise_error)?;
        if let Some(expected) = self.expected_remote {
            match state.get_remote_static() {
                Some(remote) if remote == expected => {}
                _ => return Err(NoiseError::PeerMismatch),
            }
        }
        Ok(SecureChannel {
            state,
            limits,
            send_count: 0,
            recv_count: 0,
            send_bytes: 0,
            recv_bytes: 0,
            rekeys: 0,
        })
    }
}

/// Prologue binding a handshake to a logical session.
pub fn prologue(session_id: &SessionId) -> Vec<u8> {
    let mut value = Vec::with_capacity(PROLOGUE_PREFIX.len() + SESSION_ID_LEN);
    value.extend_from_slice(PROLOGUE_PREFIX);
    value.extend_from_slice(session_id);
    value
}

/// One direction of the transport cipher with rekey bookkeeping.
pub struct SecureChannel {
    state: TransportState,
    limits: RekeyLimits,
    send_count: u64,
    recv_count: u64,
    send_bytes: u64,
    recv_bytes: u64,
    rekeys: u64,
}

impl SecureChannel {
    /// Encrypts one plaintext record.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if plaintext.len() + 16 > 65_535 {
            return Err(NoiseError::TooLarge);
        }
        self.maybe_rekey_outgoing();
        let mut out = vec![0u8; plaintext.len() + 16];
        let written = self
            .state
            .write_message(plaintext, &mut out)
            .map_err(noise_error)?;
        out.truncate(written);
        self.send_count += 1;
        self.send_bytes += plaintext.len() as u64;
        Ok(out)
    }

    /// Decrypts one ciphertext record.
    pub fn open(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if ciphertext.len() > 65_535 {
            return Err(NoiseError::TooLarge);
        }
        self.maybe_rekey_incoming();
        let mut out = vec![0u8; ciphertext.len().saturating_sub(16)];
        let read = self
            .state
            .read_message(ciphertext, &mut out)
            .map_err(|_| NoiseError::Decrypt)?;
        out.truncate(read);
        self.recv_count += 1;
        self.recv_bytes += out.len() as u64;
        Ok(out)
    }

    /// Number of messages sealed since the last rekey.
    pub fn send_count(&self) -> u64 {
        self.send_count
    }

    /// Number of messages opened since the last rekey.
    pub fn recv_count(&self) -> u64 {
        self.recv_count
    }

    /// Number of rekeys performed in either direction.
    pub fn rekeys(&self) -> u64 {
        self.rekeys
    }

    fn maybe_rekey_outgoing(&mut self) {
        let due = (self.limits.messages > 0 && self.send_count >= self.limits.messages)
            || (self.limits.bytes > 0 && self.send_bytes >= self.limits.bytes);
        if due {
            self.state.rekey_outgoing();
            self.send_count = 0;
            self.send_bytes = 0;
            self.rekeys += 1;
        }
    }

    fn maybe_rekey_incoming(&mut self) {
        let due = (self.limits.messages > 0 && self.recv_count >= self.limits.messages)
            || (self.limits.bytes > 0 && self.recv_bytes >= self.limits.bytes);
        if due {
            self.state.rekey_incoming();
            self.recv_count = 0;
            self.recv_bytes = 0;
            self.rekeys += 1;
        }
    }
}

fn noise_error(error: snow::Error) -> NoiseError {
    NoiseError::Handshake(error.to_string())
}

/// Handshake and record failures.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NoiseError {
    /// The handshake failed or a message did not authenticate.
    #[error("noise handshake failed: {0}")]
    Handshake(String),
    /// A transport record did not authenticate.
    #[error("noise record did not authenticate")]
    Decrypt,
    /// The peer's static key was not the expected one.
    #[error("peer static key did not match the pinned daemon key")]
    PeerMismatch,
    /// The handshake is not complete.
    #[error("handshake is not complete")]
    NotFinished,
    /// A static public key was missing or all zeroes.
    #[error("missing or invalid static public key")]
    MissingRemoteStatic,
    /// A record exceeded the Noise message size limit.
    #[error("record exceeds the Noise message limit")]
    TooLarge,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::DeviceIdentity;

    fn ids() -> (DeviceIdentity, DeviceIdentity, SessionId) {
        let mut session = [0u8; SESSION_ID_LEN];
        session[0] = 9;
        (
            DeviceIdentity::generate().unwrap(),
            DeviceIdentity::generate().unwrap(),
            session,
        )
    }

    fn connect(
        client: &DeviceIdentity,
        daemon: &DeviceIdentity,
        session: &SessionId,
        psk: Option<&[u8; KEY_LEN]>,
    ) -> (SecureChannel, SecureChannel) {
        let (mut initiator, message1) = Handshake::start_initiator(&HandshakeConfig {
            session_id: session,
            local_static: client.noise_secret(),
            remote_static: Some(&daemon.public().noise_public),
            psk,
        })
        .unwrap();
        let mut responder = Handshake::start_responder(&HandshakeConfig {
            session_id: session,
            local_static: daemon.noise_secret(),
            remote_static: None,
            psk,
        })
        .unwrap();
        let message2 = responder.read(&message1).unwrap().unwrap();
        initiator.read(&message2).unwrap();
        assert!(initiator.is_finished());
        assert!(responder.is_finished());
        assert_eq!(
            responder.remote_static().unwrap(),
            client.public().noise_public
        );
        (
            initiator.into_channel(RekeyLimits::default()).unwrap(),
            responder.into_channel(RekeyLimits::default()).unwrap(),
        )
    }

    #[test]
    fn ik_handshake_establishes_matching_channels() {
        let (client, daemon, session) = ids();
        let (mut a, mut b) = connect(&client, &daemon, &session, None);
        let ciphertext = a.seal(b"hello").unwrap();
        assert_eq!(b.open(&ciphertext).unwrap(), b"hello");
        let reply = b.seal(b"hi").unwrap();
        assert_eq!(a.open(&reply).unwrap(), b"hi");
    }

    #[test]
    fn psk_handshake_establishes_matching_channels() {
        let (client, daemon, session) = ids();
        let psk = [7u8; KEY_LEN];
        let (mut a, mut b) = connect(&client, &daemon, &session, Some(&psk));
        let ciphertext = a.seal(b"paired").unwrap();
        assert_eq!(b.open(&ciphertext).unwrap(), b"paired");
    }

    #[test]
    fn a_different_psk_cannot_complete_the_handshake() {
        let (client, daemon, session) = ids();
        let psk = [7u8; KEY_LEN];
        let wrong = [8u8; KEY_LEN];
        let (mut initiator, message1) = Handshake::start_initiator(&HandshakeConfig {
            session_id: &session,
            local_static: client.noise_secret(),
            remote_static: Some(&daemon.public().noise_public),
            psk: Some(&psk),
        })
        .unwrap();
        let mut responder = Handshake::start_responder(&HandshakeConfig {
            session_id: &session,
            local_static: daemon.noise_secret(),
            remote_static: None,
            psk: Some(&wrong),
        })
        .unwrap();
        // The mismatch shows up when the initiator reads the responder's
        // message: the PSK is mixed into the final handshake message.
        let message2 = responder.read(&message1).unwrap().unwrap();
        assert!(initiator.read(&message2).is_err());
    }

    #[test]
    fn a_different_session_id_breaks_the_handshake() {
        let (client, daemon, session) = ids();
        let other = [3u8; SESSION_ID_LEN];
        let (mut initiator, message1) = Handshake::start_initiator(&HandshakeConfig {
            session_id: &session,
            local_static: client.noise_secret(),
            remote_static: Some(&daemon.public().noise_public),
            psk: None,
        })
        .unwrap();
        let mut responder = Handshake::start_responder(&HandshakeConfig {
            session_id: &other,
            local_static: daemon.noise_secret(),
            remote_static: None,
            psk: None,
        })
        .unwrap();
        assert!(responder.read(&message1).is_err());
        let _ = &mut initiator;
    }

    #[test]
    fn a_pinned_key_mismatch_is_detected() {
        let (client, daemon, session) = ids();
        let impostor = DeviceIdentity::generate().unwrap();
        let (initiator, message1) = Handshake::start_initiator(&HandshakeConfig {
            session_id: &session,
            local_static: client.noise_secret(),
            remote_static: Some(&daemon.public().noise_public),
            psk: None,
        })
        .unwrap();
        let mut responder = Handshake::start_responder(&HandshakeConfig {
            session_id: &session,
            local_static: impostor.noise_secret(),
            remote_static: None,
            psk: None,
        })
        .unwrap();
        // In `IK`, message 1 encrypts the initiator's static key to the pinned
        // daemon key. An impostor holding a different static secret therefore
        // cannot even decrypt message 1, so it can never answer with a valid
        // message 2. This is the pinned-key defense.
        assert!(responder.read(&message1).is_err());
        assert!(!initiator.is_finished());
    }

    #[test]
    fn rekey_still_exchanges_messages() {
        let (client, daemon, session) = ids();
        let (mut initiator, message1) = Handshake::start_initiator(&HandshakeConfig {
            session_id: &session,
            local_static: client.noise_secret(),
            remote_static: Some(&daemon.public().noise_public),
            psk: None,
        })
        .unwrap();
        let mut responder = Handshake::start_responder(&HandshakeConfig {
            session_id: &session,
            local_static: daemon.noise_secret(),
            remote_static: None,
            psk: None,
        })
        .unwrap();
        let message2 = responder.read(&message1).unwrap().unwrap();
        initiator.read(&message2).unwrap();
        let limits = RekeyLimits {
            messages: 2,
            bytes: 0,
        };
        let mut a = initiator.into_channel(limits).unwrap();
        let mut b = responder.into_channel(limits).unwrap();
        for round in 0..6u8 {
            let ciphertext = a.seal(&[round]).unwrap();
            assert_eq!(b.open(&ciphertext).unwrap(), vec![round]);
            let reply = b.seal(&[round]).unwrap();
            assert_eq!(a.open(&reply).unwrap(), vec![round]);
        }
        assert!(a.rekeys() > 0);
        assert!(b.rekeys() > 0);
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let (client, daemon, session) = ids();
        let (mut a, mut b) = connect(&client, &daemon, &session, None);
        let mut ciphertext = a.seal(b"secret").unwrap();
        ciphertext[0] ^= 0xff;
        assert_eq!(b.open(&ciphertext).unwrap_err(), NoiseError::Decrypt);
    }

    #[test]
    fn prologue_binds_the_session() {
        let first = prologue(&[1u8; SESSION_ID_LEN]);
        let second = prologue(&[2u8; SESSION_ID_LEN]);
        assert_ne!(first, second);
        assert!(first.starts_with(PROLOGUE_PREFIX));
    }
}
