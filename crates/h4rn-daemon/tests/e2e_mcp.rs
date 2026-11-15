//! Real-harness MCP injection checks.
//!
//! Ignored by default: they spawn the installed Claude Code / Codex binary,
//! spend tokens and need credentials. They prove the shipping injection path:
//! the daemon binds its hub, mints a per-thread token, the adapter passes the
//! endpoint to the harness, and a real model can call `h4rn__echo`.
//!
//! ```text
//! cargo test -p h4rn-daemon --test e2e_mcp -- --ignored --nocapture
//! ```

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use h4rn_core::LifecycleSettings;
use h4rn_daemon::auth::TokenStore;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::harnesses::{HarnessCatalog, StaticHarnessCatalog};
use h4rn_daemon::hosts::HostInventory;
use h4rn_daemon::threads::{NewThread, ThreadManager, ThreadManagerOptions};
use h4rn_protocol::{
    ClientFrame, ClientRequest, DeviceId, DeviceInfo, Event, HarnessKind, ResponseBody,
    RuntimeMode, ServerFrame, Topic,
};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const TURN_TIMEOUT: Duration = Duration::from_secs(300);

fn real_catalog() -> Arc<StaticHarnessCatalog> {
    let mut catalog = StaticHarnessCatalog::new();
    for adapter in h4rn_harness::registry::builtin_adapters() {
        catalog.register(adapter);
    }
    Arc::new(catalog)
}

fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "e2e@h4rn.invalid"]);
    run(&["config", "user.name", "h4rn e2e"]);
    std::fs::write(dir.path().join("README.md"), "# e2e\n").expect("write readme");
    run(&["add", "README.md"]);
    run(&["commit", "-q", "-m", "init"]);
    dir
}

struct Daemon {
    addr: std::net::SocketAddr,
    token: String,
    manager: Arc<ThreadManager>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
    _dir: Option<tempfile::TempDir>,
}

impl Daemon {
    async fn stop(mut self) {
        self.manager.shutdown().await;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

async fn boot(catalog: Arc<StaticHarnessCatalog>) -> Daemon {
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
            .with_working_exit_grace(chrono::TimeDelta::milliseconds(200)),
        tick_interval: Duration::from_millis(20),
        clock: None,
    };
    let daemon = DaemonBuilder::new(config)
        .with_catalog(catalog)
        .with_lifecycle(options)
        .build()
        .await
        .expect("build daemon");
    let token = TokenStore::new(dir.path())
        .load()
        .expect("read token")
        .expect("token exists");
    let manager = daemon.manager();
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
    Daemon {
        addr,
        token,
        manager,
        shutdown: Some(shutdown),
        task,
        _dir: Some(dir),
    }
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    pending: VecDeque<Event>,
}

impl Client {
    async fn connect(daemon: &Daemon) -> Self {
        let url = format!("ws://{}/ws?token={}", daemon.addr, daemon.token);
        let (ws, _) = connect_async(url).await.expect("connect");
        Self {
            ws,
            next_id: 1,
            pending: VecDeque::new(),
        }
    }

    async fn request(&mut self, body: ClientRequest) -> ResponseBody {
        let id = self.next_id;
        self.next_id += 1;
        let frame = ClientFrame { id, body };
        self.ws
            .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
            .await
            .expect("send frame");
        loop {
            let message = tokio::time::timeout(Duration::from_secs(30), self.ws.next())
                .await
                .expect("timed out waiting for a response")
                .expect("socket closed")
                .expect("websocket error");
            match message {
                Message::Text(text) => match serde_json::from_str::<ServerFrame>(&text).unwrap() {
                    ServerFrame::Response {
                        id: response_id,
                        result,
                    } if response_id == id => return result.expect("request failed"),
                    ServerFrame::Event { event, .. } => self.pending.push_back(event),
                    _ => {}
                },
                Message::Close(_) => panic!("daemon closed the connection"),
                _ => {}
            }
        }
    }

