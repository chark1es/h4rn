//! Guest links over the real daemon: the page, the token, the thread binding
//! and the owner's Request flow (ADR-044).

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use h4rn_core::LifecycleSettings;
use h4rn_daemon::auth::TokenStore;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::threads::ThreadManagerOptions;
use h4rn_harness::FakeHarness;
use h4rn_protocol::{
    ClientFrame, ClientRequest, DeviceId, DeviceInfo, Event, GuestLinkState, GuestMessageInput,
    GuestSnapshot, HarnessKind, HostId, ResponseBody, RuntimeMode, ServerFrame, ThreadId,
    ThreadTab, Topic,
};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

struct TestDaemon {
    addr: std::net::SocketAddr,
    addrs: Vec<std::net::SocketAddr>,
    token: String,
    dir: tempfile::TempDir,
    guest: Arc<h4rn_daemon::guest::GuestService>,
    manager: Arc<h4rn_daemon::threads::ThreadManager>,
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

struct OfflineRegistry;

#[async_trait::async_trait]
impl h4rn_harness::RegistryClient for OfflineRegistry {
    async fn latest(&self, _package: &str) -> Option<String> {
        None
    }
}

async fn boot() -> TestDaemon {
    boot_with_tick(
        LifecycleSettings::default(),
        None,
        Duration::from_millis(20),
    )
    .await
}

async fn boot_with(
    settings: LifecycleSettings,
    journal: Option<Arc<dyn h4rn_daemon::journal::Journal>>,
) -> TestDaemon {
    boot_with_tick(settings, journal, Duration::from_millis(20)).await
}

async fn boot_with_tick(
    settings: LifecycleSettings,
    journal: Option<Arc<dyn h4rn_daemon::journal::Journal>>,
    tick_interval: Duration,
) -> TestDaemon {
    boot_full(
        settings,
        journal,
        tick_interval,
        "127.0.0.1:0".parse().expect("bind"),
        None,
        Arc::new(FakeHarness::new()),
    )
    .await
}

/// Boots a daemon on the listener addresses a test names.
/// Boots a daemon whose thread manager runs these harnesses.
async fn boot_with_harness(harness: Arc<dyn h4rn_harness::Harness>) -> TestDaemon {
    boot_full(
        LifecycleSettings::default(),
        None,
        Duration::from_millis(20),
        "127.0.0.1:0".parse().expect("bind"),
        None,
        harness,
    )
    .await
}

async fn boot_on(
    bind: std::net::SocketAddr,
    extra_bind: Option<std::net::SocketAddr>,
) -> TestDaemon {
    boot_full(
        LifecycleSettings::default(),
        None,
        Duration::from_millis(20),
        bind,
        extra_bind,
        Arc::new(FakeHarness::new()),
    )
    .await
}

async fn boot_full(
    settings: LifecycleSettings,
    journal: Option<Arc<dyn h4rn_daemon::journal::Journal>>,
    tick_interval: Duration,
    bind: std::net::SocketAddr,
    extra_bind: Option<std::net::SocketAddr>,
    harness: Arc<dyn h4rn_harness::Harness>,
) -> TestDaemon {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind,
        extra_bind,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Off,
        network_access: None,
        network_port: 0,
    };
    let options = ThreadManagerOptions {
        settings,
        tick_interval,
        clock: None,
    };
    let mut builder = DaemonBuilder::new(config)
        .with_local_host(HostId::new("host_guest_test"), "guest test")
        .without_title_agents()
        .with_harness_registry(Arc::new(OfflineRegistry))
        .with_lifecycle(options)
        .with_catalog(Arc::new(
            h4rn_daemon::harnesses::StaticHarnessCatalog::with(harness),
        ));
    if let Some(journal) = journal {
        builder = builder.with_journal(journal);
    }
    let daemon = builder.build().await.expect("build");
    let token = TokenStore::new(dir.path())
        .load()
        .expect("read token")
        .expect("token exists");
    let bound = daemon.bind().await.expect("bind");
    let addr = bound.local_addr();
    let addrs = bound.addrs().to_vec();
    let guest = bound.guest();
    let manager = bound.manager();
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
        addrs,
        token,
        dir,
        guest,
        manager,
        shutdown: Some(shutdown),
        task,
    }
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    events: std::collections::VecDeque<Event>,
}

impl Client {
    async fn connect(daemon: &TestDaemon) -> Self {
        Self::connect_at(daemon, daemon.addr).await
    }

    /// Connects to the daemon at a specific address, for a test whose bind is
    /// the wildcard address.
    async fn connect_at(daemon: &TestDaemon, addr: std::net::SocketAddr) -> Self {
        let url = format!("ws://{addr}/ws?token={}", daemon.token);
        let (ws, _) = connect_async(url).await.expect("connect");
        let mut client = Self {
            ws,
            next_id: 1,
            events: std::collections::VecDeque::new(),
        };
        let hello = ClientRequest::Hello {
            protocol_version: h4rn_protocol::PROTOCOL_VERSION,
            device: DeviceInfo {
                device_id: DeviceId::new("dev_owner"),
                name: "owner".into(),
                platform: "linux".into(),
                app_version: "1.0.1".into(),
            },
        };
        let response = client.request(hello).await;
        assert!(matches!(response, ResponseBody::Hello { .. }));
        client
            .request(ClientRequest::Subscribe {
                topics: vec![Topic::All],
                since_seq: None,
            })
            .await;
        client
    }

    async fn request(&mut self, body: ClientRequest) -> ResponseBody {
        let id = self.next_id;
        self.next_id += 1;
        let frame = ClientFrame { id, body };
        self.ws
            .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
            .await
            .expect("send");
        loop {
            let message = tokio::time::timeout(Duration::from_secs(20), self.ws.next())
                .await
                .expect("frame in time")
                .expect("socket open")
                .expect("frame ok");
            let Message::Text(text) = message else {
                continue;
            };
            let frame: ServerFrame = serde_json::from_str(&text).expect("server frame");
            match frame {
                ServerFrame::Response { id: got, result } if got == id => match result {
                    Ok(body) => return body,
                    Err(error) => panic!("request failed: {error:?}"),
                },
                ServerFrame::Event { event, .. } => self.events.push_back(event),
                _ => {}
            }
        }
    }

    async fn next_event(&mut self) -> Event {
        if let Some(event) = self.events.pop_front() {
            return event;
        }
        loop {
            let message = tokio::time::timeout(Duration::from_secs(20), self.ws.next())
                .await
                .expect("event in time")
                .expect("socket open")
                .expect("frame ok");
            let Message::Text(text) = message else {
                continue;
            };
            let frame: ServerFrame = serde_json::from_str(&text).expect("server frame");
            if let ServerFrame::Event { event, .. } = frame {
                return event;
            }
        }
    }
}

/// One browser talking to the guest surface.
#[derive(Clone)]
struct Browser {
    client: reqwest::Client,
    base: String,
    cookie: Option<String>,
}

impl Browser {
    fn new(base: String) -> Self {
        Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client"),
            base,
            cookie: None,
        }
    }

    async fn get(&self, suffix: &str) -> (reqwest::StatusCode, Option<String>, Vec<u8>) {
        let mut request = self.client.get(format!("{}{suffix}", self.base));
        if let Some(cookie) = &self.cookie {
            request = request.header("cookie", format!("h4rn_guest={cookie}"));
        }
        let response = request.send().await.expect("guest GET");
        let status = response.status();
        let set_cookie = response
            .headers()
            .get("set-cookie")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                value
                    .split(';')
                    .next()
                    .and_then(|pair| pair.strip_prefix("h4rn_guest="))
                    .map(str::to_owned)
            });
        let body = response.bytes().await.expect("body").to_vec();
        (status, set_cookie, body)
    }

    async fn post(&self, suffix: &str, body: &impl serde::Serialize) -> (u16, Vec<u8>) {
        let mut request = self
            .client
            .post(format!("{}{suffix}", self.base))
            .json(body);
        if let Some(cookie) = &self.cookie {
            request = request.header("cookie", format!("h4rn_guest={cookie}"));
        }
        let response = request.send().await.expect("guest POST");
        let status = response.status().as_u16();
        (status, response.bytes().await.expect("body").to_vec())
    }

    async fn state(&self, since: u64) -> GuestSnapshot {
        let (status, _, body) = self.get(&format!("/state/{since}")).await;
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&body)
        );
        serde_json::from_slice(&body).expect("snapshot")
    }
}

async fn create_thread(client: &mut Client, dir: &std::path::Path, prompt: &str) -> ThreadId {
    let response = client
        .request(ClientRequest::CreateThread {
            host: HostId::new("host_guest_test"),
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            workspace_path: dir.display().to_string(),
            model: "fake-model".into(),
            prompt: prompt.into(),
            attachments: Vec::new(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            fast: false,
            worktree: None,
        })
        .await;
    match response {
        ResponseBody::ThreadCreated { thread_id, .. } => thread_id,
        other => panic!("unexpected create response: {other:?}"),
    }
}

async fn enable_and_share(
    client: &mut Client,
    guest: &h4rn_daemon::guest::GuestService,
    thread_id: &ThreadId,
    base: &str,
) -> String {
    let response = client
        .request(ClientRequest::SetGuestSettings { enabled: true })
        .await;
    assert!(matches!(response, ResponseBody::GuestSettings { .. }));
    let created = guest
        .create(thread_id, &format!("{base}/guest"))
        .await
        .expect("create link");
    created.link.url.expect("url")
}

#[tokio::test]
async fn a_guest_reads_the_thread_and_sends_a_request_the_owner_approves() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "hello guest").await;

    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let prefix = link.strip_prefix(&base).expect("local base").to_owned();

    // The page is plain HTML and binds this browser with a cookie.
    let browser = Browser::new(format!("{base}{prefix}"));
    let (status, cookie, body) = browser.get("").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("h4rn"));
    let cookie = cookie.expect("binding cookie");
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie: Some(cookie),
    };

    // The state is the thread's own events, with no daemon path in it.
    let snapshot = browser.state(0).await;
    assert_eq!(snapshot.state, GuestLinkState::Active);
    assert_eq!(snapshot.thread.as_ref().unwrap().model, "fake-model");
    let json = serde_json::to_string(&snapshot).unwrap();
    assert!(!json.contains(daemon.dir.path().to_string_lossy().as_ref()));
    assert!(!json.contains("local_path\":\"/"));

    // A guest submits a message; nothing runs yet.
    let message = GuestMessageInput {
        text: "please check the logs".into(),
        attachments: Vec::new(),
    };
    let (status, body) = browser.post("/messages", &message).await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));

    // The owner sees one Request on the thread.
    let mut saw_request = false;
    for _ in 0..20 {
        if let Event::GuestRequested { request, .. } = owner.next_event().await {
            assert_eq!(request.text, "please check the logs");
            saw_request = true;
            break;
        }
    }
    assert!(saw_request, "owner never saw the guest request");
    let response = owner
        .request(ClientRequest::ListThreads {
            filter: Default::default(),
        })
        .await;
    let pending = match response {
        ResponseBody::Threads { threads } => {
            threads
                .into_iter()
                .find(|thread| thread.thread_id == thread_id)
                .expect("thread")
                .pending_guest_requests
        }
        other => panic!("unexpected list: {other:?}"),
    };
    assert_eq!(pending, 1);

    // Send runs a normal turn and clears the Request.
    let response = owner
        .request(ClientRequest::SendGuestRequest {
            thread_id: thread_id.clone(),
            request_id: request_id_from_snapshot(&browser).await,
        })
        .await;
    assert!(matches!(response, ResponseBody::Ack));

    // The approved message becomes the thread's next user turn, and the
    // Request is withdrawn.
    let mut ran = false;
    let mut resolved = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline && !(ran && resolved) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(remaining, owner.next_event())
            .await
            .expect("send events in time");
        match event {
            Event::ItemCompleted {
                item: h4rn_protocol::Item::Message { role, text, .. },
                ..
            } if role == h4rn_protocol::MessageRole::User && text == "please check the logs" => {
                ran = true;
            }
            Event::GuestRequestResolved { sent: true, .. } => resolved = true,
            _ => {}
        }
    }
    assert!(ran, "Send did not run the guest message as a turn");
    assert!(resolved, "Send did not clear the Request");

    // A second browser cannot claim the same link.
    let other = Browser::new(browser.base.clone());
    let (status, _, _) = other.get("").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let state = other.state(0).await;
    assert_eq!(state.state, GuestLinkState::Unbound);

    daemon.stop().await;
}

async fn request_id_from_snapshot(browser: &Browser) -> String {
    // The guest's own pending request is in the thread journal, so the newest
    // snapshot names it without any owner-only API.
    let snapshot = browser.state(0).await;
    for event in snapshot.events.iter().rev() {
        if let Event::GuestRequested { request, .. } = &event.event {
            return request.request_id.clone();
        }
    }
    panic!("no guest request in the snapshot");
}

#[tokio::test]
async fn the_owner_can_dismiss_a_request_and_its_files_go_away() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "dismiss me").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let prefix = link.strip_prefix(&base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };

    let message = GuestMessageInput {
        text: "with a file".into(),
        attachments: vec![h4rn_protocol::GuestAttachmentInput {
            name: "notes.txt".into(),
            mime: "text/plain".into(),
            data: base64_encode(b"hello from the guest"),
        }],
    };
    let (status, body) = browser.post("/messages", &message).await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let request_id = request_id_from_snapshot(&browser).await;
    let directory = daemon.dir.path().join("guest").join(&request_id);
    assert!(directory.is_dir(), "the file is stored before Send");

    let response = owner
        .request(ClientRequest::DismissGuestRequest {
            thread_id,
            request_id,
        })
        .await;
    assert!(matches!(response, ResponseBody::Ack));
    assert!(!directory.exists(), "Dismiss deletes the waiting files");
    daemon.stop().await;
}

#[tokio::test]
async fn limits_types_and_binding_are_enforced_server_side() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "limits").await;

    // Off means no link at all.
    let error = daemon
        .guest
        .create(&thread_id, &format!("{base}/guest"))
        .await
        .expect_err("guest links are off");
    assert!(matches!(error, h4rn_daemon::guest::GuestError::Invalid(_)));

    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let prefix = link.strip_prefix(&base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };

    // A cookie-less data request never gets thread bytes.
    let bare = Browser::new(browser.base.clone());
    let (status, _, _) = bare.get("/state").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let state: GuestSnapshot = serde_json::from_slice(&bare.get("/state").await.2).unwrap();
    assert_eq!(state.state, GuestLinkState::Unbound);

    // Five files is over the limit.
    let file = |name: &str| h4rn_protocol::GuestAttachmentInput {
        name: name.into(),
        mime: "text/plain".into(),
        data: base64_encode(b"x"),
    };
    let too_many = GuestMessageInput {
        text: String::new(),
        attachments: vec![file("a"), file("b"), file("c"), file("d"), file("e")],
    };
    let (status, _) = browser.post("/messages", &too_many).await;
    assert_eq!(status, 400);

    // An image that is not PNG/JPEG/GIF/WebP is refused.
    let bmp = GuestMessageInput {
        text: String::new(),
        attachments: vec![h4rn_protocol::GuestAttachmentInput {
            name: "image.bmp".into(),
            mime: "image/bmp".into(),
            data: base64_encode(b"BM\x00\x00\x00\x00"),
        }],
    };
    let (status, _) = browser.post("/messages", &bmp).await;
    assert_eq!(status, 400);

    // Paths that are not part of the guest surface are refused, including a
    // diff, a terminal and another thread.
    for path in ["/diff", "/terminal", "/files", "/threads/th_other"] {
        let (status, _, _) = browser.get(path).await;
        assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{path}");
    }
    // A blob that is not part of this thread is refused too.
    let unknown_blob = format!("/blob/{}", "a".repeat(64));
    let (status, _, _) = browser.get(&unknown_blob).await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);

    // Turning the setting off disables the live link without deleting it.
    let response = owner
        .request(ClientRequest::SetGuestSettings { enabled: false })
        .await;
    assert!(matches!(response, ResponseBody::GuestSettings { .. }));
    let state = browser.state(0).await;
    assert_eq!(state.state, GuestLinkState::Disabled);
    daemon.stop().await;
}

