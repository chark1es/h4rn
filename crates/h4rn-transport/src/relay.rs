//! Relay framing and the connector abstraction (ADR-009, ADR-018).
//!
//! h4rn-connect never sees plaintext: it forwards [`Packet`] bytes between two
//! authenticated connections that share a logical session id. This module owns
//! the small outer framing the relay needs to route and the traits the client
//! and daemon use to connect to a relay implementation.
//!
//! ```text
//! relay frame
//! offset  size  field
//! 0       2     magic     b"HR"
//! 2       1     version   1
//! 3       1     kind      1 = register, 2 = data, 3 = close
//! 4       16    session id
//! 20      1     role      1 = client, 2 = daemon (register only)
//! 21      4     payload length (little endian)
//! 25      ...   payload
//! ```
//!
//! `register` carries an optional resume token. A relay can require it for the
//! daemon role so an attacker who guesses a session id cannot attach, but it is
//! deliberately not interpreted here: the Noise layer authenticates the peer on
//! every path regardless.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::wire::{Packet, SessionId, TOKEN_LEN};

/// Magic prefix of a relay frame.
pub const RELAY_MAGIC: [u8; 2] = *b"HR";

/// Relay framing version.
pub const RELAY_VERSION: u8 = 1;

/// Length of the fixed relay header.
pub const RELAY_HEADER_LEN: usize = 25;

/// Hard ceiling on one relay payload, matching [`crate::wire::MAX_PACKET_BYTES`].
pub const MAX_RELAY_PAYLOAD: usize = crate::wire::MAX_PACKET_BYTES;

/// Which side of a session a connection represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RelayRole {
    /// A device dialling its daemon.
    Client = 1,
    /// A daemon waiting for a device.
    Daemon = 2,
}

impl RelayRole {
    /// Parses a wire discriminant.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Client),
            2 => Some(Self::Daemon),
            _ => None,
        }
    }
}

/// One relay frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelayFrame {
    /// Binds this connection to a session.
    Register {
        /// Logical session id.
        session_id: SessionId,
        /// Which side is connecting.
        role: RelayRole,
        /// Resume token the daemon may issue to a client.
        token: Option<[u8; TOKEN_LEN]>,
    },
    /// Forwards one transport packet in either direction.
    Data {
        /// Logical session id.
        session_id: SessionId,
        /// Complete [`Packet`] bytes.
        packet: Vec<u8>,
    },
    /// Closes the connection for a session.
    Close {
        /// Logical session id.
        session_id: SessionId,
        /// Application close code.
        code: u8,
    },
}

impl RelayFrame {
    /// Encodes the frame for one relay message.
    pub fn encode(&self) -> Vec<u8> {
        let (kind, session_id, role, payload): (u8, SessionId, u8, Vec<u8>) = match self {
            Self::Register {
                session_id,
                role,
                token,
            } => (
                1,
                *session_id,
                *role as u8,
                token.map(|token| token.to_vec()).unwrap_or_default(),
            ),
            Self::Data { session_id, packet } => (2, *session_id, 0, packet.clone()),
            Self::Close { session_id, code } => (3, *session_id, 0, vec![*code]),
        };
        let mut out = Vec::with_capacity(RELAY_HEADER_LEN + payload.len());
        out.extend_from_slice(&RELAY_MAGIC);
        out.push(RELAY_VERSION);
        out.push(kind);
        out.extend_from_slice(&session_id);
        out.push(role);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }

    /// Decodes one relay message, enforcing the payload ceiling.
    pub fn decode(bytes: &[u8]) -> Result<Self, RelayError> {
        if bytes.len() < RELAY_HEADER_LEN {
            return Err(RelayError::Malformed("frame is shorter than its header"));
        }
        if bytes[0..2] != RELAY_MAGIC {
            return Err(RelayError::Malformed("bad relay magic"));
        }
        if bytes[2] != RELAY_VERSION {
            return Err(RelayError::Malformed("unsupported relay version"));
        }
        let mut session_id = [0u8; crate::wire::SESSION_ID_LEN];
        session_id.copy_from_slice(&bytes[4..20]);
        let role = bytes[20];
        let length = u32::from_le_bytes(bytes[21..25].try_into().expect("slice length")) as usize;
        if length > MAX_RELAY_PAYLOAD || RELAY_HEADER_LEN + length != bytes.len() {
            return Err(RelayError::Malformed(
                "relay payload length is inconsistent",
            ));
        }
        let payload = &bytes[RELAY_HEADER_LEN..];
        match bytes[3] {
            1 => {
                let role =
                    RelayRole::from_u8(role).ok_or(RelayError::Malformed("unknown relay role"))?;
                let token = match payload.len() {
                    0 => None,
                    TOKEN_LEN => Some(payload.try_into().expect("token length")),
                    _ => return Err(RelayError::Malformed("relay token has the wrong length")),
                };
                Ok(Self::Register {
                    session_id,
                    role,
                    token,
                })
            }
            2 => Ok(Self::Data {
                session_id,
                packet: payload.to_vec(),
            }),
            3 => {
                let code = *payload
                    .first()
                    .ok_or(RelayError::Malformed("close frame carries no code"))?;
                Ok(Self::Close { session_id, code })
            }
            _ => Err(RelayError::Malformed("unknown relay frame kind")),
        }
    }
}

