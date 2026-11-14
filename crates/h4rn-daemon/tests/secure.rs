//! End-to-end tests for the Noise-encrypted multipath transport.
//!
//! These boot a real daemon with `FakeHarness`, dial its `/secure` route with a
//! real WebSocket, and drive the same protocol requests the plain `/ws` route
//! accepts. Nothing is mocked below the Noise layer: the handshake, session
//! framing and daemon session registry are the shipping implementations.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use h4rn_core::LifecycleSettings;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::remote::DeviceRegistry;
use h4rn_daemon::threads::ThreadManagerOptions;
use h4rn_harness::FakeHarness;
use h4rn_protocol::{
    ClientFrame, ClientRequest, DeviceId, DeviceInfo, Event, HarnessKind, PairScope, ProtoError,
    ResponseBody, RuntimeMode, ServerFrame, ThreadSummary, TransportPathKind,
};
use h4rn_transport::client::{Client, ClientConfig, ClientEvent, ClientPath};
use h4rn_transport::engine::EngineConfig;
use h4rn_transport::keys::DeviceIdentity;
use h4rn_transport::server::{SecureOptions, SecureService};
use h4rn_transport::server_session::PairingPolicy;
use h4rn_transport::wire::{SESSION_ID_LEN, TOKEN_LEN};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

struct TestDaemon {
    addr: std::net::SocketAddr,
    service: Arc<SecureService>,
    devices: Arc<DeviceRegistry>,
    dir: Option<tempfile::TempDir>,
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

/// Boots with every key already on the transport allowlist registered as a
/// full-access direct device, the way a finished pairing would leave it.
async fn boot(service: Arc<SecureService>) -> TestDaemon {
    let devices = Arc::new(DeviceRegistry::in_memory());
    for key in service.allowed_devices() {
        devices.insert_direct(key, PairScope::ALL.to_vec());
    }
    boot_with(service, devices).await
}

async fn boot_with(service: Arc<SecureService>, devices: Arc<DeviceRegistry>) -> TestDaemon {
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
    };
    let daemon = DaemonBuilder::new(config)
        .with_harness(Arc::new(FakeHarness::new()))
        .with_lifecycle(options)
        .with_secure(Arc::clone(&service))
        .with_devices(Arc::clone(&devices))
        .build()
        .await
        .expect("build daemon");
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
        service,
        devices,
        dir: Some(dir),
        shutdown: Some(shutdown),
        task,
    }
}

struct SecureClient {
    client: Client,
    next_id: u64,
    pending: VecDeque<(u64, Event)>,
    ticket: Option<h4rn_transport::session::ResumeTicket>,
}

impl SecureClient {
    async fn connect(
        daemon: &TestDaemon,
        identity: DeviceIdentity,
        psk: Option<[u8; TOKEN_LEN]>,
    ) -> Self {
        let mut engine = EngineConfig::new(
            [11u8; SESSION_ID_LEN],
            identity,
            daemon.service.public_identity().noise_public,
        );
        engine.psk = psk;
        let paths = vec![ClientPath::websocket(
            TransportPathKind::Direct,
            format!("ws://{}/secure", daemon.addr),
        )];
        let mut client = Client::connect(ClientConfig::new(engine, paths)).expect("start client");
        let mut ticket = None;
        let established = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match client.recv().await {
                    Some(ClientEvent::Established(value)) => {
                        ticket = Some(value);
                        return;
                    }
                    Some(ClientEvent::Closed { reason }) => {
                        panic!("secure session closed during connect: {reason}")
                    }
                    Some(_) => {}
                    None => panic!("secure client stopped during connect"),
                }
            }
        })
        .await;
        assert!(
            established.is_ok(),
            "timed out waiting for the secure session"
        );
        Self {
            client,
            next_id: 1,
            pending: VecDeque::new(),
            ticket,
        }
    }

    async fn send(&mut self, body: ClientRequest) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let frame = ClientFrame { id, body };
        let text = serde_json::to_string(&frame).expect("serialise frame");
        self.client
            .send(h4rn_transport::AppFrame::text(text))
            .await
            .expect("send secure frame");
        id
    }

    async fn read_frame(&mut self) -> ServerFrame {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), self.client.recv())
                .await
                .expect("timed out waiting for a secure frame");
            match event {
                Some(ClientEvent::Frame(frame)) => {
                    let text = frame.as_text().expect("text frame");
                    return serde_json::from_str(text).expect("parse ServerFrame");
                }
                Some(ClientEvent::Closed { reason }) => {
                    panic!("secure session closed while waiting: {reason}")
                }
                Some(ClientEvent::Error { message, .. }) => {
                    panic!("secure transport error while waiting: {message}")
                }
                Some(ClientEvent::Status(_) | ClientEvent::Established(_)) => {}
                None => panic!("secure client stopped while waiting"),
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

    async fn hello(&mut self) -> ResponseBody {
        self.request(ClientRequest::Hello {
            protocol_version: h4rn_protocol::PROTOCOL_VERSION,
            device: DeviceInfo {
                device_id: DeviceId::new("dev_secure"),
                name: "secure integration test".into(),
                platform: "linux".into(),
                app_version: "0.1.0".into(),
            },
        })
        .await
        .expect("hello over secure transport")
    }

    async fn close(self) {
        self.client.close().await;
    }
}

