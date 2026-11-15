//! End-to-end coverage for the daemon-owned MCP hub.
//!
//! The tests boot real daemons and drive the protocol; upstreams are real MCP
//! servers (HTTP in-process, or the stdio fixture exercised by `h4rn-mcp`'s own
//! integration test). The cross-machine test runs two daemons and carries
//! JSON-RPC through the Noise-encrypted `/secure-mcp` tunnel.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures::{SinkExt, StreamExt};
use h4rn_core::LifecycleSettings;
use h4rn_daemon::auth::TokenStore;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::mcp::{TransportMcpTunnel, TunnelConnection};
use h4rn_daemon::remote::DeviceRegistry;
use h4rn_daemon::threads::ThreadManagerOptions;
use h4rn_harness::FakeHarness;
use h4rn_mcp::session::McpScope;
use h4rn_mcp::sharing::McpTunnel as _;
use h4rn_protocol::ApprovalDecision;
use h4rn_protocol::HostId;
use h4rn_protocol::TransportPathKind;
use h4rn_protocol::{
    ClientFrame, ClientRequest, DeviceId, DeviceInfo, Event, HarnessKind, McpConflictResolution,
    McpEnabledMatrix, McpRequest, McpSecretInput, McpServerInput, McpSharePolicy,
    McpTransportInput, ResponseBody, ServerFrame, Topic,
};
use h4rn_transport::keys::DeviceIdentity;
use h4rn_transport::server::{SecureOptions, SecureService};
use h4rn_transport::server_session::PairingPolicy;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

struct TestDaemon {
    addr: std::net::SocketAddr,
    token: String,
    dir: Option<tempfile::TempDir>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
    mcp: Arc<h4rn_daemon::mcp::McpService>,
}

impl TestDaemon {
    fn data_dir(&self) -> &std::path::Path {
        self.dir.as_ref().expect("temporary data dir").path()
    }

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

async fn boot_with_secure(secure: Option<Arc<SecureService>>) -> TestDaemon {
    boot_with_devices(secure, None).await
}

async fn boot_with_devices(
    secure: Option<Arc<SecureService>>,
    devices: Option<Arc<DeviceRegistry>>,
) -> TestDaemon {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Auto,
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
        .with_lifecycle(options);
    if let Some(secure) = secure {
        builder = builder.with_secure(secure);
    }
    if let Some(devices) = devices {
        builder = builder.with_devices(devices);
    }
    let daemon = builder.build().await.expect("build daemon");
    let token = TokenStore::new(dir.path())
        .load()
        .expect("read token")
        .expect("token exists");
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
        token,
        dir: Some(dir),
        shutdown: Some(shutdown),
        task,
        mcp,
    }
}

async fn boot() -> TestDaemon {
    boot_with_secure(None).await
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    pending: VecDeque<Event>,
}

impl Client {
    async fn connect(daemon: &TestDaemon) -> Self {
        let url = format!("ws://{}/ws?token={}", daemon.addr, daemon.token);
        let (ws, _) = connect_async(url).await.expect("connect");
        Self {
            ws,
            next_id: 1,
            pending: VecDeque::new(),
        }
    }

    async fn send(&mut self, body: ClientRequest) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let frame = ClientFrame { id, body };
        self.ws
            .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
            .await
            .expect("send frame");
        id
    }

    async fn hello(&mut self) {
        let response = self
            .request(ClientRequest::Hello {
                protocol_version: h4rn_protocol::PROTOCOL_VERSION,
                device: DeviceInfo {
                    device_id: DeviceId::new("dev_mcp"),
                    name: "mcp integration test".into(),
                    platform: "linux".into(),
                    app_version: "0.1.0".into(),
                },
            })
            .await
            .expect("hello");
        assert!(
            matches!(response, ResponseBody::Hello { .. }),
            "{response:?}"
        );
    }

    async fn response(&mut self, id: u64) -> Result<ResponseBody, h4rn_protocol::ProtoError> {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("timed out waiting for a response")
                .expect("socket closed")
                .expect("websocket error");
            match message {
                Message::Text(text) => match serde_json::from_str::<ServerFrame>(&text).unwrap() {
                    ServerFrame::Response {
                        id: response_id,
                        result,
                    } if response_id == id => return result,
                    ServerFrame::Event { event, .. } => self.pending.push_back(event),
                    _ => {}
                },
                Message::Close(_) => panic!("daemon closed the connection"),
                _ => {}
            }
        }
    }

    async fn request(
        &mut self,
        body: ClientRequest,
    ) -> Result<ResponseBody, h4rn_protocol::ProtoError> {
        let id = self.send(body).await;
        self.response(id).await
    }

    async fn mcp(
        &mut self,
        request: McpRequest,
    ) -> Result<ResponseBody, h4rn_protocol::ProtoError> {
        self.request(ClientRequest::Mcp { request }).await
    }

    /// Waits for a subscribed event the test cares about, keeping any others.
    async fn wait_event(&mut self, matches: impl Fn(&Event) -> bool) -> Event {
        if let Some(index) = self.pending.iter().position(&matches) {
            return self.pending.remove(index).expect("queued event");
        }
        loop {
            let message = tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("timed out waiting for an event")
                .expect("socket closed")
                .expect("websocket error");
            let Message::Text(text) = message else {
                continue;
            };
            if let ServerFrame::Event { event, .. } =
                serde_json::from_str::<ServerFrame>(&text).expect("frame")
            {
                if matches(&event) {
                    return event;
                }
                self.pending.push_back(event);
            }
        }
    }
}

