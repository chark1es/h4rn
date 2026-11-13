//! Multipath behavior driven through the real client engine.
//!
//! A small in-memory peer plays the daemon: it runs the same `ServerPath` and
//! `ServerSession` types the daemon uses, so these tests exercise path probing,
//! failover, hysteresis and resume without opening sockets or reading a clock.

use std::collections::{HashMap, VecDeque};

use h4rn_protocol::TransportPathKind;
use h4rn_transport::engine::{Effect, EngineConfig, PathId, PathSpec, TransportEngine};
use h4rn_transport::keys::DeviceIdentity;
use h4rn_transport::noise::RekeyLimits;
use h4rn_transport::server_session::{
    PairingPolicy, ServerHandshakeStep, ServerPath, ServerSession,
};
use h4rn_transport::session::{AppFrame, Incoming, IncomingKind};
use h4rn_transport::wire::{
    Control, Packet, PacketKind, Record, RecordKind, SESSION_ID_LEN, TOKEN_LEN,
};

const CLIENT_SESSION: [u8; SESSION_ID_LEN] = [7u8; SESSION_ID_LEN];
const DAEMON_TOKEN: [u8; TOKEN_LEN] = [0xa5u8; TOKEN_LEN];

struct Peer {
    identity: DeviceIdentity,
    policy: PairingPolicy,
    session_id: [u8; SESSION_ID_LEN],
    session: Option<ServerSession>,
    paths: HashMap<PathId, ServerPath>,
    /// Application frames the daemon delivered, in delivery order.
    received: Vec<AppFrame>,
    /// Whether the daemon acknowledges data records immediately.
    ack_data: bool,
}

impl Peer {
    fn new(identity: DeviceIdentity, client: [u8; 32]) -> Self {
        Self {
            identity,
            policy: PairingPolicy::new([client]),
            session_id: CLIENT_SESSION,
            session: None,
            paths: HashMap::new(),
            received: Vec::new(),
            ack_data: true,
        }
    }

    fn forget_paths(&mut self) {
        self.paths.clear();
    }

    /// Feeds one client packet and returns the packets to send back.
    fn feed(&mut self, path_id: PathId, bytes: &[u8], now: u64) -> Vec<Vec<u8>> {
        let packet = Packet::decode(bytes).expect("decode client packet");
        assert_eq!(packet.session_id, self.session_id);
        let mut out = Vec::new();
        match packet.kind {
            PacketKind::Handshake => {
                let path = self.paths.entry(path_id).or_insert_with(|| {
                    ServerPath::new(
                        self.session_id,
                        self.identity.clone(),
                        None,
                        RekeyLimits::default(),
                    )
                });
                let step = path
                    .on_handshake(&packet.body, &self.policy)
                    .expect("handshake");
                let reply = match step {
                    ServerHandshakeStep::Reply(reply) => Some(reply),
                    ServerHandshakeStep::PairedThroughPsk { reply, .. } => reply,
                    ServerHandshakeStep::Complete { reply, .. } => reply,
                    ServerHandshakeStep::Waiting => None,
                    ServerHandshakeStep::Rejected(reason) => {
                        panic!("peer rejected the path: {reason:?}")
                    }
                };
                if let Some(reply) = reply {
                    out.push(handshake_packet(self.session_id, reply));
                }
            }
            PacketKind::Record => {
                let path = self
                    .paths
                    .get_mut(&path_id)
                    .expect("record on unknown path");
                let plaintext = path.open(&packet.body).expect("open record");
                let record = Record::decode_plaintext(&plaintext).expect("decode record");
                match record.kind {
                    RecordKind::Data => {
                        let session = self.session.as_mut().expect("session attached");
                        let incoming = Incoming {
                            kind: IncomingKind::Data,
                            binary: record.binary,
                            msg_id: record.msg_id,
                            frag_index: record.frag_index,
                            frag_last: record.frag_last,
                            seq: record.seq,
                            ack: record.ack,
                            ack_bits: record.ack_bits,
                            payload: record.payload,
                        };
                        let frames = session.on_record(path_id, incoming, now).expect("record");
                        self.received.extend(frames);
                        if self.ack_data {
                            let (ack, ack_bits) = session.ack_state();
                            out.push(seal(path, self.session_id, Record::ack(ack, ack_bits)));
                        }
                    }
                    RecordKind::Ack => {
                        let session = self.session.as_mut().expect("session attached");
                        let incoming = Incoming {
                            kind: IncomingKind::Ack,
                            binary: false,
                            msg_id: 0,
                            frag_index: 0,
                            frag_last: true,
                            seq: 0,
                            ack: record.ack,
                            ack_bits: record.ack_bits,
                            payload: Vec::new(),
                        };
                        session.on_record(path_id, incoming, now).expect("ack");
                    }
                    RecordKind::Control => match Control::decode(&record.payload).expect("control")
                    {
                        Control::Open {
                            recv_next,
                            send_next,
                            ..
                        } => {
                            let session = self.session.get_or_insert_with(|| {
                                ServerSession::with_token(self.session_id, DAEMON_TOKEN, now)
                            });
                            let welcome = session
                                .attach(path_id, recv_next, send_next, now)
                                .expect("attach");
                            let (ack, ack_bits) = session.ack_state();
                            let mut record = Record::control(&welcome);
                            record.ack = ack;
                            record.ack_bits = ack_bits;
                            out.push(seal(path, self.session_id, record));
                            for outgoing in session.sendable(now, true) {
                                out.push(seal_outgoing(path, self.session_id, outgoing));
                            }
                        }
                        Control::Ping { nonce, sent_ms } => {
                            let session = self.session.as_ref().expect("session attached");
                            let (ack, ack_bits) = session.ack_state();
                            let mut reply = Record::control(&Control::Pong { nonce, sent_ms });
                            reply.ack = ack;
                            reply.ack_bits = ack_bits;
                            out.push(seal(path, self.session_id, reply));
                        }
                        Control::Pong { .. } | Control::Welcome { .. } => {}
                        Control::Close { .. } => {}
                    },
                }
            }
            PacketKind::Reject => panic!("client rejected a path"),
        }
        out
    }

