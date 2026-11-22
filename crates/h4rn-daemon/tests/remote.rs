//! End-to-end remote access: device login, pairing, relay, direct failover and
//! revocation (ADR-009, ADR-010, ADR-018).
//!
//! Ignored by default. `scripts/test-remote.sh` starts Postgres and a real
//! OrbitID, exports the `H4RN_TEST_*` variables and runs this file with a single
//! test thread. The relay runs in-process on a real TCP port with the real
//! router and schema; the daemon is a real daemon with `FakeHarness`; the client
//! is the shipping multipath transport. Nothing in the Noise or relay layer is
//! mocked.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::{SinkExt as _, StreamExt as _};
use h4rn_connect::api::{build_state, router as connect_router};
use h4rn_connect::config::{Config as ConnectConfig, ProviderConfig};
use h4rn_connect::crypto::random_b64;
use h4rn_core::LifecycleSettings;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::mcp::McpService;
use h4rn_daemon::remote::{ConnectClient, TailscaleMode};
use h4rn_daemon::threads::ThreadManagerOptions;
use h4rn_harness::FakeHarness;
use h4rn_mcp::session::McpScope;
use h4rn_protocol::{
    ClientFrame, ClientRequest, ConnectDeviceKind, DeviceId, DeviceInfo, HarnessKind, HostId,
    McpEnabledMatrix, McpRequest, McpServerInput, McpSharePolicy, McpTransportInput,
    PROTOCOL_VERSION, PairingClaimRequest, ProtoError, RelayOpen, RelayService, ResponseBody,
    RuntimeMode, ServerFrame, TransportPathKind,
};
use h4rn_transport::client::{Client, ClientConfig, ClientEvent, ClientPath};
use h4rn_transport::engine::EngineConfig;
use h4rn_transport::keys::{DeviceIdentity, FileKeyStore, load_or_create};
use h4rn_transport::relay::{
    RelayConnector, RelayError, RelayFrame as WireRelayFrame, RelayRequest, RelayRole,
};
use h4rn_transport::server::{SecureOptions, SecureService};
use h4rn_transport::wire::SESSION_ID_LEN;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the remote e2e test"))
}

/// The OrbitID settings the script prepared.
struct Orbit {
    issuer: String,
    session_jwt: String,
    client_id: String,
    client_secret: String,
    redirect: String,
}

impl Orbit {
    fn load() -> Self {
        Self {
            issuer: env("H4RN_TEST_ORBITID_URL")
                .trim_end_matches('/')
                .to_string(),
            session_jwt: env("H4RN_TEST_ORBITID_SESSION_JWT"),
            client_id: env("H4RN_TEST_OIDC_CONFIDENTIAL_CLIENT_ID"),
            client_secret: env("H4RN_TEST_OIDC_CONFIDENTIAL_CLIENT_SECRET"),
            redirect: std::env::var("H4RN_TEST_OIDC_REDIRECT_URI")
                .unwrap_or_else(|_| "h4rn://callback".to_string()),
        }
    }

    /// Approves a device code with the headless user session.
    async fn approve(&self, user_code: &str) {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/client/oauth/device/approve", self.issuer))
            .bearer_auth(&self.session_jwt)
            .json(&json!({"user_code": user_code, "action": "approve"}))
            .send()
            .await
            .expect("device approval request");
        let status = response.status();
        let body: serde_json::Value = response.json().await.expect("approval JSON");
        assert!(status.is_success(), "approval failed: {body}");
    }
}

fn provider(
    id: &str,
    issuer: &str,
    client_id: &str,
    secret: &str,
    redirect: &str,
) -> ProviderConfig {
    ProviderConfig {
        id: id.to_string(),
        display_name: id.to_string(),
        issuer: issuer.to_string(),
        client_id: client_id.to_string(),
        client_secret: Some(secret.to_string()),
        scopes: vec![
            "openid".to_string(),
            "profile".to_string(),
            "email".to_string(),
            "offline_access".to_string(),
        ],
        redirect_uris: vec![redirect.to_string()],
        device_grant: true,
    }
}

struct RelayServer {
    base_url: String,
    task: JoinHandle<()>,
}

impl RelayServer {
    async fn start(orbit: &Orbit) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the in-process relay");
        let addr = listener.local_addr().expect("relay address");
        let base_url = format!("http://{addr}");
        let mut config = ConnectConfig {
            public_base_url: base_url.clone(),
            database_url: env("H4RN_TEST_DATABASE_URL"),
            kek: random_b64(32),
            signing_key: Some(random_b64(32)),
            allow_insecure: true,
            providers: vec![provider(
                "orbitid-confidential",
                &orbit.issuer,
                &orbit.client_id,
                &orbit.client_secret,
                &orbit.redirect,
            )],
            ..ConnectConfig::default()
        };
        // The e2e drives pairing and relay login in a tight loop.
        config.rate_limits.per_ip_per_minute = 10_000;
        config.rate_limits.auth_per_minute = 10_000;
        config.rate_limits.pairing_per_hour = 10_000;
        config.rate_limits.relay_connects_per_minute = 10_000;
        config.normalise().expect("normalise relay configuration");
        let state = build_state(config).await.expect("build relay state");
        let app = connect_router(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve relay");
        });
        Self { base_url, task }
    }

    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

