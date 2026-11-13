//! The native asynchronous client adapter.
//!
//! [`TransportEngine`] is a synchronous state machine; this module wires it to
//! real sockets. One task supervises the engine and one task owns each path, so
//! a stalled or closed socket can never block the others.
//!
//! The supervisor is deliberately thin: every protocol decision (handshake,
//! retransmit, path choice, hysteresis) lives in [`TransportEngine`]. That is
//! what lets the same logic run in the browser through the wasm wrapper.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use h4rn_protocol::{TransportPathKind, TransportStatus};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::engine::{Effect, EngineConfig, EngineError, PathId, PathSpec, TransportEngine};
use crate::relay::{RelayConnector, RelayError, RelayFrame, RelayRequest, RelayRole};
use crate::session::{AppFrame, ResumeTicket};
use crate::wire::{SessionId, TOKEN_LEN};

/// How a candidate path is opened.
pub enum PathLink {
    /// A WebSocket URL, `ws://` on loopback or `wss:///secure` remotely.
    WebSocket {
        /// Destination URL.
        url: String,
    },
    /// A connection through an h4rn-connect relay.
    Relay {
        /// Connector that owns the relay's wire protocol.
        connector: Arc<dyn RelayConnector>,
        /// Resume token the relay may require for the daemon side.
        token: Option<[u8; TOKEN_LEN]>,
    },
}

/// One candidate path to probe in parallel with the others.
pub struct ClientPath {
    /// Physical kind, which drives preference.
    pub kind: TransportPathKind,
    /// Endpoint shown in the status bar.
    pub endpoint: String,
    /// How to open the socket.
    pub link: PathLink,
}

impl ClientPath {
    /// A direct or Tailscale WebSocket path.
    pub fn websocket(kind: TransportPathKind, url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            kind,
            endpoint: url.clone(),
            link: PathLink::WebSocket { url },
        }
    }

    /// A relay path through `connector`.
    pub fn relay(
        connector: Arc<dyn RelayConnector>,
        endpoint: impl Into<String>,
        token: Option<[u8; TOKEN_LEN]>,
    ) -> Self {
        let endpoint = endpoint.into();
        Self {
            kind: TransportPathKind::Relay,
            endpoint,
            link: PathLink::Relay { connector, token },
        }
    }
}

/// Client configuration: one engine plus every candidate path.
pub struct ClientConfig {
    /// Engine tuning, identity, session id and pinned daemon key.
    pub engine: EngineConfig,
    /// Paths to probe in parallel.
    pub paths: Vec<ClientPath>,
    /// How often the engine ticks (probes, retransmits and selection).
    pub tick: Duration,
}

impl ClientConfig {
    /// A config with a 25 ms tick, fast enough for sub-second failover.
    pub fn new(engine: EngineConfig, paths: Vec<ClientPath>) -> Self {
        Self {
            engine,
            paths,
            tick: Duration::from_millis(25),
        }
    }
}

/// Something the application should react to.
#[derive(Clone, Debug)]
pub enum ClientEvent {
    /// A protocol frame arrived and was reassembled.
    Frame(AppFrame),
    /// Transport state changed; surface it to the UI.
    Status(TransportStatus),
    /// The session is established; persist the ticket for a reconnect.
    Established(ResumeTicket),
    /// The session ended and will not recover on its own.
    Closed {
        /// Human-readable reason.
        reason: String,
    },
    /// A path failed or a command was rejected; the session may still be up.
    Error {
        /// Path that failed, when the failure is path-local.
        path: Option<PathId>,
        /// Human-readable reason.
        message: String,
    },
}

/// A running native client.
pub struct Client {
    commands: mpsc::Sender<ClientCommand>,
    events: mpsc::Receiver<ClientEvent>,
    join: JoinHandle<()>,
    session_id: SessionId,
}