#[tokio::test]
async fn an_expired_link_stops_and_a_revoked_link_deletes_pending_files() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "expiry").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let token = link.rsplit('/').next().unwrap().to_owned();
    let browser = Browser::new(format!("{base}/guest/{token}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };

    let message = GuestMessageInput {
        text: "keep this for a moment".into(),
        attachments: vec![h4rn_protocol::GuestAttachmentInput {
            name: "notes.txt".into(),
            mime: "text/plain".into(),
            data: base64_encode(b"delete me on revoke"),
        }],
    };
    let (status, _) = browser.post("/messages", &message).await;
    assert_eq!(status, 202);
    let request_id = request_id_from_snapshot(&browser).await;
    let directory = daemon.dir.path().join("guest").join(&request_id);
    assert!(directory.is_dir());

    // Stop sharing with the message still waiting: its files go away.
    owner
        .request(ClientRequest::StopGuestSharing {
            thread_id: thread_id.clone(),
        })
        .await;
    assert!(!directory.exists(), "Stop sharing deletes waiting files");

    // A fresh link, then the 24 hours pass in the journal.
    let second = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let second_token = second.rsplit('/').next().unwrap().to_owned();
    let expired = Browser::new(format!("{base}/guest/{second_token}"));
    let (_, cookie, _) = expired.get("").await;
    let expired = Browser {
        client: expired.client,
        base: expired.base,
        cookie,
    };
    let (status, _) = expired
        .post(
            "/messages",
            &GuestMessageInput {
                text: "this should not survive expiry".into(),
                attachments: vec![h4rn_protocol::GuestAttachmentInput {
                    name: "expired.txt".into(),
                    mime: "text/plain".into(),
                    data: base64_encode(b"expired"),
                }],
            },
        )
        .await;
    assert_eq!(status, 202);
    let expired_request = request_id_from_snapshot(&expired).await;
    let expired_dir = daemon.dir.path().join("guest").join(&expired_request);
    assert!(expired_dir.is_dir());

    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite:{}",
        daemon.dir.path().join("events.db").display()
    ))
    .await
    .expect("open journal");
    sqlx::query("UPDATE guest_links SET expires_at = ? WHERE thread_id = ?")
        .bind((chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339())
        .bind(thread_id.as_str())
        .execute(&pool)
        .await
        .expect("expire link");
    pool.close().await;

    let (status, _, _) = expired.get("").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let state = expired.state(0).await;
    assert_eq!(state.state, GuestLinkState::Expired);
    assert!(
        state
            .notice
            .unwrap_or_default()
            .to_lowercase()
            .contains("expired")
    );
    assert!(!expired_dir.exists(), "expiry deletes waiting files");

    // The guest page itself says the same thing for the expired link.
    let (_, _, body) = expired.get("").await;
    assert!(String::from_utf8_lossy(&body).contains("data-state=\"expired\""));
    daemon.stop().await;
}

#[tokio::test]
async fn creating_a_new_link_revokes_the_old_one() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "rotate").await;
    let first = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let second = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    assert_ne!(first, second);

    let old = Browser::new(format!(
        "{base}/guest/{}",
        first.rsplit('/').next().unwrap()
    ));
    let (status, _, _) = old.get("").await;
    assert_eq!(status, reqwest::StatusCode::GONE);
    let new = Browser::new(format!(
        "{base}/guest/{}",
        second.rsplit('/').next().unwrap()
    ));
    let (status, _, _) = new.get("").await;
    assert_eq!(status, reqwest::StatusCode::OK);

    // Settling (archiving) the thread ends the link too.
    owner
        .request(ClientRequest::SetThreadTab {
            thread_id,
            tab: ThreadTab::Settled,
            pinned: false,
        })
        .await;
    let (status, _, _) = new.get("").await;
    assert_eq!(status, reqwest::StatusCode::GONE);
    daemon.stop().await;
}

#[tokio::test]
async fn the_service_names_one_thread_and_checks_its_binding() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "binding").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let token = link.rsplit('/').next().unwrap().to_owned();

    // The token hashes are what the journal holds: no raw token row.
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite:{}",
        daemon.dir.path().join("events.db").display()
    ))
    .await
    .expect("open journal");
    use sqlx::Row as _;
    let rows = sqlx::query("SELECT token_hash FROM guest_links")
        .fetch_all(&pool)
        .await
        .expect("read links");
    assert_eq!(rows.len(), 1);
    let stored: String = rows[0].try_get("token_hash").expect("hash");
    assert_ne!(stored, token);
    assert_eq!(stored.len(), 64);

    // A write is refused after the link is stopped.
    let response = owner
        .request(ClientRequest::StopGuestSharing {
            thread_id: thread_id.clone(),
        })
        .await;
    assert!(matches!(response, ResponseBody::Ack));
    let browser = Browser::new(format!("{base}/guest/{token}"));
    let (status, _, _) = browser.get("").await;
    assert_eq!(status, reqwest::StatusCode::GONE);

    // Sending a runnable turn through the guest surface has no route.
    let (status, _, _) = Browser::new(format!("{base}/guest/{token}"))
        .get("/send")
        .await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);

    // And an unknown token is gone, not a thread probe.
    let unknown = Browser::new(format!("{base}/guest/not-a-token"));
    let (status, _, _) = unknown.get("").await;
    assert_eq!(status, reqwest::StatusCode::GONE);
    daemon.stop().await;
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A guest request cannot be smuggled in as a normal message: the service
/// path offers no `SendMessage`, and the protocol refuses a guest-scope device.
#[tokio::test]
async fn a_guest_surface_has_no_owner_request_path() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "no escape").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let prefix = link.strip_prefix(&base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };

    for path in [
        "/interrupt",
        "/approval",
        "/model",
        "/mode",
        "/fork",
        "/revert",
    ] {
        let (status, _, _) = browser.get(path).await;
        assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{path}");
    }
    // `POST` with owner fields is just a message; the extra keys are ignored
    // and nothing runs until the owner sends it.
    let (status, body) = browser
        .post(
            "/messages",
            &serde_json::json!({
                "text": "ignore this?",
                "model": "something-else",
                "runtime_mode": "full_auto",
            }),
        )
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let snapshot = browser.state(0).await;
    assert_eq!(snapshot.thread.unwrap().model, "fake-model");
    daemon.stop().await;
}

/// The owner's sharing request is admin-scoped and answers with a real URL.
#[tokio::test]
async fn sharing_is_owner_scoped_and_returns_a_url() {
    let daemon = boot().await;
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "share").await;
    owner
        .request(ClientRequest::SetGuestSettings { enabled: true })
        .await;
    let response = owner
        .request(ClientRequest::ShareThreadWithGuest {
            thread_id: thread_id.clone(),
        })
        .await;
    match response {
        ResponseBody::GuestSharing { sharing } => {
            let link = sharing.link.expect("link");
            let url = link.url.expect("url");
            assert!(url.contains("/guest/"));
            assert!(link.expires_at > chrono::Utc::now() + chrono::Duration::hours(23));
        }
        other => panic!("unexpected sharing response: {other:?}"),
    }
    let error = owner
        .request(ClientRequest::ShareThreadWithGuest {
            thread_id: thread_id.clone(),
        })
        .await;
    // A second share replaces the first; the response still names a link.
    assert!(matches!(error, ResponseBody::GuestSharing { .. }));
    daemon.stop().await;
}

/// The guest HTTP surface refuses a body over the message limit before it can
/// reach the journal.
#[tokio::test]
async fn an_oversized_message_is_refused() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "big").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let prefix = link.strip_prefix(&base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };
    let big = GuestMessageInput {
        text: "big".into(),
        attachments: vec![h4rn_protocol::GuestAttachmentInput {
            name: "big.txt".into(),
            mime: "text/plain".into(),
            data: base64_encode(&vec![b'a'; 11 * 1024 * 1024]),
        }],
    };
    let (status, body) = browser.post("/messages", &big).await;
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));
    daemon.stop().await;
}

/// A successful guest message flow leaves one real attachment in the thread.
async fn shared_guest_message_with_file(
    owner: &mut Client,
    guest: &h4rn_daemon::guest::GuestService,
    base: &str,
    thread_id: &ThreadId,
) -> String {
    let link = enable_and_share(owner, guest, thread_id, base).await;
    let prefix = link.strip_prefix(base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };
    let (status, body) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "here is a file".into(),
                attachments: vec![h4rn_protocol::GuestAttachmentInput {
                    name: "photo.txt".into(),
                    mime: "text/plain".into(),
                    data: base64_encode(b"a real attachment from another thread"),
                }],
            },
        )
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let request_id = request_id_from_snapshot(&browser).await;
    owner
        .request(ClientRequest::SendGuestRequest {
            thread_id: thread_id.clone(),
            request_id,
        })
        .await;
    browser.base
}

#[tokio::test]
async fn a_page_load_never_claims_the_link() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "unclaimed").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let token = link.rsplit('/').next().unwrap().to_owned();
    let url = format!("{base}/guest/{token}");

    // Two cookie-less page loads both succeed, but neither browser is bound:
    // a link-preview crawler cannot steal the link by fetching it.
    for _ in 0..2 {
        let (status, cookie, body) = Browser::new(url.clone()).get("").await;
        assert_eq!(status, reqwest::StatusCode::OK);
        assert!(cookie.is_some(), "the page still offers a pending cookie");
        assert!(String::from_utf8_lossy(&body).contains("data-state=\"active\""));
    }
    let sharing = daemon
        .guest
        .sharing(&thread_id, &format!("{base}/guest"), true)
        .await
        .expect("sharing");
    assert!(!sharing.link.expect("link").bound, "a GET claimed the link");
    let state = Browser::new(url.clone()).state(0).await;
    assert_eq!(
        state.state,
        GuestLinkState::Unbound,
        "a cookieless state read"
    );

    // A page load that presents the pending cookie still only offers a new
    // one: Connect forwards the cookie on a page fetch, so a reload must not
    // bind the link. A state read with that cookie is what claims it.
    let (_, pending, _) = Browser::new(url.clone()).get("").await;
    let pending = pending.expect("pending cookie");
    let mut reload = Browser::new(url.clone());
    reload.cookie = Some(pending.clone());
    let (status, _, _) = reload.get("").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let sharing = daemon
        .guest
        .sharing(&thread_id, &format!("{base}/guest"), true)
        .await
        .expect("sharing");
    assert!(
        !sharing.link.expect("link").bound,
        "a page GET with a cookie claimed the link"
    );

    // The browser that presents a page-load cookie claims the link on state.
    let (_, cookie, _) = Browser::new(url.clone()).get("").await;
    let mut browser = Browser::new(url);
    browser.cookie = cookie;
    let snapshot = browser.state(0).await;
    assert_eq!(snapshot.state, GuestLinkState::Active);
    let sharing = daemon
        .guest
        .sharing(&thread_id, &format!("{base}/guest"), true)
        .await
        .expect("sharing");
    assert!(sharing.link.expect("link").bound);
    daemon.stop().await;
}

#[tokio::test]
async fn a_cross_origin_or_form_post_is_refused() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "origins").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let prefix = link.strip_prefix(&base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };
    let _ = browser.state(0).await;

    let message = GuestMessageInput {
        text: "cross site?".into(),
        attachments: Vec::new(),
    };
    let cross = browser
        .client
        .post(format!("{}{prefix}/messages", base))
        .header("origin", "https://evil.example")
        .header(
            "cookie",
            format!("h4rn_guest={}", browser.cookie.clone().unwrap()),
        )
        .json(&message)
        .send()
        .await
        .expect("cross-origin POST");
    assert_eq!(cross.status(), reqwest::StatusCode::FORBIDDEN);

    let form = browser
        .client
        .post(format!("{}{prefix}/messages", base))
        .header(
            "cookie",
            format!("h4rn_guest={}", browser.cookie.clone().unwrap()),
        )
        .form(&[("text", "a form")])
        .send()
        .await
        .expect("form POST");
    assert_eq!(form.status(), reqwest::StatusCode::FORBIDDEN);

    assert_eq!(
        pending_requests(&daemon, &thread_id).await,
        0,
        "a refused post still queued a request"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn turning_the_labs_switch_off_ends_every_link_and_discards_files() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "labs switch").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let prefix = link.strip_prefix(&base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };
    let (status, _) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "waiting".into(),
                attachments: vec![h4rn_protocol::GuestAttachmentInput {
                    name: "waiting.txt".into(),
                    mime: "text/plain".into(),
                    data: base64_encode(b"discard me"),
                }],
            },
        )
        .await;
    assert_eq!(status, 202);
    let request_id = request_id_from_snapshot(&browser).await;
    let directory = daemon.dir.path().join("guest").join(&request_id);
    assert!(directory.is_dir());

    owner
        .request(ClientRequest::SetGuestSettings { enabled: false })
        .await;
    // The old token no longer names a link: with guest links off the page says
    // so, and after turning them back on the link is still gone.
    let (status, _, body) = browser.get("").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("data-state=\"disabled\""));
    assert!(!directory.exists(), "turning guest links off left a file");
    assert_eq!(
        pending_requests(&daemon, &thread_id).await,
        0,
        "guest links off left a waiting request"
    );
    let sharing = daemon
        .guest
        .sharing(&thread_id, &format!("{base}/guest"), true)
        .await
        .expect("sharing");
    assert!(
        sharing.link.is_none(),
        "turning guest links off kept a link"
    );

    // Turning it back on does not revive the link.
    owner
        .request(ClientRequest::SetGuestSettings { enabled: true })
        .await;
    let (status, _, _) = browser.get("").await;
    assert_eq!(status, reqwest::StatusCode::GONE);
    daemon.stop().await;
}

#[tokio::test]
async fn the_sweep_removes_an_expired_link_and_its_files() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "sweep").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let prefix = link.strip_prefix(&base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };
    let (status, _) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "sweep me".into(),
                attachments: vec![h4rn_protocol::GuestAttachmentInput {
                    name: "sweep.txt".into(),
                    mime: "text/plain".into(),
                    data: base64_encode(b"sweep me"),
                }],
            },
        )
        .await;
    assert_eq!(status, 202);
    let request_id = request_id_from_snapshot(&browser).await;
    let directory = daemon.dir.path().join("guest").join(&request_id);
    assert!(directory.is_dir());

    let pool = journal_pool(&daemon).await;
    sqlx::query("UPDATE guest_links SET expires_at = ? WHERE thread_id = ?")
        .bind((chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339())
        .bind(thread_id.as_str())
        .execute(&pool)
        .await
        .expect("expire link");
    pool.close().await;

    let removed = daemon.guest.sweep_expired().await.expect("sweep");
    assert_eq!(removed, 1);
    assert!(!directory.exists(), "the sweep left an expired link's file");
    let (status, _, _) = browser.get("").await;
    assert_eq!(status, reqwest::StatusCode::GONE);
    daemon.stop().await;
}

#[tokio::test]
async fn a_submit_racing_stop_sharing_leaves_no_request_and_no_files() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "race").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let prefix = link.strip_prefix(&base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };
    let _ = browser.state(0).await;

    let mut submitting = Vec::new();
    for index in 0..8 {
        let browser = browser.clone();
        submitting.push(tokio::spawn(async move {
            browser
                .post(
                    "/messages",
                    &GuestMessageInput {
                        text: format!("racer {index}"),
                        attachments: vec![h4rn_protocol::GuestAttachmentInput {
                            name: format!("race-{index}.txt"),
                            mime: "text/plain".into(),
                            data: base64_encode(format!("racer {index}").as_bytes()),
                        }],
                    },
                )
                .await
        }));
    }
    owner
        .request(ClientRequest::StopGuestSharing {
            thread_id: thread_id.clone(),
        })
        .await;
    for task in submitting {
        let _ = task.await;
    }

    let pool = journal_pool(&daemon).await;
    let requests: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM guest_requests")
        .fetch_one(&pool)
        .await
        .expect("count requests");
    let links: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM guest_links")
        .fetch_one(&pool)
        .await
        .expect("count links");
    pool.close().await;
    assert_eq!(links, 0);
    assert_eq!(requests, 0, "a submit outlived Stop sharing");
    let leftovers: Vec<_> = std::fs::read_dir(daemon.dir.path().join("guest"))
        .expect("guest dir")
        .filter_map(Result::ok)
        .collect();
    assert!(
        leftovers.is_empty(),
        "files without a request: {leftovers:?}"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn send_and_dismiss_claim_a_request_exactly_once() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "claim").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let prefix = link.strip_prefix(&base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };
    let (status, _) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "once".into(),
                attachments: Vec::new(),
            },
        )
        .await;
    assert_eq!(status, 202);
    let request_id = request_id_from_snapshot(&browser).await;

    let send_guest = Arc::clone(&daemon.guest);
    let dismiss_guest = Arc::clone(&daemon.guest);
    let send_thread = thread_id.clone();
    let send_id = request_id.clone();
    let dismiss_thread = thread_id.clone();
    let dismiss_id = request_id.clone();
    let (send, dismiss) = tokio::join!(
        async move { send_guest.send_request(&send_thread, &send_id).await },
        async move {
            dismiss_guest
                .dismiss_request(&dismiss_thread, &dismiss_id)
                .await
        }
    );
    let wins = usize::from(send.is_ok()) + usize::from(dismiss.is_ok());
    assert_eq!(wins, 1, "exactly one action may claim the request");
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0,
        "the answered request still raises a hand"
    );

    // Two concurrent Sends cannot run the same message twice either.
    let second = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let second_prefix = second.strip_prefix(&base).expect("local base").to_owned();
    let browser2 = Browser::new(format!("{base}{second_prefix}"));
    let (_, cookie, _) = browser2.get("").await;
    let browser2 = Browser {
        client: browser2.client,
        base: browser2.base,
        cookie,
    };
    let (status, _) = browser2
        .post(
            "/messages",
            &GuestMessageInput {
                text: "send twice".into(),
                attachments: Vec::new(),
            },
        )
        .await;
    assert_eq!(status, 202);
    let request_id = request_id_from_snapshot(&browser2).await;
    let first_guest = Arc::clone(&daemon.guest);
    let second_guest = Arc::clone(&daemon.guest);
    let thread_a = thread_id.clone();
    let id_a = request_id.clone();
    let thread_b = thread_id.clone();
    let id_b = request_id;
    let (first, second_send) = tokio::join!(
        async move { first_guest.send_request(&thread_a, &id_a).await },
        async move { second_guest.send_request(&thread_b, &id_b).await }
    );
    assert_eq!(
        usize::from(first.is_ok()) + usize::from(second_send.is_ok()),
        1
    );
    daemon.stop().await;
}

