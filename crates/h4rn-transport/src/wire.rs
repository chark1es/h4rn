//! Byte-level framing for the transport.
//!
//! A WebSocket message (or one relay data frame) carries exactly one
//! [`Packet`]. The packet header is in the clear because it has to be: the
//! daemon must demultiplex handshake messages for a session before it has any
//! session keys, and the relay routes by the ids it can see. The body is either
//! a Noise handshake message or an encrypted [`Record`].
//!
//! ```text
//! packet header
//! offset  size  field
//! 0       2     magic   b"HT"
//! 2       1     version 1
//! 3       1     kind    1 = handshake, 2 = record, 3 = reject
//! 4       16    session id (opaque to the relay)
//! 20      ...   body
//! ```
//!
//! The record body is encrypted with the session's Noise transport keys. Its
//! plaintext layout is:
//!
//! ```text
//! offset  size  field
//! 0       1     version
//! 1       1     kind     1 = data, 2 = control, 3 = ack
//! 2       1     flags    bit 0 = binary application frame
//! 3       1     reserved
//! 4       8     message id (groups fragments of one application frame)
//! 12      2     fragment index
//! 14      1     last fragment
//! 15      1     reserved
//! 16      8     per-direction session sequence
//! 24      8     cumulative ack (highest contiguous sequence received)
//! 32      4     ack bitmap for the 64 sequences below the ack
//! 36      4     payload length
//! 40      ...   payload, then zero padding to a size bucket
//! ```
//!
//! Padding exists so the relay sees a coarse bucket instead of the exact size
//! of a prompt or a file chunk (ADR-010).

use thiserror::Error;

use crate::{MAX_RECORD_PLAINTEXT, PADDING_BUCKET};

/// Magic prefix of every transport packet.
pub const PACKET_MAGIC: [u8; 2] = *b"HT";

/// Framing version.
pub const PACKET_VERSION: u8 = 1;

/// Length of a session id.
pub const SESSION_ID_LEN: usize = 16;

/// A logical session id. Chosen by the client and stable across paths.
pub type SessionId = [u8; SESSION_ID_LEN];

/// Length of the clear packet header.
pub const PACKET_HEADER_LEN: usize = 20;

/// Hard ceiling on one packet, so a hostile peer cannot make the process
/// allocate without bound before authentication.
pub const MAX_PACKET_BYTES: usize = 1024 * 1024 + 4096;

/// Length of the encrypted-record plaintext header.
pub const RECORD_HEADER_LEN: usize = 40;

/// Length of a resume token / PSK.
pub const TOKEN_LEN: usize = 32;

/// What a [`Packet`] body contains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketKind {
    /// One Noise handshake message.
    Handshake = 1,
    /// One encrypted [`Record`].
    Record = 2,
    /// A plaintext rejection before the handshake completes.
    Reject = 3,
}

impl PacketKind {
    /// Parses a wire discriminant.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Handshake),
            2 => Some(Self::Record),
            3 => Some(Self::Reject),
            _ => None,
        }
    }
}

/// Why a peer was rejected before a session existed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RejectReason {
    /// The client's static key is not on the daemon's allowlist and no pairing
    /// PSK was offered.
    NotPaired = 1,
    /// The key was explicitly revoked.
    Revoked = 2,
    /// The handshake message did not parse or did not authenticate.
    BadHandshake = 3,
    /// The peer speaks a transport version this build does not implement.
    Unsupported = 4,
}

impl RejectReason {
    /// Parses a wire discriminant.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::NotPaired),
            2 => Some(Self::Revoked),
            3 => Some(Self::BadHandshake),
            4 => Some(Self::Unsupported),
            _ => None,
        }
    }

    /// Stable human-readable reason, deliberately free of peer-controlled text.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotPaired => "device is not paired",
            Self::Revoked => "device was revoked",
            Self::BadHandshake => "handshake failed",
            Self::Unsupported => "unsupported transport version",
        }
    }
}

/// One framed transport packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// What the body contains.
    pub kind: PacketKind,
    /// Logical session the packet belongs to.
    pub session_id: SessionId,
    /// Handshake message, encrypted record, or one reject reason byte.
    pub body: Vec<u8>,
}