fn device_identity() -> DeviceIdentity {
    DeviceIdentity::generate().expect("identity")
}

async fn create_thread_over_secure() -> ThreadSummary {
    let client_identity = device_identity();
    let daemon_identity = device_identity();
    let allowed = PairingPolicy::new([client_identity.public().noise_public]);
    let service = Arc::new(SecureService::new(
        SecureOptions::new(daemon_identity).with_policy(allowed),
    ));
    let daemon = boot(Arc::clone(&service)).await;

    let mut client = SecureClient::connect(&daemon, client_identity, None).await;
    client.hello().await;
    let host = match client
        .request(ClientRequest::ListHosts)
        .await
        .expect("hosts")
    {
        ResponseBody::Hosts { hosts } => hosts[0].host_id.clone(),
        other => panic!("unexpected host response: {other:?}"),
    };
    let response = client
        .request(ClientRequest::CreateThread {
            host,
            harness: HarnessKind::ClaudeCode,
            workspace_path: "/tmp/h4rn-secure-workspace".into(),
            model: "fake-model".into(),
            prompt: "created over noise".into(),
            attachments: Vec::new(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            worktree: None,
        })
        .await
        .expect("create thread over secure transport");
    let summary = match response {
        ResponseBody::ThreadCreated { summary, .. } => summary,
        other => panic!("unexpected CreateThread response: {other:?}"),
    };
    client.close().await;
    daemon.stop().await;
    summary
}

#[tokio::test]
async fn secure_transport_creates_a_thread() {
    let summary = create_thread_over_secure().await;
    assert_eq!(summary.harness, HarnessKind::ClaudeCode);
    assert!(!summary.thread_id.as_str().is_empty());
}

#[tokio::test]
async fn secure_transport_rejects_a_revoked_device() {
    let client_identity = device_identity();
    let client_key = client_identity.public().noise_public;
    let allowed = PairingPolicy::new([client_key]);
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_policy(allowed),
    ));
    service.revoke(client_key);
    let daemon = boot(Arc::clone(&service)).await;

    let mut engine = EngineConfig::new(
        [12u8; SESSION_ID_LEN],
        client_identity,
        service.public_identity().noise_public,
    );
    engine.psk = None;
    let paths = vec![ClientPath::websocket(
        TransportPathKind::Direct,
        format!("ws://{}/secure", daemon.addr),
    )];
    let mut client = Client::connect(ClientConfig::new(engine, paths)).expect("start client");

    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await {
                Some(ClientEvent::Closed { reason }) => return reason,
                Some(ClientEvent::Established(_)) => panic!("revoked device was admitted"),
                Some(_) => {}
                None => panic!("client stopped without a reason"),
            }
        }
    })
    .await
    .expect("timed out waiting for the rejection");
    assert!(
        outcome.contains("revoked") || outcome.contains("not paired"),
        "unexpected rejection reason: {outcome}"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_resumed_path_reattaches_without_forking_the_session() {
    let client_identity = device_identity();
    let allowed = PairingPolicy::new([client_identity.public().noise_public]);
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_policy(allowed),
    ));
    let daemon = boot(Arc::clone(&service)).await;

    // A first socket establishes the logical session and its application loop.
    let mut first = SecureClient::connect(&daemon, client_identity.clone(), None).await;
    first.hello().await;
    let ticket = first.ticket.expect("the daemon issues a resume ticket");
    assert_eq!(service.session_count(), 1);

    // The path drops (a dead LAN socket, a relay reconnect). The daemon keeps
    // the logical session and its application loop alive for the resume grace.
    first.close().await;
    for _ in 0..500 {
        if service.session_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        service.session_count(),
        1,
        "losing a path must not destroy the logical session"
    );

    // A new socket presents the same session id and resume token. It must
    // attach to the existing session rather than start a second application
    // loop, which would fork the outbound stream.
    let mut engine = EngineConfig::new(
        ticket.session_id,
        client_identity,
        service.public_identity().noise_public,
    );
    engine.resume = Some(ticket);
    let paths = vec![ClientPath::websocket(
        TransportPathKind::Tailscale,
        format!("ws://{}/secure", daemon.addr),
    )];
    let mut resumed =
        Client::connect(ClientConfig::new(engine, paths)).expect("start second client");
    let mut resumed_ticket = None;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match resumed.recv().await {
                Some(ClientEvent::Established(value)) => {
                    resumed_ticket = Some(value);
                    return;
                }
                Some(ClientEvent::Closed { reason }) => {
                    panic!("the resumed path was rejected: {reason}")
                }
                Some(_) => {}
                None => panic!("the second client stopped before resuming"),
            }
        }
    })
    .await
    .expect("timed out waiting for the resume");
    assert_eq!(
        service.session_count(),
        1,
        "a resumed path must not fork the logical session"
    );

    // The pre-existing application loop is still the one handling requests.
    let mut resumed = SecureClient {
        client: resumed,
        next_id: 1,
        pending: VecDeque::new(),
        ticket: resumed_ticket,
    };
    resumed.hello().await;
    resumed.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn secure_transport_psk_pairs_a_new_device() {
    let client_identity = device_identity();
    let daemon_identity = device_identity();
    let psk = [0x5au8; TOKEN_LEN];
    let service = Arc::new(SecureService::new(
        SecureOptions::new(daemon_identity).with_psk(psk),
    ));
    let daemon = boot(Arc::clone(&service)).await;

    let mut client = SecureClient::connect(&daemon, client_identity.clone(), Some(psk)).await;
    client.hello().await;
    assert!(
        service.is_allowed(&client_identity.public().noise_public),
        "PSK pairing must add the device to the allowlist"
    );
    client.close().await;
    daemon.stop().await;
}

