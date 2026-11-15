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
    BinaryFrame, BinaryKind, ClientFrame, ClientRequest, DeviceId, DeviceInfo, Event, HarnessKind,
    PairScope, ProtoError, ProtoErrorCode, ResponseBody, RuntimeMode, ServerFrame, ThreadSummary,
    TransportPathKind,
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
        devices
            .insert_direct(key, PairScope::ALL.to_vec())
            .expect("persist");
    }
    boot_with(service, devices).await
}

async fn boot_with(service: Arc<SecureService>, devices: Arc<DeviceRegistry>) -> TestDaemon {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    boot_at(path, service, devices, Some(dir)).await
}

/// Boots against an existing data directory, optionally owning its tempdir.
async fn boot_at(
    data_dir: std::path::PathBuf,
    service: Arc<SecureService>,
    devices: Arc<DeviceRegistry>,
    owned: Option<tempfile::TempDir>,
) -> TestDaemon {
    let config = Config {
        data_dir,
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
        dir: owned,
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

/// One frame the daemon sent over the secure session.
#[derive(Debug)]
enum SecureFrame {
    Text(Box<ServerFrame>),
    Binary(Vec<u8>),
}

impl SecureClient {
    async fn connect(
        daemon: &TestDaemon,
        identity: DeviceIdentity,
        psk: Option<[u8; TOKEN_LEN]>,
    ) -> Self {
        Self::connect_with_session(daemon, identity, psk, [11u8; SESSION_ID_LEN]).await
    }

    /// Connects with an explicit session byte, for a test that needs two
    /// simultaneous identities (a daemon refuses to reuse one session id).
    async fn connect_in(
        daemon: &TestDaemon,
        identity: DeviceIdentity,
        psk: Option<[u8; TOKEN_LEN]>,
        session: u8,
    ) -> Self {
        Self::connect_with_session(daemon, identity, psk, [session; SESSION_ID_LEN]).await
    }

    async fn connect_with_session(
        daemon: &TestDaemon,
        identity: DeviceIdentity,
        psk: Option<[u8; TOKEN_LEN]>,
        session_id: [u8; SESSION_ID_LEN],
    ) -> Self {
        let mut engine = EngineConfig::new(
            session_id,
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

    /// Reads the next frame without assuming it is text, for binary paths.
    async fn next_any(&mut self) -> SecureFrame {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), self.client.recv())
                .await
                .expect("timed out waiting for a secure frame");
            match event {
                Some(ClientEvent::Frame(frame)) => {
                    return match frame.as_text() {
                        Some(text) => SecureFrame::Text(
                            serde_json::from_str(text).expect("parse ServerFrame"),
                        ),
                        None => SecureFrame::Binary(frame.payload),
                    };
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

    /// Writes one upload chunk and reports the daemon's acknowledgement or the
    /// error event it sent instead.
    async fn send_chunk(&mut self, stream_id: u32, seq: u64, payload: &[u8]) -> Result<(), String> {
        let frame = BinaryFrame {
            kind: BinaryKind::BlobChunk,
            stream_id,
            seq,
            payload: payload.to_vec(),
        };
        self.client
            .send(h4rn_transport::AppFrame::binary(frame.encode()))
            .await
            .expect("send secure chunk");
        match self.next_any().await {
            SecureFrame::Binary(bytes) => {
                let ack = BinaryFrame::decode(&bytes).expect("decode chunk ack");
                assert_eq!(ack.kind, BinaryKind::Ack, "unexpected binary reply");
                assert_eq!(ack.stream_id, stream_id);
                assert_eq!(ack.seq, seq);
                Ok(())
            }
            SecureFrame::Text(frame) => match *frame {
                ServerFrame::Event {
                    event: Event::Error { message, .. },
                    ..
                } => Err(message),
                other => panic!("unexpected reply to a chunk: {other:?}"),
            },
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
        self.hello_as("dev_secure").await
    }

    async fn hello_as(&mut self, device_id: &str) -> ResponseBody {
        self.request(ClientRequest::Hello {
            protocol_version: h4rn_protocol::PROTOCOL_VERSION,
            device: DeviceInfo {
                device_id: DeviceId::new(device_id),
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
    assert_eq!(
        service.psk(),
        None,
        "a completed pairing must consume the one-time secret"
    );
    client.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn an_allowlisted_device_connects_plainly_while_a_psk_is_armed() {
    // M1: the daemon used to pick `IKpsk2` for every handshake while a secret
    // was armed, so an already-paired device that correctly speaks plain `IK`
    // failed with `decrypt error`. The pattern must be chosen per device.
    let client_identity = device_identity();
    let client_key = client_identity.public().noise_public;
    let psk = [0x7au8; TOKEN_LEN];
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity())
            .with_policy(PairingPolicy::new([client_key]))
            .with_psk(psk),
    ));
    let daemon = boot(Arc::clone(&service)).await;
    assert_eq!(service.psk(), Some(psk), "the secret must start armed");

    let mut client = SecureClient::connect(&daemon, client_identity, None).await;
    client.hello().await;
    assert!(
        service.is_allowed(&client_key),
        "the paired device must stay admitted"
    );
    assert_eq!(
        service.psk(),
        Some(psk),
        "a paired device's handshake must not consume the pairing secret"
    );
    client.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn a_watcher_armed_psk_does_not_lock_out_an_allowlisted_device() {
    // M1, watcher path: `h4rnd pair --direct` arms the secret with `set_psk`
    // rather than `with_psk`, and the same per-device choice must apply.
    let client_identity = device_identity();
    let client_key = client_identity.public().noise_public;
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_policy(PairingPolicy::new([client_key])),
    ));
    let daemon = boot(Arc::clone(&service)).await;
    let psk = [0x7bu8; TOKEN_LEN];
    write_pairing(
        daemon.dir.as_ref().unwrap().path(),
        psk,
        120,
        vec![PairScope::View],
    );
    wait_for_secret(&service, psk).await;

    let mut client = SecureClient::connect(&daemon, client_identity, None).await;
    client.hello().await;
    assert_eq!(
        service.psk(),
        Some(psk),
        "the watcher-armed secret must stay armed after a paired device connects"
    );
    assert!(service.is_allowed(&client_key));
    client.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn a_fresh_device_without_the_psk_is_refused_while_one_is_armed() {
    // M1(c): arming a secret must not turn `NotPaired` into admission for a
    // device that cannot prove possession of it.
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_psk([0x7cu8; TOKEN_LEN]),
    ));
    let daemon = boot(Arc::clone(&service)).await;
    let identity = device_identity();
    expect_refused_without_secret(&daemon, identity).await;
    assert_eq!(
        service.psk(),
        Some([0x7cu8; TOKEN_LEN]),
        "a refused device must not consume the secret"
    );
    daemon.stop().await;
}

fn create_thread_request(host: h4rn_protocol::HostId) -> ClientRequest {
    create_thread_in(host, "/tmp/h4rn-secure-workspace")
}

fn create_thread_in(host: h4rn_protocol::HostId, workspace: &str) -> ClientRequest {
    ClientRequest::CreateThread {
        host,
        harness: HarnessKind::ClaudeCode,
        workspace_path: workspace.into(),
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
async fn a_peer_daemon_key_cannot_issue_client_requests_on_secure() {
    // A peer daemon key is admitted by the transport (it must be, to reach
    // `/secure-mcp`) and is registered as a host with no client scopes. On the
    // client route it may not do anything: not even Hello.
    let peer_identity = device_identity();
    let key = peer_identity.public().noise_public;
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_policy(PairingPolicy::new([key])),
    ));
    let devices = Arc::new(DeviceRegistry::in_memory());
    devices
        .register_host(
            h4rn_protocol::HostId::new("host_peer"),
            key,
            Some("dev_peer".into()),
        )
        .expect("persist");
    assert_eq!(devices.scopes_for_key(&key), None);
    let daemon = boot_with(Arc::clone(&service), devices).await;
    let mut client = SecureClient::connect(&daemon, peer_identity, None).await;
    let id = client.next_id;
    let text = serde_json::to_string(&ClientFrame {
        id,
        body: ClientRequest::ListHosts,
    })
    .expect("serialise frame");
    // The daemon may already have closed the session; a refused send is a
    // refusal, not a test failure.
    let sent = client
        .client
        .send(h4rn_transport::AppFrame::text(text))
        .await
        .is_ok();
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        if !sent {
            return None;
        }
        loop {
            match client.client.recv().await {
                Some(ClientEvent::Frame(frame)) => {
                    if let Ok(ServerFrame::Response {
                        id: response_id,
                        result,
                    }) = serde_json::from_str(frame.as_text().unwrap_or_default())
                        && response_id == id
                    {
                        return Some(result);
                    }
                }
                Some(ClientEvent::Closed { .. }) | None => return None,
                Some(_) => {}
            }
        }
    })
    .await
    .expect("the daemon must refuse promptly");
    match outcome {
        None => {}
        Some(Err(denied)) => assert_eq!(denied.code, h4rn_protocol::ProtoErrorCode::Forbidden),
        Some(Ok(body)) => panic!("a peer daemon key issued a client request: {body:?}"),
    }
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
    devices
        .insert_scoped("viewer", key, vec![PairScope::View])
        .expect("persist");
    devices
        .insert_scoped("admin", [0x77u8; 32], PairScope::ALL.to_vec())
        .expect("persist");
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
    devices
        .insert_scoped("viewer", key, vec![PairScope::View])
        .expect("persist");
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
async fn a_pending_device_is_admitted_only_after_the_host_confirms() {
    // Regression for E4: the relay announces the key, the host compares the
    // SAS, and only an admin-scoped ConfirmPairing admits it.
    let phone_identity = device_identity();
    let phone_key = phone_identity.public().noise_public;
    let admin_identity = device_identity();
    let admin_key = admin_identity.public().noise_public;
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_policy(PairingPolicy::new([admin_key])),
    ));
    let devices = Arc::new(DeviceRegistry::in_memory());
    devices
        .insert_scoped("dev_admin", admin_key, PairScope::ALL.to_vec())
        .expect("persist");
    devices
        .provision_scoped("dev_phone", phone_key, vec![PairScope::View])
        .expect("persist");
    let daemon = boot_with(Arc::clone(&service), Arc::clone(&devices)).await;

    expect_rejected(&daemon, phone_identity.clone(), [0u8; TOKEN_LEN]).await;
    assert!(
        !service.is_allowed(&phone_key),
        "a pending key was admitted"
    );

    let mut admin = SecureClient::connect(&daemon, admin_identity, None).await;
    admin.hello().await;
    match admin
        .request(ClientRequest::ConfirmPairing {
            device_id: DeviceId::new("dev_phone"),
            confirm: true,
        })
        .await
        .expect("confirm the pairing")
    {
        ResponseBody::Ack => {}
        other => panic!("unexpected confirm response: {other:?}"),
    }
    assert!(service.is_allowed(&phone_key));
    assert_eq!(
        devices.scopes_for_key(&phone_key),
        Some(vec![PairScope::View])
    );
    admin.close().await;

    // A fresh logical session: the admin's session id must not be reused while
    // its close is still settling.
    let mut phone =
        SecureClient::connect_with_session(&daemon, phone_identity, None, [0x31u8; SESSION_ID_LEN])
            .await;
    phone.hello().await;
    phone.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn a_view_only_device_cannot_confirm_a_pending_pairing() {
    // E4: confirming is an admin decision. A view-only device must not be able
    // to admit a key the host has not compared, and the entry stays pending.
    let phone_key = device_identity().public().noise_public;
    let viewer_identity = device_identity();
    let viewer_key = viewer_identity.public().noise_public;
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_policy(PairingPolicy::new([viewer_key])),
    ));
    let devices = Arc::new(DeviceRegistry::in_memory());
    devices
        .insert_scoped("dev_viewer", viewer_key, vec![PairScope::View])
        .expect("persist");
    devices
        .provision_scoped("dev_phone", phone_key, vec![PairScope::View])
        .expect("persist");
    let daemon = boot_with(Arc::clone(&service), Arc::clone(&devices)).await;

    let mut viewer = SecureClient::connect(&daemon, viewer_identity, None).await;
    viewer.hello().await;
    let refused = viewer
        .request(ClientRequest::ConfirmPairing {
            device_id: DeviceId::new("dev_phone"),
            confirm: true,
        })
        .await
        .expect_err("a view-only device must not confirm a pairing");
    assert_eq!(refused.code, h4rn_protocol::ProtoErrorCode::Forbidden);
    assert!(
        devices.is_pending(&phone_key),
        "a refused decision must not change the pending entry"
    );
    assert!(!service.is_allowed(&phone_key));
    viewer.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn terminal_ownership_uses_the_verified_device_not_the_hello_claim() {
    // Regression for E8: a remote client may claim any device id in Hello.
    // Ownership and the terminal audit must name the key's registry entry.
    let client_identity = device_identity();
    let key = client_identity.public().noise_public;
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_policy(PairingPolicy::new([key])),
    ));
    let devices = Arc::new(DeviceRegistry::in_memory());
    devices
        .insert_scoped("dev_real", key, PairScope::ALL.to_vec())
        .expect("persist");
    let daemon = boot_with(Arc::clone(&service), devices).await;
    let mut client = SecureClient::connect(&daemon, client_identity, None).await;
    client.hello_as("dev_attacker").await;
    let host = host_of(&mut client).await;
    let cwd = tempfile::tempdir().expect("cwd");
    let cwd_string = cwd.path().to_string_lossy().to_string();
    let opened = client
        .request(ClientRequest::TerminalOpen {
            thread_id: None,
            host,
            cwd: cwd_string,
            shell: Some("/bin/sh".into()),
            args: Vec::new(),
            cols: 80,
            rows: 24,
        })
        .await
        .expect("open terminal");
    match opened {
        ResponseBody::TerminalOpened { terminal } => {
            assert!(terminal.terminal_id.as_str().starts_with("term_"));
        }
        other => panic!("unexpected terminal response: {other:?}"),
    }
    let audit = std::fs::read_to_string(
        daemon
            .dir
            .as_ref()
            .expect("tempdir")
            .path()
            .join("terminal-audit.log"),
    )
    .expect("terminal audit");
    assert!(
        audit.contains("dev_real"),
        "audit must name the verified device: {audit}"
    );
    assert!(
        !audit.contains("dev_attacker"),
        "audit trusted the Hello claim: {audit}"
    );
    client.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn a_view_only_device_cannot_read_a_file_diff() {
    // A diff carries file contents. It needs the files scope even though the
    // rest of the thread view is readable with `view` alone.
    let viewer_identity = device_identity();
    let admin_identity = device_identity();
    let viewer_key = viewer_identity.public().noise_public;
    let admin_key = admin_identity.public().noise_public;
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity())
            .with_policy(PairingPolicy::new([viewer_key, admin_key])),
    ));
    // A diff of an unauthorised root is refused before the scope check, which
    // would let this test pass without the fix. Only a thread workspace becomes
    // an authorised root at runtime, so the admin opens one in a real directory.
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("secret.txt"), "top secret\n").expect("secret");
    let workspace_path = workspace.path().display().to_string();

    let devices = Arc::new(DeviceRegistry::in_memory());
    devices
        .insert_scoped("viewer", viewer_key, vec![PairScope::View])
        .expect("persist");
    devices
        .insert_scoped("admin", admin_key, PairScope::ALL.to_vec())
        .expect("persist");
    let daemon = boot_with(Arc::clone(&service), devices).await;

    let mut admin = SecureClient::connect_in(&daemon, admin_identity, None, 12).await;
    admin.hello().await;
    let host = host_of(&mut admin).await;
    admin
        .request(create_thread_in(host, &workspace_path))
        .await
        .expect("the admin may create the thread that authorises the workspace");
    admin.close().await;

    let mut client = SecureClient::connect(&daemon, viewer_identity, None).await;
    client.hello().await;
    let host = host_of(&mut client).await;

    let denied = client
        .request(ClientRequest::FsDiff {
            host: host.clone(),
            workspace: workspace_path.clone(),
            path: "secret.txt".into(),
            against: h4rn_protocol::DiffAgainst::Head,
        })
        .await
        .expect_err("view-only may not read a file diff");
    assert_eq!(denied.code, h4rn_protocol::ProtoErrorCode::Forbidden);

    let denied = client
        .request(ClientRequest::FsTurnChanges {
            host: host.clone(),
            workspace: workspace_path.clone(),
            thread_id: h4rn_protocol::ThreadId::new("th_1"),
            turn_id: h4rn_protocol::TurnId::new("turn_1"),
        })
        .await
        .expect_err("view-only may not read a turn's changes");
    assert_eq!(denied.code, h4rn_protocol::ProtoErrorCode::Forbidden);
    client.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn a_direct_pairing_grants_exactly_the_requested_scopes() {
    // `h4rnd pair --direct --scope files` writes the scopes into the pairing
    // file; the daemon arms them, and the device that pairs holds only those.
    // Nothing pre-enables PSK pairing: the watcher's live secret must.
    let client_identity = device_identity();
    let service = Arc::new(SecureService::new(SecureOptions::new(device_identity())));
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

/// Writes a direct pairing file into `dir`, as `h4rnd pair --direct` does.
fn write_pairing(
    dir: &std::path::Path,
    psk: [u8; TOKEN_LEN],
    expires_in: i64,
    scopes: Vec<PairScope>,
) {
    h4rn_daemon::remote::pairing::write_direct_pairing(
        dir,
        &h4rn_daemon::remote::DirectPairingFile {
            psk: hex::encode(psk),
            expires_at: chrono::Utc::now().timestamp() + expires_in,
            endpoints: vec!["127.0.0.1:1".into()],
            label: Some("direct pairing test".into()),
            scopes,
        },
    )
    .unwrap();
}

/// Waits until the running watcher armed `psk`.
async fn wait_for_secret(service: &SecureService, psk: [u8; TOKEN_LEN]) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while service.psk() != Some(psk) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the daemon never armed the direct pairing");
}

