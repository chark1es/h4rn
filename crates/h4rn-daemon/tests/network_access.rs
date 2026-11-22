//! End-to-end tests for the daemon-owned network listener.
//!
//! The listener is a second axum server on a dedicated wildcard port. It must
//! serve liveness and the Noise routes, never the plaintext WebSocket or the
//! MCP endpoint, and it must start and stop without restarting the daemon.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use h4rn_core::LifecycleSettings;
use h4rn_daemon::auth::TokenStore;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::network_access::NetworkAccess;
use h4rn_daemon::remote::TailscaleMode;
use h4rn_daemon::threads::ThreadManagerOptions;
use h4rn_harness::FakeHarness;
use h4rn_protocol::{
    ClientFrame, ClientRequest, DeviceId, DeviceInfo, ProtoError, ResponseBody, ServerFrame,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

struct TestDaemon {
    addr: std::net::SocketAddr,
    token: String,
    network: Arc<NetworkAccess>,
    _dir: tempfile::TempDir,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl TestDaemon {
    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

/// Boots a daemon whose primary listener is loopback-only.
///
/// `network_enabled` resolves the persisted setting at build time; `port` is
/// the network listener's configured port (`0` picks an ephemeral one).
async fn boot(network_enabled: bool, port: u16) -> TestDaemon {
    boot_in(tempfile::tempdir().expect("tempdir"), network_enabled, port).await
}

/// [`boot`] in a data directory the caller may have seeded.
async fn boot_in(dir: tempfile::TempDir, network_enabled: bool, port: u16) -> TestDaemon {
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
        log_level: "off".to_owned(),
        tailscale: TailscaleMode::Off,
        network_access: Some(network_enabled),
        network_port: port,
    };
    let options = ThreadManagerOptions {
        settings: LifecycleSettings::default()
            .with_working_exit_grace(chrono::TimeDelta::milliseconds(80))
            .with_stale_bypass(chrono::TimeDelta::seconds(60)),
        tick_interval: Duration::from_millis(20),
        clock: None,
    };
    let daemon = DaemonBuilder::new(config)
        // Titles never spawn the machine's own claude or codex.
        .without_title_agents()
        .with_harness(Arc::new(FakeHarness::new()))
        .with_lifecycle(options)
        .build()
        .await
        .expect("build daemon");
    let token = TokenStore::new(dir.path())
        .load()
        .expect("read token")
        .expect("token exists");
    let bound = daemon.bind().await.expect("bind daemon");
    let addr = bound.local_addr();
    let network = bound.network();
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
        network,
        _dir: dir,
        shutdown: Some(shutdown),
        task,
    }
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl Client {
    async fn connect(daemon: &TestDaemon) -> Self {
        let url = format!("ws://{}/ws?token={}", daemon.addr, daemon.token);
        let (ws, _) = connect_async(url).await.expect("connect /ws");
        let mut client = Self { ws, next_id: 1 };
        let hello = ClientRequest::Hello {
            protocol_version: h4rn_protocol::PROTOCOL_VERSION,
            device: DeviceInfo {
                device_id: DeviceId::new("dev_local"),
                name: "network access test".into(),
                platform: "linux".into(),
                app_version: "0.1.0".into(),
            },
        };
        client.request(hello).await.expect("hello");
        client
    }

    async fn request(&mut self, body: ClientRequest) -> Result<ResponseBody, ProtoError> {
        let id = self.next_id;
        self.next_id += 1;
        let text = serde_json::to_string(&ClientFrame { id, body }).expect("serialise");
        self.ws.send(text.into()).await.expect("send frame");
        loop {
            let message = tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("timed out waiting for a reply")
                .expect("socket closed")
                .expect("socket error");
            let text = match message {
                tokio_tungstenite::tungstenite::Message::Text(text) => text,
                _ => continue,
            };
            let frame: ServerFrame = serde_json::from_str(&text).expect("parse frame");
            if let ServerFrame::Response {
                id: response_id,
                result,
            } = frame
                && response_id == id
            {
                return result;
            }
        }
    }
}

/// A plain HTTP status line, for probes that are not WebSocket upgrades.
async fn http_status(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect the network listener");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .expect("write request");
    let mut body = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut body))
        .await
        .expect("timed out reading the response")
        .expect("read response");
    String::from_utf8_lossy(&body)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn the_network_listener_serves_healthz_and_noise_routes_only() {
    let daemon = boot(true, 0).await;
    let snapshot = daemon.network.snapshot();
    assert!(snapshot.enabled);
    assert!(snapshot.listening, "network access must bind at startup");
    assert!(snapshot.port > 0);
    assert!(snapshot.error.is_none());
    let port = snapshot.port;

    assert!(
        http_status(port, "/healthz").await.contains("200"),
        "liveness must be reachable on the network port"
    );
    // `/secure` exists (it rejects the missing upgrade, not the route).
    let secure = http_status(port, "/secure").await;
    assert!(
        !secure.contains("404"),
        "the noise route must be registered: {secure}"
    );
    assert!(
        secure.contains("400"),
        "a plain GET is not an upgrade: {secure}"
    );
    // Plain `/ws` and the token-bearing `/mcp` hub stay loopback-only.
    let ws = http_status(port, "/ws").await;
    assert!(ws.contains("404"), "plain /ws must not exist: {ws}");
    assert!(
        http_status(port, "/mcp").await.contains("404"),
        "the MCP hub must not be exposed on the LAN"
    );

    // The same refusal over a real WebSocket client.
    match connect_async(format!("ws://127.0.0.1:{port}/ws")).await {
        Err(WsError::Http(response)) => assert_eq!(response.status(), 404),
        other => panic!("a LAN client must not open /ws: {other:?}"),
    }

    daemon.stop().await;
}

#[tokio::test]
async fn network_access_turns_off_and_on_without_a_restart() {
    let daemon = boot(true, 0).await;
    let mut client = Client::connect(&daemon).await;
    let ResponseBody::NetworkAccess {
        enabled,
        listening,
        port,
        addresses,
        ..
    } = client
        .request(ClientRequest::GetNetworkAccess)
        .await
        .expect("get network access")
    else {
        panic!("unexpected response");
    };
    assert!(enabled && listening);
    assert!(
        addresses
            .iter()
            .all(|address| address.ends_with(&format!(":{port}")))
    );
    assert!(http_status(port, "/healthz").await.contains("200"));

    let ResponseBody::NetworkAccess {
        enabled,
        listening,
        addresses,
        error,
        ..
    } = client
        .request(ClientRequest::SetNetworkAccess { enabled: false })
        .await
        .expect("disable network access")
    else {
        panic!("unexpected response");
    };
    assert!(!enabled);
    assert!(!listening);
    assert!(addresses.is_empty(), "an off listener has nothing to dial");
    assert!(error.is_none());
    assert!(
        TcpStream::connect(("127.0.0.1", port)).await.is_err(),
        "the old port must be closed"
    );

    let ResponseBody::NetworkAccess {
        enabled,
        listening,
        port: new_port,
        ..
    } = client
        .request(ClientRequest::SetNetworkAccess { enabled: true })
        .await
        .expect("enable network access")
    else {
        panic!("unexpected response");
    };
    assert!(enabled && listening);
    assert!(http_status(new_port, "/healthz").await.contains("200"));

    client.ws.close(None).await.ok();
    daemon.stop().await;
}

#[tokio::test]
async fn direct_pairing_is_refused_while_network_access_is_off() {
    let daemon = boot(false, 0).await;
    let mut client = Client::connect(&daemon).await;
    let error = client
        .request(ClientRequest::CreateDirectPairing {
            scopes: Vec::new(),
            label: None,
        })
        .await
        .expect_err("a pairing needs a reachable listener");
    assert!(error.message.contains("Turn on network access first"));
    assert!(error.hint.unwrap_or_default().contains("network-access on"));
    daemon.stop().await;
}

#[tokio::test]
async fn a_bind_failure_is_reported_and_retried() {
    // Hold the port so the wildcard bind cannot succeed.
    let blocker = std::net::TcpListener::bind("0.0.0.0:0").expect("reserve a port");
    let port = blocker.local_addr().expect("blocker address").port();

    let daemon = boot(true, port).await;
    let snapshot = daemon.network.snapshot();
    assert!(snapshot.enabled, "the setting stays on");
    assert!(!snapshot.listening);
    assert!(
        snapshot.error.is_some(),
        "a bind failure must be reported, not hidden"
    );
    // The loopback listener is unaffected.
    let mut client = Client::connect(&daemon).await;
    assert!(matches!(
        client
            .request(ClientRequest::GetNetworkAccess)
            .await
            .expect("daemon still answers"),
        ResponseBody::NetworkAccess {
            listening: false,
            ..
        }
    ));

    // Free the port and retry: enabling again rebinds without a restart.
    drop(blocker);
    let ResponseBody::NetworkAccess {
        listening,
        error,
        port: bound,
        ..
    } = client
        .request(ClientRequest::SetNetworkAccess { enabled: true })
        .await
        .expect("retry")
    else {
        panic!("unexpected response");
    };
    assert!(listening, "a retry must bind: {error:?}");
    assert!(error.is_none());
    assert!(http_status(bound, "/healthz").await.contains("200"));

    daemon.stop().await;
}

#[tokio::test]
async fn directly_paired_devices_are_listed_and_revocable() {
    use h4rn_daemon::remote::DeviceRegistry;
    use h4rn_protocol::PairScope;

    let dir = tempfile::tempdir().expect("tempdir");
    let devices_path = dir.path().join("devices.json");
    let phone = [7u8; 32];
    let connect_phone = [9u8; 32];
    {
        let registry = DeviceRegistry::open(&devices_path);
        registry
            .insert_direct(phone, vec![PairScope::View, PairScope::Approve])
            .expect("seed a direct device");
        registry
            .insert_scoped("dev_connect", connect_phone, vec![PairScope::View])
            .expect("seed a connect device");
    }
    let daemon = boot_in(dir, false, 0).await;
    let mut client = Client::connect(&daemon).await;

    // Only the direct pairing is listed: connect devices have their own page.
    let listed = client
        .request(ClientRequest::ListDirectDevices)
        .await
        .expect("list direct devices");
    let ResponseBody::DirectDevices { devices } = listed else {
        panic!("unexpected response: {listed:?}");
    };
    assert_eq!(devices.len(), 1, "{devices:?}");
    assert_eq!(devices[0].key, hex::encode(phone));
    assert!(devices[0].paired_at.is_some());
    assert!(!devices[0].connected);

    // A connect device cannot be revoked through the direct path.
    let error = client
        .request(ClientRequest::RevokeDirectDevice {
            key: hex::encode(connect_phone),
        })
        .await
        .expect_err("connect devices are not direct");
    assert_eq!(error.code, h4rn_protocol::ProtoErrorCode::NotFound);

    let error = client
        .request(ClientRequest::RevokeDirectDevice {
            key: "not hex".into(),
        })
        .await
        .expect_err("a malformed key is rejected");
    assert_eq!(error.code, h4rn_protocol::ProtoErrorCode::InvalidRequest);

    let revoked = client
        .request(ClientRequest::RevokeDirectDevice {
            key: hex::encode(phone),
        })
        .await
        .expect("revoke the direct device");
    assert_eq!(revoked, ResponseBody::DirectDevices { devices: vec![] });

    // The removal is durable, and the connect device is untouched.
    let reopened = DeviceRegistry::open(daemon._dir.path().join("devices.json"));
    assert!(reopened.direct_devices().is_empty());
    assert!(reopened.keys().contains(&connect_phone));
    assert!(!reopened.keys().contains(&phone));

    daemon.stop().await;
}
