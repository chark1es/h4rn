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
use h4rn_daemon::threads::ThreadManagerOptions;
use h4rn_harness::FakeHarness;
use h4rn_protocol::{
    ClientFrame, ClientRequest, DeviceId, DeviceInfo, Event, HarnessKind, ProtoError, ResponseBody,
    RuntimeMode, ServerFrame, ThreadSummary, TransportPathKind,
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
    _dir: Option<tempfile::TempDir>,
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

async fn boot(service: Arc<SecureService>) -> TestDaemon {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
        log_level: "off".to_owned(),
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
        _dir: Some(dir),
        shutdown: Some(shutdown),
        task,
    }
}

struct SecureClient {
    client: Client,
    next_id: u64,
    pending: VecDeque<(u64, Event)>,
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
        let established = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match client.recv().await {
                    Some(ClientEvent::Established(_)) => return,
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