async fn login(
    data_dir: &Path,
    identity: DeviceIdentity,
    base_url: &str,
    kind: ConnectDeviceKind,
    machine_name: Option<&str>,
    orbit: &Orbit,
) -> ConnectClient {
    let client = ConnectClient::new(data_dir, identity).expect("connect client");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let base = base_url.to_string();
    let machine_name = machine_name.map(ToOwned::to_owned);
    let task = tokio::spawn(async move {
        let result = client
            .login_as(
                &base,
                "orbitid-confidential",
                kind,
                machine_name.as_deref(),
                |summary| {
                    let _ = tx.send(summary.clone());
                },
            )
            .await;
        (client, result)
    });
    let summary = rx.recv().await.expect("a device code");
    orbit.approve(&summary.user_code).await;
    let (client, result) = task.await.expect("login task");
    result.expect("device grant login");
    client
}

/// A relay connector that speaks h4rn-connect's JSON framing for one device.
///
/// A connection attempt mints a fresh DPoP-authenticated URL: proofs are
/// single-use, so the connector cannot cache the first one across retries.
struct ConnectRelayConnector {
    client: Arc<ConnectClient>,
    machine_id: String,
    device_id: String,
    service: RelayService,
}

#[async_trait::async_trait]
impl RelayConnector for ConnectRelayConnector {
    async fn connect(
        &self,
        request: RelayRequest,
    ) -> Result<Box<dyn h4rn_transport::relay::RelayLink>, RelayError> {
        let url = self
            .client
            .relay_websocket_url()
            .await
            .map_err(|error| RelayError::Rejected(error.to_string()))?;
        let (socket, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .map_err(|error| RelayError::Unreachable(describe_handshake_error(error)))?;
        let (sink, stream) = socket.split();
        let conn_id = hex::encode(request.session_id);
        let open = RelayOpen {
            machine_id: self.machine_id.clone(),
            device_id: Some(self.device_id.clone()),
            service: self.service,
        };
        let payload = serde_json::to_vec(&open).map_err(|error| {
            RelayError::Malformed(Box::leak(error.to_string().into_boxed_str()))
        })?;
        let frame = h4rn_protocol::RelayFrame {
            conn_id: conn_id.clone(),
            kind: h4rn_protocol::RelayFrameKind::Open,
            payload,
        };
        let mut link = ConnectRelayLink {
            sink,
            stream,
            conn_id,
            session_id: request.session_id,
        };
        link.write(&frame).await?;
        Ok(Box::new(link))
    }
}

/// Keeps a rejected handshake's JSON body, which names the connect error code.
fn describe_handshake_error(error: tokio_tungstenite::tungstenite::Error) -> String {
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            let status = response.status();
            let body = response
                .body()
                .as_deref()
                .map(|bytes| String::from_utf8_lossy(bytes))
                .unwrap_or_default();
            format!("HTTP error: {status}: {body}")
        }
        other => other.to_string(),
    }
}

struct ConnectRelayLink {
    sink: futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    stream: futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    conn_id: String,
    session_id: [u8; SESSION_ID_LEN],
}

impl ConnectRelayLink {
    async fn write(&mut self, frame: &h4rn_protocol::RelayFrame) -> Result<(), RelayError> {
        let text = serde_json::to_string(frame).map_err(|error| {
            RelayError::Malformed(Box::leak(error.to_string().into_boxed_str()))
        })?;
        self.sink
            .send(Message::Text(text.into()))
            .await
            .map_err(|error| RelayError::Unreachable(error.to_string()))
    }
}

#[async_trait::async_trait]
impl h4rn_transport::relay::RelayLink for ConnectRelayLink {
    async fn send(&mut self, frame: WireRelayFrame) -> Result<(), RelayError> {
        let frame = match frame {
            WireRelayFrame::Register { .. } => return Ok(()),
            WireRelayFrame::Data { packet, .. } => {
                h4rn_protocol::RelayFrame::data(self.conn_id.clone(), packet)
            }
            WireRelayFrame::Close { .. } => h4rn_protocol::RelayFrame::close(self.conn_id.clone()),
        };
        self.write(&frame).await
    }

    async fn recv(&mut self) -> Result<Option<WireRelayFrame>, RelayError> {
        loop {
            let message = self.stream.next().await.ok_or(RelayError::Closed)?;
            let text = match message.map_err(|error| RelayError::Unreachable(error.to_string()))? {
                Message::Text(text) => text,
                Message::Close(_) => return Ok(None),
                _ => continue,
            };
            let event: h4rn_protocol::RelayServerEvent = match serde_json::from_str(&text) {
                Ok(event) => event,
                Err(_) => continue,
            };
            match event {
                h4rn_protocol::RelayServerEvent::Frame { frame }
                    if frame.conn_id != self.conn_id =>
                {
                    // One device may hold several relay sessions; the hub keys
                    // peers by device, so ignore frames for other conn ids.
                    continue;
                }
                h4rn_protocol::RelayServerEvent::Frame { frame } => match frame.kind {
                    h4rn_protocol::RelayFrameKind::Data => {
                        return Ok(Some(WireRelayFrame::Data {
                            session_id: self.session_id,
                            packet: frame.payload,
                        }));
                    }
                    h4rn_protocol::RelayFrameKind::Close => {
                        let code = frame.payload.first().copied().unwrap_or(0);
                        return Ok(Some(WireRelayFrame::Close {
                            session_id: self.session_id,
                            code,
                        }));
                    }
                    h4rn_protocol::RelayFrameKind::Ping => continue,
                    h4rn_protocol::RelayFrameKind::Open => continue,
                },
                h4rn_protocol::RelayServerEvent::Error { conn_id, .. }
                    if conn_id.as_deref().is_some_and(|id| id != self.conn_id) =>
                {
                    continue;
                }
                h4rn_protocol::RelayServerEvent::Error { error, .. } => {
                    return Err(RelayError::Rejected(format!(
                        "{}: {}",
                        error.code, error.message
                    )));
                }
                _ => continue,
            }
        }
    }