impl Packet {
    /// Encodes the header and body into one message.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(PACKET_HEADER_LEN + self.body.len());
        out.extend_from_slice(&PACKET_MAGIC);
        out.push(PACKET_VERSION);
        out.push(self.kind as u8);
        out.extend_from_slice(&self.session_id);
        out.extend_from_slice(&self.body);
        out
    }

    /// Decodes one message, enforcing the size ceiling.
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() < PACKET_HEADER_LEN {
            return Err(WireError::TooShort {
                got: bytes.len(),
                need: PACKET_HEADER_LEN,
            });
        }
        if bytes.len() > MAX_PACKET_BYTES {
            return Err(WireError::TooLarge {
                got: bytes.len(),
                max: MAX_PACKET_BYTES,
            });
        }
        if bytes[0..2] != PACKET_MAGIC {
            return Err(WireError::BadMagic);
        }
        let version = bytes[2];
        if version != PACKET_VERSION {
            return Err(WireError::UnsupportedVersion { got: version });
        }
        let kind =
            PacketKind::from_u8(bytes[3]).ok_or(WireError::UnknownPacketKind { got: bytes[3] })?;
        let mut session_id = [0u8; SESSION_ID_LEN];
        session_id.copy_from_slice(&bytes[4..PACKET_HEADER_LEN]);
        Ok(Self {
            kind,
            session_id,
            body: bytes[PACKET_HEADER_LEN..].to_vec(),
        })
    }
}

/// What an encrypted record carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordKind {
    /// One fragment of an application frame.
    Data = 1,
    /// A transport control message.
    Control = 2,
    /// A bare acknowledgement, sent when there is no data to piggyback on.
    Ack = 3,
}

impl RecordKind {
    /// Parses a wire discriminant.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Data),
            2 => Some(Self::Control),
            3 => Some(Self::Ack),
            _ => None,
        }
    }
}

/// One decrypted transport record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Record kind.
    pub kind: RecordKind,
    /// Application frame is binary rather than UTF-8 JSON.
    pub binary: bool,
    /// Groups the fragments of one application frame.
    pub msg_id: u64,
    /// Zero-based fragment index.
    pub frag_index: u16,
    /// True on the final fragment of the message.
    pub frag_last: bool,
    /// Per-direction sequence number (data records only; 0 otherwise).
    pub seq: u64,
    /// Highest contiguous sequence the sender has received.
    pub ack: u64,
    /// Bitmap of the 64 sequences immediately below `ack` that were received.
    pub ack_bits: u32,
    /// Payload (empty for control and ack records).
    pub payload: Vec<u8>,
}

impl Record {
    /// A data record with no ack information yet.
    pub fn data(binary: bool, payload: Vec<u8>) -> Self {
        Self {
            kind: RecordKind::Data,
            binary,
            msg_id: 0,
            frag_index: 0,
            frag_last: true,
            seq: 0,
            ack: 0,
            ack_bits: 0,
            payload,
        }
    }

    /// A control record wrapping an encoded [`Control`].
    pub fn control(control: &Control) -> Self {
        Self {
            kind: RecordKind::Control,
            binary: false,
            msg_id: 0,
            frag_index: 0,
            frag_last: true,
            seq: 0,
            ack: 0,
            ack_bits: 0,
            payload: control.encode(),
        }
    }

    /// A bare acknowledgement.
    pub fn ack(ack: u64, ack_bits: u32) -> Self {
        Self {
            kind: RecordKind::Ack,
            binary: false,
            msg_id: 0,
            frag_index: 0,
            frag_last: true,
            seq: 0,
            ack,
            ack_bits,
            payload: Vec::new(),
        }
    }

