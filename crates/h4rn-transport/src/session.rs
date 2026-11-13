//! The path-agnostic reliable session layer (ADR-009).
//!
//! Application frames are fragmented, assigned a per-direction sequence number
//! and kept in a bounded retransmit buffer until the peer acknowledges them.
//! Because this layer is above the path, switching from a dying LAN socket to a
//! warm relay path resumes the same sequence space: the receiver drops
//! duplicates by sequence and fills gaps from the retransmit buffer, so a
//! switch neither loses nor duplicates a frame.
//!
//! Acknowledgements are cumulative plus a 64-bit bitmap for out-of-order
//! sequences. A peer that receives a gap keeps the later records until the gap
//! is retransmitted, which is what makes a mid-stream path switch cheap.
//!
//! Replay protection has two layers: Noise rejects a replayed ciphertext on a
//! path, and [`SessionLayer`] rejects any sequence at or below `recv_next`.
//! A fresh Noise handshake on a new path therefore cannot resurrect old frames.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::wire::{SESSION_ID_LEN, SessionId, TOKEN_LEN};
use crate::{DEFAULT_MAX_FRAME_BYTES, MAX_FRAGMENT_BYTES};

/// Default number of unacknowledged records kept for retransmission.
pub const DEFAULT_WINDOW_RECORDS: usize = 1024;

/// Default byte budget of the retransmit buffer.
pub const DEFAULT_WINDOW_BYTES: usize = 16 * 1024 * 1024;

/// Default byte budget for a peer's out-of-order queue.
pub const DEFAULT_RECV_WINDOW_BYTES: usize = 16 * 1024 * 1024;

/// Default byte budget for partially reassembled frames.
pub const DEFAULT_REASSEMBLY_BYTES: usize = 16 * 1024 * 1024;

/// One application frame carried end to end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppFrame {
    /// True for a binary protocol frame, false for JSON text.
    pub binary: bool,
    /// Frame bytes.
    pub payload: Vec<u8>,
}

impl AppFrame {
    /// A JSON text frame.
    pub fn text(payload: impl Into<String>) -> Self {
        Self {
            binary: false,
            payload: payload.into().into_bytes(),
        }
    }

    /// A binary protocol frame.
    pub fn binary(payload: impl Into<Vec<u8>>) -> Self {
        Self {
            binary: true,
            payload: payload.into(),
        }
    }

    /// Returns the payload as UTF-8 when it is a text frame.
    pub fn as_text(&self) -> Option<&str> {
        if self.binary {
            None
        } else {
            std::str::from_utf8(&self.payload).ok()
        }
    }
}

/// Token that lets a client resume a session over a new path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeTicket {
    /// Logical session id.
    pub session_id: SessionId,
    /// Opaque 256-bit token issued by the daemon.
    pub token: [u8; TOKEN_LEN],
}

impl ResumeTicket {
    /// Serialises the ticket for persistence.
    pub fn encode(&self) -> String {
        format!(
            "h4rn-resume-1:{}:{}",
            hex::encode(self.session_id),
            hex::encode(self.token)
        )
    }

    /// Parses [`Self::encode`] output.
    pub fn decode(value: &str) -> Result<Self, SessionError> {
        let mut parts = value.trim().split(':');
        if parts.next() != Some("h4rn-resume-1") {
            return Err(SessionError::BadTicket);
        }
        let session = parts.next().ok_or(SessionError::BadTicket)?;
        let token = parts.next().ok_or(SessionError::BadTicket)?;
        if parts.next().is_some() {
            return Err(SessionError::BadTicket);
        }
        let session = hex::decode(session).map_err(|_| SessionError::BadTicket)?;
        let token = hex::decode(token).map_err(|_| SessionError::BadTicket)?;
        if session.len() != SESSION_ID_LEN || token.len() != TOKEN_LEN {
            return Err(SessionError::BadTicket);
        }
        let mut session_id = [0u8; SESSION_ID_LEN];
        session_id.copy_from_slice(&session);
        let mut token_bytes = [0u8; TOKEN_LEN];
        token_bytes.copy_from_slice(&token);
        Ok(Self {
            session_id,
            token: token_bytes,
        })
    }
}

