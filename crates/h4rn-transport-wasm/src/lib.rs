//! wasm-bindgen wrapper around [`h4rn_transport`].
//!
//! The JS shell owns sockets and timers; this crate owns every cryptographic
//! operation and the whole session state machine. Nothing in TypeScript touches
//! key material (ADR-001, ADR-010).
//!
//! The JS glue creates one WebSocket per candidate path, then drives this
//! object:
//!
//! ```text
//! connect(config)        build the engine and register paths
//! start(now)             begin each path's Noise handshake
//! pathOpen(index, now)   the socket opened
//! receive(index, bytes)  one binary frame from the socket
//! tick(now)              probes, retransmits and path selection
//! send(bytes, binary)    queue an application frame
//! close(now)             close the session
//! ```
//!
//! Outgoing packets and application events are delivered through the
//! callbacks registered with `onPacket`, `onFrame`, `onStatus`, `onTicket` and
//! `onClosed`.

#![deny(missing_docs)]

use h4rn_protocol::TransportPathKind;
use h4rn_transport::PairingPayload;
use h4rn_transport::engine::{Effect, EngineConfig, PathId, PathSpec, TransportEngine};
use h4rn_transport::keys::{DeviceIdentity, KEY_LEN};
use h4rn_transport::session::{AppFrame, ResumeTicket};
use h4rn_transport::wire::{SESSION_ID_LEN, TOKEN_LEN};
use serde::Deserialize;
use wasm_bindgen::prelude::*;

/// Transport configuration passed from JS as JSON.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    /// Logical session id, 16 bytes of hex.
    session_id: String,
    /// Encoded device identity from `DeviceIdentity::encode`.
    identity: String,
    /// Pinned daemon X25519 public key, 32 bytes of hex.
    daemon_public: String,
    /// Optional one-time pairing PSK, 32 bytes of hex.
    #[serde(default)]
    psk: Option<String>,
    /// Optional resume token, 32 bytes of hex.
    #[serde(default)]
    resume_token: Option<String>,
    /// Candidate paths to probe.
    paths: Vec<PathConfig>,
}

#[derive(Deserialize)]
struct PathConfig {
    /// `direct`, `tailscale` or `relay`.
    kind: String,
    /// WebSocket URL shown in the status bar.
    endpoint: String,
}

/// One application-visible event produced by the engine.
enum CoreEvent {
    Frame(AppFrame),
    Status(h4rn_protocol::TransportStatus),
    Ticket(ResumeTicket),
    Closed(String),
}

/// The socket-free transport core. Kept separate from the wasm facade so the
/// same handshake can be exercised by native tests and by wasm-bindgen-test.
struct Core {
    engine: TransportEngine,
    paths: Vec<PathId>,
    pending: Vec<(usize, Vec<u8>)>,
    events: Vec<CoreEvent>,
}