#[tokio::test]
async fn text_pending_count_and_budget_are_capped() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "caps").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let prefix = link.strip_prefix(&base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };
    let _ = browser.state(0).await;

    let long = GuestMessageInput {
        text: "x".repeat(h4rn_protocol::GUEST_MAX_TEXT_BYTES + 1),
        attachments: Vec::new(),
    };
    let (status, _) = browser.post("/messages", &long).await;
    assert_eq!(status, 400);

    // A file that claims to be a PNG but is text is sniffed and refused.
    let fake_image = GuestMessageInput {
        text: String::new(),
        attachments: vec![h4rn_protocol::GuestAttachmentInput {
            name: "fake.png".into(),
            mime: "image/png".into(),
            data: base64_encode(b"not a png at all"),
        }],
    };
    let (status, _) = browser.post("/messages", &fake_image).await;
    assert_eq!(status, 400);

    for index in 0..h4rn_protocol::GUEST_MAX_PENDING_REQUESTS {
        let (status, body) = browser
            .post(
                "/messages",
                &GuestMessageInput {
                    text: format!("waiting {index}"),
                    attachments: Vec::new(),
                },
            )
            .await;
        assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    }
    let (status, _) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "one too many".into(),
                attachments: Vec::new(),
            },
        )
        .await;
    assert_eq!(status, 400, "the pending count is not capped");
    daemon.stop().await;
}

#[tokio::test]
async fn the_pending_byte_budget_is_capped() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "budget").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let prefix = link.strip_prefix(&base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };
    let _ = browser.state(0).await;
    let file = |size: usize| h4rn_protocol::GuestAttachmentInput {
        name: "budget.txt".into(),
        mime: "text/plain".into(),
        data: base64_encode(&vec![b'a'; size]),
    };
    // Two 9 MB messages fill 18 of the 20 MB.
    for _ in 0..2 {
        let (status, body) = browser
            .post(
                "/messages",
                &GuestMessageInput {
                    text: String::new(),
                    attachments: vec![file(9 * 1024 * 1024)],
                },
            )
            .await;
        assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    }
    let (status, _) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: String::new(),
                attachments: vec![file(3 * 1024 * 1024)],
            },
        )
        .await;
    assert_eq!(status, 400, "the pending byte budget is not enforced");
    daemon.stop().await;
}

#[tokio::test]
async fn a_real_attachment_from_another_thread_is_refused() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let shared = create_thread(&mut owner, daemon.dir.path(), "shared thread").await;
    let other = create_thread(&mut owner, daemon.dir.path(), "other thread").await;

    // A real attachment lands in `other` through a guest message the owner
    // sends, so it is a genuine blob rather than a fabricated id.
    shared_guest_message_with_file(&mut owner, &daemon.guest, &base, &other).await;
    let response = owner
        .request(ClientRequest::GetThread {
            thread_id: other.clone(),
            since_seq: 0,
        })
        .await;
    let attachment_id = match response {
        ResponseBody::Thread { events, .. } => events
            .into_iter()
            .find_map(|event| match event.event {
                Event::ItemCompleted {
                    item: h4rn_protocol::Item::Message { attachments, .. },
                    ..
                } => attachments
                    .first()
                    .map(|attachment| attachment.id.as_str().to_owned()),
                _ => None,
            })
            .expect("the sent message's attachment"),
        other => panic!("unexpected thread response: {other:?}"),
    };

    let link = enable_and_share(&mut owner, &daemon.guest, &shared, &base).await;
    let prefix = link.strip_prefix(&base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };
    let _ = browser.state(0).await;
    let (status, _, _) = browser.get(&format!("/blob/{attachment_id}")).await;
    assert_eq!(
        status,
        reqwest::StatusCode::NOT_FOUND,
        "a guest read another thread's attachment"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn an_unauthenticated_oversized_post_is_refused_before_its_body() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let response = reqwest::Client::new()
        .post(format!("{base}/guest/not-a-real-token/messages"))
        .header("content-type", "application/json")
        .body(vec![b'a'; 17 * 1024 * 1024])
        .send()
        .await
        .expect("POST");
    // Without the fix the body limit answered first (413); the token is
    // checked before a single byte is buffered.
    assert_eq!(response.status(), reqwest::StatusCode::GONE);
    daemon.stop().await;
}

#[tokio::test]
async fn direct_guest_routes_follow_the_bound_address() {
    let Some(lan) = h4rn_daemon::remote::local_ipv4() else {
        eprintln!("no non-loopback address on this machine; the LAN half cannot be exercised");
        return;
    };
    // The primary bind is the wildcard, as in the documented `--bind
    // 0.0.0.0:47800` setup. It is not a loopback address, but its loopback
    // connection reaches the same socket.
    let daemon = boot_on("0.0.0.0:0".parse().expect("wildcard"), None).await;
    let port = daemon.addr.port();
    let loopback = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    // The direct link names loopback on the port the primary listener really
    // bound, not the Network-access port.
    assert_eq!(
        daemon.guest.local_base(),
        Some(format!("http://127.0.0.1:{port}/guest"))
    );

    // A real link loads over that loopback connection.
    let mut owner = Client::connect_at(&daemon, loopback).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "wildcard bind").await;
    let base = format!("http://127.0.0.1:{port}");
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    assert_eq!(browser.state(0).await.state, GuestLinkState::Active);

    // A LAN peer gets the same 404 an unmounted route gives, even though the
    // link is live.
    let lan_addr = std::net::SocketAddr::new(lan, port);
    let response = reqwest::Client::new()
        .get(format!("http://{lan_addr}/guest/not-a-real-token"))
        .send()
        .await
        .expect("LAN GET");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a guest path answered a non-loopback peer"
    );
    daemon.stop().await;

    // An extra non-loopback bind mounts no guest route at all.
    let daemon = boot_on(
        "127.0.0.1:0".parse().expect("loopback"),
        Some(std::net::SocketAddr::new(lan, 0)),
    )
    .await;
    let extra = daemon.addrs[1];
    assert!(!extra.ip().is_loopback());
    let response = reqwest::Client::new()
        .get(format!("http://{extra}/guest/not-a-real-token"))
        .send()
        .await
        .expect("extra GET");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND,
        "an extra network bind served a guest path"
    );
    // The primary loopback listener still serves it. Guest links are off in
    // this fresh daemon, so the page itself is the honest answer.
    let (status, _, body) = Browser::new(format!("http://{}/guest", daemon.addr))
        .get("/not-a-real-token")
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains(r#"data-state="disabled""#),
        "the primary loopback listener did not serve the guest page"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn the_direct_link_names_the_first_loopback_listener() {
    // An IPv6 loopback bind names itself, not an IPv4 address that cannot
    // answer.
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        eprintln!("no IPv6 loopback on this machine; the [::1] half cannot be exercised");
    } else {
        let daemon = boot_on("[::1]:0".parse().expect("ipv6 loopback"), None).await;
        let port = daemon.addr.port();
        assert_eq!(
            daemon.guest.local_base(),
            Some(format!("http://[::1]:{port}/guest")),
            "an IPv6 loopback bind named an address the link cannot reach"
        );
        daemon.stop().await;
    }

    let Some(lan) = h4rn_daemon::remote::local_ipv4() else {
        eprintln!("no non-loopback address on this machine; the LAN halves cannot be exercised");
        return;
    };

    // A LAN primary bind has no direct guest path, but a loopback extra bind
    // does; the link must name that listener's port.
    let daemon = boot_on(
        std::net::SocketAddr::new(lan, 0),
        Some("127.0.0.1:0".parse().expect("loopback extra")),
    )
    .await;
    let extra = daemon.addrs[1];
    assert!(extra.ip().is_loopback());
    assert_eq!(
        daemon.guest.local_base(),
        Some(format!("http://127.0.0.1:{}/guest", extra.port())),
        "the link ignored the loopback extra bind"
    );
    daemon.stop().await;

    // With no listener that serves direct guest routes there is no link to
    // hand out.
    let daemon = boot_on(std::net::SocketAddr::new(lan, 0), None).await;
    assert_eq!(
        daemon.guest.local_base(),
        None,
        "a LAN-only daemon offered a direct link"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn an_ipv4_mapped_loopback_bind_names_its_reachable_address() {
    // An IPv4-mapped bind really listens on the IPv4 address it names. The
    // link must say so; `[::1]` would be a host where nothing is listening.
    for (mapped, named) in [
        ("[::ffff:127.0.0.1]:0", "127.0.0.1"),
        ("[::ffff:127.0.0.2]:0", "127.0.0.2"),
    ] {
        let bind: std::net::SocketAddr = mapped.parse().expect("mapped loopback");
        if std::net::TcpListener::bind(bind).is_err() {
            eprintln!("cannot bind an IPv4-mapped loopback address here; skipping {mapped}");
            continue;
        }
        let daemon = boot_on(bind, None).await;
        let port = daemon.addr.port();
        assert_eq!(
            daemon.guest.local_base(),
            Some(format!("http://{named}:{port}/guest")),
            "a mapped-address bind handed out a link with no listener"
        );
        // The address the link names really answers.
        let (status, _, body) = Browser::new(format!("http://{named}:{port}"))
            .get("/guest/not-a-real-token")
            .await;
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "the named address did not answer: {}",
            String::from_utf8_lossy(&body)
        );
        daemon.stop().await;
    }
}

#[tokio::test]
async fn a_dual_stack_bind_treats_an_ipv4_loopback_peer_as_this_machine() {
    // Some systems make an IPv6 wildcard socket IPv6-only. Probe the same bind
    // the daemon uses, rather than assume the default.
    if !dual_stack_loopback_available().await {
        eprintln!(
            "this machine does not map IPv4 loopback onto a [::] listener; the v4-mapped peer cannot be exercised"
        );
        return;
    }
    let daemon = boot_on("[::]:0".parse().expect("dual-stack"), None).await;
    let port = daemon.addr.port();
    let v4 = std::net::SocketAddr::from(([127, 0, 0, 1], port));

    // The guest page answers a v4 connection, whose peer address on this
    // socket is `::ffff:127.0.0.1`.
    let (status, _, body) = Browser::new(format!("http://{v4}"))
        .get("/guest/not-a-real-token")
        .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "the guest page refused a v4-mapped loopback peer: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(String::from_utf8_lossy(&body).contains(r#"data-state="disabled""#));

    // `/ws` makes the same check, so the owner's socket also opens.
    let mut owner = Client::connect_at(&daemon, v4).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "mapped loopback").await;
    assert!(thread_id.as_str().starts_with("th_"));
    daemon.stop().await;
}

#[tokio::test]
async fn a_full_pending_budget_refuses_an_upload_before_its_body_is_buffered() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "full budget").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    for index in 0..h4rn_protocol::GUEST_MAX_PENDING_REQUESTS {
        let (status, body) = browser
            .post(
                "/messages",
                &guest_text_message(&format!("waiting {index}")),
            )
            .await;
        assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    }
    assert_eq!(
        pending_requests(&daemon, &thread_id).await,
        h4rn_protocol::GUEST_MAX_PENDING_REQUESTS as i64
    );

    // Send the headers for a 10 MB upload and stop: with the quota checked
    // before the body, the refusal arrives without those bytes ever being
    // buffered.
    let token = link.rsplit('/').next().expect("token");
    let cookie = browser.cookie.clone().expect("binding cookie");
    let mut stream = TcpStream::connect(daemon.addr).await.expect("connect");
    let headers = format!(
        "POST /guest/{token}/messages HTTP/1.1\r\nHost: {}\r\nCookie: h4rn_guest={cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        daemon.addr,
        10 * 1024 * 1024
    );
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    stream
        .write_all(headers.as_bytes())
        .await
        .expect("write headers");
    let mut buffer = vec![0u8; 4096];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
        .await
        .expect("the refusal must not wait for the upload body")
        .expect("read response");
    let response = String::from_utf8_lossy(&buffer[..read]);
    assert!(response.contains("400"), "{response}");
    assert!(response.contains("already waiting"), "{response}");

    // Nothing new was reserved or written.
    assert_eq!(
        pending_requests(&daemon, &thread_id).await,
        h4rn_protocol::GUEST_MAX_PENDING_REQUESTS as i64
    );
    let directories = std::fs::read_dir(daemon.dir.path().join("guest"))
        .expect("guest dir")
        .count();
    assert_eq!(directories, h4rn_protocol::GUEST_MAX_PENDING_REQUESTS);
    daemon.stop().await;
}

#[tokio::test]
#[allow(deprecated)] // SO_LINGER zero is how a test closes a socket with a reset.
async fn a_submission_survives_the_browser_disconnecting_mid_write() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "cancelled").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let token = link.rsplit('/').next().expect("token").to_owned();
    let cookie = browser.cookie.clone().expect("binding cookie");
    let body = serde_json::to_string(&guest_text_message("send me anyway")).expect("json");

    let gate = daemon
        .guest
        .pause_next_submit(h4rn_daemon::guest::SubmitPhase::AfterReserve);
    let mut stream = TcpStream::connect(daemon.addr).await.expect("connect");
    let request = format!(
        "POST /guest/{token}/messages HTTP/1.1\r\nHost: {}\r\nCookie: h4rn_guest={cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        daemon.addr,
        body.len()
    );
    use tokio::io::AsyncWriteExt as _;
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    // The row is reserved and no file exists yet.
    gate.arrived().await;
    // Abort the connection with a reset, the way a closing browser tab does.
    stream
        .set_linger(Some(Duration::ZERO))
        .expect("linger option");
    drop(stream);
    gate.release();

    // The submission finishes on the daemon even though its reader is gone.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while pending_requests(&daemon, &thread_id).await == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "a disconnected submission stayed stranded in `reserving`"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let request_id = request_id_from_snapshot(&browser).await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("pending"),
        "the cancelled submission did not reach the owner"
    );
    assert!(
        daemon.dir.path().join("guest").join(&request_id).exists(),
        "the cancelled submission lost its files"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_cleanup_claim_that_lost_to_send_keeps_the_row_and_files() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "cleanup race").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, body) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "keep me".into(),
                attachments: vec![h4rn_protocol::GuestAttachmentInput {
                    name: "keep.txt".into(),
                    mime: "text/plain".into(),
                    data: base64_encode(b"keep me"),
                }],
            },
        )
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let request_id = request_id_from_snapshot(&browser).await;
    let directory = daemon.dir.path().join("guest").join(&request_id);
    assert!(directory.exists());

    // The link is gone (the crash window after revocation), and the sweep has
    // read the row but not yet claimed it.
    let pool = journal_pool(&daemon).await;
    sqlx::query("DELETE FROM guest_links")
        .execute(&pool)
        .await
        .expect("end the link row");
    pool.close().await;

    let cleanup = daemon.guest.pause_next_cleanup();
    let cleaning = {
        let guest = Arc::clone(&daemon.guest);
        tokio::spawn(async move { guest.sweep_expired().await })
    };
    cleanup.arrived().await;

    // Send claims the same row inside that window.
    let claim = daemon.guest.pause_next_claim();
    let sending = {
        let guest = Arc::clone(&daemon.guest);
        let thread_id = thread_id.clone();
        let request_id = request_id.clone();
        tokio::spawn(async move { guest.send_request(&thread_id, &request_id).await })
    };
    claim.arrived().await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("sending")
    );

    cleanup.release();
    cleaning.await.expect("sweep task").expect("periodic sweep");
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("sending"),
        "the cleanup claim stole a row Send owned"
    );
    assert!(
        directory.join("0-keep.txt").exists(),
        "the cleanup claim deleted a file Send had claimed"
    );
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        1,
        "the cleanup claim resolved a card Send was answering"
    );

    claim.release();
    sending.await.expect("send task").expect("send completed");
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("sent")
    );
    assert!(!directory.exists());
    daemon.stop().await;
}

#[tokio::test]
async fn a_relay_prefix_scopes_the_cookie_and_works_twice() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "relay path").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let token = link.rsplit('/').next().unwrap().to_owned();
    let public_prefix = format!("/guest/machine_1/{token}");

    let page = daemon
        .guest
        .handle(h4rn_daemon::guest::GuestHttpInput {
            method: h4rn_daemon::guest::Method::Get,
            path: format!("/guest/{token}"),
            public_prefix: Some(public_prefix.clone()),
            cookie: None,
            content_type: None,
            origin: None,
            host: None,
            fetch_site: None,
            body: Vec::new(),
        })
        .await;
    let cookie = page.set_cookie.expect("binding cookie");
    // The browser's path is the public one, not the daemon's internal path.
    assert!(
        cookie.contains(&format!("Path={public_prefix}")),
        "{cookie}"
    );
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("Secure"), "{cookie}");
    let value = cookie
        .strip_prefix("h4rn_guest=")
        .and_then(|rest| rest.split(';').next())
        .expect("cookie value")
        .to_owned();

    for _ in 0..2 {
        let state = daemon
            .guest
            .handle(h4rn_daemon::guest::GuestHttpInput {
                method: h4rn_daemon::guest::Method::Get,
                path: format!("/guest/{token}/state"),
                public_prefix: Some(public_prefix.clone()),
                cookie: Some(value.clone()),
                content_type: None,
                origin: None,
                host: None,
                fetch_site: None,
                body: Vec::new(),
            })
            .await;
        assert_eq!(state.status, 200);
        let snapshot: GuestSnapshot = serde_json::from_slice(&state.body).expect("snapshot");
        assert_eq!(snapshot.state, GuestLinkState::Active);
    }
    daemon.stop().await;
}