/// One record queued for transmission.
#[derive(Clone, Debug)]
pub struct Outgoing {
    /// Per-direction sequence.
    pub seq: u64,
    /// Message id shared by the fragments of one application frame.
    pub msg_id: u64,
    /// Zero-based fragment index.
    pub frag_index: u16,
    /// True on the final fragment.
    pub frag_last: bool,
    /// Whether the frame is binary.
    pub binary: bool,
    /// Fragment payload.
    pub payload: Arc<[u8]>,
}

#[derive(Debug)]
struct SendSlot {
    record: Outgoing,
    last_sent_ms: u64,
    sent: bool,
    attempts: u32,
}

#[derive(Debug)]
struct Reassembly {
    binary: bool,
    buffer: Vec<u8>,
    expected: u16,
    done: bool,
}

/// Bounded, reliable, ordered frame delivery above a path.
#[derive(Debug)]
pub struct SessionLayer {
    next_seq: u64,
    next_msg_id: u64,
    send: VecDeque<SendSlot>,
    send_bytes: usize,
    max_window_records: usize,
    max_window_bytes: usize,
    recv_next: u64,
    recv_out_of_order: BTreeMap<u64, Pending>,
    recv_bytes: usize,
    max_recv_bytes: usize,
    reassembly: BTreeMap<u64, Reassembly>,
    reassembly_bytes: usize,
    max_reassembly_bytes: usize,
    peer_ack: u64,
    last_ack_at_ms: u64,
    ack_due: bool,
    deliveries: Vec<AppFrame>,
    retransmits: u64,
}

#[derive(Debug)]
struct Pending {
    binary: bool,
    msg_id: u64,
    frag_index: u16,
    frag_last: bool,
    payload: Vec<u8>,
}

impl Default for SessionLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionLayer {
    /// Creates an empty session starting at sequence 1.
    pub fn new() -> Self {
        Self {
            next_seq: 1,
            next_msg_id: 1,
            send: VecDeque::new(),
            send_bytes: 0,
            max_window_records: DEFAULT_WINDOW_RECORDS,
            max_window_bytes: DEFAULT_WINDOW_BYTES,
            recv_next: 1,
            recv_out_of_order: BTreeMap::new(),
            recv_bytes: 0,
            max_recv_bytes: DEFAULT_RECV_WINDOW_BYTES,
            reassembly: BTreeMap::new(),
            reassembly_bytes: 0,
            max_reassembly_bytes: DEFAULT_REASSEMBLY_BYTES,
            peer_ack: 0,
            last_ack_at_ms: 0,
            ack_due: false,
            deliveries: Vec::new(),
            retransmits: 0,
        }
    }

    /// Adjusts the retransmit window, for tests and constrained devices.
    pub fn with_window(mut self, records: usize, bytes: usize) -> Self {
        self.max_window_records = records.max(1);
        self.max_window_bytes = bytes.max(1);
        self
    }

    /// Next sequence number this side will send, and the next it expects.
    pub fn resume_state(&self) -> (u64, u64) {
        (self.recv_next, self.next_seq)
    }

    /// Restores sequence counters after a resume handshake.
    ///
    /// `recv_next` is the next sequence the peer will send; `next_seq` is the
    /// next sequence this side will send.
    pub fn set_resume_state(&mut self, recv_next: u64, next_seq: u64) {
        self.recv_next = recv_next.max(1);
        self.next_seq = next_seq.max(1);
    }

    /// Applies a peer acknowledgement immediately.
    ///
    /// A resume handshake reports the peer's actual receive position, so records
    /// below it are provably delivered and can leave the retransmit buffer.
    pub fn apply_peer_position(&mut self, recv_next: u64) {
        self.apply_peer_ack(recv_next.saturating_sub(1));
    }

    /// Discards buffered outgoing records and restarts the send sequence.
    ///
    /// Used only when a resume handshake proves the peer never saw sequences
    /// below its reported position (for example after a client restart).
    pub fn reset_outgoing(&mut self, next_seq: u64) {
        self.send.clear();
        self.send_bytes = 0;
        self.next_seq = next_seq.max(1);
    }

    /// Moves the receive expectation forward when a resume proves a gap is
    /// unrecoverable. Returns the number of sequences skipped.
    pub fn rebase_incoming(&mut self, next: u64) -> u64 {
        let next = next.max(1);
        if next <= self.recv_next {
            return 0;
        }
        let skipped = next - self.recv_next;
        self.recv_next = next;
        self.recv_out_of_order.clear();
        self.recv_bytes = 0;
        self.reassembly.clear();
        self.reassembly_bytes = 0;
        skipped
    }