    async fn close(&mut self) -> Result<(), RelayError> {
        self.write(&h4rn_protocol::RelayFrame::close(self.conn_id.clone()))
            .await
    }
}

/// A loopback TCP proxy in front of the daemon's `/secure` route.
///
/// The direct path dials the proxy, so aborting every proxy task is a real
/// socket death (RST/FIN) without touching the daemon or the relay.
struct Proxy {
    addr: SocketAddr,
    accept: std::sync::Mutex<Option<JoinHandle<()>>>,
    tasks: Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>,
}

impl Proxy {
    async fn start(target: SocketAddr) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the direct proxy");
        let addr = listener.local_addr().expect("proxy address");
        let tasks: Arc<std::sync::Mutex<Vec<JoinHandle<()>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let spawned = Arc::clone(&tasks);
        let accept = tokio::spawn(async move {
            loop {
                let Ok((mut inbound, _)) = listener.accept().await else {
                    return;
                };
                let tasks = Arc::clone(&spawned);
                let handle = tokio::spawn(async move {
                    let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await else {
                        return;
                    };
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                });
                tasks
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(handle);
            }
        });
        Self {
            addr,
            accept: std::sync::Mutex::new(Some(accept)),
            tasks,
        }
    }

    /// Closes the listener and every live connection.
    fn kill(&self) {
        if let Some(task) = self
            .accept
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            task.abort();
        }
        for task in self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
        {
            task.abort();
        }
    }
}

struct TestDaemon {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
    mcp: Arc<McpService>,
}

impl TestDaemon {
    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

async fn boot(data_dir: &Path, service: Arc<SecureService>, bind: SocketAddr) -> TestDaemon {
    boot_with_host(data_dir, service, bind, None).await
}

async fn boot_with_host(
    data_dir: &Path,
    service: Arc<SecureService>,
    bind: SocketAddr,
    local_host: Option<(HostId, &str)>,
) -> TestDaemon {
    let config = Config {
        data_dir: data_dir.to_path_buf(),
        bind,
        extra_bind: None,
        tailscale: TailscaleMode::Off,
        network_access: None,
        network_port: 0,
        log_level: "off".to_owned(),
    };
    let options = ThreadManagerOptions {
        settings: LifecycleSettings::default()
            .with_working_exit_grace(chrono::TimeDelta::milliseconds(80))
            .with_stale_bypass(chrono::TimeDelta::seconds(60)),
        tick_interval: Duration::from_millis(20),
        clock: None,
    };
    let mut builder = DaemonBuilder::new(config)
        .with_harness(Arc::new(FakeHarness::new()))
        .with_lifecycle(options)
        .with_secure(Arc::clone(&service));
    if let Some((id, name)) = local_host {
        builder = builder.with_local_host(id, name);
    }
    let daemon = builder.build().await.expect("build daemon");
    let mcp = daemon.mcp();
    let bound = daemon.bind().await.expect("bind daemon");
    let addr = bound.local_addr();
    let (shutdown, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        bound
            .serve(async move {
                let _ = rx.await;
            })
            .await
    });
    TestDaemon {
        addr,
        shutdown: Some(shutdown),
        task,
        mcp,
    }
}

struct RemoteClient {
    client: Client,
    next_id: u64,
    pending: VecDeque<(u64, h4rn_protocol::Event)>,
}

impl RemoteClient {
    fn start(
        identity: DeviceIdentity,
        daemon_public: [u8; 32],
        paths: Vec<ClientPath>,
        session: [u8; SESSION_ID_LEN],
    ) -> Self {
        let engine = EngineConfig::new(session, identity, daemon_public);
        let client = Client::connect(ClientConfig::new(engine, paths)).expect("start client");
        Self {
            client,
            next_id: 1,
            pending: VecDeque::new(),
        }
    }