impl Core {
    fn new(config_json: &str) -> Result<Self, String> {
        let config: Config = serde_json::from_str(config_json)
            .map_err(|error| format!("invalid config: {error}"))?;
        let identity = DeviceIdentity::decode(&config.identity)
            .map_err(|error| format!("invalid identity: {error}"))?;
        let daemon_public = parse_key(&config.daemon_public)?;
        let mut engine_config =
            EngineConfig::new(parse_session(&config.session_id)?, identity, daemon_public);
        engine_config.psk = config.psk.as_deref().map(parse_token).transpose()?;
        engine_config.resume = match config.resume_token.as_deref() {
            Some(token) => Some(ResumeTicket {
                session_id: engine_config.session_id,
                token: parse_token(token)?,
            }),
            None => None,
        };
        if config.paths.is_empty() {
            return Err("at least one path is required".into());
        }
        let mut engine = TransportEngine::new(engine_config).map_err(|error| error.to_string())?;
        let paths = config
            .paths
            .into_iter()
            .map(|path| {
                let kind = match path.kind.as_str() {
                    "direct" => TransportPathKind::Direct,
                    "tailscale" => TransportPathKind::Tailscale,
                    "relay" => TransportPathKind::Relay,
                    other => return Err(format!("unknown path kind: {other}")),
                };
                Ok(engine.add_path(PathSpec {
                    kind,
                    endpoint: path.endpoint,
                }))
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self {
            engine,
            paths,
            pending: Vec::new(),
            events: Vec::new(),
        })
    }

    fn index_of(&self, path: PathId) -> Result<usize, String> {
        self.paths
            .iter()
            .position(|id| *id == path)
            .ok_or_else(|| format!("unknown path {path}"))
    }

    fn apply(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Send { path, packet } => {
                    if let Ok(index) = self.index_of(path) {
                        self.pending.push((index, packet));
                    }
                }
                Effect::Frame(frame) => self.events.push(CoreEvent::Frame(frame)),
                Effect::Status(status) => self.events.push(CoreEvent::Status(status)),
                Effect::Established(ticket) => self.events.push(CoreEvent::Ticket(ticket)),
                Effect::Closed { reason } => self.events.push(CoreEvent::Closed(reason)),
            }
        }
    }

    fn start(&mut self, now: u64) -> Result<(), String> {
        for path in self.paths.clone() {
            let effects = self
                .engine
                .on_path_open(path, now)
                .map_err(|error| error.to_string())?;
            self.apply(effects);
        }
        self.events.push(CoreEvent::Status(self.engine.status()));
        Ok(())
    }

    fn open(&mut self, index: usize, now: u64) -> Result<(), String> {
        let path = self.path(index)?;
        let effects = self
            .engine
            .on_path_open(path, now)
            .map_err(|error| error.to_string())?;
        self.apply(effects);
        Ok(())
    }

    fn closed(&mut self, index: usize, reason: &str, now: u64) -> Result<(), String> {
        let path = self.path(index)?;
        let effects = self.engine.on_path_closed(path, now);
        if !reason.is_empty() {
            self.events.push(CoreEvent::Closed(reason.to_owned()));
        }
        self.apply(effects);
        Ok(())
    }

    fn receive(&mut self, index: usize, bytes: &[u8], now: u64) -> Result<(), String> {
        let path = self.path(index)?;
        let effects = self
            .engine
            .on_packet(path, bytes, now)
            .map_err(|error| error.to_string())?;
        self.apply(effects);
        Ok(())
    }

    fn send(&mut self, bytes: &[u8], binary: bool, now: u64) -> Result<(), String> {
        let frame = if binary {
            AppFrame::binary(bytes.to_vec())
        } else {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| "text frames must be valid UTF-8".to_owned())?;
            AppFrame::text(text)
        };
        let effects = self
            .engine
            .send(frame, now)
            .map_err(|error| error.to_string())?;
        self.apply(effects);
        Ok(())
    }

    fn tick(&mut self, now: u64) {
        let effects = self.engine.tick(now);
        self.apply(effects);
    }

    fn close(&mut self, now: u64) {
        let effects = self.engine.close(now);
        self.apply(effects);
    }

    fn path(&self, index: usize) -> Result<PathId, String> {
        self.paths
            .get(index)
            .copied()
            .ok_or_else(|| format!("unknown path index {index}"))
    }
}

fn parse_key(value: &str) -> Result<[u8; KEY_LEN], String> {
    let bytes = hex::decode(value.trim()).map_err(|error| format!("invalid hex: {error}"))?;
    if bytes.len() != KEY_LEN {
        return Err(format!("expected {KEY_LEN} bytes, got {}", bytes.len()));
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&bytes);
    Ok(key)
}

fn parse_token(value: &str) -> Result<[u8; TOKEN_LEN], String> {
    parse_key(value)
}