    /// Sets the receive expectation exactly, discarding buffered state.
    ///
    /// Used when a resuming peer proves it lost track of earlier sequences
    /// (for example a client process restart). The application protocol's own
    /// resume handles any frames skipped this way.
    pub fn reset_incoming(&mut self, next: u64) {
        self.recv_next = next.max(1);
        self.recv_out_of_order.clear();
        self.recv_bytes = 0;
        self.reassembly.clear();
        self.reassembly_bytes = 0;
        self.ack_due = false;
    }

    /// Highest contiguous sequence received by this side.
    pub fn peer_ack(&self) -> u64 {
        self.peer_ack
    }

    /// Next sequence this side will send.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Next sequence this side expects to receive.
    pub fn recv_next(&self) -> u64 {
        self.recv_next
    }

    /// Lowest sequence still waiting for an acknowledgement.
    pub fn oldest_unacked(&self) -> Option<u64> {
        self.send.front().map(|slot| slot.record.seq)
    }

    /// Number of records retransmitted so far, for diagnostics and tests.
    pub fn retransmits(&self) -> u64 {
        self.retransmits
    }

    /// Number of unacknowledged records.
    pub fn unacked_records(&self) -> usize {
        self.send.len()
    }

    /// Queues one application frame, fragmenting it when necessary.
    pub fn enqueue(&mut self, frame: AppFrame, _now_ms: u64) -> Result<(), SessionError> {
        if frame.payload.len() > DEFAULT_MAX_FRAME_BYTES {
            return Err(SessionError::FrameTooLarge {
                got: frame.payload.len(),
                max: DEFAULT_MAX_FRAME_BYTES,
            });
        }
        let fragments = frame.payload.len().div_ceil(MAX_FRAGMENT_BYTES).max(1);
        let already = self.send.len();
        if already + fragments > self.max_window_records
            || self.send_bytes + frame.payload.len() > self.max_window_bytes
        {
            return Err(SessionError::Backpressure);
        }
        let msg_id = self.next_msg_id;
        self.next_msg_id += 1;
        for index in 0..fragments {
            let start = index * MAX_FRAGMENT_BYTES;
            let end = ((index + 1) * MAX_FRAGMENT_BYTES).min(frame.payload.len());
            let payload = frame.payload[start..end].to_vec();
            self.send_bytes += payload.len();
            self.send.push_back(SendSlot {
                record: Outgoing {
                    seq: self.next_seq,
                    msg_id,
                    frag_index: index as u16,
                    frag_last: index + 1 == fragments,
                    binary: frame.binary,
                    payload: payload.into(),
                },
                last_sent_ms: 0,
                sent: false,
                attempts: 0,
            });
            self.next_seq += 1;
        }
        Ok(())
    }

    /// Records that can go out now.
    ///
    /// `all` ignores the retransmit timer and re-sends every unacknowledged
    /// record, which is exactly what a path switch needs.
    pub fn sendable(&mut self, now_ms: u64, rto_ms: u64, all: bool) -> Vec<Outgoing> {
        let mut out = Vec::new();
        for slot in &mut self.send {
            let due = all || !slot.sent || now_ms.saturating_sub(slot.last_sent_ms) >= rto_ms;
            if !due {
                continue;
            }
            if slot.sent {
                self.retransmits += 1;
            }
            slot.sent = true;
            slot.attempts += 1;
            slot.last_sent_ms = now_ms;
            out.push(slot.record.clone());
        }
        out
    }

    /// Current cumulative ack and bitmap to attach to outgoing records.
    pub fn ack_state(&self) -> (u64, u32) {
        let ack = self.recv_next.saturating_sub(1);
        let mut bits = 0u32;
        for bit in 0..64u64 {
            let seq = ack.saturating_sub(bit + 1);
            if self.recv_out_of_order.contains_key(&seq) {
                bits |= 1 << bit;
            }
        }
        (ack, bits)
    }

    /// Feeds one received data or ack record.
    pub fn on_record(&mut self, record: Incoming, now_ms: u64) -> Result<(), SessionError> {
        self.apply_peer_ack(record.ack);
        match record.kind {
            IncomingKind::Ack => {
                self.ack_due = false;
                self.last_ack_at_ms = now_ms;
                Ok(())
            }
            IncomingKind::Data => self.accept_data(record, now_ms),
            IncomingKind::Control => Ok(()),
        }
    }