#[tokio::test]
async fn a_link_with_no_serving_listener_is_marked_unusable() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "no listener").await;
    let _link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;

    // No relay and no loopback listener: the row still exists, but the owner
    // is told why there is no URL instead of being shown a dead link.
    let sharing = daemon
        .guest
        .sharing(&thread_id, "", true)
        .await
        .expect("sharing");
    let link = sharing.link.expect("link");
    assert_eq!(link.url, None);
    assert!(
        link.no_serving_listener,
        "an unreachable link was not flagged"
    );

    // With a base the URL is back and the flag is clear.
    let sharing = daemon
        .guest
        .sharing(&thread_id, &format!("{base}/guest"), true)
        .await
        .expect("sharing");
    let link = sharing.link.expect("link");
    assert!(link.url.is_some());
    assert!(!link.no_serving_listener);
    daemon.stop().await;
}

#[tokio::test]
async fn a_link_shared_on_127_0_0_2_works_end_to_end() {
    // Every address in 127.0.0.0/8 is loopback, but a machine may not have
    // this one configured. Skip rather than fail when the bind is refused.
    if std::net::TcpListener::bind("127.0.0.2:0").is_err() {
        eprintln!("127.0.0.2 is not available on this machine; the extra-loopback half is skipped");
        return;
    }
    let daemon = boot_on("127.0.0.2:0".parse().expect("loopback bind"), None).await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect_at(&daemon, daemon.addr).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "extra loopback").await;
    wait_until_idle(&daemon.manager, &thread_id).await;

    // Share through the request the UI uses: URL validation used to accept
    // only 127.0.0.1 and refused this bind even though it serves the routes.
    owner
        .request(ClientRequest::SetGuestSettings { enabled: true })
        .await;
    let url = match owner
        .request(ClientRequest::ShareThreadWithGuest {
            thread_id: thread_id.clone(),
        })
        .await
    {
        ResponseBody::GuestSharing { sharing } => sharing
            .link
            .expect("link")
            .url
            .expect("a 127.0.0.2 listener must produce a URL"),
        other => panic!("unexpected sharing response: {other:?}"),
    };
    assert!(
        url.starts_with(&format!("http://127.0.0.2:{}", daemon.addr.port())),
        "the link named another address: {url}"
    );

    // The link really answers on that address and binds its browser.
    let prefix = url.strip_prefix(&base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (status, cookie, body) = browser.get("").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&body).contains(r#"data-state="active""#),
        "the 127.0.0.2 link did not serve the active page"
    );
    assert!(cookie.is_some(), "the link did not bind the browser");
    daemon.stop().await;
}

#[tokio::test]
async fn sharing_hides_the_url_from_a_non_admin_reader() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "admin url").await;
    let _link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let admin = daemon
        .guest
        .sharing(&thread_id, &format!("{base}/guest"), true)
        .await
        .expect("admin sharing");
    assert!(admin.link.expect("link").url.is_some());
    let view = daemon
        .guest
        .sharing(&thread_id, &format!("{base}/guest"), false)
        .await
        .expect("view sharing");
    let view_link = view.link.expect("link metadata");
    assert!(
        view_link.url.is_none(),
        "a view-only device got the bearer URL"
    );
    assert!(view_link.expires_at > chrono::Utc::now());
    daemon.stop().await;
}

async fn pending_requests(daemon: &TestDaemon, thread_id: &ThreadId) -> i64 {
    let pool = journal_pool(daemon).await;
    let count = sqlx::query_scalar(
        "SELECT COUNT(*) FROM guest_requests WHERE thread_id = ? AND status = 'pending'",
    )
    .bind(thread_id.as_str())
    .fetch_one(&pool)
    .await
    .expect("count pending requests");
    pool.close().await;
    count
}

/// How many guest links the daemon's store holds.
async fn guest_link_count(daemon: &TestDaemon) -> i64 {
    let pool = journal_pool(daemon).await;
    let count = sqlx::query_scalar("SELECT COUNT(*) FROM guest_links")
        .fetch_one(&pool)
        .await
        .expect("count guest links");
    pool.close().await;
    count
}

/// One browser that loaded the page and claimed its binding cookie.
async fn bound_browser(base: &str, link: &str) -> Browser {
    let prefix = link.strip_prefix(base).expect("local base").to_owned();
    let browser = Browser::new(format!("{base}{prefix}"));
    let (_, cookie, _) = browser.get("").await;
    let browser = Browser {
        client: browser.client,
        base: browser.base,
        cookie,
    };
    let _ = browser.state(0).await;
    browser
}

/// The lifecycle's count of guest messages waiting for the owner.
async fn summary_pending_guest_requests(client: &mut Client, thread_id: &ThreadId) -> u32 {
    match client
        .request(ClientRequest::GetThread {
            thread_id: thread_id.clone(),
            since_seq: u64::MAX,
        })
        .await
    {
        ResponseBody::Thread { summary, .. } => summary.pending_guest_requests,
        other => panic!("unexpected thread response: {other:?}"),
    }
}

/// Every event the owner's thread has recorded, read from the start.
async fn owner_events(client: &mut Client, thread_id: &ThreadId) -> Vec<Event> {
    let mut events = Vec::new();
    let mut since = 0;
    loop {
        match client
            .request(ClientRequest::GetThread {
                thread_id: thread_id.clone(),
                since_seq: since,
            })
            .await
        {
            ResponseBody::Thread {
                events: page,
                next_seq,
                more,
                ..
            } => {
                events.extend(page.into_iter().map(|entry| entry.event));
                if !more {
                    return events;
                }
                since = next_seq;
            }
            other => panic!("unexpected thread response: {other:?}"),
        }
    }
}

/// The events the owner's live socket carries during `window`.
///
/// Used to prove something was *not* published: `next_event` would wait
/// forever for an event that must never arrive.
async fn owner_events_for(client: &mut Client, window: Duration) -> Vec<Event> {
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return events;
        }
        match tokio::time::timeout(left, client.next_event()).await {
            Ok(event) => events.push(event),
            Err(_) => return events,
        }
    }
}

fn guest_text_message(text: &str) -> GuestMessageInput {
    GuestMessageInput {
        text: text.into(),
        attachments: Vec::new(),
    }
}

#[tokio::test]
async fn a_submit_paused_before_the_reservation_never_survives_a_revocation() {
    #[derive(Clone, Copy, Debug)]
    enum Revocation {
        StopSharing,
        NewLink,
        Settle,
        LabsOff,
    }

    for revocation in [
        Revocation::StopSharing,
        Revocation::NewLink,
        Revocation::Settle,
        Revocation::LabsOff,
    ] {
        let daemon = boot().await;
        let base = format!("http://{}", daemon.addr);
        let mut owner = Client::connect(&daemon).await;
        let thread_id = create_thread(&mut owner, daemon.dir.path(), "race").await;
        let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
        let browser = bound_browser(&base, &link).await;

        let gate = daemon
            .guest
            .pause_next_submit(h4rn_daemon::guest::SubmitPhase::BeforeReserve);
        let submitting = {
            let browser = browser.clone();
            tokio::spawn(async move {
                browser
                    .post("/messages", &guest_text_message("raced"))
                    .await
            })
        };
        gate.arrived().await;
        match revocation {
            Revocation::StopSharing => {
                owner
                    .request(ClientRequest::StopGuestSharing {
                        thread_id: thread_id.clone(),
                    })
                    .await;
            }
            Revocation::NewLink => {
                let response = owner
                    .request(ClientRequest::ShareThreadWithGuest {
                        thread_id: thread_id.clone(),
                    })
                    .await;
                assert!(matches!(response, ResponseBody::GuestSharing { .. }));
            }
            Revocation::Settle => {
                owner
                    .request(ClientRequest::SetThreadTab {
                        thread_id: thread_id.clone(),
                        tab: ThreadTab::Settled,
                        pinned: false,
                    })
                    .await;
            }
            Revocation::LabsOff => {
                owner
                    .request(ClientRequest::SetGuestSettings { enabled: false })
                    .await;
            }
        }
        gate.release();
        let (status, body) = submitting.await.expect("submit task");
        assert!(
            status == 410 || status == 400,
            "{revocation:?}: the paused submission answered {status}: {}",
            String::from_utf8_lossy(&body)
        );

        let pool = journal_pool(&daemon).await;
        let requests: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM guest_requests")
            .fetch_one(&pool)
            .await
            .expect("count requests");
        let links: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM guest_links")
            .fetch_one(&pool)
            .await
            .expect("count links");
        pool.close().await;
        assert_eq!(requests, 0, "{revocation:?} left a request behind");
        // A new link replaces the old one; every other revocation ends it.
        let expected_links = if matches!(revocation, Revocation::NewLink) {
            1
        } else {
            0
        };
        assert_eq!(
            links, expected_links,
            "{revocation:?} left the wrong links behind"
        );
        let leftovers: Vec<_> = std::fs::read_dir(daemon.dir.path().join("guest"))
            .expect("guest dir")
            .filter_map(Result::ok)
            .collect();
        assert!(
            leftovers.is_empty(),
            "{revocation:?} left files: {leftovers:?}"
        );
        assert_eq!(
            summary_pending_guest_requests(&mut owner, &thread_id).await,
            0,
            "{revocation:?} left a raised hand"
        );
        // Labs-off is machine wide; turn it back on for the next case.
        let _ = owner
            .request(ClientRequest::SetGuestSettings { enabled: true })
            .await;
        daemon.stop().await;
    }
}

#[tokio::test]
async fn the_pending_budget_is_held_before_any_file_is_written() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "reserve").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let file = |size: usize| h4rn_protocol::GuestAttachmentInput {
        name: "budget.txt".into(),
        mime: "text/plain".into(),
        data: base64_encode(&vec![b'a'; size]),
    };

    let gate = daemon
        .guest
        .pause_next_submit(h4rn_daemon::guest::SubmitPhase::AfterReserve);
    let first = {
        let browser = browser.clone();
        let file = file(10 * 1024 * 1024);
        tokio::spawn(async move {
            browser
                .post(
                    "/messages",
                    &GuestMessageInput {
                        text: String::new(),
                        attachments: vec![file],
                    },
                )
                .await
        })
    };
    // The quota is reserved and no file exists yet.
    gate.arrived().await;
    assert!(
        std::fs::read_dir(daemon.dir.path().join("guest"))
            .expect("guest dir")
            .next()
            .is_none(),
        "a file was written before the budget was reserved"
    );

    // The reservation already holds 10 MB, so a second full message would
    // cross the shared 20 MB cap and must be refused before writing files.
    let (status, body) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                // The text pushes the two messages just past the shared cap.
                text: "x".repeat(h4rn_protocol::GUEST_MAX_TEXT_BYTES),
                attachments: vec![file(10 * 1024 * 1024)],
            },
        )
        .await;
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));
    let directories = std::fs::read_dir(daemon.dir.path().join("guest"))
        .expect("guest dir")
        .count();
    assert_eq!(directories, 0, "the refused message wrote a directory");

    gate.release();
    let (status, body) = first.await.expect("first submit");
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    assert_eq!(pending_requests(&daemon, &thread_id).await, 1);
    daemon.stop().await;
}

#[tokio::test]
async fn a_send_journals_its_message_before_the_card_is_answered() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "accept marker").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, _) = browser
        .post("/messages", &guest_text_message("send me"))
        .await;
    assert_eq!(status, 202);
    let request_id = request_id_from_snapshot(&browser).await;

    // The claim only owns the row. The message is accepted when the thread's
    // journal holds it, and until then the thread has nothing.
    let gate = daemon
        .guest
        .pause_next_send(h4rn_daemon::guest::SendPhase::BeforeAppend);
    let sending = {
        let guest = Arc::clone(&daemon.guest);
        let thread_id = thread_id.clone();
        let request_id = request_id.clone();
        tokio::spawn(async move { guest.send_request(&thread_id, &request_id).await })
    };
    gate.arrived().await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("sending")
    );
    assert!(
        !daemon
            .manager
            .guest_message_in_thread(&thread_id, &request_id)
            .await
            .expect("read the thread's journal"),
        "the thread held the message before the append"
    );

    gate.release();
    sending.await.expect("send task").expect("the send failed");
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("sent")
    );
    assert!(
        daemon
            .manager
            .guest_message_in_thread(&thread_id, &request_id)
            .await
            .expect("read the thread's journal"),
        "the thread never received the message its card calls sent"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_sweep_skips_a_send_in_flight_and_startup_recovers_it() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "recover").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, _) = browser
        .post("/messages", &guest_text_message("crash"))
        .await;
    assert_eq!(status, 202);
    let request_id = request_id_from_snapshot(&browser).await;
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        1
    );

    // Simulate a crash between the Send claim and its resolved event.
    let pool = journal_pool(&daemon).await;
    sqlx::query("UPDATE guest_requests SET status = 'sending' WHERE request_id = ?")
        .bind(&request_id)
        .execute(&pool)
        .await
        .expect("claim the row");
    pool.close().await;

    // The periodic sweep must not pull the row from under a running Send.
    daemon.guest.sweep_expired().await.expect("periodic sweep");
    assert_eq!(pending_requests(&daemon, &thread_id).await, 0);
    let pool = journal_pool(&daemon).await;
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM guest_requests WHERE request_id = ?")
        .bind(&request_id)
        .fetch_one(&pool)
        .await
        .expect("count row");
    pool.close().await;
    assert_eq!(rows, 1, "a periodic sweep deleted a row Send is reading");

    // Startup has no reader, so it resolves the card. The row stays while the
    // link lives: the guest snapshot only mentions a resolution whose request
    // id is still a row for that link, so deleting it here would leave the
    // guest's card up forever.
    let resume = browser.state(0).await.next_seq;
    daemon
        .guest
        .recover_after_restart()
        .await
        .expect("startup sweep");
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0
    );
    let pool = journal_pool(&daemon).await;
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM guest_requests WHERE request_id = ?")
        .bind(&request_id)
        .fetch_one(&pool)
        .await
        .expect("count row");
    pool.close().await;
    assert_eq!(rows, 1, "startup deleted the row the guest needs to poll");
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("dismissed")
    );
    let later = browser.state(resume).await;
    assert!(
        later.events.iter().any(|entry| matches!(
            &entry.event,
            Event::GuestRequestResolved { request_id: id, sent: false, .. } if id == &request_id
        )),
        "the guest never learned its recovered card resolved: {:?}",
        later.events
    );

    // Ending the link drops the provenance row like any finished request.
    owner
        .request(ClientRequest::StopGuestSharing {
            thread_id: thread_id.clone(),
        })
        .await;
    let pool = journal_pool(&daemon).await;
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM guest_requests WHERE request_id = ?")
        .bind(&request_id)
        .fetch_one(&pool)
        .await
        .expect("count row");
    pool.close().await;
    assert_eq!(rows, 0, "an ended link kept a finished request row");
    daemon.stop().await;
}

#[tokio::test]
async fn startup_recovers_a_send_whose_message_is_journaled_as_sent() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "accepted recover").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, _) = browser
        .post("/messages", &guest_text_message("went through"))
        .await;
    assert_eq!(status, 202);
    let request_id = request_id_from_snapshot(&browser).await;

    // The append happened, but the daemon stopped before the row could say
    // `sent`. Recovery must ask the journal, not a flag written beside it.
    let gate = daemon
        .guest
        .pause_next_send(h4rn_daemon::guest::SendPhase::AfterAppend);
    let sending = {
        let guest = Arc::clone(&daemon.guest);
        let thread_id = thread_id.clone();
        let request_id = request_id.clone();
        tokio::spawn(async move { guest.send_request(&thread_id, &request_id).await })
    };
    gate.arrived().await;
    sending.abort();
    let _ = sending.await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("sending"),
        "the simulated crash did not leave a claimed row"
    );
    assert!(
        daemon
            .manager
            .guest_message_in_thread(&thread_id, &request_id)
            .await
            .expect("read the thread's journal"),
        "the crash point was before the append"
    );

    let resume = browser.state(0).await.next_seq;
    daemon
        .guest
        .recover_after_restart()
        .await
        .expect("startup sweep");
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0,
        "the owner still shows a pending guest request"
    );
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("sent"),
        "a journaled send was recovered as never sent"
    );
    // The same browser's next poll carries the answer that clears its card.
    let later = browser.state(resume).await;
    assert!(
        later.events.iter().any(|entry| matches!(
            &entry.event,
            Event::GuestRequestResolved { request_id: id, sent: true, .. } if id == &request_id
        )),
        "the guest never learned its accepted message resolved: {:?}",
        later.events
    );
    daemon.stop().await;
}