fn parse_session(value: &str) -> Result<[u8; SESSION_ID_LEN], String> {
    let bytes = hex::decode(value.trim()).map_err(|error| format!("invalid hex: {error}"))?;
    if bytes.len() != SESSION_ID_LEN {
        return Err(format!(
            "expected {SESSION_ID_LEN} session bytes, got {}",
            bytes.len()
        ));
    }
    let mut session = [0u8; SESSION_ID_LEN];
    session.copy_from_slice(&bytes);
    if session == [0u8; SESSION_ID_LEN] {
        getrandom::fill(&mut session).map_err(|_| "no randomness".to_owned())?;
    }
    Ok(session)
}

type PacketCallback = js_sys::Function;
type FrameCallback = js_sys::Function;
type StatusCallback = js_sys::Function;
type TicketCallback = js_sys::Function;
type ClosedCallback = js_sys::Function;

/// The wasm-facing transport. Construct with [`connect`], then register
/// callbacks and drive the socket events from JS.
#[wasm_bindgen]
pub struct SecureTransport {
    core: Core,
    on_packet: Option<PacketCallback>,
    on_frame: Option<FrameCallback>,
    on_status: Option<StatusCallback>,
    on_ticket: Option<TicketCallback>,
    on_closed: Option<ClosedCallback>,
}

/// Returns the transport protocol version.
#[wasm_bindgen]
pub fn transport_version() -> u32 {
    u32::from(h4rn_transport::TRANSPORT_VERSION)
}

/// Generates a fresh device identity, encoded for persistence by JS.
#[wasm_bindgen]
pub fn generate_identity() -> Result<String, JsError> {
    let identity = DeviceIdentity::generate().map_err(js_error)?;
    Ok(identity.encode())
}

/// Parses a pairing URI into JSON (`endpoint`, `daemonPublic`, `psk`, `label`).
#[wasm_bindgen]
pub fn parse_pairing(uri: &str) -> Result<String, JsError> {
    let payload = PairingPayload::decode(uri).map_err(js_error)?;
    serde_json::to_string(&serde_json::json!({
        "endpoint": payload.endpoint,
        "daemonPublic": hex::encode(payload.daemon_public),
        "psk": hex::encode(payload.psk),
        "label": payload.label,
    }))
    .map_err(|error| JsError::new(&error.to_string()))
}

/// Builds a transport from a JSON config.
///
/// The config carries `sessionId`, `identity`, `daemonPublic`, optional `psk`
/// and `resumeToken`, and a non-empty `paths` array of `{kind, endpoint}`.
#[wasm_bindgen]
pub fn connect(config_json: &str) -> Result<SecureTransport, JsError> {
    Ok(SecureTransport {
        core: Core::new(config_json).map_err(|error| JsError::new(&error))?,
        on_packet: None,
        on_frame: None,
        on_status: None,
        on_ticket: None,
        on_closed: None,
    })
}

#[wasm_bindgen]
impl SecureTransport {
    /// Receives `(pathIndex, Uint8Array)` for every packet to write to a socket.
    pub fn on_packet(&mut self, callback: js_sys::Function) {
        self.on_packet = Some(callback);
    }

    /// Receives one application frame: a string for JSON, `Uint8Array` for binary.
    pub fn on_frame(&mut self, callback: js_sys::Function) {
        self.on_frame = Some(callback);
    }

    /// Receives a `TransportStatus` JSON snapshot.
    pub fn on_status(&mut self, callback: js_sys::Function) {
        self.on_status = Some(callback);
    }

    /// Receives the resume token as hex after the session is established.
    pub fn on_ticket(&mut self, callback: js_sys::Function) {
        self.on_ticket = Some(callback);
    }

    /// Receives a final close reason.
    pub fn on_closed(&mut self, callback: js_sys::Function) {
        self.on_closed = Some(callback);
    }

    /// Starts every registered path's handshake.
    pub fn start(&mut self, now_ms: f64) -> Result<(), JsError> {
        self.core.start(now_ms as u64).map_err(js_error)?;
        self.flush();
        Ok(())
    }

    /// Marks a socket as open and starts its handshake.
    pub fn path_open(&mut self, index: u32, now_ms: f64) -> Result<(), JsError> {
        self.core
            .open(index as usize, now_ms as u64)
            .map_err(js_error)?;
        self.flush();
        Ok(())
    }