    /// Whether an acknowledgement should be sent now rather than waiting for
    /// something to piggyback on.
    pub fn should_ack(&mut self, now_ms: u64, delay_ms: u64) -> bool {
        if !self.ack_due {
            return false;
        }
        if now_ms.saturating_sub(self.last_ack_at_ms) >= delay_ms {
            self.ack_due = false;
            self.last_ack_at_ms = now_ms;
            true
        } else {
            false
        }
    }

    /// Drains frames that are ready for the application.
    pub fn take_deliveries(&mut self) -> Vec<AppFrame> {
        std::mem::take(&mut self.deliveries)
    }

    fn apply_peer_ack(&mut self, ack: u64) {
        if ack > self.peer_ack {
            self.peer_ack = ack;
        }
        while let Some(front) = self.send.front() {
            if front.record.seq <= ack {
                let slot = self.send.pop_front().expect("front exists");
                self.send_bytes = self.send_bytes.saturating_sub(slot.record.payload.len());
            } else {
                break;
            }
        }
    }

    fn accept_data(&mut self, record: Incoming, now_ms: u64) -> Result<(), SessionError> {
        let seq = record.seq;
        if seq < self.recv_next {
            // Already delivered: an acknowledgement may have been lost, so ask
            // for another one but never deliver twice.
            self.ack_due = true;
            self.last_ack_at_ms = now_ms;
            return Ok(());
        }
        if seq == self.recv_next {
            self.deliver(record)?;
            self.recv_next += 1;
            self.ack_due = true;
            loop {
                let next = self.recv_next;
                if let Some(pending) = self.recv_out_of_order.remove(&next) {
                    self.recv_bytes = self.recv_bytes.saturating_sub(pending.payload.len());
                    self.deliver_pending(pending)?;
                    self.recv_next += 1;
                } else {
                    break;
                }
            }
            return Ok(());
        }
        // A gap: keep it until the missing sequence arrives.
        if self.recv_bytes + record.payload.len() > self.max_recv_bytes {
            self.ack_due = true;
            return Err(SessionError::ReceiveWindowFull);
        }
        self.recv_bytes += record.payload.len();
        self.recv_out_of_order.insert(
            seq,
            Pending {
                binary: record.binary,
                msg_id: record.msg_id,
                frag_index: record.frag_index,
                frag_last: record.frag_last,
                payload: record.payload,
            },
        );
        self.ack_due = true;
        Ok(())
    }

    fn deliver(&mut self, record: Incoming) -> Result<(), SessionError> {
        self.deliver_pending(Pending {
            binary: record.binary,
            msg_id: record.msg_id,
            frag_index: record.frag_index,
            frag_last: record.frag_last,
            payload: record.payload,
        })
    }

    fn deliver_pending(&mut self, pending: Pending) -> Result<(), SessionError> {
        if pending.frag_index == 0 && pending.frag_last {
            self.deliveries.push(AppFrame {
                binary: pending.binary,
                payload: pending.payload,
            });
            return Ok(());
        }
        if pending.frag_index == 0 {
            self.reassembly_bytes += pending.payload.len();
            self.reassembly.insert(
                pending.msg_id,
                Reassembly {
                    binary: pending.binary,
                    buffer: pending.payload,
                    expected: 1,
                    done: false,
                },
            );
            self.trim_reassembly()?;
            return Ok(());
        }
        let Some(part) = self.reassembly.get_mut(&pending.msg_id) else {
            return Err(SessionError::Protocol("fragment without a first fragment"));
        };
        if pending.frag_index != part.expected {
            return Err(SessionError::Protocol("fragment arrived out of order"));
        }
        part.buffer.extend_from_slice(&pending.payload);
        part.expected += 1;
        self.reassembly_bytes += pending.payload.len();
        if pending.frag_last {
            part.done = true;
        }
        if part.done {
            let part = self
                .reassembly
                .remove(&pending.msg_id)
                .expect("part exists");
            self.reassembly_bytes = self.reassembly_bytes.saturating_sub(part.buffer.len());
            self.deliveries.push(AppFrame {
                binary: part.binary,
                payload: part.buffer,
            });
        }
        self.trim_reassembly()?;
        Ok(())
    }