#[tokio::test]
async fn startup_recovers_a_journaled_send_whose_link_expired_while_down() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "expired after append").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, _) = browser
        .post(
            "/messages",
            &guest_text_message("arrived before the link died"),
        )
        .await;
    assert_eq!(status, 202);
    let request_id = request_id_from_snapshot(&browser).await;

    // The append happened, then the daemon stopped before recording `sent`.
    let gate = daemon
        .guest
        .pause_next_send(h4rn_daemon::guest::SendPhase::AfterAppend);
    let sending = {
        let guest = Arc::clone(&daemon.guest);
        let thread_id = thread_id.clone();
        let request_id = request_id.clone();
        tokio::spawn(async move { guest.send_request(&thread_id, &request_id).await })
    };
    gate.arrived().await;
    sending.abort();
    let _ = sending.await;
    assert!(
        daemon
            .manager
            .guest_message_in_thread(&thread_id, &request_id)
            .await
            .expect("read the thread's journal"),
        "the crash point was before the append"
    );

    // The 24 hours pass while the daemon is down, so startup ends the link
    // before it recovers the row. The owner's thread still has the message, and
    // it must be answered as sent rather than dismissed.
    let pool = journal_pool(&daemon).await;
    sqlx::query("UPDATE guest_links SET expires_at = ? WHERE thread_id = ?")
        .bind((chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339())
        .bind(thread_id.as_str())
        .execute(&pool)
        .await
        .expect("expire link");
    pool.close().await;

    daemon
        .guest
        .recover_after_restart()
        .await
        .expect("startup sweep");
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0,
        "the owner still shows a pending guest request"
    );
    let events = owner_events(&mut owner, &thread_id).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::GuestRequestResolved { request_id: id, sent: true, .. } if id == &request_id
        )),
        "a journaled send whose link expired was never answered as sent: {events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::GuestRequestResolved { request_id: id, sent: false, .. } if id == &request_id
        )),
        "a journaled send was dismissed because its link expired"
    );
    let pool = journal_pool(&daemon).await;
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM guest_requests WHERE request_id = ?")
        .bind(&request_id)
        .fetch_one(&pool)
        .await
        .expect("count row");
    pool.close().await;
    assert_eq!(rows, 0, "an ended link kept a finished request row");
    daemon.stop().await;
}

#[tokio::test]
async fn a_guest_prompt_that_may_have_been_delivered_is_recovered_as_sent() {
    // The thread's first turn is held, so the guest Send has to queue; the
    // second write is held too, so the promoted prompt sits at a `sending`
    // claim: the restart a crash there leaves shows it as "May have been
    // delivered".
    let (harness, completion) = FakeHarness::new().with_completion_gate();
    let (harness, sends) = harness.with_send_count();
    let (harness, write_gate) = harness.with_send_gate_after(1);
    let daemon = boot_with_harness(Arc::new(harness)).await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "guest in flight").await;
    sends.wait_for(1).await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, _) = browser
        .post("/messages", &guest_text_message("may have arrived"))
        .await;
    assert_eq!(status, 202);
    let request_id = request_id_from_snapshot(&browser).await;

    // The prompt is queued. The daemon stops here, after the append and before
    // the row could record `sent`, so the row is claimed and the prompt waits.
    let gate = daemon
        .guest
        .pause_next_send(h4rn_daemon::guest::SendPhase::AfterAppend);
    let sending = {
        let guest = Arc::clone(&daemon.guest);
        let thread_id = thread_id.clone();
        let request_id = request_id.clone();
        tokio::spawn(async move { guest.send_request(&thread_id, &request_id).await })
    };
    gate.arrived().await;
    sending.abort();
    let _ = sending.await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("sending"),
        "the simulated crash did not leave a claimed row"
    );

    // The running turn ends and its settlement promotes the guest prompt; the
    // adapter's write is held, so the promotion's claim is `sending`.
    completion.release();
    sends.wait_for(2).await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("sending")
    );
    let queue = daemon
        .manager
        .thread_queue(&thread_id, 0)
        .await
        .expect("queue");
    assert!(
        queue.queued.is_empty(),
        "the promoted guest prompt is still listed as waiting"
    );
    assert!(
        daemon
            .manager
            .guest_message_in_thread(&thread_id, &request_id)
            .await
            .expect("read the thread's journal"),
        "the thread owns the promoted prompt but calls it unaccepted"
    );

    // Startup recovery asks the journal. The item exists, so the guest is told
    // its message was sent and never that it was dismissed.
    let resume = browser.state(0).await.next_seq;
    daemon
        .guest
        .recover_after_restart()
        .await
        .expect("startup sweep");
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("sent"),
        "a guest prompt that may have been delivered was resolved as never sent"
    );
    let later = browser.state(resume).await;
    assert!(
        later.events.iter().any(|entry| matches!(
            &entry.event,
            Event::GuestRequestResolved { request_id: id, sent: true, .. } if id == &request_id
        )),
        "the guest never learned its prompt was sent: {:?}",
        later.events
    );
    assert!(
        !later.events.iter().any(|entry| matches!(
            &entry.event,
            Event::GuestRequestResolved { request_id: id, sent: false, .. } if id == &request_id
        )),
        "a guest prompt whose item exists was resolved as dismissed"
    );
    write_gate.release();
    daemon.stop().await;
}

#[tokio::test]
async fn a_crashed_send_releases_the_thread_attachment_it_staged() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "staged files").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, body) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "with a file".into(),
                attachments: vec![h4rn_protocol::GuestAttachmentInput {
                    name: "staged.txt".into(),
                    mime: "text/plain".into(),
                    data: base64_encode(b"staged before the append"),
                }],
            },
        )
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let request_id = request_id_from_snapshot(&browser).await;
    let guest_dir = daemon.dir.path().join("guest").join(&request_id);
    let thread_attachments = daemon
        .dir
        .path()
        .join("attachments")
        .join(thread_id.as_str());
    assert!(guest_dir.is_dir());
    assert!(count_files(&thread_attachments) == 0);

    // The append is paused: the file has already been copied into the thread
    // and its reference taken, but the message is not journaled yet.
    let gate = daemon.manager.pause_next_append();
    let sending = {
        let guest = Arc::clone(&daemon.guest);
        let thread_id = thread_id.clone();
        let request_id = request_id.clone();
        tokio::spawn(async move { guest.send_request(&thread_id, &request_id).await })
    };
    gate.arrived().await;
    sending.abort();
    let _ = sending.await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("sending")
    );
    assert!(
        !daemon
            .manager
            .guest_message_in_thread(&thread_id, &request_id)
            .await
            .expect("read the thread's journal"),
        "the crash point was after the append"
    );
    assert_eq!(
        count_files(&thread_attachments),
        1,
        "the crash point was before the attachment was resolved into the thread"
    );
    let blobs = blob_pool(&daemon).await;
    let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE refcount > 0")
        .fetch_one(&blobs)
        .await
        .expect("count retained blobs");
    assert_eq!(retained, 1, "the staged blob was not referenced");

    // Recovery dismisses the send because the thread never got it, and the
    // references it staged must go with the guest's own files.
    daemon
        .guest
        .recover_after_restart()
        .await
        .expect("startup sweep");
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("dismissed")
    );
    assert!(!guest_dir.exists(), "the dismissed send kept its files");
    assert_eq!(
        count_files(&thread_attachments),
        0,
        "the thread kept a copy only the failed send referenced"
    );
    let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blobs WHERE refcount > 0")
        .fetch_one(&blobs)
        .await
        .expect("count retained blobs");
    assert_eq!(retained, 0, "a dismissed send kept its blob reference");
    let holds: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blob_holds")
        .fetch_one(&blobs)
        .await
        .expect("count holds");
    assert_eq!(holds, 0, "a recovered send kept its staged hold");
    blobs.close().await;
    daemon.stop().await;
}

#[tokio::test]
async fn startup_recovers_a_send_that_never_reached_the_thread_as_dismissed() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "lost send").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, body) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "never arrived".into(),
                attachments: vec![h4rn_protocol::GuestAttachmentInput {
                    name: "note.txt".into(),
                    mime: "text/plain".into(),
                    data: base64_encode(b"hello"),
                }],
            },
        )
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let request_id = request_id_from_snapshot(&browser).await;
    let directory = daemon.dir.path().join("guest").join(&request_id);
    assert!(directory.exists(), "the request wrote no directory");

    // The claim happened, then the daemon stopped before the message could be
    // appended. The thread never got it, so the guest must not be told it did.
    let gate = daemon
        .guest
        .pause_next_send(h4rn_daemon::guest::SendPhase::BeforeAppend);
    let sending = {
        let guest = Arc::clone(&daemon.guest);
        let thread_id = thread_id.clone();
        let request_id = request_id.clone();
        tokio::spawn(async move { guest.send_request(&thread_id, &request_id).await })
    };
    gate.arrived().await;
    sending.abort();
    let _ = sending.await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("sending")
    );

    let resume = browser.state(0).await.next_seq;
    daemon
        .guest
        .recover_after_restart()
        .await
        .expect("startup sweep");
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0,
        "the owner still shows a pending guest request"
    );
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("dismissed"),
        "a send that never reached the thread was recovered as sent"
    );
    assert!(
        !directory.exists(),
        "a dismissed send kept the files of a message the thread never got"
    );
    let later = browser.state(resume).await;
    assert!(
        later.events.iter().any(|entry| matches!(
            &entry.event,
            Event::GuestRequestResolved { request_id: id, sent: false, .. } if id == &request_id
        )),
        "the guest never learned its lost message was dismissed: {:?}",
        later.events
    );
    daemon.stop().await;
}

#[tokio::test]
async fn an_auto_settled_thread_ends_its_guest_link() {
    let daemon = boot_with(
        LifecycleSettings::default().with_auto_settle(chrono::Duration::seconds(5)),
        None,
    )
    .await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "settle me").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    assert_eq!(browser.state(0).await.state, GuestLinkState::Active);

    // The idle rule is derived, so only the ticker can apply it. Watch the
    // thread tab, never the guest link: a poll would end the link itself. The
    // read can report Settled a moment before the tick's hook runs, so wait
    // for the link to actually go.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let response = owner
            .request(ClientRequest::GetThread {
                thread_id: thread_id.clone(),
                since_seq: u64::MAX,
            })
            .await;
        let tab = match response {
            ResponseBody::Thread { summary, .. } => summary.tab,
            other => panic!("unexpected thread response: {other:?}"),
        };
        let links = guest_link_count(&daemon).await;
        if tab == ThreadTab::Settled && links == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the thread auto-settled ({tab:?}) but kept {links} guest link(s)"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let (state, _, _) = browser.get("/state").await;
    assert_eq!(state, reqwest::StatusCode::GONE);
    daemon.stop().await;
}

#[tokio::test]
async fn the_sweep_ends_a_link_left_on_a_settled_thread() {
    // A long tick so the lifecycle hook cannot run: this is the crash window
    // where the thread was settled but the daemon stopped before it could tell
    // the guest service. The startup sweep must end the link by itself.
    let daemon = boot_with_tick(
        LifecycleSettings::default(),
        None,
        Duration::from_secs(3600),
    )
    .await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "swept settle").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, _) = browser
        .post("/messages", &guest_text_message("waiting"))
        .await;
    assert_eq!(status, 202);
    let request_id = request_id_from_snapshot(&browser).await;

    // Settle through the manager, not the request handler: the hook is what
    // normally ends the link, and this test pretends that call never happened.
    daemon
        .manager
        .set_thread_tab(&thread_id, ThreadTab::Settled, false)
        .await
        .expect("settle the thread");
    assert_eq!(guest_link_count(&daemon).await, 1);

    // The link is still live, so a Send and a Dismiss must refuse the settled
    // thread by themselves and leave the request waiting for the sweep.
    assert!(
        daemon
            .guest
            .send_request(&thread_id, &request_id)
            .await
            .is_err(),
        "a settled thread sent a waiting guest message"
    );
    assert!(
        daemon
            .guest
            .dismiss_request(&thread_id, &request_id)
            .await
            .is_err(),
        "a settled thread dismissed a waiting guest message"
    );
    assert_eq!(
        pending_requests(&daemon, &thread_id).await,
        1,
        "a refused Send or Dismiss removed the request"
    );
    let response = owner
        .request(ClientRequest::GetThread {
            thread_id: thread_id.clone(),
            since_seq: u64::MAX,
        })
        .await;
    match response {
        ResponseBody::Thread { summary, .. } => {
            assert_eq!(summary.tab, ThreadTab::Settled, "a guest un-settled it");
        }
        other => panic!("unexpected thread response: {other:?}"),
    }

    let removed = daemon
        .guest
        .recover_after_restart()
        .await
        .expect("startup sweep");
    assert!(removed >= 1, "the sweep did not end the settled link");
    assert_eq!(guest_link_count(&daemon).await, 0);
    assert_eq!(pending_requests(&daemon, &thread_id).await, 0);
    let pool = journal_pool(&daemon).await;
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM guest_requests WHERE request_id = ?")
        .bind(&request_id)
        .fetch_one(&pool)
        .await
        .expect("count row");
    pool.close().await;
    assert_eq!(rows, 0, "a settled sweep left its request behind");
    assert!(
        std::fs::read_dir(daemon.dir.path().join("guest"))
            .expect("guest dir")
            .next()
            .is_none(),
        "a settled sweep left its files behind"
    );
    let (state, _, _) = browser.get("/state").await;
    assert_eq!(state, reqwest::StatusCode::GONE);
    daemon.stop().await;
}

#[tokio::test]
async fn a_guest_read_of_a_settled_thread_revokes_instead_of_reading() {
    // A long tick so the lifecycle hook cannot hide the refusal: the resolve
    // path itself must notice the settled thread and end the link.
    let daemon = boot_with_tick(
        LifecycleSettings::default(),
        None,
        Duration::from_secs(3600),
    )
    .await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "read settled").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    assert_eq!(browser.state(0).await.state, GuestLinkState::Active);

    daemon
        .manager
        .set_thread_tab(&thread_id, ThreadTab::Settled, false)
        .await
        .expect("settle the thread");
    assert_eq!(guest_link_count(&daemon).await, 1);

    let snapshot = browser.state(0).await;
    assert_eq!(
        snapshot.state,
        GuestLinkState::Revoked,
        "a settled thread's guest read was answered instead of revoked"
    );
    assert_eq!(guest_link_count(&daemon).await, 0);
    daemon.stop().await;
}

#[tokio::test]
async fn a_guest_write_to_a_settled_thread_is_refused_before_it_is_stored() {
    let daemon = boot_with_tick(
        LifecycleSettings::default(),
        None,
        Duration::from_secs(3600),
    )
    .await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "write settled").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;

    daemon
        .manager
        .set_thread_tab(&thread_id, ThreadTab::Settled, false)
        .await
        .expect("settle the thread");
    let (status, body) = browser
        .post("/messages", &guest_text_message("too late"))
        .await;
    assert_eq!(
        status,
        410,
        "a settled thread took a guest message: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(pending_requests(&daemon, &thread_id).await, 0);
    assert_eq!(guest_link_count(&daemon).await, 0);
    let response = owner
        .request(ClientRequest::GetThread {
            thread_id: thread_id.clone(),
            since_seq: u64::MAX,
        })
        .await;
    match response {
        ResponseBody::Thread { summary, .. } => {
            assert_eq!(summary.tab, ThreadTab::Settled, "a guest un-settled it");
        }
        other => panic!("unexpected thread response: {other:?}"),
    }
    assert!(
        std::fs::read_dir(daemon.dir.path().join("guest"))
            .expect("guest dir")
            .next()
            .is_none(),
        "the refused message wrote a directory"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_settled_thread_takes_no_new_link_and_no_guest_message() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "settled").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, _) = browser
        .post("/messages", &guest_text_message("before"))
        .await;
    assert_eq!(status, 202);
    let request_id = request_id_from_snapshot(&browser).await;

    owner
        .request(ClientRequest::SetThreadTab {
            thread_id: thread_id.clone(),
            tab: ThreadTab::Settled,
            pinned: false,
        })
        .await;
    // Settling ends the link and resolves the waiting card.
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0
    );
    let created = daemon
        .guest
        .create(&thread_id, &format!("{base}/guest"))
        .await;
    assert!(created.is_err(), "a settled thread accepted a new link");
    let (state, _, _) = browser.get("/state").await;
    assert_eq!(state, reqwest::StatusCode::GONE);
    let sent = daemon.guest.send_request(&thread_id, &request_id).await;
    assert!(sent.is_err(), "a settled thread accepted a guest message");
    let response = owner
        .request(ClientRequest::GetThread {
            thread_id: thread_id.clone(),
            since_seq: u64::MAX,
        })
        .await;
    match response {
        ResponseBody::Thread { summary, .. } => {
            assert_eq!(summary.tab, ThreadTab::Settled, "the guest un-settled it");
            assert_eq!(summary.pending_guest_requests, 0);
        }
        other => panic!("unexpected thread response: {other:?}"),
    }
    daemon.stop().await;
}

/// A journal with no SQL pool, as an embedder's custom journal has.
struct PoolLessJournal(Arc<h4rn_daemon::journal::SqliteJournal>);

#[async_trait::async_trait]
impl h4rn_daemon::journal::Journal for PoolLessJournal {
    async fn upsert_thread(
        &self,
        meta: &h4rn_daemon::journal::ThreadMeta,
    ) -> Result<(), h4rn_daemon::journal::JournalError> {
        self.0.upsert_thread(meta).await
    }

    async fn list_threads(
        &self,
    ) -> Result<Vec<h4rn_daemon::journal::ThreadMeta>, h4rn_daemon::journal::JournalError> {
        self.0.list_threads().await
    }

    async fn append(
        &self,
        meta: &h4rn_daemon::journal::ThreadMeta,
        domain: &[h4rn_core::ThreadEvent],
        wire: &[Event],
    ) -> Result<Vec<u64>, h4rn_daemon::journal::JournalError> {
        self.0.append(meta, domain, wire).await
    }

