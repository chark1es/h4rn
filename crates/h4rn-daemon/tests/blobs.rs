//! End-to-end attachment tests over the real WebSocket (ADR-008).
//!
//! These tests exercise the wire contract, not the store's internals: a real
//! `axum` server, a `tokio-tungstenite` client, chunked binary frames, the
//! two-phase commit, resumable progress, range downloads and refcount GC.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use h4rn_daemon::auth::TokenStore;
use h4rn_daemon::blobs::{AttachmentResolver, BlobStore, UploadOwner};
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_harness::FakeHarness;
use h4rn_protocol::{
    AttachmentId, BinaryFrame, BinaryKind, BlobInfo, BlobRange, ClientFrame, ClientRequest,
    DeviceId, DeviceInfo, Event, HarnessKind, Item, ProtoError, ProtoErrorCode, ResponseBody,
    ServerFrame, UploadTicket,
};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const TIMEOUT: Duration = Duration::from_secs(5);

struct TestDaemon {
    addr: std::net::SocketAddr,
    token: String,
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

async fn boot() -> TestDaemon {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind: None,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Auto,
        network_access: None,
        network_port: 0,
    };
    let daemon = DaemonBuilder::new(config)
        .with_harness(Arc::new(FakeHarness::new()))
        .build()
        .await
        .expect("build daemon");
    let token = TokenStore::new(dir.path())
        .load()
        .expect("read token")
        .expect("token exists");
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
    }
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    pending: VecDeque<(u64, Event)>,
}

