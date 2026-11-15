//! Cross-machine MCP sharing, end to end, with no hand-built tunnel.
//!
//! Two real daemons boot in-process with distinct host ids and their own
//! Noise identities. The owner shares a real stdio MCP server; the target
//! daemon discovers the grant over its own grants session, builds the tunnel
//! itself and registers the server, and its harness-facing `/mcp` endpoint can
//! call the tool. Unsharing (and revoking the peer) removes it again.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use h4rn_core::LifecycleSettings;
use h4rn_daemon::auth::TokenStore;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::mcp_peers::Peer;
use h4rn_daemon::threads::ThreadManagerOptions;
use h4rn_harness::FakeHarness;
use h4rn_mcp::session::McpScope;
use h4rn_protocol::{
    ClientFrame, ClientRequest, DeviceId, DeviceInfo, Event, HarnessKind, HostId, McpEnabledMatrix,
    McpRequest, McpServerInput, McpServerView, McpSharePolicy, McpTransportInput, ResponseBody,
    ServerFrame,
};
use h4rn_transport::keys::DeviceIdentity;
use h4rn_transport::server::{SecureOptions, SecureService};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

struct TestDaemon {
    addr: std::net::SocketAddr,
    token: String,
    _dir: tempfile::TempDir,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
    mcp: Arc<h4rn_daemon::mcp::McpService>,
    identity: DeviceIdentity,
    host: HostId,
}

impl TestDaemon {
    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }

    fn as_peer(&self) -> Peer {
        Peer {
            host: self.host.clone(),
            name: self.host.as_str().to_owned(),
            noise_public: self.identity.public().noise_public,
            endpoints: vec![self.addr.to_string()],
            machine_id: None,
            relay: None,
            device_id: None,
            online: true,
        }
    }
}

async fn boot(host: &str) -> TestDaemon {
    let dir = tempfile::tempdir().expect("tempdir");
    let identity = DeviceIdentity::generate().expect("identity");
    let secure = Arc::new(SecureService::new(SecureOptions::new(identity.clone())));
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Off,
    };
    let options = ThreadManagerOptions {
        settings: LifecycleSettings::default(),
        tick_interval: Duration::from_millis(20),
        clock: None,
    };
    let host = HostId::new(host);
    let daemon = DaemonBuilder::new(config)
        .with_harness(Arc::new(FakeHarness::new()))
        .with_lifecycle(options)
        .with_secure(secure)
        .with_local_host(host.clone(), host.as_str())
        .build()
        .await
        .expect("build daemon");
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
        _dir: dir,
        shutdown: Some(shutdown),
        task,
        mcp,
        identity,
        host,
    }
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
        let mut client = Self {
            ws,
            next_id: 1,
            pending: VecDeque::new(),
        };
        let hello = client
            .request(ClientRequest::Hello {
                protocol_version: h4rn_protocol::PROTOCOL_VERSION,
                device: DeviceInfo {
                    device_id: DeviceId::new("dev_peering"),
                    name: "peering test".into(),
                    platform: "linux".into(),
                    app_version: "0.1.0".into(),
                },
            })
            .await
            .expect("hello");
        assert!(matches!(hello, ResponseBody::Hello { .. }), "{hello:?}");
        client
    }

    async fn request(
        &mut self,
        body: ClientRequest,
    ) -> Result<ResponseBody, h4rn_protocol::ProtoError> {
        let id = self.next_id;
        self.next_id += 1;
        self.ws
            .send(Message::Text(
                serde_json::to_string(&ClientFrame { id, body })
                    .unwrap()
                    .into(),
            ))
            .await
            .expect("send frame");
        loop {
            let message = tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("timed out waiting for a response")
                .expect("socket closed")
                .expect("websocket error");
            if let Message::Text(text) = message {
                match serde_json::from_str::<ServerFrame>(&text).unwrap() {
                    ServerFrame::Response {
                        id: response_id,
                        result,
                    } if response_id == id => return result,
                    ServerFrame::Event { event, .. } => self.pending.push_back(event),
                    _ => {}
                }
            }
        }
    }

    async fn mcp(&mut self, request: McpRequest) -> ResponseBody {
        self.request(ClientRequest::Mcp { request })
            .await
            .expect("mcp request")
    }

    async fn servers(&mut self) -> Vec<McpServerView> {
        match self.mcp(McpRequest::ListServers).await {
            ResponseBody::McpServers { servers } => servers,
            other => panic!("unexpected list response: {other:?}"),
        }
    }
}

/// The stdio fixture is another package's binary, so build it on demand next
/// to the test binaries instead of relying on `CARGO_BIN_EXE_*`.
fn fixture_binary() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("test exe");
    let profile_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("target profile dir");
    let path = profile_dir.join(if cfg!(windows) {
        "mcp-fixture.exe"
    } else {
        "mcp-fixture"
    });
    if !path.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "-p", "h4rn-mcp", "--bin", "mcp-fixture"])
            .status()
            .expect("run cargo build for the MCP fixture");
        assert!(status.success(), "building the MCP fixture failed");
    }
    assert!(
        path.exists(),
        "missing fixture binary at {}",
        path.display()
    );
    path
}