    async fn hello_and_subscribe(&mut self) {
        self.request(ClientRequest::Hello {
            protocol_version: h4rn_protocol::PROTOCOL_VERSION,
            device: DeviceInfo {
                device_id: DeviceId::new("dev_e2e_mcp"),
                name: "mcp e2e".into(),
                platform: "linux".into(),
                app_version: "0.1.0".into(),
            },
        })
        .await;
        self.request(ClientRequest::Subscribe {
            topics: vec![Topic::Mcp, Topic::Threads],
            since_seq: None,
        })
        .await;
    }

    /// Waits for the first `McpToolCalled` for `qualified` and then for the
    /// turn to complete, collecting assistant text on the way.
    async fn wait_for_echo(&mut self, qualified: &str) -> (bool, String) {
        let deadline = tokio::time::Instant::now() + TURN_TIMEOUT;
        let mut text = String::new();
        let mut called = None;
        loop {
            if let Some(event) = self.pending.pop_front() {
                match event {
                    Event::McpToolCalled {
                        qualified_name,
                        ok,
                        error,
                        ..
                    } if qualified_name == qualified => called = Some((ok, error)),
                    Event::MessageDelta { text: delta, .. } => text.push_str(&delta),
                    Event::TurnCompleted { .. } => {
                        return (called.map(|(ok, _)| ok).unwrap_or(false), text);
                    }
                    _ => {}
                }
                continue;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let message = tokio::time::timeout(remaining, self.ws.next())
                .await
                .expect("timed out waiting for the harness turn")
                .expect("socket closed")
                .expect("websocket error");
            if let Message::Text(text) = message
                && let ServerFrame::Event { event, .. } =
                    serde_json::from_str::<ServerFrame>(&text).unwrap()
            {
                self.pending.push_back(event);
            }
        }
    }
}

async fn real_harness_calls_h4rn_echo(harness: HarnessKind) {
    let catalog = real_catalog();
    let adapter = catalog.get(harness).expect("adapter registered");
    if adapter.detect().is_none() {
        eprintln!("skipping: {harness:?} is not installed on this host");
        return;
    }

    let repo = git_repo();
    let daemon = boot(Arc::clone(&catalog)).await;
    let mut client = Client::connect(&daemon).await;
    client.hello_and_subscribe().await;

    let host = HostInventory::detect(Arc::clone(&catalog) as Arc<dyn HarnessCatalog>).local_id();
    let nonce = format!("mcp-e2e-{}", std::process::id());
    let prompt = format!(
        "Use the MCP tool whose name ends with `h4rn__echo` exactly once, with the message \
         \"{nonce}\". Then reply with exactly the text the tool returned and nothing else."
    );
    let created = daemon
        .manager
        .create_thread(NewThread {
            thread_id: None,
            parent_thread_id: None,
            subagent_id: None,
            host,
            harness,
            workspace_path: repo.path().display().to_string(),
            model: "default".into(),
            prompt,
            attachments: Vec::new(),
            runtime_mode: RuntimeMode::FullAccess,
            worktree: None,
        })
        .await
        .expect("create thread");
    assert!(
        !created.thread_id.as_str().is_empty(),
        "the harness thread needs an id"
    );

    let (ok, text) = client.wait_for_echo("h4rn__echo").await;
    assert!(ok, "{harness:?} did not complete h4rn__echo successfully");
    assert!(
        text.contains(&nonce),
        "{harness:?} reply did not contain the echoed nonce: {text:?}"
    );

    daemon.stop().await;
}

#[tokio::test]
#[ignore = "spawns a real claude process and spends tokens"]
async fn real_claude_calls_h4rn_echo_through_the_hub() {
    real_harness_calls_h4rn_echo(HarnessKind::ClaudeCode).await;
}

#[tokio::test]
#[ignore = "spawns a real codex process and spends tokens"]
async fn real_codex_calls_h4rn_echo_through_the_hub() {
    real_harness_calls_h4rn_echo(HarnessKind::Codex).await;
}