    async fn append_sent(
        &self,
        meta: &h4rn_daemon::journal::ThreadMeta,
        domain: &[h4rn_core::ThreadEvent],
        wire: &[Event],
        item_id: &str,
    ) -> Result<Vec<u64>, h4rn_daemon::journal::JournalError> {
        self.0.append_sent(meta, domain, wire, item_id).await
    }

    async fn append_steer(
        &self,
        meta: &h4rn_daemon::journal::ThreadMeta,
        domain: &[h4rn_core::ThreadEvent],
        wire: &[Event],
        item_id: &str,
    ) -> Result<Vec<u64>, h4rn_daemon::journal::JournalError> {
        self.0.append_steer(meta, domain, wire, item_id).await
    }

    async fn append_wire(
        &self,
        thread_id: &ThreadId,
        events: &[Event],
    ) -> Result<Vec<u64>, h4rn_daemon::journal::JournalError> {
        self.0.append_wire(thread_id, events).await
    }

    async fn read_wire_since(
        &self,
        thread_id: &ThreadId,
        after: u64,
    ) -> Result<Vec<h4rn_daemon::journal::WireEvent>, h4rn_daemon::journal::JournalError> {
        self.0.read_wire_since(thread_id, after).await
    }

    async fn read_wire_since_global(
        &self,
        after: u64,
    ) -> Result<Vec<h4rn_daemon::journal::WireEvent>, h4rn_daemon::journal::JournalError> {
        self.0.read_wire_since_global(after).await
    }

    async fn read_wire_page(
        &self,
        thread_id: &ThreadId,
        after: u64,
        max_events: usize,
        max_bytes: usize,
    ) -> Result<h4rn_daemon::journal::WirePage, h4rn_daemon::journal::JournalError> {
        self.0
            .read_wire_page(thread_id, after, max_events, max_bytes)
            .await
    }

    async fn head(&self) -> Result<u64, h4rn_daemon::journal::JournalError> {
        self.0.head().await
    }

    async fn read_domain(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<h4rn_core::ThreadEvent>, h4rn_daemon::journal::JournalError> {
        self.0.read_domain(thread_id).await
    }

    async fn read_journaled_turns(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<h4rn_daemon::journal::JournaledTurn>, h4rn_daemon::journal::JournalError> {
        self.0.read_journaled_turns(thread_id).await
    }
    async fn note_native_turn(
        &self,
        thread_id: &ThreadId,
        turn_id: &h4rn_protocol::TurnId,
        native_turn_id: &str,
    ) -> Result<(), h4rn_daemon::journal::JournalError> {
        self.0
            .note_native_turn(thread_id, turn_id, native_turn_id)
            .await
    }
    async fn clear_handoff_pending(
        &self,
        thread_id: &ThreadId,
        switch_generation: u64,
    ) -> Result<bool, h4rn_daemon::journal::JournalError> {
        self.0
            .clear_handoff_pending(thread_id, switch_generation)
            .await
    }

    async fn fall_back_from_native_resume(
        &self,
        thread_id: &ThreadId,
        switch_generation: u64,
        rewind_fallback: bool,
    ) -> Result<bool, h4rn_daemon::journal::JournalError> {
        self.0
            .fall_back_from_native_resume(thread_id, switch_generation, rewind_fallback)
            .await
    }

    async fn mark_item_accepted(
        &self,
        thread_id: &ThreadId,
        item_id: &str,
    ) -> Result<(), h4rn_daemon::journal::JournalError> {
        self.0.mark_item_accepted(thread_id, item_id).await
    }

    async fn pending_item_ids(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<String>, h4rn_daemon::journal::JournalError> {
        self.0.pending_item_ids(thread_id).await
    }

    async fn upsert_pr_link(
        &self,
        link: &h4rn_daemon::pr_links::PrLinkRecord,
    ) -> Result<(), h4rn_daemon::journal::JournalError> {
        self.0.upsert_pr_link(link).await
    }

    async fn remove_pr_link(
        &self,
        thread_id: &ThreadId,
        repo: &h4rn_protocol::ForgeRepoRef,
        number: u32,
    ) -> Result<(), h4rn_daemon::journal::JournalError> {
        self.0.remove_pr_link(thread_id, repo, number).await
    }

    async fn pr_links_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<h4rn_daemon::pr_links::PrLinkRecord>, h4rn_daemon::journal::JournalError> {
        self.0.pr_links_for_thread(thread_id).await
    }

    async fn pr_links_for_pr(
        &self,
        repo: &h4rn_protocol::ForgeRepoRef,
        number: u32,
    ) -> Result<Vec<h4rn_daemon::pr_links::PrLinkRecord>, h4rn_daemon::journal::JournalError> {
        self.0.pr_links_for_pr(repo, number).await
    }

    async fn list_schedules(
        &self,
    ) -> Result<Vec<h4rn_protocol::ScheduleTask>, h4rn_daemon::journal::JournalError> {
        self.0.list_schedules().await
    }

    async fn schedule(
        &self,
        task_id: &h4rn_protocol::ScheduleTaskId,
    ) -> Result<Option<h4rn_protocol::ScheduleTask>, h4rn_daemon::journal::JournalError> {
        self.0.schedule(task_id).await
    }

    async fn upsert_schedule(
        &self,
        task: &h4rn_protocol::ScheduleTask,
    ) -> Result<(), h4rn_daemon::journal::JournalError> {
        self.0.upsert_schedule(task).await
    }

    async fn delete_schedule(
        &self,
        task_id: &h4rn_protocol::ScheduleTaskId,
    ) -> Result<bool, h4rn_daemon::journal::JournalError> {
        self.0.delete_schedule(task_id).await
    }

    async fn append_schedule_run(
        &self,
        run: &mut h4rn_protocol::ScheduleRun,
        keep: usize,
    ) -> Result<(), h4rn_daemon::journal::JournalError> {
        self.0.append_schedule_run(run, keep).await
    }

    async fn commit_schedule_run(
        &self,
        run: &mut h4rn_protocol::ScheduleRun,
        task: &h4rn_protocol::ScheduleTask,
        keep: usize,
    ) -> Result<(), h4rn_daemon::journal::JournalError> {
        self.0.commit_schedule_run(run, task, keep).await
    }

    async fn schedule_runs(
        &self,
        task_id: &h4rn_protocol::ScheduleTaskId,
        limit: usize,
    ) -> Result<Vec<h4rn_protocol::ScheduleRun>, h4rn_daemon::journal::JournalError> {
        self.0.schedule_runs(task_id, limit).await
    }

    async fn upsert_issue_link(
        &self,
        link: &h4rn_daemon::issue_links::IssueLinkRecord,
    ) -> Result<(), h4rn_daemon::journal::JournalError> {
        self.0.upsert_issue_link(link).await
    }

    async fn remove_issue_link(
        &self,
        thread_id: &ThreadId,
        scope: &h4rn_protocol::IssueScope,
        issue_id: &str,
    ) -> Result<(), h4rn_daemon::journal::JournalError> {
        self.0.remove_issue_link(thread_id, scope, issue_id).await
    }

    async fn issue_links_for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<h4rn_daemon::issue_links::IssueLinkRecord>, h4rn_daemon::journal::JournalError>
    {
        self.0.issue_links_for_thread(thread_id).await
    }

    async fn list_imported_sources(
        &self,
    ) -> Result<Vec<h4rn_daemon::journal::ImportedSourceRecord>, h4rn_daemon::journal::JournalError>
    {
        self.0.list_imported_sources().await
    }

    async fn record_imported_source(
        &self,
        record: &h4rn_daemon::journal::ImportedSourceRecord,
    ) -> Result<(), h4rn_daemon::journal::JournalError> {
        self.0.record_imported_source(record).await
    }

    async fn commit_thread_import(
        &self,
        claim: &h4rn_daemon::journal::ImportedSourceRecord,
        meta: &h4rn_daemon::journal::ThreadMeta,
        domain: &[h4rn_core::ThreadEvent],
        wire: &[Event],
    ) -> Result<Option<Vec<u64>>, h4rn_daemon::journal::JournalError> {
        self.0.commit_thread_import(claim, meta, domain, wire).await
    }
}

#[tokio::test]
async fn a_custom_journal_still_gets_a_working_guest_store() {
    let journal = Arc::new(
        h4rn_daemon::journal::SqliteJournal::in_memory()
            .await
            .expect("journal"),
    );
    let daemon = boot_with(
        LifecycleSettings::default(),
        Some(Arc::new(PoolLessJournal(journal))),
    )
    .await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "custom journal").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;

    // Every pooled connection to `sqlite::memory:` is a different database, so
    // the fallback store must cap the pool at one. Without the cap a second
    // connection opens at once and the tables vanish on it.
    let pool = daemon.guest.store_pool().clone();
    let first = pool.acquire().await.expect("first connection");
    let second = tokio::time::timeout(Duration::from_millis(300), pool.acquire()).await;
    assert!(
        second.is_err(),
        "the fallback guest store opened a second in-memory connection"
    );
    drop(first);

    // With one connection, concurrent reads and writes must still all find the
    // guest tables.
    let mut tasks = Vec::new();
    for index in 0..8 {
        let browser = browser.clone();
        tasks.push(tokio::spawn(async move {
            if index % 2 == 0 {
                let (status, body) = browser.post("/messages", &guest_text_message("hi")).await;
                assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
            } else {
                let snapshot = browser.state(0).await;
                assert_eq!(snapshot.state, GuestLinkState::Active);
            }
        }));
    }
    for task in tasks {
        task.await.expect("guest task");
    }
    let snapshot = browser.state(0).await;
    assert_eq!(snapshot.state, GuestLinkState::Active);
    daemon.stop().await;
}

async fn journal_pool(daemon: &TestDaemon) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect(&format!(
        "sqlite:{}",
        daemon.dir.path().join("events.db").display()
    ))
    .await
    .expect("open journal")
}

/// The blob store's own database, for reference and hold assertions.
async fn blob_pool(daemon: &TestDaemon) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect(&format!(
        "sqlite:{}",
        daemon.dir.path().join("blobs").join("meta.db").display()
    ))
    .await
    .expect("open blob store")
}

/// How many files one directory holds; a missing directory is empty.
fn count_files(directory: &std::path::Path) -> usize {
    std::fs::read_dir(directory)
        .map(|entries| entries.flatten().count())
        .unwrap_or(0)
}

/// The stored status of one guest request, or none when the row is gone.
async fn request_status(daemon: &TestDaemon, request_id: &str) -> Option<String> {
    let pool = journal_pool(daemon).await;
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM guest_requests WHERE request_id = ?")
            .bind(request_id)
            .fetch_optional(&pool)
            .await
            .expect("read request status");
    pool.close().await;
    status
}

/// Writes a request row in the state a crash during publication would leave.
///
/// The files and the row exist, but no card was raised: this is the window the
/// brief names between the row and the owner's event.
async fn a_row_interrupted_while_publishing(
    daemon: &TestDaemon,
    thread_id: &ThreadId,
    text: &str,
) -> String {
    let pool = journal_pool(daemon).await;
    let link_id: String = sqlx::query_scalar("SELECT link_id FROM guest_links WHERE thread_id = ?")
        .bind(thread_id.as_str())
        .fetch_one(&pool)
        .await
        .expect("the thread's link");
    let request_id = format!("greq_{}", uuid::Uuid::now_v7());
    let directory = daemon.dir.path().join("guest").join(&request_id);
    std::fs::create_dir_all(&directory).expect("request directory");
    std::fs::write(directory.join("0-note.txt"), b"file").expect("request file");
    let attachments = serde_json::json!([{
        "name": "note.txt",
        "mime": "text/plain",
        "size": 4,
        "file": "0-note.txt",
    }]);
    sqlx::query(
        "INSERT INTO guest_requests
            (request_id, link_id, thread_id, text, attachments_json, size_bytes, created_at, status)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'publishing')",
    )
    .bind(&request_id)
    .bind(&link_id)
    .bind(thread_id.as_str())
    .bind(text)
    .bind(attachments.to_string())
    .bind((text.len() + 4) as i64)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&pool)
    .await
    .expect("insert a publishing row");
    pool.close().await;
    request_id
}

#[tokio::test]
async fn a_publishing_row_is_recovered_into_a_card_never_an_orphan() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "publishing").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;

    // The crash window: the row and its files exist, the card does not.
    let request_id =
        a_row_interrupted_while_publishing(&daemon, &thread_id, "half published").await;
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0
    );
    let removed = daemon
        .guest
        .recover_after_restart()
        .await
        .expect("startup recovery");
    assert_eq!(removed, 0, "a live link was ended by startup");
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("pending"),
        "recovery left the row publishing"
    );
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        1,
        "recovery left a row that uses quota and has no card"
    );
    let snapshot = browser.state(0).await;
    assert!(
        snapshot.events.iter().any(|entry| matches!(
            &entry.event,
            Event::GuestRequested { request, .. } if request.request_id == request_id
        )),
        "recovery did not raise the guest's waiting card"
    );
    assert!(
        daemon
            .dir
            .path()
            .join("guest")
            .join(&request_id)
            .join("0-note.txt")
            .exists(),
        "recovery discarded a live request's files"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_publishing_row_with_no_link_is_discarded_with_its_files() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "orphan").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let _browser = bound_browser(&base, &link).await;
    let request_id = a_row_interrupted_while_publishing(&daemon, &thread_id, "lost row").await;

    // The link went away while the row was mid-publication.
    let pool = journal_pool(&daemon).await;
    sqlx::query("DELETE FROM guest_links")
        .execute(&pool)
        .await
        .expect("remove the link");
    pool.close().await;

    daemon
        .guest
        .recover_after_restart()
        .await
        .expect("startup recovery");
    assert_eq!(request_status(&daemon, &request_id).await, None);
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0
    );
    assert!(
        !daemon.dir.path().join("guest").join(&request_id).exists(),
        "a discarded publishing row kept its files"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_sweep_never_touches_a_send_that_claimed_its_row() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "send race").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    // A real file so the sweep has something it could wrongly delete.
    let (status, body) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "send me".into(),
                attachments: vec![h4rn_protocol::GuestAttachmentInput {
                    name: "keep.txt".into(),
                    mime: "text/plain".into(),
                    data: base64_encode(b"keep me"),
                }],
            },
        )
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let request_id = request_id_from_snapshot(&browser).await;
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        1
    );
    let directory = daemon.dir.path().join("guest").join(&request_id);
    assert!(directory.exists());

    // Pause the Send after it claimed the row, then run the sweep against it.
    let gate = daemon.guest.pause_next_claim();
    let sending = {
        let guest = Arc::clone(&daemon.guest);
        let thread_id = thread_id.clone();
        let request_id = request_id.clone();
        tokio::spawn(async move { guest.send_request(&thread_id, &request_id).await })
    };
    gate.arrived().await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("sending")
    );
    daemon.guest.sweep_expired().await.expect("periodic sweep");
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("sending"),
        "the sweep stole a row Send owned"
    );
    assert!(
        directory.join("0-keep.txt").exists(),
        "the sweep deleted an attachment Send had claimed"
    );
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        1,
        "the sweep resolved a card Send was answering"
    );

    gate.release();
    sending.await.expect("send task").expect("send completed");
    assert!(!directory.exists(), "the sent request kept its files");
    // The approved message is in the transcript with its file.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let response = owner
            .request(ClientRequest::GetThread {
                thread_id: thread_id.clone(),
                since_seq: 0,
            })
            .await;
        let found = match response {
            ResponseBody::Thread { events, .. } => events.iter().any(|entry| {
                matches!(
                    &entry.event,
                    Event::ItemCompleted { item: h4rn_protocol::Item::Message { text, attachments, .. }, .. }
                        if text == "send me" && !attachments.is_empty()
                )
            }),
            other => panic!("unexpected thread response: {other:?}"),
        };
        if found {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the sent guest message never reached the transcript"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon.stop().await;
}

#[tokio::test]
async fn a_paused_reservation_never_leaks_after_a_revocation() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "reserve race").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;

    let gate = daemon
        .guest
        .pause_next_submit(h4rn_daemon::guest::SubmitPhase::AfterReserve);
    let submitting = {
        let browser = browser.clone();
        tokio::spawn(async move {
            browser
                .post(
                    "/messages",
                    &GuestMessageInput {
                        text: "raced reservation".into(),
                        attachments: vec![h4rn_protocol::GuestAttachmentInput {
                            name: "raced.txt".into(),
                            mime: "text/plain".into(),
                            data: base64_encode(b"raced"),
                        }],
                    },
                )
                .await
        })
    };
    gate.arrived().await;
    // The reservation is held by a live writer; the sweep must leave it alone.
    daemon.guest.sweep_expired().await.expect("periodic sweep");
    let pool = journal_pool(&daemon).await;
    let reserving: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM guest_requests WHERE status = 'reserving'")
            .fetch_one(&pool)
            .await
            .expect("count reservations");
    pool.close().await;
    assert_eq!(
        reserving, 1,
        "a sweep released a reservation its writer still owned"
    );
    // End the link while the writer still owns it.
    owner
        .request(ClientRequest::StopGuestSharing {
            thread_id: thread_id.clone(),
        })
        .await;
    gate.release();
    let (status, body) = submitting.await.expect("submit task");
    assert!(
        status == 410 || status == 400,
        "the paused reservation answered {status}: {}",
        String::from_utf8_lossy(&body)
    );
    let pool = journal_pool(&daemon).await;
    let requests: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM guest_requests")
        .fetch_one(&pool)
        .await
        .expect("count requests");
    pool.close().await;
    assert_eq!(requests, 0, "a revoked reservation left a row");
    let leftovers: Vec<_> = std::fs::read_dir(daemon.dir.path().join("guest"))
        .expect("guest dir")
        .filter_map(Result::ok)
        .collect();
    assert!(leftovers.is_empty(), "a revoked reservation left files");
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0
    );
    let _ = owner
        .request(ClientRequest::SetGuestSettings { enabled: true })
        .await;
    daemon.stop().await;
}