/// A minimal streamable-HTTP MCP server; one echo tool and empty resources.
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
        "resources/list" => json!({"resources": [{
            "uri": "secret://account",
            "name": "account",
            "mimeType": "text/plain"
        }]}),
        "resources/read" => json!({"contents": [{
            "uri": "secret://account",
            "mimeType": "text/plain",
            "text": "fixture confidential data"
        }]}),
        "prompts/list" => json!({"prompts": [{
            "name": "brief",
            "description": "A fixture prompt."
        }]}),
        "prompts/get" => json!({
            "description": "A fixture prompt.",
            "messages": [{
                "role": "user",
                "content": {"type": "text", "text": "fixture prompt"}
            }]
        }),
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

#[tokio::test]
async fn mcp_servers_round_trip_through_the_protocol() {
    let daemon = boot().await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    match client.mcp(McpRequest::ListServers).await.expect("list") {
        ResponseBody::McpServers { servers } => assert!(servers.is_empty()),
        other => panic!("unexpected list response: {other:?}"),
    }

    let url = start_http_upstream("upstream").await;
    let server = match client
        .mcp(McpRequest::Upsert {
            input: http_server(url),
        })
        .await
        .expect("upsert")
    {
        ResponseBody::McpServer { server } => server,
        other => panic!("unexpected upsert response: {other:?}"),
    };
    assert_eq!(server.name, "local");
    assert_eq!(server.imported_from, None);

    let tools = match client
        .mcp(McpRequest::ListTools {
            name: "local".into(),
        })
        .await
        .expect("list tools")
    {
        ResponseBody::McpTools { server, tools } => {
            assert_eq!(server, "local");
            tools
        }
        other => panic!("unexpected tools response: {other:?}"),
    };
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "local__echo");

    let call = match client
        .mcp(McpRequest::TestCall {
            name: "local".into(),
            tool: "echo".into(),
            arguments: json!({"message": "from the daemon"}),
        })
        .await
        .expect("test call")
    {
        ResponseBody::McpCall { result } => result,
        other => panic!("unexpected call response: {other:?}"),
    };
    assert_eq!(call.content, "upstream:from the daemon");
    assert!(!call.is_error);

    match client
        .mcp(McpRequest::Remove {
            name: "local".into(),
        })
        .await
    {
        Ok(ResponseBody::Ack) => {}
        other => panic!("unexpected remove response: {other:?}"),
    }
    match client.mcp(McpRequest::ListServers).await.expect("list") {
        ResponseBody::McpServers { servers } => assert!(servers.is_empty()),
        other => panic!("unexpected list response: {other:?}"),
    }

    daemon.stop().await;
}

#[tokio::test]
async fn imports_preview_conflicts_and_resolve_them() {
    let daemon = boot().await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let workspace = tempfile::tempdir().expect("workspace");
    let url = start_http_upstream("imported").await;

    // A local server with the same name makes the project entry a conflict.
    client
        .mcp(McpRequest::Upsert {
            input: McpServerInput {
                name: "project_server".into(),
                transport: McpTransportInput::Stdio {
                    command: "true".into(),
                    args: Vec::new(),
                    cwd: None,
                    env: Vec::new(),
                },
                tags: Vec::new(),
                enabled: McpEnabledMatrix::default(),
            },
        })
        .await
        .expect("seed local server");
    std::fs::write(
        workspace.path().join(".mcp.json"),
        json!({
            "mcpServers": {
                "project_server": {"type": "http", "url": url}
            }
        })
        .to_string(),
    )
    .unwrap();

    let preview = match client
        .mcp(McpRequest::Import {
            harness: HarnessKind::ClaudeCode,
            workspace: Some(workspace.path().to_string_lossy().into_owned()),
        })
        .await
        .expect("preview")
    {
        ResponseBody::McpImportPreview { preview } => preview,
        other => panic!("unexpected preview response: {other:?}"),
    };
    assert!(
        preview
            .conflicts
            .iter()
            .any(|conflict| conflict.name == "project_server"),
        "expected a project_server conflict: {preview:?}"
    );
    assert!(
        preview
            .candidates
            .iter()
            .all(|candidate| candidate.name != "project_server"),
        "a conflicting entry must not also be a candidate"
    );

    let resolved = match client
        .mcp(McpRequest::ResolveConflict {
            harness: HarnessKind::ClaudeCode,
            name: "project_server".into(),
            resolution: McpConflictResolution::UseTheirs,
            workspace: Some(workspace.path().to_string_lossy().into_owned()),
        })
        .await
        .expect("resolve")
    {
        ResponseBody::McpServer { server } => server,
        other => panic!("unexpected resolve response: {other:?}"),
    };
    assert_eq!(resolved.name, "project_server");
    assert!(matches!(
        resolved.transport,
        h4rn_protocol::McpTransportView::Http { .. }
    ));

    daemon.stop().await;
}