    async fn wait_established(&mut self) {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match self.client.recv().await {
                    Some(ClientEvent::Established(_)) => return,
                    Some(ClientEvent::Closed { reason }) => {
                        panic!("session closed during connect: {reason}")
                    }
                    Some(ClientEvent::Error { path, message }) => {
                        panic!("path error during connect ({path:?}): {message}")
                    }
                    Some(_) => {}
                    None => panic!("client stopped during connect"),
                }
            }
        })
        .await
        .expect("timed out waiting for the session");
    }

    async fn wait_direct_active(&mut self) {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match self.client.recv().await {
                    Some(ClientEvent::Status(status))
                        if status.active.as_ref().map(|path| path.kind)
                            == Some(TransportPathKind::Direct) =>
                    {
                        return;
                    }
                    Some(ClientEvent::Closed { reason }) => {
                        panic!("session closed while waiting for direct: {reason}")
                    }
                    Some(_) => {}
                    None => panic!("client stopped while waiting for direct"),
                }
            }
        })
        .await
        .expect("the direct path never became active");
    }

    async fn wait_switch_within(&mut self, kind: TransportPathKind, limit: Duration) {
        let mut seen: Vec<String> = Vec::new();
        let outcome = tokio::time::timeout(limit, async {
            loop {
                match self.client.recv().await {
                    Some(ClientEvent::Status(status)) => {
                        seen.push(format!(
                            "active={:?} switched={} state={:?} candidates={:?} detail={:?}",
                            status.active.as_ref().map(|path| path.kind),
                            status.switched,
                            status.state,
                            status
                                .candidates
                                .iter()
                                .map(|path| (path.kind, path.state))
                                .collect::<Vec<_>>(),
                            status.detail
                        ));
                        if status.switched
                            && status.active.as_ref().map(|path| path.kind) == Some(kind)
                        {
                            return;
                        }
                    }
                    Some(ClientEvent::Error { path, message }) => {
                        seen.push(format!("error on {path:?}: {message}"));
                    }
                    Some(ClientEvent::Closed { reason }) => {
                        panic!("session closed instead of switching: {reason}")
                    }
                    Some(_) => {}
                    None => panic!("client stopped instead of switching"),
                }
            }
        })
        .await;
        assert!(
            outcome.is_ok(),
            "the transport did not switch paths in time; saw {seen:#?}"
        );
    }

    async fn send(&mut self, body: ClientRequest) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let frame = ClientFrame { id, body };
        self.client
            .send(h4rn_transport::AppFrame::text(
                serde_json::to_string(&frame).expect("serialise frame"),
            ))
            .await
            .expect("send frame");
        id
    }

    async fn read_frame(&mut self) -> ServerFrame {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), self.client.recv())
                .await
                .expect("timed out waiting for a frame");
            match event {
                Some(ClientEvent::Frame(frame)) => {
                    return serde_json::from_str(frame.as_text().expect("text protocol frame"))
                        .expect("parse ServerFrame");
                }
                Some(ClientEvent::Closed { reason }) => {
                    panic!("session closed while waiting: {reason}")
                }
                Some(ClientEvent::Error { message, .. }) => {
                    panic!("transport error while waiting: {message}")
                }
                Some(_) => {}
                None => panic!("client stopped while waiting"),
            }
        }
    }

    async fn response(&mut self, id: u64) -> Result<ResponseBody, ProtoError> {
        loop {
            match self.read_frame().await {
                ServerFrame::Response {
                    id: response_id,
                    result,
                } if response_id == id => return result,
                ServerFrame::Event { seq, event } => self.pending.push_back((seq, event)),
                _ => {}
            }
        }
    }

    async fn request(&mut self, body: ClientRequest) -> Result<ResponseBody, ProtoError> {
        let id = self.send(body).await;
        self.response(id).await
    }
}