    /// Encodes the plaintext (including padding) that Noise will encrypt.
    pub fn encode_plaintext(&self) -> Result<Vec<u8>, WireError> {
        // Data records carry application bytes and control records carry the
        // encoded `Control`; a bare acknowledgement has no payload at all.
        if self.kind == RecordKind::Ack && !self.payload.is_empty() {
            return Err(WireError::PayloadNotAllowed);
        }
        if self.payload.len() > MAX_RECORD_PLAINTEXT - RECORD_HEADER_LEN {
            return Err(WireError::TooLarge {
                got: self.payload.len(),
                max: MAX_RECORD_PLAINTEXT - RECORD_HEADER_LEN,
            });
        }
        let len = self.payload.len();
        let padded = padded_len(RECORD_HEADER_LEN + len);
        let mut out = Vec::with_capacity(padded);
        out.push(PACKET_VERSION);
        out.push(self.kind as u8);
        out.push(u8::from(self.binary));
        out.push(0);
        out.extend_from_slice(&self.msg_id.to_le_bytes());
        out.extend_from_slice(&self.frag_index.to_le_bytes());
        out.push(u8::from(self.frag_last));
        out.push(0);
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.ack.to_le_bytes());
        out.extend_from_slice(&self.ack_bits.to_le_bytes());
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        out.resize(padded, 0);
        Ok(out)
    }

    /// Decodes a decrypted plaintext record.
    pub fn decode_plaintext(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() < RECORD_HEADER_LEN {
            return Err(WireError::TooShort {
                got: bytes.len(),
                need: RECORD_HEADER_LEN,
            });
        }
        if bytes[0] != PACKET_VERSION {
            return Err(WireError::UnsupportedVersion { got: bytes[0] });
        }
        let kind =
            RecordKind::from_u8(bytes[1]).ok_or(WireError::UnknownRecordKind { got: bytes[1] })?;
        let binary = bytes[2] & 1 == 1;
        let msg_id = u64::from_le_bytes(bytes[4..12].try_into().expect("slice length"));
        let frag_index = u16::from_le_bytes(bytes[12..14].try_into().expect("slice length"));
        let frag_last = bytes[14] != 0;
        let seq = u64::from_le_bytes(bytes[16..24].try_into().expect("slice length"));
        let ack = u64::from_le_bytes(bytes[24..32].try_into().expect("slice length"));
        let ack_bits = u32::from_le_bytes(bytes[32..36].try_into().expect("slice length"));
        let payload_len =
            u32::from_le_bytes(bytes[36..40].try_into().expect("slice length")) as usize;
        let end = RECORD_HEADER_LEN
            .checked_add(payload_len)
            .ok_or(WireError::BadLength)?;
        if end > bytes.len() {
            return Err(WireError::BadLength);
        }
        Ok(Self {
            kind,
            binary,
            msg_id,
            frag_index,
            frag_last,
            seq,
            ack,
            ack_bits,
            payload: bytes[RECORD_HEADER_LEN..end].to_vec(),
        })
    }
}

/// Round a plaintext length up to a padding bucket, never past the Noise limit.
fn padded_len(n: usize) -> usize {
    let bucketed = n.div_ceil(PADDING_BUCKET) * PADDING_BUCKET;
    if bucketed > MAX_RECORD_PLAINTEXT || bucketed < n {
        n.min(MAX_RECORD_PLAINTEXT)
    } else {
        bucketed
    }
}

/// Transport control messages, carried inside an encrypted record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Control {
    /// First message after a handshake: bind this path to a session.
    ///
    /// `token` is `None` for a brand-new session and `Some` when resuming.
    /// `recv_next` tells the peer where this side's receive sequence stands.
    Open {
        /// Resume token, or `None` to create a session.
        token: Option<[u8; TOKEN_LEN]>,
        /// Next sequence number this side expects to receive.
        recv_next: u64,
        /// Next sequence number this side will send.
        send_next: u64,
    },
    /// Answer to [`Control::Open`], carrying the resume token to persist.
    Welcome {
        /// Token to present when resuming this session.
        token: [u8; TOKEN_LEN],
        /// Next sequence number the responder expects to receive.
        recv_next: u64,
        /// Next sequence number the responder will send.
        send_next: u64,
    },
    /// Liveness and RTT probe.
    Ping {
        /// Correlation nonce.
        nonce: u64,
        /// Sender's clock in milliseconds.
        sent_ms: u64,
    },
    /// Reply to [`Control::Ping`].
    Pong {
        /// The nonce from the ping.
        nonce: u64,
        /// The ping's `sent_ms`, echoed for a clock-free RTT computation.
        sent_ms: u64,
    },
    /// The sender is closing the session.
    Close {
        /// Application-level close code.
        code: u8,
    },
}