/// Dials `/secure` with `psk` and expects the daemon to reject it.
async fn expect_rejected(daemon: &TestDaemon, identity: DeviceIdentity, psk: [u8; TOKEN_LEN]) {
    let public = identity.public().noise_public;
    let mut engine = EngineConfig::new(
        [21u8; SESSION_ID_LEN],
        identity,
        daemon.service.public_identity().noise_public,
    );
    engine.psk = Some(psk);
    let paths = vec![ClientPath::websocket(
        TransportPathKind::Direct,
        format!("ws://{}/secure", daemon.addr),
    )];
    let mut client = Client::connect(ClientConfig::new(engine, paths)).expect("start client");
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await {
                Some(ClientEvent::Closed { reason }) => return reason,
                Some(ClientEvent::Established(_)) => panic!("a consumed secret paired a device"),
                Some(_) => {}
                None => panic!("the client stopped without a reason"),
            }
        }
    })
    .await
    .expect("timed out waiting for the rejection");
    // A refused handshake closes the socket right after the `Reject` packet;
    // depending on flush timing the client reports either the daemon's reason
    // or that the path stopped. Either way it must never have been established.
    assert!(
        outcome.contains("not paired")
            || outcome.contains("revoked")
            || outcome.contains("transport client stopped"),
        "unexpected rejection reason: {outcome}"
    );
    assert!(
        !daemon.service.is_allowed(&public),
        "a rejected device ended up on the allowlist"
    );
    client.close().await;
}