    fn queue(&mut self, frame: AppFrame, now: u64) {
        self.session
            .as_mut()
            .expect("session attached")
            .send(frame, now)
            .expect("queue frame");
    }
}

fn handshake_packet(session_id: [u8; SESSION_ID_LEN], body: Vec<u8>) -> Vec<u8> {
    Packet {
        kind: PacketKind::Handshake,
        session_id,
        body,
    }
    .encode()
}

fn seal(path: &mut ServerPath, session_id: [u8; SESSION_ID_LEN], record: Record) -> Vec<u8> {
    let plaintext = record.encode_plaintext().expect("encode record");
    let ciphertext = path.seal(&plaintext).expect("seal record");
    Packet {
        kind: PacketKind::Record,
        session_id,
        body: ciphertext,
    }
    .encode()
}

fn seal_outgoing(
    path: &mut ServerPath,
    session_id: [u8; SESSION_ID_LEN],
    outgoing: h4rn_transport::session::Outgoing,
) -> Vec<u8> {
    let record = Record {
        kind: RecordKind::Data,
        binary: outgoing.binary,
        msg_id: outgoing.msg_id,
        frag_index: outgoing.frag_index,
        frag_last: outgoing.frag_last,
        seq: outgoing.seq,
        ack: 0,
        ack_bits: 0,
        payload: outgoing.payload.to_vec(),
    };
    seal(path, session_id, record)
}

struct Harness {
    engine: TransportEngine,
    peer: Peer,
    paths: Vec<PathId>,
    pending: VecDeque<(u64, PathId, Vec<u8>)>,
    delays: HashMap<PathId, u64>,
    frames: Vec<AppFrame>,
    now: u64,
}

impl Harness {
    fn start(config: EngineConfig, kinds: &[TransportPathKind], peer: Peer) -> Self {
        let peer = peer;
        let mut engine = TransportEngine::new(config).expect("engine");
        let paths = kinds
            .iter()
            .enumerate()
            .map(|(index, kind)| {
                engine.add_path(PathSpec {
                    kind: *kind,
                    endpoint: format!("path-{index}"),
                })
            })
            .collect();
        Self {
            engine,
            peer,
            paths,
            pending: VecDeque::new(),
            delays: HashMap::new(),
            frames: Vec::new(),
            now: 0,
        }
    }

    fn establish(&mut self) {
        for path in self.paths.clone() {
            let effects = self.engine.on_path_open(path, self.now).expect("open path");
            self.apply(effects, self.now);
        }
        let mut guard = 0;
        while !self.engine.is_established() {
            self.now += 5;
            let effects = self.engine.tick(self.now);
            self.apply(effects, self.now);
            guard += 1;
            assert!(guard < 200, "session did not establish");
        }
    }