#[tokio::test]
async fn secret_values_never_reach_the_protocol_or_sqlite() {
    const SECRET: &str = "mcp-super-secret-value-4711";
    let daemon = boot().await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let url = start_http_upstream("secretive").await;
    let response = client
        .mcp(McpRequest::Upsert {
            input: McpServerInput {
                name: "secretive".into(),
                transport: McpTransportInput::Http {
                    url,
                    headers: vec![McpSecretInput {
                        name: "Authorization".into(),
                        value: Some(format!("Bearer {SECRET}")),
                    }],
                },
                tags: Vec::new(),
                enabled: McpEnabledMatrix::default(),
            },
        })
        .await
        .expect("upsert");
    let body = serde_json::to_string(&response).unwrap();
    assert!(body.contains("Authorization"));
    assert!(!body.contains(SECRET), "the view leaked the secret: {body}");

    let listed = client.mcp(McpRequest::ListServers).await.expect("list");
    let body = serde_json::to_string(&listed).unwrap();
    assert!(!body.contains(SECRET), "the list leaked the secret: {body}");

    // The registry runs in WAL mode, so a just-written row may still live in
    // the sidecar file. Only the reference must appear; the value must not.
    let mut db = Vec::new();
    for suffix in ["", "-wal"] {
        let path = daemon.data_dir().join(format!("mcp.db{suffix}"));
        if let Ok(mut bytes) = std::fs::read(path) {
            db.append(&mut bytes);
        }
    }
    let db = String::from_utf8_lossy(&db).into_owned();
    assert!(!db.contains(SECRET), "the registry leaked the secret");
    assert!(db.contains("mcp/srv_"), "the registry stores a reference");

    let secrets_path = daemon.data_dir().join("mcp-secrets");
    if let Ok(bytes) = std::fs::read(&secrets_path) {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            !text.contains(SECRET),
            "the fallback secret file stores plaintext"
        );
    }

    daemon.stop().await;
}

#[tokio::test]
async fn a_remote_hub_proxies_a_shared_server_over_the_secure_tunnel() {
    let a_identity = DeviceIdentity::generate().expect("A identity");
    let b_identity = DeviceIdentity::generate().expect("B identity");

    // A admits B's device key on `/secure-mcp`.
    let a_secure = Arc::new(SecureService::new(
        SecureOptions::new(a_identity.clone())
            .with_policy(PairingPolicy::new([b_identity.public().noise_public])),
    ));
    let b_secure = Arc::new(SecureService::new(SecureOptions::new(b_identity.clone())));
    let b = boot_with_secure(Some(b_secure)).await;
    let b_host = b.mcp.hub().local_host().clone();
    // ... and registers that key as the daemon of machine B.
    let a_devices = Arc::new(DeviceRegistry::in_memory());
    a_devices
        .register_host(
            b_host.clone(),
            b_identity.public().noise_public,
            Some("dev_b".into()),
        )
        .expect("persist");
    let a = boot_with_devices(Some(Arc::clone(&a_secure)), Some(a_devices)).await;

    let a_host = a.mcp.hub().local_host().clone();

    // A owns a server and shares it with B's machine id.
    let upstream = start_http_upstream("owner").await;
    let mut a_client = Client::connect(&a).await;
    a_client.hello().await;
    a_client
        .mcp(McpRequest::Upsert {
            input: http_server(upstream),
        })
        .await
        .expect("upsert on A");
    a_client
        .mcp(McpRequest::Share {
            name: "local".into(),
            to_machine: b_host.clone(),
            policy: McpSharePolicy::default(),
        })
        .await
        .expect("share");

    // B proxies it through the encrypted tunnel.
    let tunnel = TransportMcpTunnel::new(TunnelConnection {
        paths: vec![h4rn_transport::client::ClientPath::websocket(
            TransportPathKind::Direct,
            format!("ws://{}/secure-mcp", a.addr),
        )],
        identity: b_identity,
        daemon_public: a_secure.public_identity().noise_public,
        machine: b_host.clone(),
        device_id: Some("dev_b".into()),
    });
    b.mcp
        .hub()
        .register_remote("local", a_host.clone(), Arc::new(tunnel))
        .await;

    // B's own HTTP endpoint exposes the remote tool under the same namespace.
    let endpoint = b.mcp.hub().endpoint().await.expect("B endpoint");
    let token = b
        .mcp
        .hub()
        .issue_session(McpScope::client(b_host.clone()))
        .token;
    let http = reqwest::Client::new();
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
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        names.contains(&"local__echo"),
        "B should see A's shared tool: {names:?}"
    );

    let called: Value = http
        .post(&endpoint)
        .bearer_auth(&token)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "local__echo", "arguments": {"message": "over the wire"}}
        }))
        .send()
        .await
        .expect("tools/call on B")
        .json()
        .await
        .expect("JSON");
    assert_eq!(
        called["result"]["content"][0]["text"],
        "owner:over the wire"
    );
    assert_eq!(called["result"]["isError"], false);

    // The share is visible on A's side.
    let view = a.mcp.hub().server("local").await.expect("A server");
    assert!(view.shared_to.contains(&b_host));

    b.stop().await;
    a.stop().await;
}

