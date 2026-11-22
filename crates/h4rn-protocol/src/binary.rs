//! Binary frames for PTY and blob streams (ADR-007, ADR-008).
//!
//! Every binary WebSocket message starts with this fixed 16-byte header,
//! little-endian throughout:
//!
//! ```text
//! offset  size  field
//! 0       2     magic       b"H4"
//! 2       1     version     1
//! 3       1     kind        BinaryKind (1 = pty_output, 2 = pty_snapshot,
//!                           3 = pty_input, 4 = blob_chunk, 5 = ack,
//!                           6 = resize)
//! 4       4     stream_id   u32
//! 8       8     seq         u64, per-stream monotonic
//! 16      ...   payload     rest of the message
//! ```
//!
//! `seq` is the stream sequence, not the JSON event sequence. A PTY snapshot
//! carries the sequence it replaces in `seq`, so a reconnecting client can
//! discard replayed history and resume from the snapshot. A PTY input frame
//! has no sequence and uses `seq` for flags instead (see [`PTY_INPUT_REPLY`]):
//! bit 0 marks a query reply, bits 1-32 name the output chunk that carried the
//! query's first byte and bits 33-52 name the answer's ordinal among the
//! queries that start in that chunk (see [`pty_input_reply_seq`]). Payload
//! length is implied by the WebSocket message length; there is no separate
//! length field.

use thiserror::Error;

/// Length of every binary frame header.
pub const BINARY_HEADER_LEN: usize = 16;

/// Magic bytes at the start of a binary frame.
pub const BINARY_MAGIC: [u8; 2] = *b"H4";

/// Binary framing version.
pub const BINARY_VERSION: u8 = 1;

/// Flag in the `seq` field of a [`BinaryKind::PtyInput`] frame.
///
/// Input frames do not need a sequence, so `seq` carries flags instead. Bit 0
/// marks the payload as xterm's own answer to a terminal query (device
/// attributes, cursor position, mode or colour report) rather than typed
/// input. An older daemon ignores `seq` for input and writes the frame as
/// ordinary input, which is what every viewer did before the flag existed.
pub const PTY_INPUT_REPLY: u64 = 1;

/// Shift of the output position in a reply input frame's `seq` flag word.
///
/// Bit 0 remains [`PTY_INPUT_REPLY`]; bits 1-32 carry the sequence of the
/// output chunk that contained the query.
pub const PTY_INPUT_POSITION_SHIFT: u32 = 1;

/// Number of bits the output position occupies in a reply's flag word.
pub const PTY_INPUT_POSITION_BITS: u32 = 32;

/// Largest output position the flag word can carry.
///
/// One terminal would have to emit more than four billion chunks before it
/// reached this bound; the client falls back to an unpositioned reply rather
/// than wrapping.
pub const PTY_INPUT_POSITION_MAX: u64 = u32::MAX as u64;

/// Shift of the answer ordinal in a reply input frame's `seq` flag word.
///
/// One output chunk can carry several queries (two DSR/OSC queries, a batched
/// `OSC 4` palette query) and xterm answers each separately. The ordinal is the
/// query's index among the queries that start in that chunk, so two answers
/// naming the same position stay distinct and every viewer numbers them alike.
pub const PTY_INPUT_ORDINAL_SHIFT: u32 = PTY_INPUT_POSITION_SHIFT + PTY_INPUT_POSITION_BITS;

/// Number of bits the answer ordinal occupies in a reply's flag word.
pub const PTY_INPUT_ORDINAL_BITS: u32 = 20;

/// Largest answer ordinal the flag word can carry.
///
/// The whole flag word then stays within JavaScript's safe integer range
/// (`2^53 - 1`), so the UI encodes it without losing precision.
pub const PTY_INPUT_ORDINAL_MAX: u64 = (1 << PTY_INPUT_ORDINAL_BITS) - 1;

/// Packs a marked reply's output position and answer ordinal into an input
/// frame's `seq` field.
///
/// `position` is the sequence of the output chunk that carried the query's
/// first byte; a query split across chunks names the chunk where it started,
/// which every viewer that parsed that chunk computes the same way. `ordinal`
/// is the answer's index among the queries that start in that chunk, starting
/// at zero.
///
/// Both values must fit their fields; a const context fails to compile and a
/// call with a larger value panics rather than silently wrapping into the
/// other field. An old daemon still sees bit 0 and writes the frame as ordinary
/// input; an old client sends a bare flag word and names no position.
pub const fn pty_input_reply_seq(position: u64, ordinal: u64) -> u64 {
    assert!(
        position <= PTY_INPUT_POSITION_MAX,
        "a terminal output position must fit in 32 bits"
    );
    assert!(
        ordinal <= PTY_INPUT_ORDINAL_MAX,
        "an answer ordinal must fit in 20 bits"
    );
    (ordinal << PTY_INPUT_ORDINAL_SHIFT) | (position << PTY_INPUT_POSITION_SHIFT) | PTY_INPUT_REPLY
}