/// One JSON-RPC call to a daemon's harness-facing `/mcp` endpoint.
async fn hub_rpc(daemon: &TestDaemon, method: &str, params: Value) -> Value {
    let hub = daemon.mcp.hub();
    let endpoint = hub.endpoint().await.expect("endpoint");
    let token = hub
        .issue_session(McpScope::harness(
            HarnessKind::ClaudeCode,
            daemon.host.clone(),
            None,
        ))
        .token;
    reqwest::Client::new()
        .post(endpoint)
        .bearer_auth(token)
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .send()
        .await
        .expect("hub request")
        .json()
        .await
        .expect("JSON")
}

async fn tool_names(daemon: &TestDaemon) -> Vec<String> {
    hub_rpc(daemon, "tools/list", json!({})).await["result"]["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Waits for a pushed grant to land. The product never polls; the test has to
/// wait for an asynchronous push.
async fn wait_for_tool(daemon: &TestDaemon, tool: &str, present: bool) {
    for _ in 0..200 {
        if tool_names(daemon).await.iter().any(|name| name == tool) == present {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "{tool} never became {} on {}: {:?}",
        if present { "visible" } else { "hidden" },
        daemon.host,
        tool_names(daemon).await
    );
}

#[tokio::test]
async fn a_shared_stdio_server_reaches_the_target_daemon_and_unshare_removes_it() {
    let a = boot("host_owner").await;
    let b = boot("host_target").await;
    // Each daemon knows the other as a peer (connect fills this in production).
    a.mcp.peers().replace_source("static", vec![b.as_peer()]);
    b.mcp.peers().replace_source("static", vec![a.as_peer()]);

    let mut a_client = Client::connect(&a).await;
    let upserted = a_client
        .mcp(McpRequest::Upsert {
            input: McpServerInput {
                name: "fixture".into(),
                transport: McpTransportInput::Stdio {
                    command: fixture_binary().to_string_lossy().into_owned(),
                    args: Vec::new(),
                    cwd: None,
                    env: Vec::new(),
                },
                tags: Vec::new(),
                enabled: McpEnabledMatrix::default(),
            },
        })
        .await;
    assert!(matches!(upserted, ResponseBody::McpServer { .. }));

    // Nothing is shared yet, so B sees nothing.
    assert!(
        !tool_names(&b)
            .await
            .iter()
            .any(|name| name == "fixture__echo")
    );

    a_client
        .mcp(McpRequest::Share {
            name: "fixture".into(),
            to_machine: b.host.clone(),
            policy: McpSharePolicy::default(),
        })
        .await;

    // B's harness side sees and can call the tool.
    wait_for_tool(&b, "fixture__echo", true).await;
    let called = hub_rpc(
        &b,
        "tools/call",
        json!({"name": "fixture__echo", "arguments": {"message": "across machines"}}),
    )
    .await;
    assert_eq!(called["result"]["content"][0]["text"], "across machines");
    assert_eq!(called["result"]["isError"], false);

    // B's McpList shows it as running on the owner.
    let mut b_client = Client::connect(&b).await;
    let listed = b_client.servers().await;
    let remote = listed
        .iter()
        .find(|server| server.name == "fixture")
        .expect("B lists the shared server");
    assert_eq!(remote.runs_on.as_ref(), Some(&a.host));
    assert!(remote.tool_count >= 1);

    // A policy change is pushed too: restrict the grant to a different tool.
    a_client
        .mcp(McpRequest::Share {
            name: "fixture".into(),
            to_machine: b.host.clone(),
            policy: McpSharePolicy {
                allowed_tools: Some(vec!["fail".into()]),
                ..McpSharePolicy::default()
            },
        })
        .await;
    // (The owner enforces the policy on every call.)
    let denied = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let reply = hub_rpc(
                &b,
                "tools/call",
                json!({"name": "fixture__echo", "arguments": {"message": "nope"}}),
            )
            .await;
            if reply.get("error").is_some() || reply["result"]["isError"] == true {
                return reply;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the owner never enforced the narrowed policy");
    assert!(denied.to_string().contains("not shared"), "{denied}");

    // Unshare removes it from B's hub and McpList.
    a_client
        .mcp(McpRequest::Unshare {
            name: "fixture".into(),
            machine: b.host.clone(),
        })
        .await;
    wait_for_tool(&b, "fixture__echo", false).await;
    assert!(
        !b_client
            .servers()
            .await
            .iter()
            .any(|server| server.name == "fixture"),
        "B still lists the unshared server"
    );

    // Share again, then revoke the peer itself: the binding goes away.
    a_client
        .mcp(McpRequest::Share {
            name: "fixture".into(),
            to_machine: b.host.clone(),
            policy: McpSharePolicy::default(),
        })
        .await;
    wait_for_tool(&b, "fixture__echo", true).await;
    a.mcp.peers().replace_source("static", Vec::new());
    wait_for_tool(&b, "fixture__echo", false).await;

    b.stop().await;
    a.stop().await;
}