    /// Marks a socket as closed.
    pub fn path_closed(&mut self, index: u32, reason: &str, now_ms: f64) -> Result<(), JsError> {
        self.core
            .closed(index as usize, reason, now_ms as u64)
            .map_err(js_error)?;
        self.flush();
        Ok(())
    }

    /// Feeds one binary packet received on a socket.
    pub fn receive(&mut self, index: u32, bytes: &[u8], now_ms: f64) -> Result<(), JsError> {
        self.core
            .receive(index as usize, bytes, now_ms as u64)
            .map_err(js_error)?;
        self.flush();
        Ok(())
    }

    /// Queues one application frame.
    pub fn send(&mut self, bytes: &[u8], binary: bool, now_ms: f64) -> Result<(), JsError> {
        self.core
            .send(bytes, binary, now_ms as u64)
            .map_err(js_error)?;
        self.flush();
        Ok(())
    }

    /// Runs probes, retransmits and path selection.
    pub fn tick(&mut self, now_ms: f64) {
        self.core.tick(now_ms as u64);
        self.flush();
    }

    /// Closes the session.
    pub fn close(&mut self, now_ms: f64) {
        self.core.close(now_ms as u64);
        self.flush();
    }

    /// The current resume token, when the daemon has issued one.
    pub fn resume_token_hex(&self) -> String {
        self.core
            .engine
            .ticket()
            .map(|ticket| hex::encode(ticket.token))
            .unwrap_or_default()
    }

    /// The current status as JSON.
    pub fn status_json(&self) -> String {
        serde_json::to_string(&self.core.engine.status()).unwrap_or_else(|_| "{}".into())
    }

    #[allow(clippy::needless_pass_by_value)]
    fn flush(&mut self) {
        let pending = std::mem::take(&mut self.core.pending);
        for (index, packet) in pending {
            if let Some(callback) = &self.on_packet {
                let bytes = js_sys::Uint8Array::from(packet.as_slice());
                let _ = callback.call2(&JsValue::NULL, &JsValue::from(index as u32), &bytes);
            }
        }
        let events = std::mem::take(&mut self.core.events);
        for event in events {
            match event {
                CoreEvent::Frame(frame) => {
                    if let Some(callback) = &self.on_frame {
                        let value = if frame.binary {
                            js_sys::Uint8Array::from(frame.payload.as_slice()).into()
                        } else {
                            JsValue::from(String::from_utf8_lossy(&frame.payload).into_owned())
                        };
                        let _ = callback.call1(&JsValue::NULL, &value);
                    }
                }
                CoreEvent::Status(status) => {
                    if let Some(callback) = &self.on_status {
                        let json = serde_json::to_string(&status).unwrap_or_default();
                        let _ = callback.call1(&JsValue::NULL, &JsValue::from(json));
                    }
                }
                CoreEvent::Ticket(ticket) => {
                    if let Some(callback) = &self.on_ticket {
                        let _ = callback
                            .call1(&JsValue::NULL, &JsValue::from(hex::encode(ticket.token)));
                    }
                }
                CoreEvent::Closed(reason) => {
                    if let Some(callback) = &self.on_closed {
                        let _ = callback.call1(&JsValue::NULL, &JsValue::from(reason));
                    }
                }
            }
        }
    }
}

fn js_error(error: impl std::fmt::Display) -> JsError {
    JsError::new(&error.to_string())
}

/// Test-only helpers shared by native and wasm smoke tests.
#[cfg(test)]
mod tests {
    use super::*;
    use h4rn_transport::noise::RekeyLimits;
    use h4rn_transport::server_session::{
        PairingPolicy, ServerHandshakeStep, ServerPath, ServerSession,
    };
    use h4rn_transport::wire::{Control, Packet, PacketKind, Record, RecordKind};

    struct Peer {
        policy: PairingPolicy,
        session: ServerSession,
        path: ServerPath,
    }

