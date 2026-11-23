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
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
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
            h4rn_daemon::harnesses::StaticHarnessCatalog::with(Arc::new(FakeHarness::new())),
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
        let url = format!("ws://{}/ws?token={}", daemon.addr, daemon.token);
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

    // Startup has no reader, so it resolves the card and removes the row.
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
    assert_eq!(rows, 0);
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

    async fn head(&self) -> Result<u64, h4rn_daemon::journal::JournalError> {
        self.0.head().await
    }

    async fn read_domain(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<h4rn_core::ThreadEvent>, h4rn_daemon::journal::JournalError> {
        self.0.read_domain(thread_id).await
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