async fn wait_for<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    for _ in 0..300 {
        if let Some(value) = check() {
            return value;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

/// A loopback UI session (`/ws`) standing in for the host's Settings page.
///
/// A relay-provisioned device is only admitted after the host confirms the
/// short authentication string. The test drives that decision through the same
/// authenticated request the UI sends, never by calling the registry directly.
struct HostClient {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: u64,
}

impl HostClient {
    async fn connect(addr: SocketAddr, data_dir: &Path) -> Self {
        let token = h4rn_daemon::auth::TokenStore::new(data_dir)
            .load_or_create()
            .expect("provision the local access token");
        let url = format!("ws://{addr}/ws?token={token}");
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .expect("connect the host UI");
        let mut client = Self { ws, next_id: 1 };
        let hello = ClientRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
            device: DeviceInfo {
                device_id: DeviceId::new("host-ui"),
                name: "host ui".into(),
                platform: "linux".into(),
                app_version: "0.1.0".into(),
            },
        };
        match client.request(hello).await {
            Ok(ResponseBody::Hello { .. }) => {}
            other => panic!("the host UI could not say hello: {other:?}"),
        }
        client
    }

    async fn request(&mut self, body: ClientRequest) -> Result<ResponseBody, ProtoError> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = ClientFrame { id, body };
        self.ws
            .send(Message::Text(
                serde_json::to_string(&frame)
                    .expect("serialise frame")
                    .into(),
            ))
            .await
            .expect("send host frame");
        loop {
            let message = tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("timed out waiting for the host UI")
                .expect("host socket closed")
                .expect("host websocket error");
            let Message::Text(text) = message else {
                continue;
            };
            match serde_json::from_str::<ServerFrame>(&text).expect("parse ServerFrame") {
                ServerFrame::Response {
                    id: response,
                    result,
                } if response == id => return result,
                _ => {}
            }
        }
    }

    /// Waits for the daemon to list the pairing and returns its SAS.
    async fn pending_sas(&mut self, device_id: &str) -> String {
        for _ in 0..300 {
            match self.request(ClientRequest::PairingConfirmations).await {
                Ok(ResponseBody::PairingConfirmations { confirmations }) => {
                    if let Some(entry) = confirmations
                        .iter()
                        .find(|entry| entry.device_id.as_str() == device_id)
                    {
                        return entry.sas.clone();
                    }
                }
                Ok(other) => panic!("unexpected confirmation response: {other:?}"),
                Err(error) => panic!("could not list pairing confirmations: {error:?}"),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the daemon never listed the pending pairing for {device_id}");
    }

    async fn confirm(&mut self, device_id: &str, confirm: bool) {
        match self
            .request(ClientRequest::ConfirmPairing {
                device_id: DeviceId::new(device_id),
                confirm,
            })
            .await
        {
            Ok(ResponseBody::Ack) => {}
            other => panic!("unexpected confirmation decision: {other:?}"),
        }
    }
}

/// Runs the wasm client (Node-owned relay socket, wasm-owned Noise) once.
///
/// `scripts/test-remote.sh` builds the wasm glue and sets `H4RN_TEST_WASM_DIR`;
/// a plain `cargo test` run without that directory skips the check rather than
/// requiring node and a wasm toolchain.
async fn run_wasm_relay_client(
    client: &ConnectClient,
    identity: &DeviceIdentity,
    daemon_public: [u8; 32],
    machine_id: &str,
) {
    let Some(wasm_dir) = std::env::var_os("H4RN_TEST_WASM_DIR") else {
        eprintln!("wasm relay check skipped: H4RN_TEST_WASM_DIR is not set");
        return;
    };
    let script = std::env::var("H4RN_TEST_NODE_SCRIPT")
        .expect("H4RN_TEST_NODE_SCRIPT must point at scripts/wasm-relay-client.mjs");
    let relay_url = client
        .relay_websocket_url()
        .await
        .expect("authenticated relay URL for the wasm client");
    let output = tokio::process::Command::new("node")
        .arg(script)
        .env("WASM_DIR", wasm_dir)
        .env("RELAY_URL", relay_url)
        .env("MACHINE_ID", machine_id)
        .env("IDENTITY", identity.encode())
        .env("DAEMON_PUBLIC", hex::encode(daemon_public))
        .output()
        .await
        .expect("run the wasm relay client with node");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("\"ok\":true"),
        "the wasm client did not complete a relay session\nstdout: {stdout}\nstderr: {stderr}"
    );
    // Let the relay observe the wasm socket close before the native client
    // registers the same device id again.
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Waits until connect reports `machine_id` online.
async fn wait_machine_online(client: &ConnectClient, machine_id: &str) {
    for _ in 0..200 {
        if let Ok(machines) = client.machines().await
            && machines
                .iter()
                .any(|machine| machine.id == machine_id && machine.online)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{machine_id} never came online");
}

/// A minimal Streamable-HTTP MCP server exposing one echo tool.
async fn start_http_upstream(name: &'static str) -> String {
    let app = Router::new().route(
        "/mcp",
        post(move |body: String| async move { http_mcp(name, &body).await }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let address = listener.local_addr().expect("upstream address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}/mcp")
}

async fn http_mcp(name: &str, body: &str) -> Response {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(id) = value.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": name, "version": "0.1.0"}
        }),
        "ping" => json!({}),
        "tools/list" => json!({
            "tools": [{
                "name": "echo",
                "description": "Echo a message.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"]
                }
            }]
        }),
        "tools/call" => {
            let message = params
                .get("arguments")
                .and_then(|arguments| arguments.get("message"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            json!({
                "content": [{"type": "text", "text": format!("{name}:{message}")}],
                "isError": false
            })
        }
        "resources/list" => json!({"resources": []}),
        "prompts/list" => json!({"prompts": []}),
        other => {
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("no method {other}")}
                })
                .to_string(),
            )
                .into_response();
        }
    };
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
    )
        .into_response()
}

fn http_server(url: String) -> McpServerInput {
    McpServerInput {
        name: "local".into(),
        transport: McpTransportInput::Http {
            url,
            headers: Vec::new(),
        },
        tags: Vec::new(),
        enabled: McpEnabledMatrix::default(),
    }
}