/// Dials `/secure` as an unpaired device with no PSK and expects a refusal.
async fn expect_refused_without_secret(daemon: &TestDaemon, identity: DeviceIdentity) {
    let public = identity.public().noise_public;
    let mut engine = EngineConfig::new(
        [22u8; SESSION_ID_LEN],
        identity,
        daemon.service.public_identity().noise_public,
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
                Some(ClientEvent::Established(_)) => {
                    panic!("a device without the secret was admitted")
                }
                Some(_) => {}
                None => panic!("the client stopped without a reason"),
            }
        }
    })
    .await
    .expect("timed out waiting for the rejection");
    assert!(
        outcome.contains("device is not paired"),
        "the daemon must answer a plain unpaired device with NotPaired, got: {outcome}"
    );
    assert!(
        !daemon.service.is_allowed(&public),
        "a device without the secret ended up on the allowlist"
    );
    client.close().await;
}

/// Dials with `psk` and reports whether the session was ever established
/// within `window`.
/// Dial with `psk` and wait for the outcome: `Ok(())` is a session,
/// `Err(reason)` is the handshake failure or refusal. Waits on that event, never on
/// a fixed window, so a loaded machine cannot turn "not answered yet" into
/// "refused".
async fn dial_outcome(
    daemon: &TestDaemon,
    identity: DeviceIdentity,
    psk: [u8; TOKEN_LEN],
) -> Result<(), String> {
    let mut engine = EngineConfig::new(
        [23u8; SESSION_ID_LEN],
        identity,
        daemon.service.public_identity().noise_public,
    );
    engine.psk = Some(psk);
    let paths = vec![ClientPath::websocket(
        TransportPathKind::Direct,
        format!("ws://{}/secure", daemon.addr),
    )];
    let mut client = Client::connect(ClientConfig::new(engine, paths)).expect("start client");
    let outcome = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match client.recv().await {
                Some(ClientEvent::Established(_)) => return Ok(()),
                Some(ClientEvent::Closed { reason }) => return Err(reason),
                // A wrong secret fails the initiator's IKpsk2 attempt (it
                // cannot open message 2): the first handshake error is the
                // terminal answer. The client keeps retrying with backoff and
                // never reports `Closed`, so this is the event to wait on.
                Some(ClientEvent::Error { message, .. }) if message.contains("handshake") => {
                    return Err(message);
                }
                Some(_) => {}
                None => return Err("the client stopped without a reason".to_owned()),
            }
        }
    })
    .await
    .expect("timed out waiting for the handshake outcome");
    client.close().await;
    outcome
}