    impl Peer {
        fn new(identity: DeviceIdentity, client: [u8; KEY_LEN]) -> Self {
            Self {
                policy: PairingPolicy::new([client]),
                session: ServerSession::with_token([9u8; SESSION_ID_LEN], [3u8; TOKEN_LEN], 0),
                path: ServerPath::new(
                    [9u8; SESSION_ID_LEN],
                    identity,
                    None,
                    RekeyLimits::default(),
                ),
            }
        }

        fn feed(&mut self, bytes: &[u8], now: u64) -> Vec<Vec<u8>> {
            let packet = Packet::decode(bytes).expect("packet");
            let mut out = Vec::new();
            match packet.kind {
                PacketKind::Handshake => {
                    let step = self.path.on_handshake(&packet.body, &self.policy).unwrap();
                    let reply = match step {
                        ServerHandshakeStep::Complete { reply, .. } => reply,
                        ServerHandshakeStep::Reply(reply) => Some(reply),
                        other => panic!("unexpected handshake step: {other:?}"),
                    };
                    if let Some(reply) = reply {
                        out.push(
                            Packet {
                                kind: PacketKind::Handshake,
                                session_id: packet.session_id,
                                body: reply,
                            }
                            .encode(),
                        );
                    }
                }
                PacketKind::Record => {
                    let plaintext = self.path.open(&packet.body).unwrap();
                    let record = Record::decode_plaintext(&plaintext).unwrap();
                    if record.kind == RecordKind::Control {
                        match Control::decode(&record.payload).unwrap() {
                            Control::Open {
                                recv_next,
                                send_next,
                                ..
                            } => {
                                let welcome =
                                    self.session.attach(1, recv_next, send_next, now).unwrap();
                                out.push(self.seal(Record::control(&welcome)));
                            }
                            Control::Ping { nonce, sent_ms } => {
                                out.push(
                                    self.seal(Record::control(&Control::Pong { nonce, sent_ms })),
                                );
                            }
                            _ => {}
                        }
                    }
                }
                PacketKind::Reject => panic!("rejected"),
            }
            out
        }

        fn seal(&mut self, record: Record) -> Vec<u8> {
            let plaintext = record.encode_plaintext().unwrap();
            let ciphertext = self.path.seal(&plaintext).unwrap();
            Packet {
                kind: PacketKind::Record,
                session_id: [9u8; SESSION_ID_LEN],
                body: ciphertext,
            }
            .encode()
        }
    }

    fn config(identity: &DeviceIdentity, daemon: &DeviceIdentity) -> String {
        serde_json::json!({
            "sessionId": hex::encode([9u8; SESSION_ID_LEN]),
            "identity": identity.encode(),
            "daemonPublic": hex::encode(daemon.public().noise_public),
            "paths": [{"kind": "direct", "endpoint": "ws://localhost/secure"}],
        })
        .to_string()
    }

    /// Drives a real Noise handshake and asserts the session is established.
    /// This is the wasm smoke test: it runs natively and, with
    /// `wasm-pack test --node`, on `wasm32-unknown-unknown`.
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn handshake_smoke() {
        let identity = DeviceIdentity::generate().unwrap();
        let daemon = DeviceIdentity::generate().unwrap();
        let mut core = Core::new(&config(&identity, &daemon)).unwrap();
        let mut peer = Peer::new(daemon, identity.public().noise_public);

        core.start(0).unwrap();
        let mut now = 0u64;
        for _ in 0..20 {
            let pending = std::mem::take(&mut core.pending);
            for (_, packet) in pending {
                for reply in peer.feed(&packet, now) {
                    core.receive(0, &reply, now).unwrap();
                }
            }
            if core.engine.is_established() {
                break;
            }
            now += 25;
            core.tick(now);
        }
        assert!(core.engine.is_established(), "handshake did not complete");
        assert!(
            core.events
                .iter()
                .any(|event| matches!(event, CoreEvent::Ticket(_)))
        );
    }
}