/// A request to open one relay connection.
#[derive(Clone, Debug)]
pub struct RelayRequest {
    /// Relay endpoint, `wss://` URL or a connector-defined name.
    pub endpoint: String,
    /// Logical session id.
    pub session_id: SessionId,
    /// Which side is connecting.
    pub role: RelayRole,
    /// Resume token, when the connector requires one.
    pub token: Option<[u8; TOKEN_LEN]>,
}

/// One live relay connection carrying opaque transport packets.
#[async_trait]
pub trait RelayLink: Send {
    /// Sends one frame.
    async fn send(&mut self, frame: RelayFrame) -> Result<(), RelayError>;
    /// Receives the next frame, or `None` when the connection closed.
    async fn recv(&mut self) -> Result<Option<RelayFrame>, RelayError>;
    /// Closes the connection.
    async fn close(&mut self) -> Result<(), RelayError>;
}

/// Creates relay connections.
///
/// The production implementation speaks the h4rn-connect WebSocket protocol;
/// tests use [`MockRelayConnector`]. Keeping this a trait means the transport
/// core never depends on the relay's HTTP stack.
#[async_trait]
pub trait RelayConnector: Send + Sync {
    /// Opens one connection.
    async fn connect(&self, request: RelayRequest) -> Result<Box<dyn RelayLink>, RelayError>;
}

/// Relay transport failures.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RelayError {
    /// The frame or response was malformed.
    #[error("relay protocol error: {0}")]
    Malformed(&'static str),
    /// The connector could not reach the relay.
    #[error("relay is unreachable: {0}")]
    Unreachable(String),
    /// The relay rejected the connection.
    #[error("relay rejected the connection: {0}")]
    Rejected(String),
    /// The connection closed.
    #[error("the relay connection is closed")]
    Closed,
}

/// An in-process relay used by tests and the UI's mock transport.
///
/// It pairs one client and one daemon connection by session id and forwards
/// frames between them. Nothing is encrypted here: the packets are already
/// Noise ciphertext, exactly as they would be on the wire.
#[derive(Default)]
pub struct MockRelay {
    slots: Mutex<HashMap<SessionId, MockSlot>>,
}

#[derive(Default)]
struct MockSlot {
    client: Option<mpsc::Sender<RelayFrame>>,
    daemon: Option<mpsc::Sender<RelayFrame>>,
    token: Option<[u8; TOKEN_LEN]>,
}

impl MockRelay {
    /// Creates an empty relay.
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens one side of a session and returns its link.
    pub fn connect(self: &Arc<Self>, request: RelayRequest) -> Result<MockRelayLink, RelayError> {
        let (tx, rx) = mpsc::channel(256);
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let slot = slots.entry(request.session_id).or_default();
        match request.role {
            RelayRole::Client => {
                slot.token = request.token;
                slot.client = Some(tx);
            }
            RelayRole::Daemon => {
                if let Some(required) = &request.token
                    && !slot.token.as_ref().is_some_and(|token| token == required)
                    && slot.client.is_some()
                {
                    return Err(RelayError::Rejected("session token did not match".into()));
                }
                slot.daemon = Some(tx);
            }
        }
        Ok(MockRelayLink {
            rx,
            relay: Arc::clone(self),
            session_id: request.session_id,
            role: request.role,
        })
    }
}

/// One side of a [`MockRelay`] connection.
pub struct MockRelayLink {
    rx: mpsc::Receiver<RelayFrame>,
    relay: Arc<MockRelay>,
    session_id: SessionId,
    role: RelayRole,
}