impl Client {
    /// Starts the client and returns immediately.
    ///
    /// Progress is reported through [`Client::recv`]: the first
    /// [`ClientEvent::Established`] means the session is usable.
    pub fn connect(config: ClientConfig) -> Result<Self, ClientError> {
        let engine = TransportEngine::new(config.engine)?;
        let session_id = *engine.session_id();
        let (commands_tx, commands_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(256);
        let join = tokio::spawn(supervise(
            engine,
            config.paths,
            commands_rx,
            events_tx,
            config.tick,
        ));
        Ok(Self {
            commands: commands_tx,
            events: events_rx,
            join,
            session_id,
        })
    }

    /// Logical session id.
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Logical session id as hex.
    pub fn session_id_hex(&self) -> String {
        hex::encode(self.session_id)
    }

    /// Queues one application frame.
    pub async fn send(&self, frame: AppFrame) -> Result<(), ClientError> {
        self.commands
            .send(ClientCommand::Send(frame))
            .await
            .map_err(|_| ClientError::Closed)
    }

    /// Waits for the next event, or `None` once the supervisor stopped.
    pub async fn recv(&mut self) -> Option<ClientEvent> {
        self.events.recv().await
    }

    /// Returns the next event without waiting.
    pub fn try_recv(&mut self) -> Result<ClientEvent, mpsc::error::TryRecvError> {
        self.events.try_recv()
    }

    /// Closes the session and waits for the supervisor to finish.
    pub async fn close(mut self) {
        let _ = self.commands.send(ClientCommand::Close).await;
        let _ = (&mut self.join).await;
    }
}

/// Client startup and lifecycle failures.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The engine rejected its configuration.
    #[error(transparent)]
    Engine(#[from] EngineError),
    /// A relay connector failed.
    #[error(transparent)]
    Relay(#[from] RelayError),
    /// The supervisor already stopped.
    #[error("the transport client is closed")]
    Closed,
}

enum ClientCommand {
    Send(AppFrame),
    Close,
}

enum PathCommand {
    Send(Vec<u8>),
    Close,
}

enum PathEvent {
    Opened(PathId),
    Message(PathId, Vec<u8>),
    Closed(PathId, String),
}

async fn supervise(
    mut engine: TransportEngine,
    paths: Vec<ClientPath>,
    mut commands: mpsc::Receiver<ClientCommand>,
    events: mpsc::Sender<ClientEvent>,
    tick: Duration,
) {
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    let (path_tx, mut path_rx) = mpsc::channel(256);
    let mut senders: BTreeMap<PathId, mpsc::Sender<PathCommand>> = BTreeMap::new();
    let mut tasks: Vec<JoinHandle<()>> = Vec::new();

    for path in paths {
        let id = engine.add_path(PathSpec {
            kind: path.kind,
            endpoint: path.endpoint.clone(),
        });
        let (tx, rx) = mpsc::channel(64);
        senders.insert(id, tx);
        tasks.push(tokio::spawn(run_path(
            id,
            *engine.session_id(),
            path,
            rx,
            path_tx.clone(),
        )));
    }
    drop(path_tx);

    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut open = true;

    if !emit_status(&mut engine, &events).await {
        open = false;
    }

    while open {
        tokio::select! {
            event = path_rx.recv() => {
                let Some(event) = event else { break; };
                let now = now();
                let result = match event {
                    PathEvent::Opened(id) => engine.on_path_open(id, now),
                    PathEvent::Message(id, bytes) => engine.on_packet(id, &bytes, now),
                    PathEvent::Closed(id, reason) => {
                        if !reason.is_empty() {
                            let _ = events.try_send(ClientEvent::Error {
                                path: Some(id),
                                message: reason,
                            });
                        }
                        Ok(engine.on_path_closed(id, now))
                    }
                };
                match result {
                    Ok(effects) => open = apply(effects, &mut senders, &events).await,
                    Err(error) => {
                        let _ = events
                            .try_send(ClientEvent::Error {
                                path: None,
                                message: error.to_string(),
                            })
                            .is_ok();
                    }
                }
            }
            command = commands.recv() => {
                match command {
                    Some(ClientCommand::Send(frame)) => {
                        match engine.send(frame, now()) {
                            Ok(effects) => open = apply(effects, &mut senders, &events).await,
                            Err(error) => {
                                let _ = events.try_send(ClientEvent::Error {
                                    path: None,
                                    message: error.to_string(),
                                });
                            }
                        }
                    }
                    Some(ClientCommand::Close) | None => {
                        let effects = engine.close(now());
                        let _ = apply(effects, &mut senders, &events).await;
                        break;
                    }
                }
            }
            _ = interval.tick() => {
                let effects = engine.tick(now());
                open = apply(effects, &mut senders, &events).await;
            }
        }
    }

    for (_, sender) in senders {
        let _ = sender.send(PathCommand::Close).await;
    }
    for task in tasks {
        task.abort();
    }
    let _ = events
        .send(ClientEvent::Closed {
            reason: "transport client stopped".into(),
        })
        .await;
}

async fn emit_status(engine: &mut TransportEngine, events: &mpsc::Sender<ClientEvent>) -> bool {
    events
        .send(ClientEvent::Status(engine.status()))
        .await
        .is_ok()
}

/// Applies engine effects. Returns `false` when the session is over.
async fn apply(
    effects: Vec<Effect>,
    senders: &mut BTreeMap<PathId, mpsc::Sender<PathCommand>>,
    events: &mpsc::Sender<ClientEvent>,
) -> bool {
    let mut open = true;
    for effect in effects {
        match effect {
            Effect::Send { path, packet } => {
                if let Some(sender) = senders.get(&path)
                    && sender.send(PathCommand::Send(packet)).await.is_err()
                {
                    // The path task already exited; its `Closed` event will make
                    // the engine fail the path.
                }
            }
            Effect::Frame(frame) => {
                if events.send(ClientEvent::Frame(frame)).await.is_err() {
                    open = false;
                }
            }
            Effect::Status(status) => {
                if events.send(ClientEvent::Status(status)).await.is_err() {
                    open = false;
                }
            }
            Effect::Established(ticket) => {
                if events.send(ClientEvent::Established(ticket)).await.is_err() {
                    open = false;
                }
            }
            Effect::Closed { reason } => {
                let _ = events.send(ClientEvent::Closed { reason }).await;
                open = false;
            }
        }
    }
    open
}

async fn run_path(
    id: PathId,
    session_id: SessionId,
    path: ClientPath,
    mut commands: mpsc::Receiver<PathCommand>,
    events: mpsc::Sender<PathEvent>,
) {
    let reason = match path.link {
        PathLink::WebSocket { url } => run_websocket(id, url, &mut commands, &events).await,
        PathLink::Relay { connector, token } => {
            run_relay(
                id,
                session_id,
                connector,
                token,
                path.endpoint,
                &mut commands,
                &events,
            )
            .await
        }
    };
    let _ = events.send(PathEvent::Closed(id, reason)).await;
}

async fn run_websocket(
    id: PathId,
    url: String,
    commands: &mut mpsc::Receiver<PathCommand>,
    events: &mpsc::Sender<PathEvent>,
) -> String {
    let socket = match tokio_tungstenite::connect_async(&url).await {
        Ok((socket, _response)) => socket,
        Err(error) => return format!("WebSocket {url} failed: {error}"),
    };
    let (mut sink, mut stream) = socket.split();
    if events.send(PathEvent::Opened(id)).await.is_err() {
        return "transport stopped".into();
    }
    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(PathCommand::Send(bytes)) => {
                        if let Err(error) = sink.send(Message::Binary(bytes.into())).await {
                            return format!("WebSocket write failed: {error}");
                        }
                    }
                    Some(PathCommand::Close) | None => {
                        let _ = sink.close().await;
                        return String::new();
                    }
                }
            }
            message = stream.next() => {
                match message {
                    Some(Ok(Message::Binary(bytes))) => {
                        if events.send(PathEvent::Message(id, bytes.to_vec())).await.is_err() {
                            return "transport stopped".into();
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return "WebSocket closed".into(),
                    // The secure route only speaks binary; text is a protocol
                    // error, not a frame. Ignoring it keeps ping/pong flowing.
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return format!("WebSocket read failed: {error}"),
                }
            }
        }
    }
}