/// Owner A with one shared server and a hand-built tunnel from key `b`.
struct TunnelRig {
    a: TestDaemon,
    tunnel: TransportMcpTunnel,
    identity: DeviceIdentity,
    daemon_public: [u8; 32],
}

async fn tunnel_rig(
    registry: impl FnOnce(&DeviceRegistry, &HostId, [u8; 32]),
    policy: McpSharePolicy,
    claimed_machine: Option<&str>,
    claimed_device: Option<&str>,
) -> TunnelRig {
    let a_identity = DeviceIdentity::generate().expect("A identity");
    let b_identity = DeviceIdentity::generate().expect("B identity");
    let b_key = b_identity.public().noise_public;
    let a_secure = Arc::new(SecureService::new(
        SecureOptions::new(a_identity).with_policy(PairingPolicy::new([b_key])),
    ));
    let b_host = HostId::new("host_b");
    let devices = Arc::new(DeviceRegistry::in_memory());
    registry(&devices, &b_host, b_key);
    let a = boot_with_devices(Some(Arc::clone(&a_secure)), Some(devices)).await;
    let upstream = start_http_upstream("owner").await;
    let mut client = Client::connect(&a).await;
    client.hello().await;
    client
        .mcp(McpRequest::Upsert {
            input: http_server(upstream),
        })
        .await
        .expect("upsert");
    client
        .mcp(McpRequest::Share {
            name: "local".into(),
            to_machine: b_host.clone(),
            policy,
        })
        .await
        .expect("share");
    let tunnel = TransportMcpTunnel::new(TunnelConnection {
        paths: vec![h4rn_transport::client::ClientPath::websocket(
            TransportPathKind::Direct,
            format!("ws://{}/secure-mcp", a.addr),
        )],
        identity: b_identity.clone(),
        daemon_public: a_secure.public_identity().noise_public,
        machine: HostId::new(claimed_machine.unwrap_or("host_b")),
        device_id: claimed_device.map(str::to_owned),
    });
    TunnelRig {
        a,
        tunnel,
        identity: b_identity,
        daemon_public: a_secure.public_identity().noise_public,
    }
}

async fn list_tools_over(rig: &TunnelRig) -> Result<Value, String> {
    let session = rig.tunnel.open("local").await.map_err(|e| e.to_string())?;
    session
        .request("tools/list", json!({}), Duration::from_secs(5))
        .await
        .map_err(|e| e.to_string())
}

/// Opens the grants handshake on the rig's `/secure-mcp` with the rig's key and
/// returns the first grants snapshot, or why the owner refused.
async fn open_grants(rig: &TunnelRig) -> Result<Value, String> {
    use h4rn_transport::client::{Client, ClientConfig, ClientEvent, ClientPath};
    use h4rn_transport::engine::EngineConfig;
    let engine = EngineConfig::new([7u8; 16], rig.identity.clone(), rig.daemon_public);
    let paths = vec![ClientPath::websocket(
        h4rn_protocol::TransportPathKind::Direct,
        format!("ws://{}/secure-mcp", rig.a.addr),
    )];
    let mut client =
        Client::connect(ClientConfig::new(engine, paths)).map_err(|e| e.to_string())?;
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        let mut sent = false;
        loop {
            match client.recv().await {
                Some(ClientEvent::Established(_)) if !sent => {
                    sent = true;
                    client
                        .send(h4rn_transport::AppFrame::text(
                            json!({"h4rn_mcp_grants": "h4rn-mcp-grants-1"}).to_string(),
                        ))
                        .await
                        .map_err(|e| e.to_string())?;
                }
                Some(ClientEvent::Frame(frame)) => {
                    let value: Value = serde_json::from_str(frame.as_text().unwrap_or("null"))
                        .map_err(|e| e.to_string())?;
                    return if value.get("grants").is_some() {
                        Ok(value)
                    } else {
                        Err(value.to_string())
                    };
                }
                Some(ClientEvent::Closed { reason }) => return Err(format!("closed: {reason}")),
                Some(_) => {}
                None => return Err("client stopped".into()),
            }
        }
    })
    .await
    .unwrap_or_else(|_| Err("timed out".into()));
    client.close().await;
    outcome
}

#[tokio::test]
async fn a_registered_peer_daemon_can_open_the_grants_handshake() {
    let rig = tunnel_rig(
        |devices, host, key| {
            devices
                .register_host(host.clone(), key, Some("dev_b".into()))
                .expect("persist");
        },
        McpSharePolicy::default(),
        None,
        None,
    )
    .await;
    let grants = open_grants(&rig).await.expect("peer daemon is admitted");
    let servers: Vec<&str> = grants["grants"]
        .as_array()
        .expect("grants array")
        .iter()
        .filter_map(|grant| grant["server"].as_str())
        .collect();
    assert_eq!(servers, ["local"], "machine comes from the key: {grants}");
    rig.a.stop().await;
}