use std::collections::VecDeque;

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

    async fn request(&mut self, body: ClientRequest) -> Result<ResponseBody, ProtoError> {
        let id = self.next_id;
        self.next_id += 1;
        let frame = ClientFrame { id, body };
        self.ws
            .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
            .await
            .expect("send");
        loop {
            match self.read_json().await {
                ServerFrame::Response {
                    id: response_id,
                    result,
                } if response_id == id => return result,
                ServerFrame::Event { seq, event } => self.pending.push_back((seq, event)),
                _ => {}
            }
        }
    }

    async fn hello(&mut self) {
        self.request(ClientRequest::Hello {
            protocol_version: h4rn_protocol::PROTOCOL_VERSION,
            device: DeviceInfo {
                device_id: DeviceId::new("dev_blob"),
                name: "blob test".into(),
                platform: "linux".into(),
                app_version: "0.1.0".into(),
            },
        })
        .await
        .expect("hello");
    }

    async fn send_chunk(&mut self, stream_id: u32, seq: u64, payload: &[u8]) {
        let frame = BinaryFrame {
            kind: BinaryKind::BlobChunk,
            stream_id,
            seq,
            payload: payload.to_vec(),
        };
        self.ws
            .send(Message::Binary(frame.encode().into()))
            .await
            .expect("send chunk");
        let ack = self.read_binary().await;
        let ack = BinaryFrame::decode(&ack).expect("decode ack");
        assert_eq!(ack.kind, BinaryKind::Ack);
        assert_eq!(ack.stream_id, stream_id);
        assert_eq!(ack.seq, seq);
    }

    async fn upload(&mut self, name: &str, mime: Option<&str>, bytes: &[u8]) -> UploadTicket {
        let result = self
            .request(ClientRequest::BeginUpload {
                name: name.into(),
                size: bytes.len() as u64,
                mime_hint: mime.map(str::to_owned),
            })
            .await
            .expect("begin upload");
        let ResponseBody::UploadStarted { upload } = result else {
            panic!("unexpected BeginUpload response: {result:?}");
        };
        for (seq, chunk) in bytes.chunks(upload.chunk_size as usize).enumerate() {
            self.send_chunk(upload.stream_id, seq as u64, chunk).await;
        }
        upload
    }

    async fn commit(&mut self, upload: &UploadTicket, hash: &str) -> Result<BlobInfo, ProtoError> {
        match self
            .request(ClientRequest::CommitUpload {
                upload_id: upload.upload_id.clone(),
                blake3: hash.into(),
            })
            .await?
        {
            ResponseBody::UploadCommitted { blob } => Ok(blob),
            other => panic!("unexpected CommitUpload response: {other:?}"),
        }
    }

    async fn download(
        &mut self,
        id: &AttachmentId,
        range: Option<BlobRange>,
        thumbnail: bool,
    ) -> (h4rn_protocol::BlobDownload, Vec<u8>) {
        let result = self
            .request(ClientRequest::GetBlob {
                id: id.clone(),
                range,
                thumbnail,
            })
            .await
            .expect("get blob");
        let ResponseBody::Blob { download } = result else {
            panic!("unexpected GetBlob response: {result:?}");
        };
        let mut bytes = Vec::new();
        while bytes.len() < download.size as usize {
            let frame = self.read_binary().await;
            let frame = BinaryFrame::decode(&frame).expect("decode blob frame");
            assert_eq!(frame.kind, BinaryKind::BlobChunk);
            assert_eq!(frame.stream_id, download.stream_id);
            bytes.extend_from_slice(&frame.payload);
            if frame.payload.is_empty() {
                break;
            }
        }
        (download, bytes)
    }

    async fn read_json(&mut self) -> ServerFrame {
        loop {
            let message = tokio::time::timeout(TIMEOUT, self.ws.next())
                .await
                .expect("timed out")
                .expect("closed")
                .expect("websocket error");
            match message {
                Message::Text(text) => return serde_json::from_str(&text).expect("frame"),
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                Message::Close(_) => panic!("server closed the socket"),
            }
        }
    }

    async fn read_binary(&mut self) -> Vec<u8> {
        loop {
            let message = tokio::time::timeout(TIMEOUT, self.ws.next())
                .await
                .expect("timed out waiting for binary")
                .expect("closed")
                .expect("websocket error");
            match message {
                Message::Binary(bytes) => return bytes.into(),
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                Message::Text(text) => panic!("unexpected text while waiting for binary: {text}"),
                Message::Close(_) => panic!("server closed the socket"),
            }
        }
    }

    async fn next_event(&mut self) -> Event {
        if let Some((_, event)) = self.pending.pop_front() {
            return event;
        }
        loop {
            match self.read_json().await {
                ServerFrame::Event { event, .. } => return event,
                ServerFrame::Response { .. }
                | ServerFrame::TerminalEvent { .. }
                | ServerFrame::ProxyEvent { .. }
                | ServerFrame::UsageEvent { .. }
                | ServerFrame::HarnessEvent { .. } => {}
            }
        }
    }
}

fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[tokio::test]
async fn uploads_in_chunks_commits_dedupes_and_reads_ranges() {
    let daemon = boot().await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let bytes: Vec<u8> = (0..2_500_000u32).map(|value| value as u8).collect();
    let upload = client
        .upload("report.bin", Some("application/octet-stream"), &bytes)
        .await;
    let blob = client
        .commit(&upload, &content_hash(&bytes))
        .await
        .expect("commit");
    assert_eq!(blob.id, AttachmentId::from_content(&bytes));
    assert_eq!(blob.size, bytes.len() as u64);
    assert_eq!(blob.name, "report.bin");

    // A retried commit is idempotent and does not hash again.
    let again = client
        .commit(&upload, &content_hash(&bytes))
        .await
        .expect("idempotent commit");
    assert_eq!(again.id, blob.id);

    let (download, whole) = client.download(&blob.id, None, false).await;
    assert_eq!(download.size, bytes.len() as u64);
    assert_eq!(whole, bytes);

    let (range, slice) = client
        .download(
            &blob.id,
            Some(BlobRange {
                offset: 1_000,
                length: Some(512),
            }),
            false,
        )
        .await;
    assert_eq!(range.offset, 1_000);
    assert_eq!(range.size, 512);
    assert_eq!(slice, &bytes[1_000..1_512]);

    // The same bytes under another name dedupe to one content address.
    let second = client.upload("copy.bin", None, &bytes).await;
    let deduped = client
        .commit(&second, &content_hash(&bytes))
        .await
        .expect("dedupe commit");
    assert_eq!(deduped.id, blob.id);
    assert_eq!(deduped.name, "report.bin");

    daemon.stop().await;
}