impl Control {
    /// Encodes the control message.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Open {
                token,
                recv_next,
                send_next,
            } => {
                out.push(1);
                match token {
                    Some(token) => {
                        out.push(1);
                        out.extend_from_slice(token);
                    }
                    None => out.push(0),
                }
                out.extend_from_slice(&recv_next.to_le_bytes());
                out.extend_from_slice(&send_next.to_le_bytes());
            }
            Self::Welcome {
                token,
                recv_next,
                send_next,
            } => {
                out.push(2);
                out.extend_from_slice(token);
                out.extend_from_slice(&recv_next.to_le_bytes());
                out.extend_from_slice(&send_next.to_le_bytes());
            }
            Self::Ping { nonce, sent_ms } => {
                out.push(3);
                out.extend_from_slice(&nonce.to_le_bytes());
                out.extend_from_slice(&sent_ms.to_le_bytes());
            }
            Self::Pong { nonce, sent_ms } => {
                out.push(4);
                out.extend_from_slice(&nonce.to_le_bytes());
                out.extend_from_slice(&sent_ms.to_le_bytes());
            }
            Self::Close { code } => {
                out.push(5);
                out.push(*code);
            }
        }
        out
    }

    /// Decodes a control message, rejecting trailing or short input.
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let tag = *bytes.first().ok_or(WireError::BadLength)?;
        let rest = &bytes[1..];
        match tag {
            1 => {
                let has_token = *rest.first().ok_or(WireError::BadLength)?;
                let (token, tail) = if has_token != 0 {
                    if rest.len() < 1 + TOKEN_LEN + 16 {
                        return Err(WireError::BadLength);
                    }
                    let mut token = [0u8; TOKEN_LEN];
                    token.copy_from_slice(&rest[1..1 + TOKEN_LEN]);
                    (Some(token), &rest[1 + TOKEN_LEN..])
                } else {
                    (None, &rest[1..])
                };
                if tail.len() < 16 {
                    return Err(WireError::BadLength);
                }
                Ok(Self::Open {
                    token,
                    recv_next: u64::from_le_bytes(tail[0..8].try_into().expect("slice length")),
                    send_next: u64::from_le_bytes(tail[8..16].try_into().expect("slice length")),
                })
            }
            2 => {
                if rest.len() < TOKEN_LEN + 16 {
                    return Err(WireError::BadLength);
                }
                let mut token = [0u8; TOKEN_LEN];
                token.copy_from_slice(&rest[0..TOKEN_LEN]);
                Ok(Self::Welcome {
                    token,
                    recv_next: u64::from_le_bytes(
                        rest[TOKEN_LEN..TOKEN_LEN + 8]
                            .try_into()
                            .expect("slice length"),
                    ),
                    send_next: u64::from_le_bytes(
                        rest[TOKEN_LEN + 8..TOKEN_LEN + 16]
                            .try_into()
                            .expect("slice length"),
                    ),
                })
            }
            3 | 4 => {
                if rest.len() < 16 {
                    return Err(WireError::BadLength);
                }
                let nonce = u64::from_le_bytes(rest[0..8].try_into().expect("slice length"));
                let sent_ms = u64::from_le_bytes(rest[8..16].try_into().expect("slice length"));
                if tag == 3 {
                    Ok(Self::Ping { nonce, sent_ms })
                } else {
                    Ok(Self::Pong { nonce, sent_ms })
                }
            }
            5 => Ok(Self::Close {
                code: *rest.first().ok_or(WireError::BadLength)?,
            }),
            other => Err(WireError::UnknownControl { got: other }),
        }
    }
}