async fn run_relay(
    id: PathId,
    session_id: SessionId,
    connector: Arc<dyn RelayConnector>,
    token: Option<[u8; TOKEN_LEN]>,
    endpoint: String,
    commands: &mut mpsc::Receiver<PathCommand>,
    events: &mpsc::Sender<PathEvent>,
) -> String {
    let mut link = match connector
        .connect(RelayRequest {
            endpoint: endpoint.clone(),
            session_id,
            role: RelayRole::Client,
            token,
        })
        .await
    {
        Ok(link) => link,
        Err(error) => return format!("relay {endpoint} failed: {error}"),
    };
    if events.send(PathEvent::Opened(id)).await.is_err() {
        return "transport stopped".into();
    }
    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(PathCommand::Send(bytes)) => {
                        if let Err(error) = link
                            .send(RelayFrame::Data {
                                session_id,
                                packet: bytes,
                            })
                            .await
                        {
                            return format!("relay write failed: {error}");
                        }
                    }
                    Some(PathCommand::Close) | None => {
                        let _ = link.close().await;
                        return String::new();
                    }
                }
            }
            frame = link.recv() => {
                match frame {
                    Ok(Some(RelayFrame::Data { packet, .. })) => {
                        if events.send(PathEvent::Message(id, packet)).await.is_err() {
                            return "transport stopped".into();
                        }
                    }
                    Ok(Some(RelayFrame::Close { code, .. })) => {
                        return format!("relay closed the path ({code})");
                    }
                    Ok(Some(RelayFrame::Register { .. })) => {}
                    Ok(None) => return "relay closed the path".into(),
                    Err(error) => return format!("relay read failed: {error}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::DeviceIdentity;

    #[test]
    fn config_starts_with_a_fast_tick() {
        let daemon = DeviceIdentity::generate().unwrap();
        let client = DeviceIdentity::generate().unwrap();
        let engine = EngineConfig::new(
            [1u8; crate::wire::SESSION_ID_LEN],
            client,
            daemon.public().noise_public,
        );
        let config = ClientConfig::new(engine, Vec::new());
        assert!(config.tick <= Duration::from_millis(50));
    }
}