/// Two daemons on the same account, each bound only to loopback, so a
/// daemon-to-daemon MCP session has no direct or tailnet candidate: the
/// shared relay is the only path.
#[tokio::test]
#[ignore = "requires Postgres and OrbitID (use ./scripts/test-remote.sh)"]
async fn mcp_sharing_and_revocation_use_a_relay_only_path() {
    h4rn_daemon::init_tracing("h4rn_daemon=debug,h4rn_transport=debug,h4rn_connect=debug");
    let orbit = Orbit::load();
    let relay = RelayServer::start(&orbit).await;

    let a_dir = tempfile::tempdir().expect("A dir");
    let a_identity = load_or_create(&FileKeyStore::new(a_dir.path().join("transport-identity")))
        .expect("A identity");
    let a_connect = login(
        a_dir.path(),
        a_identity.clone(),
        &relay.base_url,
        ConnectDeviceKind::Daemon,
        Some("relay-share-a"),
        &orbit,
    )
    .await;
    let a_machine = a_connect.machine_id().expect("A machine");
    let a_service = Arc::new(SecureService::new(SecureOptions::new(a_identity.clone())));
    let a = boot_with_host(
        a_dir.path(),
        Arc::clone(&a_service),
        "127.0.0.1:0".parse().unwrap(),
        Some((HostId::new("host_relay_share_a"), "relay-share-a")),
    )
    .await;

    let b_dir = tempfile::tempdir().expect("B dir");
    let b_identity = load_or_create(&FileKeyStore::new(b_dir.path().join("transport-identity")))
        .expect("B identity");
    let b_connect = login(
        b_dir.path(),
        b_identity.clone(),
        &relay.base_url,
        ConnectDeviceKind::Daemon,
        Some("relay-share-b"),
        &orbit,
    )
    .await;
    let b_machine = b_connect.machine_id().expect("B machine");
    let b_service = Arc::new(SecureService::new(SecureOptions::new(b_identity.clone())));
    let b = boot_with_host(
        b_dir.path(),
        b_service,
        "127.0.0.1:0".parse().unwrap(),
        Some((HostId::new("host_relay_share_b"), "relay-share-b")),
    )
    .await;

    wait_machine_online(&a_connect, &a_machine).await;
    wait_machine_online(&b_connect, &b_machine).await;

    // Both daemons learned each other from connect, including the relay
    // machine id and the peer device id, before sharing is attempted.
    let a_host = a.mcp.hub().local_host().clone();
    let b_host = b.mcp.hub().local_host().clone();
    let mut peer = None;
    for _ in 0..200 {
        if let Some(found) = b
            .mcp
            .peers()
            .peers()
            .into_iter()
            .find(|peer| peer.host == a_host)
        {
            peer = Some(found);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let peer = peer.expect("B should discover A as a peer");
    assert!(peer.machine_id.is_some(), "relay machine id is required");
    assert!(peer.device_id.is_some(), "connect device id is required");
    assert!(peer.relay.is_some(), "relay address is required");
    // A listens on loopback only, so no advertised LAN candidate can answer.
    // Clear them anyway so this test fails if the tunnel ever needs anything
    // but the relay: the only path left is the multiplexed relay session.
    let mut relay_only = peer.clone();
    relay_only.endpoints.clear();
    b.mcp
        .peers()
        .replace_source("connect", vec![relay_only.clone()]);
    assert!(
        relay_only.endpoints.is_empty(),
        "the relay is the only remaining path"
    );

    // `/secure-mcp` is peer-only: a user device cannot open the route even
    // when the relay lets it reach the machine for the client protocol.
    let c_dir = tempfile::tempdir().expect("C dir");
    let c_identity = DeviceIdentity::generate().expect("C identity");
    let c_connect = login(
        c_dir.path(),
        c_identity,
        &relay.base_url,
        ConnectDeviceKind::Desktop,
        Some("relay-share-c"),
        &orbit,
    )
    .await;
    let c_device = c_connect
        .me()
        .await
        .expect("C identity")
        .devices
        .into_iter()
        .find(|device| device.kind == ConnectDeviceKind::Desktop)
        .map(|device| device.id)
        .expect("C device id");
    let user_device = ConnectRelayConnector {
        client: Arc::new(c_connect),
        machine_id: a_machine.clone(),
        device_id: c_device,
        service: RelayService::SecureMcp,
    };
    let mut link = user_device
        .connect(RelayRequest {
            endpoint: format!("relay:{}", relay.base_url),
            session_id: [9u8; SESSION_ID_LEN],
            role: RelayRole::Client,
            token: None,
        })
        .await
        .expect("the relay accepts the open frame");
    let error = link
        .recv()
        .await
        .expect_err("a user device cannot open /secure-mcp");
    assert!(
        error.to_string().contains("open_forbidden"),
        "the refusal must name the route: {error}"
    );

    // A owns the server and shares it with B's machine. B has a relay-only
    // path because A listens on loopback only.
    let upstream = start_http_upstream("owner").await;
    a.mcp
        .handle(McpRequest::Upsert {
            input: http_server(upstream),
        })
        .await
        .expect("upsert on A");
    a.mcp
        .handle(McpRequest::Share {
            name: "local".into(),
            to_machine: b_host.clone(),
            policy: McpSharePolicy::default(),
        })
        .await
        .expect("share from A");

    let endpoint = b.mcp.hub().endpoint().await.expect("B endpoint");
    let token = b
        .mcp
        .hub()
        .issue_session(McpScope::client(b_host.clone()))
        .token;
    let http = reqwest::Client::new();
    let mut discovered = false;
    for _ in 0..200 {
        let listed: Value = http
            .post(&endpoint)
            .bearer_auth(&token)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .send()
            .await
            .expect("tools/list on B")
            .json()
            .await
            .expect("JSON");
        if listed["result"]["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "local__echo"))
        {
            discovered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        discovered,
        "B never received the grant over the relay-only path"
    );

    let called: Value = http
        .post(&endpoint)
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "local__echo", "arguments": {"message": "over the relay"}}
        }))
        .send()
        .await
        .expect("tools/call on B")
        .json()
        .await
        .expect("JSON");
    assert_eq!(
        called["result"]["content"][0]["text"],
        "owner:over the relay"
    );

    // Revocation travels the same push channel: the tunnel is unregistered.
    a.mcp
        .handle(McpRequest::Unshare {
            name: "local".into(),
            machine: b_host.clone(),
        })
        .await
        .expect("unshare");
    let mut revoked = false;
    for _ in 0..200 {
        let servers = b.mcp.hub().list_servers().await.expect("list on B");
        if !servers.iter().any(|server| server.name == "local") {
            revoked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        revoked,
        "revoking the share must unregister B's relay-only tunnel"
    );

    b.stop().await;
    a.stop().await;
    relay.stop().await;
}

#[tokio::test]
#[ignore = "requires Postgres and OrbitID (use ./scripts/test-remote.sh)"]
async fn relay_pairing_failover_and_revocation() {
    h4rn_daemon::init_tracing("h4rn_daemon=debug,h4rn_transport=debug,h4rn_connect=debug");
    let orbit = Orbit::load();
    let relay = RelayServer::start(&orbit).await;

    // The daemon signs in with the device grant before it starts, exactly like
    // `h4rnd login`.
    let daemon_dir = tempfile::tempdir().expect("daemon dir");
    let daemon_identity = load_or_create(&FileKeyStore::new(
        daemon_dir.path().join("transport-identity"),
    ))
    .expect("daemon identity");
    let daemon_connect = login(
        daemon_dir.path(),
        daemon_identity.clone(),
        &relay.base_url,
        ConnectDeviceKind::Daemon,
        Some("relay-e2e-daemon"),
        &orbit,
    )
    .await;
    let machine_id = daemon_connect.machine_id().expect("daemon machine");

    let service = Arc::new(SecureService::new(SecureOptions::new(
        daemon_identity.clone(),
    )));
    let daemon = boot(
        daemon_dir.path(),
        Arc::clone(&service),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await;
    let proxy = Proxy::start(daemon.addr).await;

    // The relay connector is a background task; wait until connect reports the
    // machine online before pairing, so the `DevicePaired` event has a socket.
    let mut online = false;
    for _ in 0..100 {
        if let Ok(machines) = daemon_connect.machines().await
            && machines
                .iter()
                .any(|machine| machine.id == machine_id && machine.online)
        {
            online = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(online, "the daemon relay connector never came online");

    // A second device signs in and claims a pairing QR created for the daemon.
    // The claim links the device to the machine and tells the daemon its Noise
    // key over the relay.
    let client_dir = tempfile::tempdir().expect("client dir");
    let client_identity = DeviceIdentity::generate().expect("client identity");
    let client = Arc::new(
        login(
            client_dir.path(),
            client_identity.clone(),
            &relay.base_url,
            ConnectDeviceKind::Desktop,
            Some("relay-e2e-client"),
            &orbit,
        )
        .await,
    );
    let pairing = daemon_connect
        .create_pairing(None)
        .await
        .expect("create pairing");
    let claim = client
        .claim_pairing(PairingClaimRequest {
            payload: pairing.qr.clone(),
            device_public_key: URL_SAFE_NO_PAD.encode(client_identity.public().signing_public),
            noise_static_public_key: Some(
                URL_SAFE_NO_PAD.encode(client_identity.public().noise_public),
            ),
            device_name: Some("relay-e2e-client".into()),
        })
        .await
        .expect("claim pairing");
    assert_eq!(claim.machine.id, machine_id);
    let client_noise_b64 = URL_SAFE_NO_PAD.encode(client_identity.public().noise_public);
    let client_device_id = client
        .me()
        .await
        .expect("read the client identity")
        .devices
        .into_iter()
        .find(|device| device.noise_static_public_key.as_deref() == Some(client_noise_b64.as_str()))
        .map(|device| device.id)
        .expect("the claiming device is listed");

    let client_noise = client_identity.public().noise_public;
    // E4: the relay's `DevicePaired` is a claim, not admission. The device is
    // registered as pending and stays off the allowlist until the host sees the
    // same SAS on both screens and confirms it.
    let mut host = HostClient::connect(daemon.addr, daemon_dir.path()).await;
    let host_sas = host.pending_sas(&client_device_id).await;
    let expected = h4rn_transport::pairing::short_auth_string(
        &client_noise,
        &daemon_identity.public().noise_public,
    );
    assert_eq!(
        host_sas, expected,
        "the host must derive the SAS from the announced device key"
    );
    assert!(
        !service.is_allowed(&client_noise),
        "a pending relay device reached the allowlist before the host confirmed it"
    );
    host.confirm(&client_device_id, true).await;
    wait_for("the daemon allowlist", || {
        service.is_allowed(&client_noise).then_some(())
    })
    .await;

    // The browser path: Node owns the relay socket and JSON framing, wasm owns
    // the Noise handshake and application frames. It uses the same paired
    // device, so it runs before the native connections take over that id.
    run_wasm_relay_client(
        &client,
        &client_identity,
        daemon_identity.public().noise_public,
        &machine_id,
    )
    .await;

    let connector = Arc::new(ConnectRelayConnector {
        client: Arc::clone(&client),
        machine_id: machine_id.clone(),
        device_id: client_device_id.clone(),
        service: RelayService::Secure,
    });

    // Relay-only first: the relay is the only path the client knows about.
    let relay_path = || {
        ClientPath::relay(
            Arc::clone(&connector) as Arc<dyn RelayConnector>,
            format!("relay:{}", relay.base_url),
            None,
        )
    };
    let mut session = [0u8; SESSION_ID_LEN];
    session[0] = 1;
    let mut remote = RemoteClient::start(
        client_identity.clone(),
        daemon_identity.public().noise_public,
        vec![relay_path()],
        session,
    );
    remote.wait_established().await;
    remote
        .request(ClientRequest::Hello {
            protocol_version: h4rn_protocol::PROTOCOL_VERSION,
            device: DeviceInfo {
                device_id: DeviceId::new("dev_remote_e2e"),
                name: "remote e2e".into(),
                platform: "linux".into(),
                app_version: "0.1.0".into(),
            },
        })
        .await
        .expect("hello over the relay");

    let host = match remote
        .request(ClientRequest::ListHosts)
        .await
        .expect("hosts over the relay")
    {
        ResponseBody::Hosts { hosts } => hosts[0].host_id.clone(),
        other => panic!("unexpected host response: {other:?}"),
    };
    let created = remote
        .request(ClientRequest::CreateThread {
            host: host.clone(),
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            workspace_path: "/tmp/h4rn-remote-workspace".into(),
            model: "fake-model".into(),
            prompt: "created over the relay".into(),
            attachments: Vec::new(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            fast: false,
            worktree: None,
        })
        .await
        .expect("create a thread over the relay");
    let thread_id = match created {
        ResponseBody::ThreadCreated { summary, .. } => summary.thread_id,
        other => panic!("unexpected CreateThread response: {other:?}"),
    };

    // Now a direct path exists in front of the relay; the engine prefers it.
    let paths = vec![
        relay_path(),
        ClientPath::websocket(
            TransportPathKind::Direct,
            format!("ws://{}/secure", proxy.addr),
        ),
    ];
    let mut session = [0u8; SESSION_ID_LEN];
    session[0] = 2;
    let mut switched = RemoteClient::start(
        client_identity.clone(),
        daemon_identity.public().noise_public,
        paths,
        session,
    );
    switched.wait_established().await;
    switched
        .request(ClientRequest::Hello {
            protocol_version: h4rn_protocol::PROTOCOL_VERSION,
            device: DeviceInfo {
                device_id: DeviceId::new("dev_remote_failover"),
                name: "remote failover e2e".into(),
                platform: "linux".into(),
                app_version: "0.1.0".into(),
            },
        })
        .await
        .expect("hello on the multipath session");
    switched.wait_direct_active().await;

    // Kill every direct socket. The logical session and its application loop
    // stay alive, and the relay takes over within the two-second budget.
    proxy.kill();
    switched
        .wait_switch_within(TransportPathKind::Relay, Duration::from_secs(2))
        .await;
    match switched
        .request(ClientRequest::ListThreads {
            filter: h4rn_protocol::ThreadFilter {
                host: Some(host),
                ..Default::default()
            },
        })
        .await
        .expect("threads after the failover")
    {
        ResponseBody::Threads { threads } => assert!(
            threads.iter().any(|thread| thread.thread_id == thread_id),
            "the thread created over the relay must survive the switch"
        ),
        other => panic!("unexpected ListThreads response: {other:?}"),
    }

    // Revoking the device closes the live session and removes the key, so a
    // fresh relay connection cannot even complete the Noise handshake.
    daemon_connect
        .revoke_device(&client_device_id)
        .await
        .expect("revoke the paired device");
    let closure = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match switched.client.recv().await {
                Some(ClientEvent::Closed { reason }) => return reason,
                Some(_) => {}
                None => panic!("client stopped without a reason"),
            }
        }
    })
    .await
    .expect("the revoked session was not dropped");
    assert!(
        closure.contains("revoked") || closure.contains("not paired") || closure.contains("closed"),
        "unexpected closure reason: {closure}"
    );
    assert!(
        !service.is_allowed(&client_noise),
        "revocation must remove the device from the allowlist"
    );

    let mut again = RemoteClient::start(
        client_identity,
        daemon_identity.public().noise_public,
        vec![ClientPath::relay(
            connector as Arc<dyn RelayConnector>,
            "relay",
            None,
        )],
        [3u8; SESSION_ID_LEN],
    );
    let refusal = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match again.client.recv().await {
                Some(ClientEvent::Error { path: _, message })
                    if message.contains("revoked") || message.contains("not paired") =>
                {
                    return message;
                }
                Some(ClientEvent::Closed { reason }) => return reason,
                Some(ClientEvent::Established(_)) => {
                    panic!("a revoked device was allowed to reconnect")
                }
                Some(_) => {}
                None => panic!("client stopped without a reason"),
            }
        }
    })
    .await
    .expect("the revoked device was not refused");
    assert!(
        refusal.contains("revoked") || refusal.contains("not paired"),
        "unexpected refusal reason: {refusal}"
    );

    // A paired device is recorded in the local audit trail.
    let audit = std::fs::read_to_string(daemon_dir.path().join("remote-audit.jsonl"))
        .expect("the remote audit trail");
    assert!(
        audit.contains("\"action\":\"thread.created\""),
        "a remote thread creation must be audited: {audit}"
    );
    assert!(
        audit.contains(&client_device_id),
        "audit must name the device"
    );

    switched.client.close().await;
    daemon.stop().await;
    proxy.kill();
    relay.stop().await;
}