fn create_thread_request(host: h4rn_protocol::HostId) -> ClientRequest {
    ClientRequest::CreateThread {
        host,
        harness: HarnessKind::ClaudeCode,
        workspace_path: "/tmp/h4rn-secure-workspace".into(),
        model: "fake-model".into(),
        prompt: "scope test".into(),
        attachments: Vec::new(),
        runtime_mode: RuntimeMode::ApprovalRequired,
        worktree: None,
    }
}

async fn host_of(client: &mut SecureClient) -> h4rn_protocol::HostId {
    match client
        .request(ClientRequest::ListHosts)
        .await
        .expect("hosts")
    {
        ResponseBody::Hosts { hosts } => hosts[0].host_id.clone(),
        other => panic!("unexpected host response: {other:?}"),
    }
}

impl SecureClient {
    async fn hello_result(&mut self) -> Result<ResponseBody, ProtoError> {
        self.request(ClientRequest::Hello {
            protocol_version: h4rn_protocol::PROTOCOL_VERSION,
            device: DeviceInfo {
                device_id: DeviceId::new("dev_secure"),
                name: "secure integration test".into(),
                platform: "linux".into(),
                app_version: "0.1.0".into(),
            },
        })
        .await
    }

    async fn send_pty_input(&mut self, stream_id: u32, data: &[u8]) {
        let frame = h4rn_protocol::BinaryFrame {
            kind: h4rn_protocol::BinaryKind::PtyInput,
            stream_id,
            seq: 0,
            payload: data.to_vec(),
        };
        self.client
            .send(h4rn_transport::AppFrame::binary(frame.encode()))
            .await
            .expect("send binary frame");
    }

    /// The next error event the daemon pushes (it answers refused binary frames
    /// with one, because they carry no request id).
    async fn next_error_event(&mut self) -> String {
        loop {
            if let Some(index) = self
                .pending
                .iter()
                .position(|(_, event)| matches!(event, Event::Error { .. }))
            {
                if let Some((_, Event::Error { message, .. })) = self.pending.remove(index) {
                    return message;
                }
            }
            if let ServerFrame::Event { seq, event } = self.read_frame().await {
                self.pending.push_back((seq, event));
            }
        }
    }
}