#[tokio::test]
async fn a_new_link_never_replays_the_previous_links_requests() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "two links").await;

    // Link A sends text and a file name; the owner dismisses it.
    let link_a = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser_a = bound_browser(&base, &link_a).await;
    let (status, body) = browser_a
        .post(
            "/messages",
            &GuestMessageInput {
                text: "secret from the first guest".into(),
                attachments: vec![h4rn_protocol::GuestAttachmentInput {
                    name: "first-guest-secret.txt".into(),
                    mime: "text/plain".into(),
                    data: base64_encode(b"first guest secret"),
                }],
            },
        )
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let first_request = request_id_from_snapshot(&browser_a).await;
    owner
        .request(ClientRequest::DismissGuestRequest {
            thread_id: thread_id.clone(),
            request_id: first_request,
        })
        .await;

    // An approved message still belongs to the thread and shows for everyone.
    let (status, _) = browser_a
        .post("/messages", &guest_text_message("approved for the thread"))
        .await;
    assert_eq!(status, 202);
    let approved = request_id_from_snapshot(&browser_a).await;
    owner
        .request(ClientRequest::SendGuestRequest {
            thread_id: thread_id.clone(),
            request_id: approved,
        })
        .await;

    // Link B is a different guest and must not see link A's request text,
    // dismissed text or attachment names.
    let link_b = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser_b = bound_browser(&base, &link_b).await;
    let snapshot = browser_b.state(0).await;
    let json = serde_json::to_string(&snapshot).expect("snapshot json");
    assert!(
        !json.contains("secret from the first guest"),
        "a new link saw the previous link's dismissed text: {json}"
    );
    assert!(
        !json.contains("first-guest-secret.txt"),
        "a new link saw the previous link's file name: {json}"
    );
    assert!(
        json.contains("approved for the thread"),
        "the approved message must appear through the normal transcript"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_sweep_removes_a_row_whose_link_is_gone() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "no link").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, _) = browser
        .post("/messages", &guest_text_message("row without a link"))
        .await;
    assert_eq!(status, 202);
    let request_id = request_id_from_snapshot(&browser).await;

    // The link row vanishes without running its cleanup: the sweep is the only
    // remaining owner of a pending row whose link no longer exists.
    let pool = journal_pool(&daemon).await;
    sqlx::query("DELETE FROM guest_links")
        .execute(&pool)
        .await
        .expect("remove the link row");
    pool.close().await;
    daemon.guest.sweep_expired().await.expect("periodic sweep");
    assert_eq!(request_status(&daemon, &request_id).await, None);
    assert!(
        !daemon.dir.path().join("guest").join(&request_id).exists(),
        "a sweep left the files of a row with no link"
    );
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_thumbnail_zero_request_serves_the_original_image() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "image").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;

    // A real image big enough to have a thumbnail of its own.
    let image = image::RgbaImage::from_pixel(640, 400, image::Rgba([120, 40, 200, 255]));
    let mut original = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(
            &mut std::io::Cursor::new(&mut original),
            image::ImageFormat::Png,
        )
        .expect("encode png");
    let (status, body) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "a picture".into(),
                attachments: vec![h4rn_protocol::GuestAttachmentInput {
                    name: "picture.png".into(),
                    mime: "image/png".into(),
                    data: base64_encode(&original),
                }],
            },
        )
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let request_id = request_id_from_snapshot(&browser).await;
    owner
        .request(ClientRequest::SendGuestRequest {
            thread_id: thread_id.clone(),
            request_id,
        })
        .await;

    let response = owner
        .request(ClientRequest::GetThread {
            thread_id: thread_id.clone(),
            since_seq: 0,
        })
        .await;
    let attachment_id = match response {
        ResponseBody::Thread { events, .. } => events
            .into_iter()
            .find_map(|entry| match entry.event {
                Event::ItemCompleted {
                    item: h4rn_protocol::Item::Message { attachments, .. },
                    ..
                } => attachments
                    .first()
                    .map(|attachment| attachment.id.as_str().to_owned()),
                _ => None,
            })
            .expect("the sent image's attachment"),
        other => panic!("unexpected thread response: {other:?}"),
    };

    let (status, _, thumbnail) = browser
        .get(&format!("/blob/{attachment_id}?thumbnail=1"))
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let (status, _, full) = browser
        .get(&format!("/blob/{attachment_id}?thumbnail=0"))
        .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        full, original,
        "?thumbnail=0 did not serve the original image bytes"
    );
    assert!(
        thumbnail.len() < full.len(),
        "?thumbnail=1 served the full image ({} vs {})",
        thumbnail.len(),
        full.len()
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_create_racing_the_labs_switch_never_leaves_a_link_behind() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "labs race").await;

    for round in 0..20 {
        owner
            .request(ClientRequest::SetGuestSettings { enabled: true })
            .await;
        let guest = Arc::clone(&daemon.guest);
        let creating = {
            let thread_id = thread_id.clone();
            let prefix = format!("{base}/guest");
            tokio::spawn(async move { guest.create(&thread_id, &prefix).await.is_ok() })
        };
        let disabling = {
            let guest = Arc::clone(&daemon.guest);
            tokio::spawn(async move { guest.set_enabled(false).await.is_ok() })
        };
        let (created, disabled) = tokio::join!(creating, disabling);
        let created = created.expect("create task");
        assert!(disabled.expect("disable task"));
        // Whatever the order, a switch that ends off leaves no link, and a
        // create that won must be the one that runs before the disable.
        if !daemon.guest.enabled().await.expect("setting") {
            assert_eq!(
                guest_link_count(&daemon).await,
                0,
                "round {round}: a create racing the disable left a live link"
            );
        } else if created {
            assert_eq!(guest_link_count(&daemon).await, 1);
        }
    }

    // Re-enabling never revives a link the switch ended.
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    owner
        .request(ClientRequest::SetGuestSettings { enabled: false })
        .await;
    assert_eq!(guest_link_count(&daemon).await, 0);
    assert_eq!(pending_requests(&daemon, &thread_id).await, 0);
    owner
        .request(ClientRequest::SetGuestSettings { enabled: true })
        .await;
    let token = link.rsplit('/').next().unwrap().to_owned();
    let stale = reqwest::get(format!("{base}/guest/{token}/state")).await;
    let stale = stale.expect("stale link request");
    assert_eq!(
        stale.status(),
        reqwest::StatusCode::GONE,
        "re-enabling the Labs switch revived a link it had ended"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_publication_after_the_link_expired_is_refused() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "expire at publish").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;

    // Pause after the quota is reserved; the link expires before publication.
    let gate = daemon
        .guest
        .pause_next_submit(h4rn_daemon::guest::SubmitPhase::AfterReserve);
    let submitting = {
        let browser = browser.clone();
        tokio::spawn(async move {
            browser
                .post("/messages", &guest_text_message("too late"))
                .await
        })
    };
    gate.arrived().await;
    let pool = journal_pool(&daemon).await;
    sqlx::query("UPDATE guest_links SET expires_at = ?")
        .bind((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339())
        .execute(&pool)
        .await
        .expect("expire the link");
    pool.close().await;
    gate.release();
    let (status, body) = submitting.await.expect("submit task");
    assert_eq!(
        status,
        410,
        "a submission whose link expired was answered: {}",
        String::from_utf8_lossy(&body)
    );
    // No row, no directory, no card: publication refused everything.
    let pool = journal_pool(&daemon).await;
    let requests: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM guest_requests")
        .fetch_one(&pool)
        .await
        .expect("count requests");
    pool.close().await;
    assert_eq!(requests, 0, "an expired publication left a row");
    assert!(
        std::fs::read_dir(daemon.dir.path().join("guest"))
            .expect("guest dir")
            .next()
            .is_none(),
        "an expired publication left files"
    );
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0,
        "an expired publication raised a card"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_dismiss_that_claimed_its_row_is_never_reported_missing() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "dismiss race").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, _) = browser
        .post("/messages", &guest_text_message("dismiss me twice"))
        .await;
    assert_eq!(status, 202);
    let request_id = request_id_from_snapshot(&browser).await;

    // Pause the Dismiss after it claimed the row, then let the sweep remove the
    // claimed row. The Dismiss already has its data, so it must still succeed.
    let gate = daemon.guest.pause_next_claim();
    let dismissing = {
        let guest = Arc::clone(&daemon.guest);
        let thread_id = thread_id.clone();
        let request_id = request_id.clone();
        tokio::spawn(async move { guest.dismiss_request(&thread_id, &request_id).await })
    };
    gate.arrived().await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("dismissed")
    );
    daemon.guest.sweep_expired().await.expect("periodic sweep");
    gate.release();
    dismissing
        .await
        .expect("dismiss task")
        .expect("a claimed dismissal reported missing");
    // The row stays while its link lives: its id is what lets the guest learn
    // that its card resolved. Ending the link removes it.
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("dismissed")
    );
    // The sweep emitted the resolution while the Dismiss was paused; the
    // Dismiss then resumed with the row it read before its claim. The answer
    // belongs to the request, not to whichever writer ran last: one event.
    let pool = journal_pool(&daemon).await;
    let resolutions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM daemon_wire_events
         WHERE thread_id = ? AND event_json LIKE '%guest_request_resolved%'
           AND event_json LIKE ?",
    )
    .bind(thread_id.as_str())
    .bind(format!("%{request_id}%"))
    .fetch_one(&pool)
    .await
    .expect("count resolutions");
    pool.close().await;
    assert_eq!(
        resolutions, 1,
        "the dismissal journalled the same answer twice"
    );
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0
    );
    daemon.stop().await;
}

/// A card is journaled only once its row can be sent or dismissed.
///
/// The row transition to `pending` is the card's second half. Pausing between
/// them proves the row is already answerable before anything is published, so
/// the owner's first act on the card can never be answered "no longer exists".
#[tokio::test]
async fn a_card_is_published_only_after_its_row_is_answerable() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "card order").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;

    let gate = daemon
        .guest
        .pause_next_submit(h4rn_daemon::guest::SubmitPhase::AfterCommit);
    let posting = {
        let browser = browser.clone();
        tokio::spawn(async move {
            browser
                .post("/messages", &guest_text_message("answer me at once"))
                .await
        })
    };
    gate.arrived().await;
    // The card is journaled, and its row already says it can be answered.
    let pool = journal_pool(&daemon).await;
    let statuses: Vec<String> = sqlx::query_scalar("SELECT status FROM guest_requests")
        .fetch_all(&pool)
        .await
        .expect("read request rows");
    pool.close().await;
    assert_eq!(
        statuses,
        vec!["pending".to_string()],
        "a card was published before its row could be answered"
    );
    // And nothing has been published yet: the owner's live stream stays empty
    // while the submit is paused, so no client can act on a card early.
    let seen = owner_events_for(&mut owner, Duration::from_millis(200)).await;
    assert!(
        seen.iter()
            .all(|event| !matches!(event, Event::GuestRequested { .. })),
        "the card was published before its row could be answered: {seen:?}"
    );
    gate.release();

    let (status, body) = posting.await.expect("post task");
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let request_id = request_id_from_snapshot(&browser).await;
    // The owner's first act on the published card succeeds.
    owner
        .request(ClientRequest::DismissGuestRequest {
            thread_id: thread_id.clone(),
            request_id: request_id.clone(),
        })
        .await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("dismissed")
    );
    daemon.stop().await;
}

/// A card whose row update fails is never published, and the sweep raises it
/// only once its row can be answered.
#[tokio::test]
async fn a_card_whose_row_update_fails_is_never_published_and_the_sweep_recovers_it() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "card commit").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;

    // Make the row's last half fail exactly as a refused database would: the
    // card is journaled and the row stays `publishing`.
    let pool = journal_pool(&daemon).await;
    sqlx::query(
        "CREATE TRIGGER fail_pending BEFORE UPDATE ON guest_requests
         WHEN NEW.status = 'pending'
         BEGIN SELECT RAISE(ABORT, 'injected'); END",
    )
    .execute(&pool)
    .await
    .expect("install the failure");
    let (status, body) = browser
        .post("/messages", &guest_text_message("never unanswerable"))
        .await;
    assert_eq!(
        status,
        500,
        "a card that could not become answerable was accepted: {}",
        String::from_utf8_lossy(&body)
    );
    let rows: Vec<String> = sqlx::query_scalar("SELECT status FROM guest_requests")
        .fetch_all(&pool)
        .await
        .expect("read request rows");
    assert_eq!(
        rows,
        vec!["publishing".to_string()],
        "a failed update lost the row the sweep needs"
    );
    sqlx::query("DROP TRIGGER fail_pending")
        .execute(&pool)
        .await
        .expect("remove the failure");
    pool.close().await;

    // Nothing reached the owner or the guest: the card is not in the live
    // stream, the guest's snapshot hides an unanswerable row, and the thread
    // shows no pending request.
    let published = owner_events_for(&mut owner, Duration::from_millis(300)).await;
    assert!(
        published
            .iter()
            .all(|event| !matches!(event, Event::GuestRequested { .. })),
        "a card the owner cannot answer was published: {published:?}"
    );
    let snapshot = browser.state(0).await;
    assert!(
        snapshot
            .events
            .iter()
            .all(|entry| !matches!(entry.event, Event::GuestRequested { .. })),
        "the guest saw a card it could not have answered: {:?}",
        snapshot.events
    );
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0,
        "an unanswerable card raised a pending count"
    );

    // The sweep finishes the second half, and the card is answerable when it
    // appears.
    daemon.guest.sweep_expired().await.expect("sweep");
    let request_id = request_id_from_snapshot(&browser).await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("pending")
    );
    owner
        .request(ClientRequest::DismissGuestRequest {
            thread_id: thread_id.clone(),
            request_id: request_id.clone(),
        })
        .await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("dismissed")
    );
    daemon.stop().await;
}

/// Opens an owner socket and completes `Hello`, leaving it ready to send one
/// request whose reply the test may never read.
async fn owner_socket(daemon: &TestDaemon) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
    let url = format!("ws://{}/ws?token={}", daemon.addr, daemon.token);
    let (mut ws, _) = connect_async(url).await.expect("connect");
    let hello = ClientFrame {
        id: 1,
        body: ClientRequest::Hello {
            protocol_version: h4rn_protocol::PROTOCOL_VERSION,
            device: DeviceInfo {
                device_id: DeviceId::new("dev_owner"),
                name: "owner".into(),
                platform: "linux".into(),
                app_version: "1.0.1".into(),
            },
        },
    };
    ws.send(Message::Text(
        serde_json::to_string(&hello).expect("hello json").into(),
    ))
    .await
    .expect("send hello");
    loop {
        let message = tokio::time::timeout(Duration::from_secs(20), ws.next())
            .await
            .expect("hello in time")
            .expect("socket open")
            .expect("frame ok");
        let Message::Text(text) = message else {
            continue;
        };
        let frame: ServerFrame = serde_json::from_str(&text).expect("server frame");
        if let ServerFrame::Response { id: 1, result } = frame {
            assert!(result.is_ok(), "hello failed: {result:?}");
            break;
        }
    }
    ws
}

/// Closes an owner socket with a reset, as a closing app window does.
#[allow(deprecated)] // SO_LINGER zero is how a test closes a socket with a reset.
fn reset_socket(ws: WebSocketStream<MaybeTlsStream<TcpStream>>) {
    if let MaybeTlsStream::Plain(stream) = ws.get_ref() {
        stream
            .set_linger(Some(Duration::ZERO))
            .expect("linger option");
    }
    drop(ws);
}