#[tokio::test]
async fn a_view_only_client_key_cannot_open_the_grants_handshake() {
    let rig = tunnel_rig(
        |devices, _host, key| {
            devices
                .insert_scoped("phone", key, vec![h4rn_protocol::PairScope::View])
                .expect("persist");
        },
        McpSharePolicy::default(),
        None,
        None,
    )
    .await;
    let refused = open_grants(&rig).await.expect_err("a client is not a peer");
    assert!(!refused.contains("local"), "no grant may leak: {refused}");
    rig.a.stop().await;
}

#[tokio::test]
async fn a_key_with_no_registry_entry_cannot_open_the_grants_handshake() {
    let rig = tunnel_rig(|_, _, _| {}, McpSharePolicy::default(), None, None).await;
    assert!(open_grants(&rig).await.is_err());
    rig.a.stop().await;
}

#[tokio::test]
async fn a_registered_peer_daemon_can_use_its_grant() {
    let rig = tunnel_rig(
        |devices, host, key| {
            devices
                .register_host(host.clone(), key, Some("dev_b".into()))
                .expect("persist");
        },
        McpSharePolicy::default(),
        None,
        None,
    )
    .await;
    let tools = list_tools_over(&rig)
        .await
        .expect("peer daemon is admitted");
    assert!(tools["tools"].as_array().is_some_and(|t| !t.is_empty()));
    rig.a.stop().await;
}

#[tokio::test]
async fn a_device_limited_grant_matches_the_registered_peer_device_id() {
    // F3: `allowed_devices` compares the id the *owner* recorded for the peer
    // daemon's connect device, not a caller-supplied claim. The tunnel below
    // claims no device id at all.
    let policy = McpSharePolicy {
        allowed_devices: Some(vec!["dev_b".into()]),
        ..McpSharePolicy::default()
    };
    let rig = tunnel_rig(
        |devices, host, key| {
            devices
                .register_host(host.clone(), key, Some("dev_b".into()))
                .expect("persist");
        },
        policy,
        None,
        None,
    )
    .await;
    let tools = list_tools_over(&rig)
        .await
        .expect("the registered device id is admitted");
    assert!(tools["tools"].as_array().is_some_and(|t| !t.is_empty()));
    rig.a.stop().await;
}

#[tokio::test]
async fn a_confirmable_share_waits_for_the_owner_before_running() {
    let policy = McpSharePolicy {
        confirm_all: true,
        ..McpSharePolicy::default()
    };
    let rig = tunnel_rig(
        |devices, host, key| {
            devices
                .register_host(host.clone(), key, Some("dev_b".into()))
                .expect("persist");
        },
        policy,
        None,
        None,
    )
    .await;
    let session = rig.tunnel.open("local").await.expect("open tunnel");

    let call = {
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            session
                .request(
                    "tools/call",
                    json!({"name": "echo", "arguments": {"message": "needs approval"}}),
                    Duration::from_secs(10),
                )
                .await
        })
    };

    // The owner's production gate raised an approval instead of denying.
    let approval = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rig
                .a
                .mcp
                .handle(McpRequest::PendingApprovals)
                .await
                .expect("list approvals")
            {
                ResponseBody::McpApprovals { approvals } if !approvals.is_empty() => {
                    break approvals[0].clone();
                }
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("an approval should be pending");
    assert_eq!(approval.server, "local");
    assert_eq!(approval.tool, "echo");
    assert!(
        approval.arguments_preview.contains("needs approval"),
        "the card must show what will run: {}",
        approval.arguments_preview
    );

    let response = rig
        .a
        .mcp
        .handle(McpRequest::RespondApproval {
            approval_id: approval.approval_id.clone(),
            decision: ApprovalDecision::Approve,
        })
        .await
        .expect("approve");
    assert!(matches!(response, ResponseBody::Ack));
    let result = call.await.expect("call task").expect("approved call runs");
    assert_eq!(result["content"][0]["text"], "owner:needs approval");

    // A denied call fails closed and is never executed.
    let denied = {
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            session
                .request(
                    "tools/call",
                    json!({"name": "echo", "arguments": {"message": "denied"}}),
                    Duration::from_secs(10),
                )
                .await
        })
    };
    let second = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rig
                .a
                .mcp
                .handle(McpRequest::PendingApprovals)
                .await
                .expect("list approvals")
            {
                ResponseBody::McpApprovals { approvals } if !approvals.is_empty() => {
                    break approvals[0].clone();
                }
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("a second approval should be pending");
    rig.a
        .mcp
        .handle(McpRequest::RespondApproval {
            approval_id: second.approval_id,
            decision: ApprovalDecision::Deny,
        })
        .await
        .expect("deny");
    assert!(
        denied.await.expect("deny task").is_err(),
        "a denied shared call must not run"
    );

    rig.a.stop().await;
}