/// The output position a marked reply names, or `None` when it names none.
///
/// `None` covers both typed input (no reply flag) and a reply from an older
/// client that sent the bare [`PTY_INPUT_REPLY`] flag without a position.
pub const fn pty_input_reply_position(seq: u64) -> Option<u64> {
    if seq & PTY_INPUT_REPLY == 0 {
        return None;
    }
    let position = (seq >> PTY_INPUT_POSITION_SHIFT) & PTY_INPUT_POSITION_MAX;
    if position == 0 { None } else { Some(position) }
}

/// The answer ordinal a marked reply names, or zero when it names none.
///
/// Only meaningful together with [`pty_input_reply_position`]: a reply that
/// names no position is an older client's and has no ordinal.
pub const fn pty_input_reply_ordinal(seq: u64) -> u64 {
    (seq >> PTY_INPUT_ORDINAL_SHIFT) & PTY_INPUT_ORDINAL_MAX
}

/// What a binary frame carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BinaryKind {
    /// Incremental PTY output.
    PtyOutput = 1,
    /// A full PTY screen snapshot; replaces history up to `seq`.
    PtySnapshot = 2,
    /// User input written to a PTY.
    PtyInput = 3,
    /// A chunk of an attachment blob.
    BlobChunk = 4,
    /// Acknowledges receipt of a chunk.
    Ack = 5,
    /// A PTY resize request.
    Resize = 6,
}

impl BinaryKind {
    /// Parses a wire discriminant.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::PtyOutput),
            2 => Some(Self::PtySnapshot),
            3 => Some(Self::PtyInput),
            4 => Some(Self::BlobChunk),
            5 => Some(Self::Ack),
            6 => Some(Self::Resize),
            _ => None,
        }
    }

    /// Returns the wire discriminant.
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// A decoded binary frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinaryFrame {
    /// What the frame carries.
    pub kind: BinaryKind,
    /// Logical stream (PTY session or blob upload).
    pub stream_id: u32,
    /// Per-stream sequence number.
    pub seq: u64,
    /// Frame body.
    pub payload: Vec<u8>,
}

impl BinaryFrame {
    /// Creates an empty-payload frame.
    pub fn new(kind: BinaryKind, stream_id: u32, seq: u64) -> Self {
        Self {
            kind,
            stream_id,
            seq,
            payload: Vec::new(),
        }
    }

