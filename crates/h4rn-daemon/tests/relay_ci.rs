//! Postgres-backed relay coverage that runs without OrbitID.
//!
//! The full remote e2e in `remote.rs` signs devices in through a real OIDC
//! provider. These tests replace only that step: they mint a real user, device
//! and session through h4rn-connect's own store (the same rows a device-grant
//! approval produces) and persist the resulting [`Credentials`] for the daemon.
//! Everything after login is the shipping path: the real axum router, the real
//! Postgres schema, the daemon's own relay connector and registry, the real
//! Noise transport and the shipping multipath client.
//!
//! Ignored by default because they need Postgres. The `connect-postgres` CI job
//! sets `H4RN_TEST_DATABASE_URL` and runs this binary with `--ignored
//! --test-threads=1`; the OrbitID suite stays the full end-to-end run
//! (`scripts/test-remote.sh`).

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
use chrono::Utc;
use futures::{SinkExt as _, StreamExt as _};
use h4rn_connect::api::{AppState, build_state, router as connect_router};
use h4rn_connect::config::Config as ConnectConfig;
use h4rn_connect::crypto::random_b64;
use h4rn_connect::store::NewDevice;
use h4rn_core::LifecycleSettings;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::mcp::McpService;
use h4rn_daemon::remote::{ConnectClient, Credentials, TailscaleMode};
use h4rn_daemon::threads::ThreadManagerOptions;
use h4rn_harness::FakeHarness;
use h4rn_mcp::session::McpScope;
use h4rn_protocol::{
    ClientFrame, ClientRequest, ConnectDeviceKind, DeviceId, DeviceInfo, HostId, McpEnabledMatrix,
    McpRequest, McpServerInput, McpSharePolicy, McpTransportInput,
    PROTOCOL_VERSION, PairingClaimRequest, ProtoError, RelayOpen, RelayService, ResponseBody,
    ServerFrame,
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

fn database_url() -> String {
    std::env::var("H4RN_TEST_DATABASE_URL").expect(
        "H4RN_TEST_DATABASE_URL must be set; run this in the connect-postgres CI job or \
         ./scripts/test-connect.sh",
    )
}

/// An in-process h4rn-connect on a real TCP port and the CI Postgres schema.
///
/// No OIDC provider is configured: the tests never call the device grant, they
/// mint the session row it would have produced.
struct RelayServer {
    base_url: String,
    state: AppState,
    task: JoinHandle<()>,
}

impl RelayServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the in-process relay");
        let addr = listener.local_addr().expect("relay address");
        let base_url = format!("http://{addr}");
        let mut config = ConnectConfig {
            public_base_url: base_url.clone(),
            database_url: database_url(),
            kek: random_b64(32),
            signing_key: Some(random_b64(32)),
            allow_insecure: true,
            ..ConnectConfig::default()
        };
        // The daemons reconnect eagerly while the test converges.
        config.rate_limits.per_ip_per_minute = 10_000;
        config.rate_limits.auth_per_minute = 10_000;
        config.rate_limits.pairing_per_hour = 10_000;
        config.rate_limits.relay_connects_per_minute = 10_000;
        config.normalise().expect("normalise relay configuration");
        let state = build_state(config).await.expect("build relay state");
        let app = connect_router(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve relay");
        });
        Self {
            base_url,
            state,
            task,
        }
    }

    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// Mints the store rows a successful device-grant approval would create and