#[tokio::test]
async fn a_view_only_client_key_cannot_use_the_tunnel() {
    // The key completes the Noise handshake (it is on the allowlist) but it is
    // a user device with `view`, not a peer daemon.
    let rig = tunnel_rig(
        |devices, _host, key| {
            devices
                .insert_scoped("phone", key, vec![h4rn_protocol::PairScope::View])
                .expect("persist");
        },
        McpSharePolicy::default(),
        None,
        None,
    )
    .await;
    assert!(list_tools_over(&rig).await.is_err());
    rig.a.stop().await;
}

#[tokio::test]
async fn a_key_with_no_registry_entry_cannot_use_the_tunnel() {
    let rig = tunnel_rig(|_, _, _| {}, McpSharePolicy::default(), None, None).await;
    assert!(list_tools_over(&rig).await.is_err());
    rig.a.stop().await;
}

#[tokio::test]
async fn a_spoofed_machine_or_device_claim_is_refused() {
    let register = |devices: &DeviceRegistry, host: &HostId, key: [u8; 32]| {
        devices
            .register_host(host.clone(), key, Some("dev_b".into()))
            .expect("persist");
    };
    // Claims to be another machine that holds a grant.
    let rig = tunnel_rig(register, McpSharePolicy::default(), Some("host_c"), None).await;
    assert!(list_tools_over(&rig).await.is_err());
    rig.a.stop().await;
    // Claims another device id than the one the key is registered under.
    let rig = tunnel_rig(register, McpSharePolicy::default(), None, Some("dev_admin")).await;
    assert!(list_tools_over(&rig).await.is_err());
    rig.a.stop().await;
}

#[tokio::test]
async fn an_explicit_device_list_compares_the_verified_device_only() {
    let register = |devices: &DeviceRegistry, host: &HostId, key: [u8; 32]| {
        devices
            .register_host(host.clone(), key, Some("dev_b".into()))
            .expect("persist");
    };
    let allowed = McpSharePolicy {
        allowed_devices: Some(vec!["dev_b".into()]),
        ..McpSharePolicy::default()
    };
    let rig = tunnel_rig(register, allowed, None, None).await;
    let session = rig.tunnel.open("local").await.expect("open");
    let called = session
        .request(
            "tools/call",
            json!({"name": "echo", "arguments": {"message": "hi"}}),
            Duration::from_secs(5),
        )
        .await
        .expect("verified device in the list");
    assert_eq!(called["isError"], false);
    rig.a.stop().await;

    let other = McpSharePolicy {
        allowed_devices: Some(vec!["dev_other".into()]),
        ..McpSharePolicy::default()
    };
    let rig = tunnel_rig(register, other, None, None).await;
    let session = rig.tunnel.open("local").await.expect("open");
    assert!(
        session
            .request(
                "tools/call",
                json!({"name": "echo", "arguments": {"message": "hi"}}),
                Duration::from_secs(5),
            )
            .await
            .is_err()
    );
    rig.a.stop().await;
}

#[tokio::test]
async fn a_persistent_tunnel_revalidates_resources_and_prompts() {
    // K1: resources/read and prompts/get used the scope fixed at handshake
    // time, so an excluded device could read them and a tunnel kept reading
    // after the owner unshared.
    fn register(devices: &DeviceRegistry, host: &HostId, key: [u8; 32]) {
        devices
            .register_host(host.clone(), key, Some("dev_b".into()))
            .expect("persist");
    }
    let others = McpSharePolicy {
        allowed_devices: Some(vec!["dev_other".into()]),
        ..McpSharePolicy::default()
    };
    let rig = tunnel_rig(register, others, None, None).await;
    let session = rig.tunnel.open("local").await.expect("open tunnel");
    for (method, params) in [
        ("resources/list", json!({})),
        (
            "resources/read",
            json!({"uri": "secret://account"}),
        ),
        ("prompts/list", json!({})),
        ("prompts/get", json!({"name": "brief"})),
    ] {
        assert!(
            session
                .request(method, params, Duration::from_secs(5))
                .await
                .is_err(),
            "{method} must be refused for an excluded device"
        );
    }
    assert!(
        session
            .request(
                "tools/call",
                json!({"name": "echo", "arguments": {"message": "hi"}}),
                Duration::from_secs(5),
            )
            .await
            .is_err(),
        "tools/call must stay refused"
    );
    rig.a.stop().await;

    // A live policy change and an unshare both apply to the same tunnel.
    let rig = tunnel_rig(register, McpSharePolicy::default(), None, None).await;
    let mut owner = Client::connect(&rig.a).await;
    owner.hello().await;
    let session = rig.tunnel.open("local").await.expect("open tunnel");
    let leaked = session
        .request(
            "resources/read",
            json!({"uri": "secret://account"}),
            Duration::from_secs(5),
        )
        .await
        .expect("a permitted device reads the resource");
    assert_eq!(leaked["contents"][0]["text"], "fixture confidential data");

    owner
        .mcp(McpRequest::Share {
            name: "local".into(),
            to_machine: HostId::new("host_b"),
            policy: McpSharePolicy {
                allowed_devices: Some(vec!["dev_other".into()]),
                ..McpSharePolicy::default()
            },
        })
        .await
        .expect("narrow the grant");
    for (method, params) in [
        (
            "resources/read",
            json!({"uri": "secret://account"}),
        ),
        ("prompts/get", json!({"name": "brief"})),
    ] {
        assert!(
            session
                .request(method, params, Duration::from_secs(5))
                .await
                .is_err(),
            "{method} must honour a policy change on an open tunnel"
        );
    }

    owner
        .mcp(McpRequest::Unshare {
            name: "local".into(),
            machine: HostId::new("host_b"),
        })
        .await
        .expect("unshare");
    for (method, params) in [
        ("tools/list", json!({})),
        ("resources/list", json!({})),
        (
            "resources/read",
            json!({"uri": "secret://account"}),
        ),
        ("prompts/list", json!({})),
        ("prompts/get", json!({"name": "brief"})),
    ] {
        assert!(
            session
                .request(method, params, Duration::from_secs(5))
                .await
                .is_err(),
            "{method} must be refused after unshare"
        );
    }
    rig.a.stop().await;
}