fn assert_untouched(daemon: &TestDaemon, key: &[u8; 32], armed: [u8; TOKEN_LEN]) {
    assert!(
        !daemon.service.is_allowed(key),
        "a device without the secret was allowlisted"
    );
    assert_eq!(
        daemon.service.psk(),
        Some(armed),
        "a wrong secret burned the armed one"
    );
    assert_eq!(
        daemon.devices.scopes_for_key(key),
        None,
        "a wrong secret recorded a device"
    );
}

#[tokio::test]
async fn a_wrong_psk_is_refused_and_touches_nothing() {
    // Round 6 P0: IKpsk2 message 1 cannot prove the secret. A wrong one used to
    // allowlist the key, burn the secret and record the device.
    for armed_via_set_psk in [false, true] {
        let armed = [0x7eu8; TOKEN_LEN];
        let options = SecureOptions::new(device_identity());
        let service = Arc::new(SecureService::new(if armed_via_set_psk {
            options
        } else {
            options.with_psk(armed)
        }));
        if armed_via_set_psk {
            service.set_psk(armed);
        }
        let daemon = boot(Arc::clone(&service)).await;
        let attacker = device_identity();
        let key = attacker.public().noise_public;
        let refusal = dial_outcome(&daemon, attacker.clone(), [0x11u8; TOKEN_LEN]).await;
        match refusal {
            Ok(()) => panic!("a wrong secret established a session"),
            Err(reason) => assert!(
                reason.contains("handshake failed") || reason.contains("device is not paired"),
                "a wrong secret must fail the handshake, got: {reason}"
            ),
        }
        // The wrong-secret client could not finish the handshake, so it never
        // sent the first record that would let the daemon record the device.
        assert_untouched(&daemon, &key, armed);

        // The same key cannot come back over plain IK either.
        expect_refused_without_secret(&daemon, attacker).await;
        assert_untouched(&daemon, &key, armed);

        // And the failed attempt did not lock the real device out.
        let phone = device_identity();
        let mut client = SecureClient::connect(&daemon, phone.clone(), Some(armed)).await;
        client.hello().await;
        assert!(service.is_allowed(&phone.public().noise_public));
        assert_eq!(service.psk(), None, "the correct pairing consumes the PSK");
        assert!(
            daemon
                .devices
                .scopes_for_key(&phone.public().noise_public)
                .is_some()
        );
        client.close().await;
        daemon.stop().await;
    }
}