#[tokio::test]
async fn a_transport_key_without_a_registry_entry_is_refused() {
    // A key can complete the Noise handshake (it is on the transport
    // allowlist) and still have no recorded authorisation. That used to grant
    // unrestricted access; it must be denied.
    let client_identity = device_identity();
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity())
            .with_policy(PairingPolicy::new([client_identity.public().noise_public])),
    ));
    let daemon = boot_with(Arc::clone(&service), Arc::new(DeviceRegistry::in_memory())).await;
    let mut client = SecureClient::connect(&daemon, client_identity, None).await;
    let denied = client.hello_result().await.expect_err("must be denied");
    assert_eq!(denied.code, h4rn_protocol::ProtoErrorCode::Forbidden);
    client.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn a_relay_claim_cannot_replace_the_authenticated_key() {
    // The viewer's key is registered view-only; whatever device id a relay
    // might attach, the daemon resolves the identity from the key.
    let client_identity = device_identity();
    let key = client_identity.public().noise_public;
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_policy(PairingPolicy::new([key])),
    ));
    let devices = Arc::new(DeviceRegistry::in_memory());
    devices.insert_scoped("viewer", key, vec![PairScope::View]);
    devices.insert_scoped("admin", [0x77u8; 32], PairScope::ALL.to_vec());
    let daemon = boot_with(Arc::clone(&service), devices).await;
    let mut client = SecureClient::connect(&daemon, client_identity, None).await;
    client.hello().await;
    let host = host_of(&mut client).await;
    let denied = client
        .request(create_thread_request(host))
        .await
        .expect_err("view-only may not create threads");
    assert_eq!(denied.code, h4rn_protocol::ProtoErrorCode::Forbidden);
    client.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn a_view_only_device_cannot_type_into_a_terminal() {
    let client_identity = device_identity();
    let key = client_identity.public().noise_public;
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_policy(PairingPolicy::new([key])),
    ));
    let devices = Arc::new(DeviceRegistry::in_memory());
    devices.insert_scoped("viewer", key, vec![PairScope::View]);
    let daemon = boot_with(Arc::clone(&service), devices).await;
    let mut client = SecureClient::connect(&daemon, client_identity, None).await;
    client.hello().await;
    // Stream ids are sequential; this device did not open or attach anything.
    client.send_pty_input(1, b"touch pwned\n").await;
    let message = client.next_error_event().await;
    assert!(message.contains("terminal"), "{message}");
    client.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn a_direct_pairing_grants_exactly_the_requested_scopes() {
    // `h4rnd pair --direct --scope files` writes the scopes into the pairing
    // file; the daemon arms them, and the device that pairs holds only those.
    let client_identity = device_identity();
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_psk([1u8; TOKEN_LEN]),
    ));
    let daemon = boot_with(Arc::clone(&service), Arc::new(DeviceRegistry::in_memory())).await;
    let psk = [0x42u8; TOKEN_LEN];
    h4rn_daemon::remote::pairing::write_direct_pairing(
        daemon.dir.as_ref().unwrap().path(),
        &h4rn_daemon::remote::DirectPairingFile {
            psk: hex::encode(psk),
            expires_at: chrono::Utc::now().timestamp() + 120,
            endpoints: vec!["127.0.0.1:1".into()],
            label: None,
            scopes: vec![PairScope::View, PairScope::Files],
        },
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while service.psk() != Some(psk) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the daemon never armed the direct pairing");

    let mut client = SecureClient::connect(&daemon, client_identity.clone(), Some(psk)).await;
    client.hello().await;
    let host = host_of(&mut client).await;
    let denied = client
        .request(create_thread_request(host))
        .await
        .expect_err("approve was not granted");
    assert_eq!(denied.code, h4rn_protocol::ProtoErrorCode::Forbidden);
    let auth = daemon
        .devices
        .authorization(&client_identity.public().noise_public)
        .expect("recorded");
    assert_eq!(auth.provenance, h4rn_daemon::remote::Provenance::Direct);
    assert_eq!(auth.scopes, vec![PairScope::View, PairScope::Files]);
    client.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn the_pairing_secret_pairs_one_device_even_when_handshakes_race() {
    let psk = [0x33u8; TOKEN_LEN];
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_psk(psk),
    ));
    let daemon = boot(Arc::clone(&service)).await;
    let mut attempts = Vec::new();
    for index in 0..6u8 {
        let identity = device_identity();
        let addr = daemon.addr;
        let daemon_key = service.public_identity().noise_public;
        attempts.push(tokio::spawn(async move {
            let mut engine = EngineConfig::new([20 + index; SESSION_ID_LEN], identity, daemon_key);
            engine.psk = Some(psk);
            let paths = vec![ClientPath::websocket(
                TransportPathKind::Direct,
                format!("ws://{addr}/secure"),
            )];
            let mut client =
                Client::connect(ClientConfig::new(engine, paths)).expect("start client");
            let outcome = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match client.recv().await {
                        Some(ClientEvent::Established(_)) => return true,
                        Some(ClientEvent::Closed { .. }) | None => return false,
                        Some(_) => {}
                    }
                }
            })
            .await
            .unwrap_or(false);
            client.close().await;
            outcome
        }));
    }
    let mut established = 0;
    for attempt in attempts {
        if attempt.await.unwrap() {
            established += 1;
        }
    }
    assert_eq!(established, 1, "one QR must pair exactly one device");
    assert_eq!(service.allowed_devices().len(), 1);
    assert_eq!(service.psk(), None);
    daemon.stop().await;
}