/// Opens the owner's UI connection, subscribed so approval cards arrive.
async fn owner_ui(daemon: &TestDaemon) -> Client {    let mut owner = Client::connect(daemon).await;
    owner.hello().await;
    owner
        .request(ClientRequest::Subscribe {
            topics: vec![Topic::Mcp],
            since_seq: None,
        })
        .await
        .expect("subscribe to MCP events");
    owner
}

/// Waits for the card the owner's daemon raised for this tunnel's call.
async fn next_approval(owner: &mut Client) -> h4rn_protocol::McpShareApproval {
    match owner
        .wait_event(|event| matches!(event, Event::McpShareApprovalRequested { .. }))
        .await
    {
        Event::McpShareApprovalRequested { approval } => approval,
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn unsharing_during_an_approval_withdraws_the_card_and_refuses_the_call() {
    // I1: reading the grant before the wait let an answer to the stale card
    // run a tool after the owner had already unshared its server.
    let policy = McpSharePolicy {
        confirm_all: true,
        ..McpSharePolicy::default()
    };
    let rig = tunnel_rig(
        |devices, host, key| {
            devices
                .register_host(host.clone(), key, Some("dev_b".into()))
                .expect("persist");
        },
        policy,
        None,
        None,
    )
    .await;
    let mut owner = owner_ui(&rig.a).await;
    let session = rig.tunnel.open("local").await.expect("open tunnel");

    let call = {
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            session
                .request(
                    "tools/call",
                    json!({"name": "echo", "arguments": {"message": "after unshare"}}),
                    Duration::from_secs(10),
                )
                .await
        })
    };
    let approval = next_approval(&mut owner).await;

    owner
        .mcp(McpRequest::Unshare {
            name: "local".into(),
            machine: HostId::new("host_b"),
        })
        .await
        .expect("unshare while the card is up");

    // The card is withdrawn through the event the UI listens for.
    match owner
        .wait_event(|event| matches!(event, Event::McpShareApprovalResolved { .. }))
        .await
    {
        Event::McpShareApprovalResolved {
            approval_id,
            decision,
        } => {
            assert_eq!(approval_id, approval.approval_id);
            assert_eq!(decision, ApprovalDecision::Cancel);
        }
        other => panic!("unexpected event: {other:?}"),
    }
    // The call never ran, and there is nothing left to approve.
    assert!(
        call.await.expect("call task").is_err(),
        "an unshared call must not run"
    );
    match rig
        .a
        .mcp
        .handle(McpRequest::PendingApprovals)
        .await
        .expect("list approvals")
    {
        ResponseBody::McpApprovals { approvals } => {
            assert!(
                approvals.is_empty(),
                "the withdrawn card is gone: {approvals:?}"
            )
        }
        other => panic!("unexpected approvals response: {other:?}"),
    }
    rig.a.stop().await;
}

#[tokio::test]
async fn a_policy_change_that_drops_the_tool_during_an_approval_refuses_the_call() {
    let policy = McpSharePolicy {
        allowed_tools: Some(vec!["echo".into()]),
        confirm_all: true,
        ..McpSharePolicy::default()
    };
    let rig = tunnel_rig(
        |devices, host, key| {
            devices
                .register_host(host.clone(), key, Some("dev_b".into()))
                .expect("persist");
        },
        policy,
        None,
        None,
    )
    .await;
    let mut owner = owner_ui(&rig.a).await;
    let session = rig.tunnel.open("local").await.expect("open tunnel");

    let call = {
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            session
                .request(
                    "tools/call",
                    json!({"name": "echo", "arguments": {"message": "dropped"}}),
                    Duration::from_secs(10),
                )
                .await
        })
    };
    let approval = next_approval(&mut owner).await;

    // The owner narrows the grant while the approval waits: `echo` is gone.
    owner
        .mcp(McpRequest::Share {
            name: "local".into(),
            to_machine: HostId::new("host_b"),
            policy: McpSharePolicy {
                allowed_tools: Some(vec!["other".into()]),
                ..McpSharePolicy::default()
            },
        })
        .await
        .expect("replace the grant");

    match owner
        .wait_event(|event| matches!(event, Event::McpShareApprovalResolved { .. }))
        .await
    {
        Event::McpShareApprovalResolved {
            approval_id,
            decision,
        } => {
            assert_eq!(approval_id, approval.approval_id);
            assert_eq!(decision, ApprovalDecision::Cancel);
        }
        other => panic!("unexpected event: {other:?}"),
    }
    assert!(
        call.await.expect("call task").is_err(),
        "a tool dropped from the grant must not run"
    );
    rig.a.stop().await;
}