#[tokio::test]
async fn a_paired_device_that_lost_its_ticket_reconnects_plainly() {
    // Round 6 N1: the device was admitted and the secret consumed, but the
    // socket dropped before the ticket arrived. It still holds the stale
    // secret and no ticket; it must not retry IKpsk2 forever.
    let stale = [0x44u8; TOKEN_LEN];
    let identity = device_identity();
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity())
            .with_policy(PairingPolicy::new([identity.public().noise_public])),
    ));
    let daemon = boot(Arc::clone(&service)).await;
    assert_eq!(service.psk(), None);
    let mut client = SecureClient::connect(&daemon, identity, Some(stale)).await;
    client.hello().await;
    client.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn a_secret_armed_by_the_watcher_enables_psk_pairing() {
    // Regression for E1: `SecureOptions::new` with no `with_psk`, then arm
    // through the real file watcher. `set_psk` must open the pairing window.
    let client_identity = device_identity();
    let service = Arc::new(SecureService::new(SecureOptions::new(device_identity())));
    let devices = Arc::new(DeviceRegistry::in_memory());
    let daemon = boot_with(Arc::clone(&service), Arc::clone(&devices)).await;
    let psk = [0x5bu8; TOKEN_LEN];
    write_pairing(
        daemon.dir.as_ref().unwrap().path(),
        psk,
        120,
        vec![PairScope::View],
    );
    wait_for_secret(&service, psk).await;
    assert!(
        !service.is_allowed(&client_identity.public().noise_public),
        "the device must not be allowed before it pairs"
    );

    let mut client = SecureClient::connect(&daemon, client_identity.clone(), Some(psk)).await;
    client.hello().await;
    assert!(
        service.is_allowed(&client_identity.public().noise_public),
        "a watcher-armed secret must pair the device"
    );
    assert_eq!(
        devices.scopes_for_key(&client_identity.public().noise_public),
        Some(vec![PairScope::View])
    );
    assert_eq!(
        service.psk(),
        None,
        "a completed pairing must consume the watcher-armed secret"
    );
    client.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn a_consumed_direct_pairing_cannot_be_reused_after_a_restart() {
    // Regression for E2: consuming the secret must delete the file durably, so
    // a restart before expiry never re-arms the same QR for a second device.
    let dir = tempfile::tempdir().expect("tempdir");
    let identity = device_identity();
    let psk = [0x6cu8; TOKEN_LEN];
    write_pairing(
        dir.path(),
        psk,
        300,
        vec![PairScope::View, PairScope::Approve],
    );

    let service = Arc::new(SecureService::new(SecureOptions::new(identity.clone())));
    let devices = Arc::new(DeviceRegistry::in_memory());
    let daemon = boot_at(
        dir.path().to_path_buf(),
        Arc::clone(&service),
        Arc::clone(&devices),
        None,
    )
    .await;
    wait_for_secret(&service, psk).await;
    let first_identity = device_identity();
    let mut first = SecureClient::connect(&daemon, first_identity.clone(), Some(psk)).await;
    first.hello().await;
    assert!(service.is_allowed(&first_identity.public().noise_public));
    assert_eq!(service.psk(), None, "the secret is single use");
    assert!(
        h4rn_daemon::remote::pairing::read_direct_pairing(dir.path())
            .unwrap()
            .is_none(),
        "consuming the secret must delete direct-pairing.json before granting access"
    );
    first.close().await;
    daemon.stop().await;

    // Restart before the original expiry with the same data directory.
    let service = Arc::new(SecureService::new(SecureOptions::new(identity)));
    let devices = Arc::new(DeviceRegistry::in_memory());
    let daemon = boot_at(
        dir.path().to_path_buf(),
        Arc::clone(&service),
        Arc::clone(&devices),
        None,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(service.psk(), None, "a restart re-armed a consumed secret");
    expect_rejected(&daemon, device_identity(), psk).await;
    daemon.stop().await;
}

#[tokio::test]
async fn an_expired_direct_pairing_file_is_deleted() {
    // Regression for E3: docs promised removal; the watcher must delete it.
    let dir = tempfile::tempdir().expect("tempdir");
    let service = Arc::new(SecureService::new(SecureOptions::new(device_identity())));
    let daemon = boot_at(
        dir.path().to_path_buf(),
        Arc::clone(&service),
        Arc::new(DeviceRegistry::in_memory()),
        None,
    )
    .await;
    write_pairing(dir.path(), [0x7du8; TOKEN_LEN], -1, vec![PairScope::View]);
    let path = h4rn_daemon::remote::pairing::direct_pairing_path(dir.path());
    tokio::time::timeout(Duration::from_secs(10), async {
        while path.exists() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("an expired direct pairing file was left on disk");
    assert_eq!(service.psk(), None);
    daemon.stop().await;
}

#[tokio::test]
async fn the_watcher_never_replaces_or_clears_an_operator_secret() {
    // Regression for E3: an operator's H4RN_PAIRING_PSK is not the watcher's
    // to replace, and expiry must not clear it.
    let operator = [0x11u8; TOKEN_LEN];
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_psk(operator),
    ));
    let dir = tempfile::tempdir().expect("tempdir");
    let daemon = boot_at(
        dir.path().to_path_buf(),
        Arc::clone(&service),
        Arc::new(DeviceRegistry::in_memory()),
        None,
    )
    .await;

    write_pairing(dir.path(), [0x22u8; TOKEN_LEN], 120, vec![PairScope::View]);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        service.psk(),
        Some(operator),
        "a direct pairing file replaced the operator secret"
    );

    write_pairing(dir.path(), [0x22u8; TOKEN_LEN], -1, vec![PairScope::View]);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        service.psk(),
        Some(operator),
        "direct pairing expiry cleared the operator secret"
    );
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

#[tokio::test]
async fn a_pairing_whose_registry_write_fails_is_refused_not_admitted() {
    // K3(a): the pairing hook only logged a registry write failure, then the
    // device was still allowed -> a phantom "paired" device whose every
    // request is Forbidden. The hook is fallible now: NotPaired, no allowlist.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("devices.json");
    std::fs::create_dir(&path).expect("a directory in place of the registry file");
    let devices = Arc::new(DeviceRegistry::open(&path));
    let psk = [0x7eu8; TOKEN_LEN];
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_psk(psk),
    ));
    let daemon = boot_with(Arc::clone(&service), Arc::clone(&devices)).await;
    let identity = device_identity();
    expect_rejected(&daemon, identity.clone(), psk).await;
    assert!(
        devices
            .authorization(&identity.public().noise_public)
            .is_none(),
        "a pairing whose authorisation could not be recorded must not be admitted"
    );
    daemon.stop().await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_consumed_pairing_that_cannot_be_deleted_is_not_rearmed_on_restart() {
    // K3(b): if the consumed file cannot be deleted, a restart re-armed the
    // same PSK. The failed delete now leaves a consumed marker the watcher
    // honours.
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let psk = [0x7fu8; TOKEN_LEN];
    write_pairing(dir.path(), psk, 300, vec![PairScope::View]);
    // Pre-create the marker so the best-effort write can succeed while the
    // directory itself is read-only; deleting the pairing file cannot.
    std::fs::write(dir.path().join("direct-pairing.consumed"), b"").expect("marker");

    let mut permissions = std::fs::metadata(dir.path())
        .expect("metadata")
        .permissions();
    permissions.set_mode(0o555);
    std::fs::set_permissions(dir.path(), permissions).expect("read-only dir");
    let failed = h4rn_daemon::remote::pairing::invalidate_direct_pairing(dir.path(), &psk);
    let mut permissions = std::fs::metadata(dir.path())
        .expect("metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(dir.path(), permissions).expect("writable dir");
    assert!(
        failed.is_err(),
        "an undeletable consumed file must still be reported"
    );
    assert!(
        h4rn_daemon::remote::pairing::direct_pairing_path(dir.path()).exists(),
        "the fixture must still show the failed delete"
    );

    // Restart on the same data directory before the original expiry.
    let service = Arc::new(SecureService::new(SecureOptions::new(device_identity())));
    let daemon = boot_at(
        dir.path().to_path_buf(),
        Arc::clone(&service),
        Arc::new(DeviceRegistry::in_memory()),
        None,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        service.psk(),
        None,
        "a restart re-armed a consumed direct pairing"
    );
    expect_rejected(&daemon, device_identity(), psk).await;
    daemon.stop().await;
}

fn assert_upload_not_found(result: Result<ResponseBody, ProtoError>, upload_id: &str) {
    let error = result.expect_err("another device must not see this upload");
    assert_eq!(error.code, ProtoErrorCode::NotFound);
    assert!(
        error.message.contains(upload_id),
        "the error should name the upload: {error:?}"
    );
}

#[tokio::test]
async fn an_upload_is_owned_by_the_device_that_opened_it() {
    // K4: any paired device could append to, query, commit or abort another
    // device's upload by id. An upload now belongs to the key the handshake
    // authenticated, so it follows the device across reconnects.
    let owner = device_identity();
    let other = device_identity();
    let allowed = PairingPolicy::new([owner.public().noise_public, other.public().noise_public]);
    let service = Arc::new(SecureService::new(
        SecureOptions::new(device_identity()).with_policy(allowed),
    ));
    let daemon = boot(Arc::clone(&service)).await;

    let mut a = SecureClient::connect_in(&daemon, owner.clone(), None, 11).await;
    let mut b = SecureClient::connect_in(&daemon, other, None, 22).await;
    a.hello_as("dev_upload_owner").await;
    b.hello_as("dev_upload_other").await;

    let bytes = b"an upload belongs to the device that opened it";
    let split = bytes.len() / 2;
    let hash = blake3::hash(bytes).to_hex().to_string();
    let ResponseBody::UploadStarted { upload } = a
        .request(ClientRequest::BeginUpload {
            name: "owned.txt".into(),
            size: bytes.len() as u64,
            mime_hint: None,
        })
        .await
        .expect("begin upload")
    else {
        panic!("unexpected BeginUpload response");
    };
    a.send_chunk(upload.stream_id, 0, &bytes[..split])
        .await
        .expect("the owner may append");

    assert_upload_not_found(
        b.request(ClientRequest::QueryUpload {
            upload_id: upload.upload_id.clone(),
        })
        .await,
        &upload.upload_id,
    );
    let append = b
        .send_chunk(upload.stream_id, 1, &bytes[split..])
        .await
        .expect_err("another device must not append");
    assert!(
        append.contains(&upload.upload_id),
        "the chunk error should name the upload: {append}"
    );
    assert_upload_not_found(
        b.request(ClientRequest::CommitUpload {
            upload_id: upload.upload_id.clone(),
            blake3: hash.clone(),
        })
        .await,
        &upload.upload_id,
    );
    assert_upload_not_found(
        b.request(ClientRequest::AbortUpload {
            upload_id: upload.upload_id.clone(),
        })
        .await,
        &upload.upload_id,
    );

    // The owner reconnects as a new secure session: ownership follows the
    // device key, not the connection that opened it.
    let upload_id = upload.upload_id.clone();
    let stream_id = upload.stream_id;
    a.close().await;
    let mut resumed = SecureClient::connect_in(&daemon, owner, None, 33).await;
    resumed.hello_as("dev_upload_owner").await;
    let ResponseBody::UploadStatus { upload: status } = resumed
        .request(ClientRequest::QueryUpload {
            upload_id: upload_id.clone(),
        })
        .await
        .expect("the owner may resume")
    else {
        panic!("unexpected QueryUpload response");
    };
    assert_eq!(status.received_bytes, split as u64);
    resumed
        .send_chunk(stream_id, 1, &bytes[split..])
        .await
        .expect("the owner may append after reconnecting");
    let ResponseBody::UploadCommitted { blob } = resumed
        .request(ClientRequest::CommitUpload {
            upload_id,
            blake3: hash,
        })
        .await
        .expect("the owner may commit")
    else {
        panic!("unexpected CommitUpload response");
    };
    assert_eq!(blob.size, bytes.len() as u64);

    resumed.close().await;
    b.close().await;
    daemon.stop().await;
}