/// persists the daemon-side session file.
///
/// `complete_login` + `issue_session` are the production store calls the relay
/// makes when it approves a grant; writing [`Credentials`] is what
/// `h4rnd login` writes. No test-only endpoint or auth bypass is involved.
async fn mint_login(
    relay: &RelayServer,
    data_dir: &Path,
    identity: &DeviceIdentity,
    kind: ConnectDeviceKind,
    machine_name: Option<&str>,
    subject: &str,
) -> ConnectClient {
    let public = identity.public();
    let (user, device) = relay
        .state
        .store
        .complete_login(
            "test",
            "https://idp.test",
            subject,
            Some(&format!("{subject}@example.test")),
            true,
            Some("Relay CI"),
            None,
            json!({"sub": subject}),
            NewDevice {
                kind,
                name: format!("{} device", kind.as_str()),
                proof_public_key: public.signing_public.to_vec(),
                noise_static_public_key: Some(public.noise_public.to_vec()),
                platform: Some("linux".to_string()),
                machine_name: machine_name.map(ToOwned::to_owned),
            },
        )
        .await
        .expect("complete login");
    let issued = relay
        .state
        .store
        .issue_session(
            &relay.state.keystore,
            &relay.state.config,
            user.id,
            device.id,
            None,
            None,
        )
        .await
        .expect("issue session");
    let credentials = Credentials {
        connect_url: relay.base_url.clone(),
        user_id: user.id.to_string(),
        device_id: device.id.to_string(),
        machine_id: device.machine_id.map(|id| id.to_string()).unwrap_or_default(),
        access_token: issued.tokens.access_token,
        refresh_token: issued.tokens.refresh_token,
        expires_at: Utc::now().timestamp() + issued.tokens.expires_in,
        token_type: issued.tokens.token_type,
    };
    credentials.save(data_dir).expect("persist credentials");
    ConnectClient::new(data_dir, identity.clone()).expect("connect client")
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

/// Boots the production daemon. It loads the minted credentials itself and
/// starts the shipping relay connector, peer sync and presence tasks.
async fn boot_with_host(
    data_dir: &Path,
    service: Arc<SecureService>,
    local_host: Option<(HostId, &str)>,
) -> TestDaemon {
    let config = Config {
        data_dir: data_dir.to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind address"),
        extra_bind: None,
        tailscale: TailscaleMode::Off,
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

async fn boot(data_dir: &Path, service: Arc<SecureService>) -> TestDaemon {
    boot_with_host(data_dir, service, None).await
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

/// Relay-only MCP sharing, dispatch and revocation between two daemons.
#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (run in the connect-postgres CI job)"]
async fn relay_only_mcp_sharing_and_revocation() {
    let relay = RelayServer::start().await;
    // One account: sharing and peer discovery are per-user.
    let subject = format!("relay-ci-mcp-{}", uuid::Uuid::now_v7());

    let a_dir = tempfile::tempdir().expect("A dir");
    let a_identity = load_or_create(&FileKeyStore::new(a_dir.path().join("transport-identity")))
        .expect("A identity");
    let a_connect = mint_login(
        &relay,
        a_dir.path(),
        &a_identity,
        ConnectDeviceKind::Daemon,
        Some("relay-ci-share-a"),
        &subject,
    )
    .await;
    let a_machine = a_connect.machine_id().expect("A machine");
    let a_service = Arc::new(SecureService::new(SecureOptions::new(a_identity)));
    let a = boot_with_host(
        a_dir.path(),
        Arc::clone(&a_service),
        Some((HostId::new("host_relay_ci_share_a"), "relay-ci-share-a")),
    )
    .await;

    let b_dir = tempfile::tempdir().expect("B dir");
    let b_identity = load_or_create(&FileKeyStore::new(b_dir.path().join("transport-identity")))
        .expect("B identity");
    let b_connect = mint_login(
        &relay,
        b_dir.path(),
        &b_identity,
        ConnectDeviceKind::Daemon,
        Some("relay-ci-share-b"),
        &subject,
    )
    .await;
    let b_machine = b_connect.machine_id().expect("B machine");
    let b_service = Arc::new(SecureService::new(SecureOptions::new(b_identity)));
    let b = boot_with_host(
        b_dir.path(),
        b_service,
        Some((HostId::new("host_relay_ci_share_b"), "relay-ci-share-b")),
    )
    .await;

    wait_machine_online(&a_connect, &a_machine).await;
    wait_machine_online(&b_connect, &b_machine).await;

    // B discovers A from connect, including A's relay machine id and device id.
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

    // A listens on loopback only. Clear any advertised endpoints so the only
    // remaining path to A is the multiplexed relay session.
    let mut relay_only = peer.clone();
    relay_only.endpoints.clear();
    b.mcp
        .peers()
        .replace_source("connect", vec![relay_only.clone()]);
    assert!(
        relay_only.endpoints.is_empty(),
        "the relay is the only remaining path"
    );

    // `/secure-mcp` is peer-only: a user device cannot open the route even when
    // the relay lets it reach the machine for the client protocol.
    let c_dir = tempfile::tempdir().expect("C dir");
    let c_identity = DeviceIdentity::generate().expect("C identity");
    let c_connect = Arc::new(
        mint_login(
            &relay,
            c_dir.path(),
            &c_identity,
            ConnectDeviceKind::Desktop,
            Some("relay-ci-share-c"),
            &subject,
        )
        .await,
    );
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
        client: Arc::clone(&c_connect),
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

    // A owns the server and shares it with B's machine. B reaches A only over
    // the relay because A advertises no other endpoint.
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

/// Pairing over the relay is only admitted after the host confirms the SAS.
#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (run in the connect-postgres CI job)"]
async fn relay_pairing_waits_for_host_sas_confirmation() {
    let relay = RelayServer::start().await;
    // The daemon and the phone must belong to the same account.
    let subject = format!("relay-ci-pair-{}", uuid::Uuid::now_v7());

    let daemon_dir = tempfile::tempdir().expect("daemon dir");
    let daemon_identity = load_or_create(&FileKeyStore::new(
        daemon_dir.path().join("transport-identity"),
    ))
    .expect("daemon identity");
    let daemon_connect = mint_login(
        &relay,
        daemon_dir.path(),
        &daemon_identity,
        ConnectDeviceKind::Daemon,
        Some("relay-ci-daemon"),
        &subject,
    )
    .await;
    let machine_id = daemon_connect.machine_id().expect("daemon machine");
    let daemon_noise = daemon_identity.public().noise_public;
    let service = Arc::new(SecureService::new(SecureOptions::new(
        daemon_identity.clone(),
    )));
    let daemon = boot(daemon_dir.path(), Arc::clone(&service)).await;

    // The claim needs the daemon's relay socket online so `DevicePaired` has a
    // destination.
    wait_machine_online(&daemon_connect, &machine_id).await;

    let client_dir = tempfile::tempdir().expect("client dir");
    let client_identity = DeviceIdentity::generate().expect("client identity");
    let client = Arc::new(
        mint_login(
            &relay,
            client_dir.path(),
            &client_identity,
            ConnectDeviceKind::Desktop,
            Some("relay-ci-client"),
            &subject,
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
            device_name: Some("relay-ci-client".into()),
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

    // `DevicePaired` is a claim, not admission: the key is pending and the host
    // must compare the same SAS on both screens before confirming.
    let mut host = HostClient::connect(daemon.addr, daemon_dir.path()).await;
    let host_sas = host.pending_sas(&client_device_id).await;
    let expected = h4rn_transport::pairing::short_auth_string(
        &client_identity.public().noise_public,
        &daemon_noise,
    );
    assert_eq!(
        host_sas, expected,
        "the host must derive the SAS from the announced device key"
    );
    assert!(
        !service.is_allowed(&client_identity.public().noise_public),
        "a pending relay device reached the allowlist before the host confirmed it"
    );

    host.confirm(&client_device_id, true).await;
    wait_for("the daemon allowlist", || {
        service
            .is_allowed(&client_identity.public().noise_public)
            .then_some(())
    })
    .await;

    // The paired device now completes a Noise handshake over the relay alone
    // and gets a protocol response.
    let connector = Arc::new(ConnectRelayConnector {
        client: Arc::clone(&client),
        machine_id: machine_id.clone(),
        device_id: client_device_id.clone(),
        service: RelayService::Secure,
    });
    let mut remote = RemoteClient::start(
        client_identity.clone(),
        daemon_noise,
        vec![ClientPath::relay(
            connector as Arc<dyn RelayConnector>,
            format!("relay:{}", relay.base_url),
            None,
        )],
        [7u8; SESSION_ID_LEN],
    );
    remote.wait_established().await;
    remote
        .request(ClientRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
            device: DeviceInfo {
                device_id: DeviceId::new("dev_relay_ci"),
                name: "relay ci".into(),
                platform: "linux".into(),
                app_version: "0.1.0".into(),
            },
        })
        .await
        .expect("hello over the relay");

    // Revoking the device removes it from the transport allowlist.
    daemon_connect
        .revoke_device(&client_device_id)
        .await
        .expect("revoke the paired device");
    wait_for("revocation to remove the allowlist key", || {
        (!service.is_allowed(&client_identity.public().noise_public)).then_some(())
    })
    .await;

    daemon.stop().await;
    relay.stop().await;
}