    fn trim_reassembly(&mut self) -> Result<(), SessionError> {
        if self.reassembly_bytes <= self.max_reassembly_bytes {
            return Ok(());
        }
        // A peer that never completes a frame cannot be allowed to grow memory
        // without bound. Drop the oldest partial frame; its retransmission will
        // arrive again after this side reports a gap.
        if let Some(key) = self.reassembly.keys().next().copied() {
            if let Some(part) = self.reassembly.remove(&key) {
                self.reassembly_bytes = self.reassembly_bytes.saturating_sub(part.buffer.len());
            }
        }
        Err(SessionError::ReceiveWindowFull)
    }
}

/// One received record, decoupled from the wire type.
#[derive(Clone, Debug)]
pub struct Incoming {
    /// Data, control or ack.
    pub kind: IncomingKind,
    /// Whether a data frame is binary.
    pub binary: bool,
    /// Message id.
    pub msg_id: u64,
    /// Fragment index.
    pub frag_index: u16,
    /// Last-fragment flag.
    pub frag_last: bool,
    /// Per-direction sequence (0 for control and ack).
    pub seq: u64,
    /// Sender's cumulative ack.
    pub ack: u64,
    /// Sender's ack bitmap.
    pub ack_bits: u32,
    /// Payload.
    pub payload: Vec<u8>,
}

/// Why a record was received.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IncomingKind {
    /// Application data.
    Data,
    /// Transport control.
    Control,
    /// A bare acknowledgement.
    Ack,
}