impl std::fmt::Debug for MockRelayLink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MockRelayLink")
            .field("session_id", &hex::encode(self.session_id))
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl RelayLink for MockRelayLink {
    async fn send(&mut self, frame: RelayFrame) -> Result<(), RelayError> {
        let peer = {
            let slots = self
                .relay
                .slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let slot = slots.get(&self.session_id).ok_or(RelayError::Closed)?;
            match self.role {
                RelayRole::Client => slot.daemon.clone(),
                RelayRole::Daemon => slot.client.clone(),
            }
        }
        .ok_or(RelayError::Closed)?;
        peer.send(frame).await.map_err(|_| RelayError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<RelayFrame>, RelayError> {
        Ok(self.rx.recv().await)
    }

    async fn close(&mut self) -> Result<(), RelayError> {
        let mut slots = self
            .relay
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(slot) = slots.get_mut(&self.session_id) {
            match self.role {
                RelayRole::Client => slot.client = None,
                RelayRole::Daemon => slot.daemon = None,
            }
        }
        Ok(())
    }
}

/// A connector backed by [`MockRelay`].
#[derive(Clone, Default)]
pub struct MockRelayConnector {
    relay: Arc<MockRelay>,
}

impl MockRelayConnector {
    /// Wraps a shared mock relay.
    pub fn new(relay: Arc<MockRelay>) -> Self {
        Self { relay }
    }

    /// The relay behind this connector.
    pub fn relay(&self) -> Arc<MockRelay> {
        Arc::clone(&self.relay)
    }
}

#[async_trait]
impl RelayConnector for MockRelayConnector {
    async fn connect(&self, request: RelayRequest) -> Result<Box<dyn RelayLink>, RelayError> {
        Ok(Box::new(self.relay.connect(request)?))
    }
}

/// A connector for a build without relay support.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRelayConnector;

#[async_trait]
impl RelayConnector for NoRelayConnector {
    async fn connect(&self, _request: RelayRequest) -> Result<Box<dyn RelayLink>, RelayError> {
        Err(RelayError::Unreachable(
            "this build has no relay connector".into(),
        ))
    }
}

/// Convenience wrapper around [`Packet::encode`] for relay data frames.
pub fn relay_data(packet: &Packet) -> RelayFrame {
    RelayFrame::Data {
        session_id: packet.session_id,
        packet: packet.encode(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{PacketKind, TOKEN_LEN};

    fn session() -> SessionId {
        [7u8; crate::wire::SESSION_ID_LEN]
    }

    #[test]
    fn every_frame_round_trips() {
        for frame in [
            RelayFrame::Register {
                session_id: session(),
                role: RelayRole::Client,
                token: None,
            },
            RelayFrame::Register {
                session_id: session(),
                role: RelayRole::Daemon,
                token: Some([3u8; TOKEN_LEN]),
            },
            RelayFrame::Data {
                session_id: session(),
                packet: vec![1, 2, 3],
            },
            RelayFrame::Close {
                session_id: session(),
                code: 9,
            },
        ] {
            assert_eq!(RelayFrame::decode(&frame.encode()).unwrap(), frame);
        }
    }

    #[test]
    fn malformed_frames_are_rejected() {
        assert!(RelayFrame::decode(b"HR").is_err());
        let mut frame = RelayFrame::Close {
            session_id: session(),
            code: 1,
        }
        .encode();
        frame[0] = b'X';
        assert!(RelayFrame::decode(&frame).is_err());
        let mut frame = RelayFrame::Data {
            session_id: session(),
            packet: vec![1],
        }
        .encode();
        frame[21] = 9;
        assert!(RelayFrame::decode(&frame).is_err());
    }

    #[tokio::test]
    async fn the_mock_relay_forwards_between_roles() {
        let relay = Arc::new(MockRelay::new());
        let connector = MockRelayConnector::new(Arc::clone(&relay));
        let mut client = connector
            .connect(RelayRequest {
                endpoint: "mock".into(),
                session_id: session(),
                role: RelayRole::Client,
                token: Some([1u8; TOKEN_LEN]),
            })
            .await
            .unwrap();
        let mut daemon = connector
            .connect(RelayRequest {
                endpoint: "mock".into(),
                session_id: session(),
                role: RelayRole::Daemon,
                token: Some([1u8; TOKEN_LEN]),
            })
            .await
            .unwrap();
        client
            .send(RelayFrame::Data {
                session_id: session(),
                packet: b"ciphertext".to_vec(),
            })
            .await
            .unwrap();
        let received = daemon.recv().await.unwrap().unwrap();
        assert_eq!(
            received,
            RelayFrame::Data {
                session_id: session(),
                packet: b"ciphertext".to_vec()
            }
        );
        daemon
            .send(RelayFrame::Data {
                session_id: session(),
                packet: Packet {
                    kind: PacketKind::Record,
                    session_id: session(),
                    body: vec![2],
                }
                .encode(),
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await.unwrap().unwrap(),
            RelayFrame::Data { .. }
        ));
    }
}
