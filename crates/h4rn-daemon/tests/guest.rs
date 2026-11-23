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
        settings: LifecycleSettings::default(),
        tick_interval: Duration::from_millis(20),
        clock: None,
    };
    let daemon = DaemonBuilder::new(config)
        .with_local_host(HostId::new("host_guest_test"), "guest test")
        .without_title_agents()
        .with_harness_registry(Arc::new(OfflineRegistry))
        .with_lifecycle(options)
        .with_catalog(Arc::new(
            h4rn_daemon::harnesses::StaticHarnessCatalog::with(Arc::new(FakeHarness::new())),
        ))
        .build()
        .await
        .expect("build");
    let token = TokenStore::new(dir.path())
        .load()
        .expect("read token")
        .expect("token exists");
    let bound = daemon.bind().await.expect("bind");
    let addr = bound.local_addr();
    let guest = bound.guest();
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

    let expired = Browser::new(format!("{base}/guest/{second_token}"));
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