#[tokio::test]
async fn bad_hashes_keep_the_upload_open_for_a_retry() {
    let daemon = boot().await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let bytes = b"the quick brown fox";
    let upload = client.upload("notes.txt", Some("text/plain"), bytes).await;
    let error = client
        .commit(&upload, &"0".repeat(64))
        .await
        .expect_err("mismatch must fail");
    assert_eq!(error.code, ProtoErrorCode::InvalidRequest);
    assert!(error.message.contains("do not match"));

    let acknowledged = client
        .commit(&upload, &content_hash(bytes))
        .await
        .expect("retry succeeds");
    assert_eq!(acknowledged.size, bytes.len() as u64);

    daemon.stop().await;
}

#[tokio::test]
async fn query_upload_reports_progress_and_duplicates_are_retries() {
    let daemon = boot().await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let bytes = b"0123456789";
    let upload = client.upload("digits.txt", None, bytes).await;
    let status = match client
        .request(ClientRequest::QueryUpload {
            upload_id: upload.upload_id.clone(),
        })
        .await
        .expect("query")
    {
        ResponseBody::UploadStatus { upload } => upload,
        other => panic!("unexpected query response: {other:?}"),
    };
    assert_eq!(status.received_bytes, bytes.len() as u64);
    assert_eq!(status.size, bytes.len() as u64);
    assert!(status.committed.is_none());

    // Replaying the last chunk is a no-op rather than a corruption.
    client.send_chunk(upload.stream_id, 0, bytes).await;
    let status = match client
        .request(ClientRequest::QueryUpload {
            upload_id: upload.upload_id.clone(),
        })
        .await
        .expect("query")
    {
        ResponseBody::UploadStatus { upload } => upload,
        other => panic!("unexpected query response: {other:?}"),
    };
    assert_eq!(status.received_bytes, bytes.len() as u64);

    daemon.stop().await;
}

#[tokio::test]
async fn files_that_claim_to_be_images_must_contain_image_bytes() {
    let daemon = boot().await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let upload = client
        .upload("screenshot.png", Some("image/png"), b"not really a png")
        .await;
    let error = client
        .commit(&upload, &content_hash(b"not really a png"))
        .await
        .expect_err("invalid image must fail");
    assert_eq!(error.code, ProtoErrorCode::InvalidRequest);
    assert!(error.message.contains("image"));

    daemon.stop().await;
}

#[tokio::test]
async fn uploads_above_the_configured_limit_are_refused_before_bytes_sent() {
    let daemon = boot().await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let error = client
        .request(ClientRequest::BeginUpload {
            name: "huge.bin".into(),
            size: 51 * 1024 * 1024,
            mime_hint: None,
        })
        .await
        .expect_err("limit must be enforced");
    assert_eq!(error.code, ProtoErrorCode::InvalidRequest);

    daemon.stop().await;
}

#[tokio::test]
async fn thread_messages_carry_attachment_metadata_and_materialise_a_path() {
    let daemon = boot().await;
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let bytes = b"# Notes\nattachment content";
    let upload = client
        .upload("notes.md", Some("text/markdown"), bytes)
        .await;
    let blob = client
        .commit(&upload, &content_hash(bytes))
        .await
        .expect("commit");

    let host = match client
        .request(ClientRequest::ListHosts)
        .await
        .expect("hosts")
    {
        ResponseBody::Hosts { hosts } => hosts[0].host_id.clone(),
        other => panic!("unexpected hosts: {other:?}"),
    };
    let created = client
        .request(ClientRequest::CreateThread {
            host,
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            workspace_path: "/tmp/h4rn-workspace".into(),
            model: "fake-model".into(),
            prompt: "read the notes".into(),
            attachments: vec![blob.id.clone()],
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            fast: false,
            worktree: None,
        })
        .await
        .expect("create thread");
    let ResponseBody::ThreadCreated { .. } = created else {
        panic!("unexpected create response: {created:?}");
    };

    let mut found = None;
    for _ in 0..20 {
        if let Event::ItemCompleted {
            item: Item::Message { attachments, .. },
            ..
        } = client.next_event().await
            && !attachments.is_empty()
        {
            found = Some(attachments);
            break;
        }
    }
    let attachments = found.expect("the user item must carry the attachment");
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].id, blob.id);
    assert_eq!(attachments[0].name, "notes.md");
    let path = std::path::Path::new(&attachments[0].local_path);
    assert!(
        path.is_file(),
        "materialised path {} must exist",
        path.display()
    );
    assert_eq!(std::fs::read(path).unwrap(), bytes);

    daemon.stop().await;
}