    /// Encodes the header and payload into one WebSocket message.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(BINARY_HEADER_LEN + self.payload.len());
        out.extend_from_slice(&BINARY_MAGIC);
        out.push(BINARY_VERSION);
        out.push(self.kind.as_u8());
        out.extend_from_slice(&self.stream_id.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decodes one WebSocket message.
    pub fn decode(bytes: &[u8]) -> Result<Self, BinaryFrameError> {
        if bytes.len() < BINARY_HEADER_LEN {
            return Err(BinaryFrameError::TooShort { got: bytes.len() });
        }
        if bytes[0..2] != BINARY_MAGIC {
            return Err(BinaryFrameError::BadMagic {
                got: [bytes[0], bytes[1]],
            });
        }
        let version = bytes[2];
        if version != BINARY_VERSION {
            return Err(BinaryFrameError::UnsupportedVersion { got: version });
        }
        let kind =
            BinaryKind::from_u8(bytes[3]).ok_or(BinaryFrameError::UnknownKind { got: bytes[3] })?;
        let stream_id = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let seq = u64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        Ok(Self {
            kind,
            stream_id,
            seq,
            payload: bytes[BINARY_HEADER_LEN..].to_vec(),
        })
    }
}

/// Binary framing errors.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum BinaryFrameError {
    /// The message was shorter than the fixed header.
    #[error("binary frame is {got} bytes, need at least {BINARY_HEADER_LEN}")]
    TooShort {
        /// Bytes received.
        got: usize,
    },
    /// The magic prefix did not match.
    #[error("binary frame magic was {got:?}, expected {BINARY_MAGIC:?}")]
    BadMagic {
        /// Magic received.
        got: [u8; 2],
    },
    /// The framing version is not supported.
    #[error("unsupported binary frame version {got}, expected {BINARY_VERSION}")]
    UnsupportedVersion {
        /// Version received.
        got: u8,
    },
    /// The kind discriminant is not defined.
    #[error("unknown binary frame kind {got}")]
    UnknownKind {
        /// Kind received.
        got: u8,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_round_trips() {
        for kind in [
            BinaryKind::PtyOutput,
            BinaryKind::PtySnapshot,
            BinaryKind::PtyInput,
            BinaryKind::BlobChunk,
            BinaryKind::Ack,
            BinaryKind::Resize,
        ] {
            let frame = BinaryFrame {
                kind,
                stream_id: 42,
                seq: 9_007_199_254_740_993,
                payload: vec![0, 1, 2, 0xff],
            };
            let bytes = frame.encode();
            assert_eq!(bytes.len(), BINARY_HEADER_LEN + 4);
            assert_eq!(BinaryFrame::decode(&bytes).unwrap(), frame);
        }
    }

    #[test]
    fn empty_payload_round_trips() {
        let frame = BinaryFrame::new(BinaryKind::Ack, 1, 0);
        assert_eq!(BinaryFrame::decode(&frame.encode()).unwrap(), frame);
    }

    #[test]
    fn reply_positions_and_ordinals_round_trip_in_the_flag_word() {
        for (position, ordinal) in [
            (1_u64, 0_u64),
            (2, 0),
            (42, 1),
            (123_456, 7),
            (PTY_INPUT_POSITION_MAX, PTY_INPUT_ORDINAL_MAX),
        ] {
            let seq = pty_input_reply_seq(position, ordinal);
            assert_ne!(seq & PTY_INPUT_REPLY, 0);
            assert_eq!(pty_input_reply_position(seq), Some(position));
            assert_eq!(pty_input_reply_ordinal(seq), ordinal);
        }
    }

    #[test]
    fn the_flag_word_has_a_literal_golden_vector() {
        // The UI's `ptyInputReplySeq(1, 1)` asserts the same number: position
        // 1 sits one bit in and ordinal 1 starts at bit 33.
        assert_eq!(pty_input_reply_seq(1, 1), 8_589_934_595);
        assert_eq!(pty_input_reply_seq(1, 1), (1_u64 << 33) + 3);
    }

    #[test]
    fn a_reply_flag_word_stays_within_the_javascript_safe_integer() {
        // The UI encodes the same layout with a plain number.
        let seq = pty_input_reply_seq(PTY_INPUT_POSITION_MAX, PTY_INPUT_ORDINAL_MAX);
        assert_eq!(seq, 9_007_199_254_740_991);
        assert_eq!(seq, (1_u64 << 53) - 1);
    }

    #[test]
    fn two_answers_in_one_chunk_are_distinct() {
        let first = pty_input_reply_seq(7, 0);
        let second = pty_input_reply_seq(7, 1);
        assert_ne!(first, second);
        assert_eq!(pty_input_reply_position(first), Some(7));
        assert_eq!(pty_input_reply_position(second), Some(7));
        assert_eq!(pty_input_reply_ordinal(first), 0);
        assert_eq!(pty_input_reply_ordinal(second), 1);
    }

    #[test]
    fn a_bare_reply_flag_names_no_position() {
        // An old client sends only the flag; typed input sends neither.
        assert_eq!(pty_input_reply_position(PTY_INPUT_REPLY), None);
        assert_eq!(pty_input_reply_position(0), None);
        // Higher bits without bit 0 are not a reply at all.
        assert_eq!(
            pty_input_reply_position(pty_input_reply_seq(7, 0) & !1),
            None
        );
    }

    #[test]
    fn header_layout_is_little_endian() {
        let frame = BinaryFrame::new(BinaryKind::PtyOutput, 0x0102_0304, 0x0807_0605_0403_0201);
        let bytes = frame.encode();
        assert_eq!(&bytes[0..2], b"H4");
        assert_eq!(bytes[2], 1);
        assert_eq!(bytes[3], 1);
        assert_eq!(&bytes[4..8], &[4, 3, 2, 1]);
        assert_eq!(&bytes[8..16], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn short_frame_is_rejected() {
        assert_eq!(
            BinaryFrame::decode(&[b'H', b'4', 1]).unwrap_err(),
            BinaryFrameError::TooShort { got: 3 }
        );
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut bytes = BinaryFrame::new(BinaryKind::Ack, 0, 0).encode();
        bytes[0] = b'X';
        assert_eq!(
            BinaryFrame::decode(&bytes).unwrap_err(),
            BinaryFrameError::BadMagic { got: *b"X4" }
        );
    }

    #[test]
    fn bad_version_and_kind_are_rejected() {
        let mut bytes = BinaryFrame::new(BinaryKind::Ack, 0, 0).encode();
        bytes[2] = 99;
        assert_eq!(
            BinaryFrame::decode(&bytes).unwrap_err(),
            BinaryFrameError::UnsupportedVersion { got: 99 }
        );

        let mut bytes = BinaryFrame::new(BinaryKind::Ack, 0, 0).encode();
        bytes[3] = 99;
        assert_eq!(
            BinaryFrame::decode(&bytes).unwrap_err(),
            BinaryFrameError::UnknownKind { got: 99 }
        );
    }
}