    fn apply(&mut self, effects: Vec<Effect>, now: u64) {
        for effect in effects {
            match effect {
                Effect::Send { path, packet } => {
                    let at = now + self.delays.get(&path).copied().unwrap_or(0);
                    self.pending.push_back((at, path, packet));
                }
                Effect::Frame(frame) => self.frames.push(frame),
                Effect::Established(_) | Effect::Status(_) => {}
                Effect::Closed { reason } => panic!("session closed unexpectedly: {reason}"),
            }
        }
        self.pump(now);
    }

    fn pump(&mut self, limit: u64) {
        loop {
            let due = self
                .pending
                .iter()
                .enumerate()
                .filter(|(_, (at, _, _))| *at <= limit)
                .min_by_key(|(_, (at, _, _))| *at)
                .map(|(index, _)| index);
            let Some(index) = due else { break };
            let (at, path, packet) = self.pending.remove(index).expect("due packet");
            let replies = self.peer.feed(path, &packet, at);
            for reply in replies {
                // A late packet can arrive after the path already timed out.
                // The production supervisor treats that as a path-local error,
                // so the harness does the same instead of aborting the test.
                if let Ok(effects) = self.engine.on_packet(path, &reply, at) {
                    self.apply(effects, at);
                }
            }
        }
    }

    fn send(&mut self, text: &str) {
        let effects = self
            .engine
            .send(AppFrame::text(text), self.now)
            .expect("send frame");
        self.apply(effects, self.now);
    }

    fn tick(&mut self, now: u64) {
        self.now = now;
        let effects = self.engine.tick(now);
        self.apply(effects, now);
    }

    fn advance(&mut self, until: u64) {
        while self.now < until {
            let next = (self.now + 25).min(until);
            self.tick(next);
        }
    }

    fn active(&self) -> PathId {
        self.engine.active_path().expect("active path")
    }

    fn texts(&self) -> Vec<String> {
        self.frames
            .iter()
            .map(|frame| frame.as_text().expect("text frame").to_owned())
            .collect()
    }

    fn peer_texts(&self) -> Vec<String> {
        self.peer
            .received
            .iter()
            .map(|frame| frame.as_text().expect("text frame").to_owned())
            .collect()
    }

    fn into_peer(self) -> Peer {
        self.peer
    }
}

fn identities() -> (DeviceIdentity, DeviceIdentity) {
    (
        DeviceIdentity::generate().expect("client identity"),
        DeviceIdentity::generate().expect("daemon identity"),
    )
}

fn config(
    client: &DeviceIdentity,
    daemon: &DeviceIdentity,
    resume: Option<h4rn_transport::session::ResumeTicket>,
) -> EngineConfig {
    let mut config =
        EngineConfig::new(CLIENT_SESSION, client.clone(), daemon.public().noise_public);
    config.resume = resume;
    config
}

#[test]
fn reordered_and_duplicated_records_deliver_once_in_order() {
    use h4rn_transport::session::{Incoming, IncomingKind, SessionLayer};

    // A Noise path is an ordered stream (TCP or a relay stream), so the
    // reordering and duplication that matters is at the session layer: a
    // retransmitted record carries the same sequence number but a fresh
    // ciphertext. Drive the public session API directly.
    let mut sender = SessionLayer::new();
    for text in ["one", "two", "three"] {
        sender.enqueue(AppFrame::text(text), 0).expect("enqueue");
    }
    let outgoing = sender.sendable(0, 250, true);
    assert_eq!(outgoing.len(), 3);

    let mut receiver = SessionLayer::new();
    for index in [2usize, 0, 1, 0, 1] {
        let record = &outgoing[index];
        receiver
            .on_record(
                Incoming {
                    kind: IncomingKind::Data,
                    binary: record.binary,
                    msg_id: record.msg_id,
                    frag_index: record.frag_index,
                    frag_last: record.frag_last,
                    seq: record.seq,
                    ack: 0,
                    ack_bits: 0,
                    payload: record.payload.to_vec(),
                },
                0,
            )
            .expect("receive record");
    }

    let delivered: Vec<String> = receiver
        .take_deliveries()
        .iter()
        .map(|frame| frame.as_text().expect("text frame").to_owned())
        .collect();
    assert_eq!(delivered, vec!["one", "two", "three"]);
}