#[tokio::test]
async fn one_pending_approval_does_not_block_the_next_call_on_the_tunnel() {
    // I2: the owner loop awaited the approval, so the next tools/call on the
    // same tunnel sat unanswered behind it.
    let policy = McpSharePolicy {
        confirm_all: true,
        ..McpSharePolicy::default()
    };
    let rig = tunnel_rig(
        |devices, host, key| {
            devices
                .register_host(host.clone(), key, Some("dev_b".into()))
                .expect("persist");
        },
        policy,
        None,
        None,
    )
    .await;
    let mut owner = owner_ui(&rig.a).await;
    let session = rig.tunnel.open("local").await.expect("open tunnel");

    let first = {
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            session
                .request(
                    "tools/call",
                    json!({"name": "echo", "arguments": {"message": "first waits"}}),
                    Duration::from_secs(10),
                )
                .await
        })
    };
    let first_card = next_approval(&mut owner).await;
    assert!(first_card.arguments_preview.contains("first waits"));

    // The second call is read, admitted and raises its own card while the
    // first is still waiting for the owner.
    let second = {
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            session
                .request(
                    "tools/call",
                    json!({"name": "echo", "arguments": {"message": "second runs"}}),
                    Duration::from_secs(10),
                )
                .await
        })
    };
    let second_card = next_approval(&mut owner).await;
    assert_eq!(second_card.tool, "echo");
    assert!(second_card.arguments_preview.contains("second runs"));
    assert_ne!(second_card.approval_id, first_card.approval_id);

    rig.a
        .mcp
        .handle(McpRequest::RespondApproval {
            approval_id: second_card.approval_id,
            decision: ApprovalDecision::Approve,
        })
        .await
        .expect("approve the second call");
    let ran = second.await.expect("second call task").expect("approved");
    assert_eq!(ran["content"][0]["text"], "owner:second runs");

    // The first card is still pending until the owner answers it.
    match rig
        .a
        .mcp
        .handle(McpRequest::PendingApprovals)
        .await
        .expect("list approvals")
    {
        ResponseBody::McpApprovals { approvals } => {
            assert_eq!(approvals.len(), 1);
            assert_eq!(approvals[0].approval_id, first_card.approval_id);
        }
        other => panic!("unexpected approvals response: {other:?}"),
    }
    rig.a
        .mcp
        .handle(McpRequest::RespondApproval {
            approval_id: first_card.approval_id,
            decision: ApprovalDecision::Deny,
        })
        .await
        .expect("deny the first call");
    assert!(first.await.expect("first call task").is_err());
    rig.a.stop().await;
}

#[tokio::test]
async fn dropping_the_caller_withdraws_its_pending_approval() {
    // I2: a pending card outlived the tunnel that raised it, so the owner
    // could answer a call whose caller was gone.
    let policy = McpSharePolicy {
        confirm_all: true,
        ..McpSharePolicy::default()
    };
    let rig = tunnel_rig(
        |devices, host, key| {
            devices
                .register_host(host.clone(), key, Some("dev_b".into()))
                .expect("persist");
        },
        policy,
        None,
        None,
    )
    .await;
    let mut owner = owner_ui(&rig.a).await;
    let session = rig.tunnel.open("local").await.expect("open tunnel");

    let call = {
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            session
                .request(
                    "tools/call",
                    json!({"name": "echo", "arguments": {"message": "caller hangs up"}}),
                    Duration::from_secs(30),
                )
                .await
        })
    };
    let approval = next_approval(&mut owner).await;

    // The caller drops its tunnel while the owner is deciding.
    session.shutdown().await;

    // The card is withdrawn through the event the UI listens for.
    match owner
        .wait_event(|event| matches!(event, Event::McpShareApprovalResolved { .. }))
        .await
    {
        Event::McpShareApprovalResolved {
            approval_id,
            decision,
        } => {
            assert_eq!(approval_id, approval.approval_id);
            assert_eq!(decision, ApprovalDecision::Cancel);
        }
        other => panic!("unexpected event: {other:?}"),
    }
    match rig
        .a
        .mcp
        .handle(McpRequest::PendingApprovals)
        .await
        .expect("list approvals")
    {
        ResponseBody::McpApprovals { approvals } => {
            assert!(approvals.is_empty(), "the card must be gone: {approvals:?}")
        }
        other => panic!("unexpected approvals response: {other:?}"),
    }
    assert!(
        call.await.expect("call task").is_err(),
        "the dropped caller does not get a result"
    );
    rig.a.stop().await;
}