#[tokio::test]
async fn unreferenced_blobs_are_collected_but_retained_ones_survive() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = BlobStore::open(
        dir.path().join("blobs"),
        dir.path().join("attachments"),
        1024 * 1024,
        Duration::from_millis(1),
    )
    .await
    .expect("store");

    let bytes = b"collect me";
    let upload = store
        .begin_upload(UploadOwner::Local, "collect.txt", bytes.len() as u64, None)
        .unwrap();
    store
        .append_chunk(UploadOwner::Local, upload.stream_id, 0, bytes)
        .unwrap();
    let blob = store
        .commit_upload(UploadOwner::Local, &upload.upload_id, &content_hash(bytes))
        .await
        .expect("commit");
    let path = dir
        .path()
        .join("blobs")
        .join(&blob.id.as_str()[..2])
        .join(&blob.id.as_str()[2..]);
    assert!(path.is_file());

    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(store.gc().await.expect("gc"), 1);
    assert!(!path.exists(), "unreferenced blob must be removed");

    let kept = store
        .begin_upload(
            UploadOwner::Local,
            "keep.txt",
            b"keep me".len() as u64,
            None,
        )
        .unwrap();
    store
        .append_chunk(UploadOwner::Local, kept.stream_id, 0, b"keep me")
        .unwrap();
    let kept = store
        .commit_upload(
            UploadOwner::Local,
            &kept.upload_id,
            &content_hash(b"keep me"),
        )
        .await
        .expect("commit");
    let thread = h4rn_protocol::ThreadId::new("th_gc");
    let resolved = store
        .resolve(&thread, std::slice::from_ref(&kept.id))
        .await
        .expect("resolve");
    assert_eq!(resolved.len(), 1);
    assert!(std::path::Path::new(&resolved[0].local_path).is_file());
    tokio::time::sleep(Duration::from_millis(5)).await;
    store.gc().await.expect("gc");
    assert_eq!(store.info(&kept.id).await.expect("retained").id, kept.id);
}

#[tokio::test]
async fn display_names_cannot_traverse_out_of_the_thread_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = BlobStore::open(
        dir.path().join("blobs"),
        dir.path().join("attachments"),
        1024 * 1024,
        Duration::from_secs(60),
    )
    .await
    .expect("store");

    let bytes = b"traversal";
    let upload = store
        .begin_upload(
            UploadOwner::Local,
            "../../etc/passwd",
            bytes.len() as u64,
            None,
        )
        .unwrap();
    store
        .append_chunk(UploadOwner::Local, upload.stream_id, 0, bytes)
        .unwrap();
    let blob = store
        .commit_upload(UploadOwner::Local, &upload.upload_id, &content_hash(bytes))
        .await
        .expect("commit");
    let resolved = store
        .resolve(
            &h4rn_protocol::ThreadId::new("th_safe"),
            std::slice::from_ref(&blob.id),
        )
        .await
        .expect("resolve");
    let path = std::path::Path::new(&resolved[0].local_path);
    assert!(path.starts_with(dir.path().join("attachments").join("th_safe")));
    assert!(
        path.file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("passwd")
    );
}