/// Session-layer failures.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// The retransmit window is full; the caller must wait for an ack.
    #[error("retransmit window is full")]
    Backpressure,
    /// The application frame exceeds the configured maximum.
    #[error("frame is {got} bytes, maximum is {max}")]
    FrameTooLarge {
        /// Bytes offered.
        got: usize,
        /// Ceiling.
        max: usize,
    },
    /// Out-of-order or reassembly budget exhausted.
    #[error("receive window is full")]
    ReceiveWindowFull,
    /// The peer sent something that violates the record grammar.
    #[error("session protocol violation: {0}")]
    Protocol(&'static str),
    /// The resume ticket could not be parsed.
    #[error("resume ticket is malformed")]
    BadTicket,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(binary: bool, payload: Vec<u8>) -> Incoming {
        Incoming {
            kind: IncomingKind::Data,
            binary,
            msg_id: 1,
            frag_index: 0,
            frag_last: true,
            seq: 0,
            ack: 0,
            ack_bits: 0,
            payload,
        }
    }

    #[test]
    fn frames_deliver_in_order() {
        let mut sender = SessionLayer::new();
        let mut receiver = SessionLayer::new();
        sender.enqueue(AppFrame::text("one"), 0).unwrap();
        sender.enqueue(AppFrame::text("two"), 0).unwrap();
        for record in sender.sendable(0, 100, false) {
            let mut incoming = data(false, record.payload.to_vec());
            incoming.seq = record.seq;
            incoming.msg_id = record.msg_id;
            incoming.ack = receiver.ack_state().0;
            receiver.on_record(incoming, 0).unwrap();
            sender
                .on_record(
                    Incoming {
                        kind: IncomingKind::Ack,
                        binary: false,
                        msg_id: 0,
                        frag_index: 0,
                        frag_last: true,
                        seq: 0,
                        ack: receiver.ack_state().0,
                        ack_bits: 0,
                        payload: Vec::new(),
                    },
                    0,
                )
                .unwrap();
        }
        let delivered = receiver.take_deliveries();
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0].as_text(), Some("one"));
        assert_eq!(delivered[1].as_text(), Some("two"));
        assert_eq!(sender.unacked_records(), 0);
    }

    #[test]
    fn loss_is_repaired_by_retransmission() {
        let mut sender = SessionLayer::new();
        let mut receiver = SessionLayer::new();
        sender.enqueue(AppFrame::text("a"), 0).unwrap();
        sender.enqueue(AppFrame::text("b"), 0).unwrap();
        let records = sender.sendable(0, 100, false);
        // Drop the first record on the floor.
        let mut incoming = data(false, records[1].payload.to_vec());
        incoming.seq = records[1].seq;
        incoming.ack = receiver.ack_state().0;
        receiver.on_record(incoming, 0).unwrap();
        assert!(receiver.take_deliveries().is_empty());
        // The acknowledgement reports seq 1 missing, so the sender keeps it.
        let acked = receiver.ack_state().0;
        assert_eq!(acked, 0);
        sender
            .on_record(
                Incoming {
                    kind: IncomingKind::Ack,
                    binary: false,
                    msg_id: 0,
                    frag_index: 0,
                    frag_last: true,
                    seq: 0,
                    ack: acked,
                    ack_bits: receiver.ack_state().1,
                    payload: Vec::new(),
                },
                0,
            )
            .unwrap();
        assert_eq!(sender.unacked_records(), 2);
        let retransmit = sender.sendable(150, 100, false);
        assert_eq!(retransmit.len(), 2);
        for record in retransmit {
            let mut incoming = data(false, record.payload.to_vec());
            incoming.seq = record.seq;
            incoming.ack = receiver.ack_state().0;
            receiver.on_record(incoming, 150).unwrap();
        }
        let delivered = receiver.take_deliveries();
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0].as_text(), Some("a"));
        assert_eq!(delivered[1].as_text(), Some("b"));
    }

    #[test]
    fn reordering_and_duplication_deliver_once_in_order() {
        let mut sender = SessionLayer::new();
        let mut receiver = SessionLayer::new();
        for index in 0..4 {
            sender
                .enqueue(AppFrame::text(format!("{index}")), 0)
                .unwrap();
        }
        let records = sender.sendable(0, 100, false);
        let order = [2usize, 0, 3, 1];
        for index in order {
            let record = &records[index];
            let mut incoming = data(false, record.payload.to_vec());
            incoming.seq = record.seq;
            receiver.on_record(incoming, 0).unwrap();
            // Duplicate it too; the receiver must ignore the replay.
            let mut duplicate = data(false, record.payload.to_vec());
            duplicate.seq = record.seq;
            receiver.on_record(duplicate, 0).unwrap();
        }
        let delivered = receiver.take_deliveries();
        let text: Vec<_> = delivered
            .iter()
            .filter_map(|frame| frame.as_text())
            .collect();
        assert_eq!(text, ["0", "1", "2", "3"]);
        assert_eq!(receiver.ack_state().0, 4);
    }

    #[test]
    fn large_frames_are_fragmented_and_reassembled() {
        let mut sender = SessionLayer::new();
        let mut receiver = SessionLayer::new();
        let payload = vec![0xAB; MAX_FRAGMENT_BYTES * 2 + 17];
        sender
            .enqueue(AppFrame::binary(payload.clone()), 0)
            .unwrap();
        let records = sender.sendable(0, 100, false);
        assert_eq!(records.len(), 3);
        for record in records {
            let mut incoming = data(true, record.payload.to_vec());
            incoming.seq = record.seq;
            incoming.msg_id = record.msg_id;
            incoming.frag_index = record.frag_index;
            incoming.frag_last = record.frag_last;
            receiver.on_record(incoming, 0).unwrap();
        }
        let delivered = receiver.take_deliveries();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].payload, payload);
    }

    #[test]
    fn a_full_window_reports_backpressure() {
        let mut sender = SessionLayer::new().with_window(2, 1024 * 1024);
        sender.enqueue(AppFrame::text("a"), 0).unwrap();
        sender.enqueue(AppFrame::text("b"), 0).unwrap();
        assert_eq!(
            sender.enqueue(AppFrame::text("c"), 0).unwrap_err(),
            SessionError::Backpressure
        );
    }

    #[test]
    fn resume_state_survives_a_new_layer() {
        let mut sender = SessionLayer::new();
        sender.enqueue(AppFrame::text("a"), 0).unwrap();
        sender.enqueue(AppFrame::text("b"), 0).unwrap();
        let (recv_next, next_seq) = sender.resume_state();
        assert_eq!((recv_next, next_seq), (1, 3));
        let mut resumed = SessionLayer::new();
        resumed.set_resume_state(recv_next, next_seq);
        resumed.enqueue(AppFrame::text("c"), 0).unwrap();
        let records = resumed.sendable(0, 100, false);
        assert_eq!(records[0].seq, 3);
    }

    #[test]
    fn ticket_round_trips() {
        let ticket = ResumeTicket {
            session_id: [7u8; SESSION_ID_LEN],
            token: [9u8; TOKEN_LEN],
        };
        assert_eq!(ResumeTicket::decode(&ticket.encode()).unwrap(), ticket);
        assert_eq!(
            ResumeTicket::decode("nope").unwrap_err(),
            SessionError::BadTicket
        );
    }
}