/// Waits until a thread has no turn in flight, so a test can send the next one
/// into a quiet record.
async fn wait_until_idle(manager: &h4rn_daemon::threads::ThreadManager, thread_id: &ThreadId) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while manager.turn_in_flight(thread_id).await {
        assert!(
            std::time::Instant::now() < deadline,
            "the thread never became idle"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Whether an IPv6 wildcard listener accepts an IPv4 loopback connection on
/// this machine, which is what the daemon's `[::]` bind relies on.
async fn dual_stack_loopback_available() -> bool {
    let Ok(listener) = tokio::net::TcpListener::bind("[::]:0").await else {
        return false;
    };
    let Ok(addr) = listener.local_addr() else {
        return false;
    };
    tokio::time::timeout(
        Duration::from_secs(2),
        TcpStream::connect(("127.0.0.1", addr.port())),
    )
    .await
    .is_ok_and(|result| result.is_ok())
}

#[tokio::test]
async fn a_send_message_mid_append_still_delivers_after_the_socket_resets() {
    let daemon = boot().await;
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "append disconnect").await;
    wait_until_idle(&daemon.manager, &thread_id).await;

    // Hold the next turn's journal write after the record already marks the
    // turn active, then reset the owner's socket in that window. The handler
    // must not be dropped: nothing else owns the delivery yet.
    let gate = daemon.manager.pause_next_append();
    let mut ws = owner_socket(&daemon).await;
    let request = ClientFrame {
        id: 2,
        body: ClientRequest::SendMessage {
            thread_id: thread_id.clone(),
            text: "deliver me anyway".into(),
            attachments: Vec::new(),
            steer: false,
            turn_id: None,
        },
    };
    ws.send(Message::Text(
        serde_json::to_string(&request)
            .expect("request json")
            .into(),
    ))
    .await
    .expect("send request");
    gate.arrived().await;
    reset_socket(ws);
    // Let the connection observe the reset before the append is released.
    tokio::time::sleep(Duration::from_millis(100)).await;
    gate.release();

    // The turn must finish: a turn marked active with no delivery would keep
    // `turn_in_flight` true forever.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while daemon.manager.turn_in_flight(&thread_id).await {
        assert!(
            std::time::Instant::now() < deadline,
            "a reset socket stranded the turn before its delivery"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // It was a real turn, not just a settled record.
    let snapshot = daemon
        .manager
        .get_thread(&thread_id, 0)
        .await
        .expect("thread snapshot");
    assert!(
        snapshot.events.iter().any(|event| matches!(
            &event.event,
            Event::ItemCompleted {
                item: h4rn_protocol::Item::Message { role: h4rn_protocol::MessageRole::User, text, .. },
                ..
            } if text == "deliver me anyway"
        )),
        "the canceled SendMessage never wrote its message"
    );
    assert!(
        snapshot
            .events
            .iter()
            .any(|event| matches!(event.event, Event::TurnCompleted { .. })),
        "the turn was left active with no completion"
    );
    daemon.stop().await;
}

#[tokio::test]
#[allow(deprecated)]
async fn a_send_finishes_after_the_owner_disconnects_mid_claim() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "send disconnect").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, body) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "run even if I leave".into(),
                attachments: vec![h4rn_protocol::GuestAttachmentInput {
                    name: "note.txt".into(),
                    mime: "text/plain".into(),
                    data: base64_encode(b"keep me"),
                }],
            },
        )
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let request_id = request_id_from_snapshot(&browser).await;
    let directory = daemon.dir.path().join("guest").join(&request_id);
    assert!(directory.exists());

    // Hold the Send after it claimed the row, then drop the owner's socket and
    // let the claim continue. The work belongs to the daemon, not the socket.
    let gate = daemon.guest.pause_next_claim();
    let mut ws = owner_socket(&daemon).await;
    let request = ClientFrame {
        id: 2,
        body: ClientRequest::SendGuestRequest {
            thread_id: thread_id.clone(),
            request_id: request_id.clone(),
        },
    };
    ws.send(Message::Text(
        serde_json::to_string(&request)
            .expect("request json")
            .into(),
    ))
    .await
    .expect("send request");
    gate.arrived().await;
    reset_socket(ws);
    // Let the reader task observe the reset; the paused claim must not be the
    // only thing this connection is waiting on.
    tokio::time::sleep(Duration::from_millis(100)).await;
    gate.release();

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if request_status(&daemon, &request_id).await.as_deref() == Some("sent") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a dropped owner socket stranded the Send in {request_id}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!directory.exists(), "the sent request kept its files");
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0,
        "the sent request still shows as a Request"
    );
    daemon.stop().await;
}

#[tokio::test]
#[allow(deprecated)]
async fn a_dismiss_finishes_after_the_owner_disconnects_mid_claim() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "dismiss disconnect").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, body) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "throw me away".into(),
                attachments: vec![h4rn_protocol::GuestAttachmentInput {
                    name: "note.txt".into(),
                    mime: "text/plain".into(),
                    data: base64_encode(b"delete me"),
                }],
            },
        )
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let request_id = request_id_from_snapshot(&browser).await;
    let directory = daemon.dir.path().join("guest").join(&request_id);
    assert!(directory.exists());

    let gate = daemon.guest.pause_next_claim();
    let mut ws = owner_socket(&daemon).await;
    let request = ClientFrame {
        id: 2,
        body: ClientRequest::DismissGuestRequest {
            thread_id: thread_id.clone(),
            request_id: request_id.clone(),
        },
    };
    ws.send(Message::Text(
        serde_json::to_string(&request)
            .expect("request json")
            .into(),
    ))
    .await
    .expect("send request");
    gate.arrived().await;
    reset_socket(ws);
    // Let the reader task observe the reset; the paused claim must not be the
    // only thing this connection is waiting on.
    tokio::time::sleep(Duration::from_millis(100)).await;
    gate.release();

    // The claim alone is not a finished Dismiss: its files go and its card is
    // resolved after the claim, and a dropped socket must not stop that. Wait
    // for the end of the dismissal, not for the status word.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let status = request_status(&daemon, &request_id).await;
        let pending = summary_pending_guest_requests(&mut owner, &thread_id).await;
        if status.as_deref() == Some("dismissed") && pending == 0 && !directory.exists() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a dropped owner socket stranded the Dismiss in {request_id}: {status:?}, {pending} pending"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!directory.exists(), "the dismissed request kept its files");
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0
    );
    daemon.stop().await;
}

#[tokio::test]
async fn an_error_with_a_host_path_reaches_the_owner_but_not_the_guest() {
    let host_path = "/home/owner/.h4rn/missing-workspace";
    let daemon = boot_full(
        LifecycleSettings::default(),
        None,
        Duration::from_millis(20),
        "127.0.0.1:0".parse().expect("bind"),
        None,
        Arc::new(FakeHarness::new().with_start_failure(format!(
            "the agent could not start in its fixed working directory {host_path}"
        ))),
    )
    .await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "error path").await;

    // The owner's own view keeps the whole diagnostic.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let owner_message = loop {
        if let Event::Error { message, .. } = owner.next_event().await {
            break message;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the failed startup never reached the owner"
        );
    };
    assert!(
        owner_message.contains(host_path),
        "the owner lost the diagnostic: {owner_message}"
    );

    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let snapshot = browser.state(0).await;
    let json = serde_json::to_string(&snapshot).expect("snapshot json");
    assert!(
        !json.contains(host_path),
        "a guest received the host path: {json}"
    );
    let errors: Vec<_> = snapshot
        .events
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Error { message, hint, .. } => Some((message.clone(), hint.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "the guest lost the error entirely");
    assert_eq!(errors[0].0, "The agent hit an error.");
    assert!(
        errors[0]
            .1
            .as_deref()
            .is_none_or(|hint| !hint.contains('/')),
        "the guest hint carried a path: {:?}",
        errors[0].1
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_thread_settled_after_the_placement_check_gets_no_card() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "settle window").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;

    let gate = daemon
        .guest
        .pause_next_submit(h4rn_daemon::guest::SubmitPhase::BeforePublish);
    let posting = {
        let browser = browser.clone();
        tokio::spawn(async move {
            browser
                .post(
                    "/messages",
                    &GuestMessageInput {
                        text: "too late".into(),
                        attachments: vec![h4rn_protocol::GuestAttachmentInput {
                            name: "late.txt".into(),
                            mime: "text/plain".into(),
                            data: base64_encode(b"too late"),
                        }],
                    },
                )
                .await
        })
    };
    gate.arrived().await;

    // The placement commits before the settle hook can run: the hook needs the
    // submission lock this submit is holding.
    daemon
        .manager
        .set_thread_tab(&thread_id, ThreadTab::Settled, false)
        .await
        .expect("settle the thread");
    gate.release();

    let (status, body) = posting.await.expect("post task");
    assert_eq!(
        status,
        410,
        "a settled thread took a guest message: {}",
        String::from_utf8_lossy(&body)
    );
    let pool = journal_pool(&daemon).await;
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM guest_requests WHERE thread_id = ?")
        .bind(thread_id.as_str())
        .fetch_one(&pool)
        .await
        .expect("count requests");
    pool.close().await;
    assert_eq!(rows, 0, "a card was published on a settled thread");
    assert_eq!(
        summary_pending_guest_requests(&mut owner, &thread_id).await,
        0
    );
    let leftovers: Vec<_> = std::fs::read_dir(daemon.dir.path().join("guest"))
        .expect("guest dir")
        .filter_map(Result::ok)
        .collect();
    assert!(leftovers.is_empty(), "the refused message kept files");
    daemon.stop().await;
}

#[tokio::test]
async fn a_refused_placement_keeps_its_cleanup_record_when_files_cannot_be_removed() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "conflict cleanup").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;

    let gate = daemon
        .guest
        .pause_next_submit(h4rn_daemon::guest::SubmitPhase::BeforePublish);
    let posting = {
        let browser = browser.clone();
        tokio::spawn(async move {
            browser
                .post(
                    "/messages",
                    &GuestMessageInput {
                        text: "settle under me".into(),
                        attachments: vec![h4rn_protocol::GuestAttachmentInput {
                            name: "kept.txt".into(),
                            mime: "text/plain".into(),
                            data: base64_encode(b"keep me"),
                        }],
                    },
                )
                .await
        })
    };
    gate.arrived().await;
    let request_id = std::fs::read_dir(daemon.dir.path().join("guest"))
        .expect("guest dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("greq_"))
        .expect("the reserved request directory");
    let directory = daemon.dir.path().join("guest").join(&request_id);
    // A refused disk: the cleanup cannot remove this path as a directory.
    std::fs::remove_dir_all(&directory).expect("clear the request directory");
    std::fs::write(&directory, b"not a directory").expect("block the cleanup");

    // The settle wins the placement decision, so the card is refused. The
    // row must stay reachable for the sweep rather than follow the failed
    // file removal out of the database.
    daemon
        .manager
        .set_thread_tab(&thread_id, ThreadTab::Settled, false)
        .await
        .expect("settle the thread");
    gate.release();
    let (status, body) = posting.await.expect("post task");
    assert_eq!(
        status,
        410,
        "a settled thread took a guest message: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("deleting"),
        "a refused placement dropped the row whose files are still on disk"
    );
    assert!(directory.exists());

    std::fs::remove_file(&directory).expect("unblock the cleanup");
    daemon.guest.sweep_expired().await.expect("sweep");
    assert_eq!(
        request_status(&daemon, &request_id).await,
        None,
        "the retried cleanup never removed the row"
    );
    assert!(!directory.exists());
    daemon.stop().await;
}

#[tokio::test]
async fn a_snapshot_read_under_a_replaced_link_never_answers() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "revoked poll").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    // One message makes the refused snapshot's content recognisable.
    let (status, body) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "before revocation".into(),
                attachments: Vec::new(),
            },
        )
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));

    let gate = daemon.guest.pause_next_snapshot();
    let polling = {
        let browser = browser.clone();
        tokio::spawn(async move { browser.get("/state/0").await })
    };
    gate.arrived().await;

    // Replace the link, don't end it: the thread still has a live link when
    // the old poll resumes, so a check for "any live link on this thread"
    // would wrongly answer it. Only the exact link that started the read may.
    daemon
        .guest
        .create(&thread_id, &format!("{base}/guest"))
        .await
        .expect("replace the link");
    gate.release();

    let (status, _, body) = polling.await.expect("poll task");
    assert_eq!(
        status,
        reqwest::StatusCode::GONE,
        "a revoked link answered a snapshot: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        !String::from_utf8_lossy(&body).contains("before revocation"),
        "the refused snapshot still carried thread text"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_guest_polling_after_a_dismiss_and_a_sweep_still_sees_its_card_resolve() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "resolved poll").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, body) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "dismiss me".into(),
                attachments: Vec::new(),
            },
        )
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let request_id = request_id_from_snapshot(&browser).await;
    let resume = browser.state(0).await.next_seq;

    owner
        .request(ClientRequest::DismissGuestRequest {
            thread_id: thread_id.clone(),
            request_id: request_id.clone(),
        })
        .await;
    // The sweep runs with the link still alive: it must keep the row that
    // carries the request id, or the resolution has no way to reach the guest.
    daemon.guest.sweep_expired().await.expect("sweep");

    let later = browser.state(resume).await;
    assert!(
        later.events.iter().any(|entry| matches!(
            &entry.event,
            Event::GuestRequestResolved { request_id: id, sent: false, .. } if id == &request_id
        )),
        "a guest polling after the sweep never learned its card resolved: {:?}",
        later.events
    );
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("dismissed")
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_resolution_lost_from_the_journal_is_emitted_by_the_sweep() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "lost resolution").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, body) = browser
        .post("/messages", &guest_text_message("answer me"))
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let request_id = request_id_from_snapshot(&browser).await;

    owner
        .request(ClientRequest::DismissGuestRequest {
            thread_id: thread_id.clone(),
            request_id: request_id.clone(),
        })
        .await;
    let resume = browser.state(0).await.next_seq;

    // The exact state a failed journal write leaves: the row records the
    // answer, but no `GuestRequestResolved` exists on the wire.
    let pool = journal_pool(&daemon).await;
    sqlx::query(
        "DELETE FROM daemon_wire_events
         WHERE thread_id = ? AND event_json LIKE '%\"guest_request_resolved\"%'",
    )
    .bind(thread_id.as_str())
    .execute(&pool)
    .await
    .expect("drop the resolution event");
    sqlx::query("UPDATE guest_requests SET resolution_emitted = 0 WHERE request_id = ?")
        .bind(&request_id)
        .execute(&pool)
        .await
        .expect("clear the emitted marker");
    pool.close().await;

    daemon.guest.sweep_expired().await.expect("sweep");

    let later = browser.state(resume).await;
    assert!(
        later.events.iter().any(|entry| matches!(
            &entry.event,
            Event::GuestRequestResolved { request_id: id, sent: false, .. } if id == &request_id
        )),
        "the sweep never re-emitted a lost resolution: {:?}",
        later.events
    );
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("dismissed")
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_finished_request_retries_file_removal_while_its_link_lives() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "sent cleanup").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, body) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "with a file".into(),
                attachments: vec![h4rn_protocol::GuestAttachmentInput {
                    name: "note.txt".into(),
                    mime: "text/plain".into(),
                    data: base64_encode(b"hello"),
                }],
            },
        )
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let request_id = request_id_from_snapshot(&browser).await;
    let directory = daemon.dir.path().join("guest").join(&request_id);
    assert!(directory.exists(), "the request wrote no directory");

    // A refused disk during Dismiss leaves the finished row naming its files.
    std::fs::remove_dir_all(&directory).expect("clear the request directory");
    std::fs::write(&directory, b"not a directory").expect("block the cleanup");
    owner
        .request(ClientRequest::DismissGuestRequest {
            thread_id: thread_id.clone(),
            request_id: request_id.clone(),
        })
        .await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("dismissed")
    );

    // The periodic sweep retries while the link still lives, so the orphan is
    // not kept until Stop sharing.
    std::fs::remove_file(&directory).expect("unblock the cleanup");
    std::fs::create_dir_all(&directory).expect("restore the request directory");
    std::fs::write(directory.join("0-note.txt"), b"hello").expect("restore the file");
    daemon.guest.sweep_expired().await.expect("sweep");
    assert!(
        !directory.exists(),
        "the sweep never retried a finished request's files"
    );
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("dismissed"),
        "the sweep dropped the row the guest still needs to resolve"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn a_dismiss_whose_files_cannot_be_removed_stays_retryable_after_stop_sharing() {
    let daemon = boot().await;
    let base = format!("http://{}", daemon.addr);
    let mut owner = Client::connect(&daemon).await;
    let thread_id = create_thread(&mut owner, daemon.dir.path(), "dismiss cleanup").await;
    let link = enable_and_share(&mut owner, &daemon.guest, &thread_id, &base).await;
    let browser = bound_browser(&base, &link).await;
    let (status, body) = browser
        .post(
            "/messages",
            &GuestMessageInput {
                text: "with a file".into(),
                attachments: vec![h4rn_protocol::GuestAttachmentInput {
                    name: "note.txt".into(),
                    mime: "text/plain".into(),
                    data: base64_encode(b"hello"),
                }],
            },
        )
        .await;
    assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    let request_id = request_id_from_snapshot(&browser).await;
    let directory = daemon.dir.path().join("guest").join(&request_id);
    assert!(directory.exists(), "the request wrote no directory");

    // A refused disk: the path exists but cannot be removed as a directory.
    std::fs::remove_dir_all(&directory).expect("clear the request directory");
    std::fs::write(&directory, b"not a directory").expect("block the cleanup");

    owner
        .request(ClientRequest::DismissGuestRequest {
            thread_id: thread_id.clone(),
            request_id: request_id.clone(),
        })
        .await;
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("dismissed")
    );

    owner
        .request(ClientRequest::StopGuestSharing {
            thread_id: thread_id.clone(),
        })
        .await;
    // The link is gone, but the row is a cleanup record until its files are
    // gone: dropping it with the orphan still on disk would make the cleanup
    // unreachable forever.
    assert_eq!(
        request_status(&daemon, &request_id).await.as_deref(),
        Some("deleting"),
        "Stop sharing dropped a row whose files are still on disk"
    );
    assert!(directory.exists());

    std::fs::remove_file(&directory).expect("unblock the cleanup");
    daemon.guest.sweep_expired().await.expect("sweep");
    assert_eq!(
        request_status(&daemon, &request_id).await,
        None,
        "the retried cleanup never removed the row"
    );
    assert!(!directory.exists());
    daemon.stop().await;
}