#[test]
fn failover_moves_unacknowledged_frames_without_loss_or_duplication() {
    let (client, daemon) = identities();
    let mut peer = Peer::new(daemon.clone(), client.public().noise_public);
    peer.ack_data = false;
    let mut harness = Harness::start(
        config(&client, &daemon, None),
        &[TransportPathKind::Direct, TransportPathKind::Relay],
        peer,
    );
    harness.establish();
    let direct = harness.active();

    // Everything the client sends on the direct path is lost: the peer never
    // sees it and never acknowledges it.
    harness.delays.insert(direct, 5_000);
    for index in 0..20 {
        harness.send(&format!("frame-{index}"));
    }
    assert!(harness.peer_texts().is_empty());

    // Keep the direct path silent until the engine declares it dead.
    let failure_at = harness.now;
    let mut guard = 0;
    while harness.active() == direct {
        harness.now += 25;
        harness.tick(harness.now);
        guard += 1;
        assert!(guard < 200, "engine never failed over");
    }
    let failover_ms = harness.now - failure_at;
    assert!(
        failover_ms < 2_000,
        "failover took {failover_ms} ms, expected under 2 s"
    );
    assert_eq!(
        harness.engine.path_state(direct),
        Some(h4rn_protocol::TransportPathState::Failed)
    );

    // Deliver the retransmissions on the relay path and check the peer saw each
    // application frame exactly once, in order.
    harness.advance(harness.now + 1_000);
    let expected: Vec<String> = (0..20).map(|index| format!("frame-{index}")).collect();
    assert_eq!(harness.peer_texts(), expected);
}

#[test]
fn switching_back_waits_for_a_sustained_rtt_margin() {
    let (client, daemon) = identities();
    let peer = Peer::new(daemon.clone(), client.public().noise_public);
    // Two Tailscale paths share a rank, so only RTT decides between them.
    let mut harness = Harness::start(
        config(&client, &daemon, None),
        &[TransportPathKind::Tailscale, TransportPathKind::Tailscale],
        peer,
    );
    harness.establish();
    // Let both paths complete a probe round so each has a baseline RTT.
    harness.advance(500);
    let first = harness.active();
    let second = *harness.paths.iter().find(|id| **id != first).unwrap();
    assert!(harness.engine.path_rtt(first).is_some());
    assert!(harness.engine.path_rtt(second).is_some());

    // Make the active path dramatically slower and the standby faster. The
    // margin is large enough that the first slow sample already wins, so the
    // test can count the confirmation ticks exactly.
    harness.delays.insert(first, 700);
    harness.delays.insert(second, 20);

    // Advance until the slow path's next probe lands and its RTT is known.
    let mut guard = 0;
    while harness.engine.path_rtt(first).unwrap_or(0) < 150 {
        harness.now += 25;
        harness.tick(harness.now);
        guard += 1;
        assert!(guard < 200, "slow path RTT was never measured");
    }
    assert!(harness.engine.path_rtt(second).unwrap_or(u32::MAX) <= 40);
    assert_eq!(harness.engine.active_path(), Some(first));

    // One evaluation only records the candidate; the second performs the switch.
    harness.tick(harness.now + 25);
    assert_eq!(
        harness.engine.active_path(),
        Some(first),
        "a single better sample must not flap the path"
    );
    harness.tick(harness.now + 25);
    assert_eq!(harness.engine.active_path(), Some(second));
}

#[test]
fn resume_reconnects_without_replaying_delivered_frames() {
    let (client, daemon) = identities();
    let peer = Peer::new(daemon.clone(), client.public().noise_public);
    let mut harness = Harness::start(
        config(&client, &daemon, None),
        &[TransportPathKind::Direct],
        peer,
    );
    harness.establish();
    harness.send("before-disconnect");
    assert_eq!(harness.peer_texts(), vec!["before-disconnect"]);
    let ticket = harness.engine.ticket().expect("resume ticket");

    // The daemon continues the session while the client is away.
    let mut peer = harness.into_peer();
    peer.queue(AppFrame::text("while-away"), 100);
    peer.forget_paths();

    let mut resumed = Harness::start(
        config(&client, &daemon, Some(ticket)),
        &[TransportPathKind::Tailscale],
        peer,
    );
    resumed.now = 100;
    resumed.establish();
    resumed.advance(250);

    assert_eq!(resumed.texts(), vec!["while-away"]);
    assert_eq!(
        resumed.peer_texts(),
        vec!["before-disconnect"],
        "a delivered frame must not be replayed after resume"
    );
    assert_eq!(resumed.engine.session_id(), &CLIENT_SESSION);
}