/// Framing errors. Every variant is peer-triggerable and non-fatal.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum WireError {
    /// The message was shorter than the header.
    #[error("packet is {got} bytes, need at least {need}")]
    TooShort {
        /// Bytes received.
        got: usize,
        /// Bytes required.
        need: usize,
    },
    /// The message exceeded the hard ceiling.
    #[error("packet is {got} bytes, maximum is {max}")]
    TooLarge {
        /// Bytes received.
        got: usize,
        /// Ceiling.
        max: usize,
    },
    /// Magic prefix did not match.
    #[error("packet magic did not match")]
    BadMagic,
    /// The framing version is not supported.
    #[error("unsupported transport version {got}")]
    UnsupportedVersion {
        /// Version received.
        got: u8,
    },
    /// The packet kind is not defined.
    #[error("unknown packet kind {got}")]
    UnknownPacketKind {
        /// Kind received.
        got: u8,
    },
    /// The record kind is not defined.
    #[error("unknown record kind {got}")]
    UnknownRecordKind {
        /// Kind received.
        got: u8,
    },
    /// The control tag is not defined.
    #[error("unknown control tag {got}")]
    UnknownControl {
        /// Tag received.
        got: u8,
    },
    /// A length field did not fit the buffer.
    #[error("length field does not fit the record")]
    BadLength,
    /// A control or ack record carried a payload.
    #[error("control and ack records carry no payload")]
    PayloadNotAllowed,
    /// Session id in the packet did not match the engine's session.
    #[error("packet belongs to a different session")]
    WrongSession,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionId {
        let mut id = [0u8; SESSION_ID_LEN];
        for (index, byte) in id.iter_mut().enumerate() {
            *byte = index as u8;
        }
        id
    }

    #[test]
    fn packet_round_trips() {
        let packet = Packet {
            kind: PacketKind::Handshake,
            session_id: session(),
            body: vec![1, 2, 3],
        };
        assert_eq!(Packet::decode(&packet.encode()).unwrap(), packet);
    }

    #[test]
    fn packet_rejects_bad_input() {
        assert!(matches!(
            Packet::decode(b"HT"),
            Err(WireError::TooShort { .. })
        ));
        let mut bytes = Packet {
            kind: PacketKind::Record,
            session_id: session(),
            body: Vec::new(),
        }
        .encode();
        bytes[0] = b'X';
        assert_eq!(Packet::decode(&bytes).unwrap_err(), WireError::BadMagic);
        bytes[0] = b'H';
        bytes[2] = 9;
        assert_eq!(
            Packet::decode(&bytes).unwrap_err(),
            WireError::UnsupportedVersion { got: 9 }
        );
    }

    #[test]
    fn data_record_round_trips_and_pads() {
        let mut record = Record::data(false, br#"{"type":"hello"}"#.to_vec());
        record.seq = 7;
        record.ack = 3;
        record.ack_bits = 0b1010;
        record.msg_id = 42;
        let bytes = record.encode_plaintext().unwrap();
        assert_eq!(bytes.len() % PADDING_BUCKET, 0);
        assert_eq!(Record::decode_plaintext(&bytes).unwrap(), record);
    }

    #[test]
    fn ack_records_carry_no_payload() {
        let mut record = Record::ack(1, 0);
        record.payload.push(0);
        assert_eq!(
            record.encode_plaintext().unwrap_err(),
            WireError::PayloadNotAllowed
        );
    }

    #[test]
    fn control_records_round_trip_through_the_record_layer() {
        let control = Control::Ping {
            nonce: 7,
            sent_ms: 11,
        };
        let record = Record::control(&control);
        let bytes = record.encode_plaintext().unwrap();
        let decoded = Record::decode_plaintext(&bytes).unwrap();
        assert_eq!(decoded.kind, RecordKind::Control);
        assert_eq!(Control::decode(&decoded.payload).unwrap(), control);
    }

    #[test]
    fn every_control_round_trips() {
        let token = [9u8; TOKEN_LEN];
        for control in [
            Control::Open {
                token: None,
                recv_next: 0,
                send_next: 0,
            },
            Control::Open {
                token: Some(token),
                recv_next: 5,
                send_next: 6,
            },
            Control::Welcome {
                token,
                recv_next: 12,
                send_next: 13,
            },
            Control::Ping {
                nonce: 1,
                sent_ms: 2,
            },
            Control::Pong {
                nonce: 3,
                sent_ms: 4,
            },
            Control::Close { code: 7 },
        ] {
            assert_eq!(Control::decode(&control.encode()).unwrap(), control);
        }
    }

    #[test]
    fn padding_never_exceeds_the_noise_limit() {
        assert_eq!(padded_len(1), PADDING_BUCKET);
        assert!(padded_len(MAX_RECORD_PLAINTEXT + 10) <= MAX_RECORD_PLAINTEXT);
    }
}
