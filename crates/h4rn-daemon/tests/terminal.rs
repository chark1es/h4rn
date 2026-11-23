//! End-to-end tests for daemon-owned PTY terminals (ADR-007).
//!
//! Each test boots a real daemon on an ephemeral port and drives the typed
//! WebSocket protocol with real shells (`sh`, `cat`, `yes`). Binary frames are
//! decoded through the same header the UI uses.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use h4rn_daemon::auth::TokenStore;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::DaemonBuilder;
use h4rn_daemon::server::Outbound;
use h4rn_daemon::terminal::{
    OpenSpec, QueryReply, TerminalManager, TracingAudit, Viewer, run_viewer,
};
use h4rn_protocol::{
    BINARY_HEADER_LEN, BinaryFrame, BinaryKind, ClientFrame, ClientRequest, DeviceId, DeviceInfo,
    HostId, ResponseBody, ServerFrame, TerminalId, TerminalInfo, pty_input_reply_seq,
};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

struct TestDaemon {
    addr: std::net::SocketAddr,
    token: String,
    dir: tempfile::TempDir,
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
        // Titles never spawn the machine's own claude or codex.
        .without_title_agents()
        .build()
        .await
        .expect("build");
    let token = TokenStore::new(dir.path())
        .load()
        .expect("read token")
        .expect("token exists");
    let bound = daemon.bind().await.expect("bind");
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
        dir,
        shutdown: Some(shutdown),
        task,
    }
}

struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    texts: VecDeque<ServerFrame>,
    binaries: VecDeque<BinaryFrame>,
    /// Terminal this connection attached, so output can be acknowledged.
    attached: Option<TerminalId>,
}

impl Client {
    async fn connect(daemon: &TestDaemon) -> Self {
        let url = format!("ws://{}/ws?token={}", daemon.addr, daemon.token);
        let (ws, _) = connect_async(url).await.expect("connect");
        Self {
            ws,
            next_id: 1,
            texts: VecDeque::new(),
            binaries: VecDeque::new(),
            attached: None,
        }
    }

    async fn hello(&mut self) {
        self.request(ClientRequest::Hello {
            protocol_version: h4rn_protocol::PROTOCOL_VERSION,
            device: device(),
        })
        .await
        .expect("hello");
    }

    async fn pump(&mut self) {
        let message = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
            .await
            .expect("timed out waiting for a frame")
            .expect("socket closed")
            .expect("websocket error");
        match message {
            Message::Text(text) => {
                let frame: ServerFrame = serde_json::from_str(&text).expect("parse ServerFrame");
                self.texts.push_back(frame);
            }
            Message::Binary(bytes) => {
                let frame = BinaryFrame::decode(&bytes).expect("decode binary frame");
                let ack = match (&self.attached, frame.kind) {
                    (Some(terminal_id), BinaryKind::PtyOutput | BinaryKind::PtySnapshot) => {
                        Some((terminal_id.clone(), frame.seq))
                    }
                    _ => None,
                };
                self.binaries.push_back(frame);
                if let Some((terminal_id, up_to_chunk)) = ack {
                    // The ack's own response is drained by the next pump.
                    self.send(ClientRequest::TerminalAck {
                        terminal_id,
                        up_to_chunk,
                    })
                    .await;
                }
            }
            Message::Close(_) => panic!("server closed the connection unexpectedly"),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }

    async fn send(&mut self, body: ClientRequest) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let frame = ClientFrame { id, body };
        self.ws
            .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
            .await
            .expect("send frame");
        id
    }

    async fn send_input(&mut self, stream_id: u32, data: &[u8]) {
        self.send_input_frame(stream_id, 0, data).await;
    }

    async fn send_input_frame(&mut self, stream_id: u32, seq: u64, data: &[u8]) {
        let frame = BinaryFrame {
            kind: BinaryKind::PtyInput,
            stream_id,
            seq,
            payload: data.to_vec(),
        };
        self.ws
            .send(Message::Binary(frame.encode().into()))
            .await
            .expect("send binary input");
    }

    async fn response(&mut self, id: u64) -> Result<ResponseBody, h4rn_protocol::ProtoError> {
        loop {
            while let Some(frame) = self.texts.pop_front() {
                if let ServerFrame::Response {
                    id: response_id,
                    result,
                } = frame
                    && response_id == id
                {
                    return result;
                }
            }
            self.pump().await;
        }
    }

    async fn request(
        &mut self,
        body: ClientRequest,
    ) -> Result<ResponseBody, h4rn_protocol::ProtoError> {
        let id = self.send(body).await;
        self.response(id).await
    }

    async fn next_binary(&mut self) -> BinaryFrame {
        loop {
            if let Some(frame) = self.binaries.pop_front() {
                return frame;
            }
            self.pump().await;
        }
    }

    async fn output_until(&mut self, needle: &str) -> String {
        let mut output = String::new();
        loop {
            let frame = self.next_binary().await;
            if matches!(frame.kind, BinaryKind::PtyOutput | BinaryKind::PtySnapshot) {
                output.push_str(&String::from_utf8_lossy(&frame.payload));
                if output.contains(needle) {
                    return output;
                }
            }
        }
    }

    async fn open(
        &mut self,
        host: HostId,
        cwd: &str,
        shell: Option<&str>,
        args: Vec<String>,
        cols: u16,
        rows: u16,
    ) -> TerminalInfo {
        match self
            .request(ClientRequest::TerminalOpen {
                thread_id: None,
                host,
                cwd: cwd.to_owned(),
                shell: shell.map(str::to_owned),
                args,
                cols,
                rows,
            })
            .await
            .expect("open terminal")
        {
            ResponseBody::TerminalOpened { terminal } => terminal,
            other => panic!("unexpected open response: {other:?}"),
        }
    }

    async fn attach(&mut self, terminal_id: &TerminalId, cols: u16, rows: u16) -> AttachedView {
        self.attach_declaring(terminal_id, cols, rows, true).await
    }

    async fn attach_declaring(
        &mut self,
        terminal_id: &TerminalId,
        cols: u16,
        rows: u16,
        reply_positions_v2: bool,
    ) -> AttachedView {
        match self
            .request(ClientRequest::TerminalAttach {
                terminal_id: terminal_id.clone(),
                cols: Some(cols),
                rows: Some(rows),
                reply_positions_v2,
            })
            .await
            .expect("attach terminal")
        {
            ResponseBody::TerminalAttached {
                terminal,
                snapshot,
                start_seq,
            } => {
                self.attached = Some(terminal.terminal_id.clone());
                AttachedView {
                    terminal,
                    snapshot,
                    start_seq,
                }
            }
            other => panic!("unexpected attach response: {other:?}"),
        }
    }
}

struct AttachedView {
    terminal: TerminalInfo,
    snapshot: String,
    start_seq: u64,
}

fn device() -> DeviceInfo {
    DeviceInfo {
        device_id: DeviceId::new("dev_test"),
        name: "terminal test".into(),
        platform: "linux".into(),
        app_version: "0.1.0".into(),
    }
}

async fn host_id_for(daemon: &TestDaemon) -> HostId {
    let mut client = Client::connect(daemon).await;
    client.hello().await;
    match client.request(ClientRequest::ListHosts).await.unwrap() {
        ResponseBody::Hosts { hosts } => hosts[0].host_id.clone(),
        other => panic!("unexpected host response: {other:?}"),
    }
}

#[tokio::test]
async fn echoes_input_and_outlives_the_viewer() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let cwd = tempfile::tempdir().unwrap();
    let cwd = cwd.path().to_string_lossy().to_string();

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let terminal = client
        .open(host, &cwd, Some("/bin/sh"), Vec::new(), 80, 24)
        .await;
    assert_eq!(terminal.cols, 80);
    assert_eq!(terminal.rows, 24);
    assert_eq!(terminal.shell, "/bin/sh");

    let attached = client.attach(&terminal.terminal_id, 80, 24).await;
    assert!(attached.terminal.alive);
    // The shell may already have written its prompt, so `start_seq` is not
    // necessarily zero; the snapshot is the state as of the attach.
    let attached_start = attached.start_seq;

    client.send_input(terminal.stream_id, b"echo h4rn\n").await;
    let output = client.output_until("h4rn").await;
    assert!(output.contains("h4rn"), "output was {output:?}");

    // Dropping the socket detaches the viewer; the PTY keeps running.
    drop(client);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut second = Client::connect(&daemon).await;
    second.hello().await;
    let again = second.attach(&terminal.terminal_id, 80, 24).await;
    assert!(
        again.snapshot.contains("h4rn"),
        "snapshot was {:?}",
        again.snapshot
    );
    assert!(again.start_seq >= attached_start);

    second
        .request(ClientRequest::TerminalClose {
            terminal_id: terminal.terminal_id,
        })
        .await
        .expect("close terminal");
    daemon.stop().await;
}

#[tokio::test]
async fn shell_command_runs_and_reports_exit() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let cwd = tempfile::tempdir().unwrap();
    let cwd = cwd.path().to_string_lossy().to_string();

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let terminal = client
        .open(
            host,
            &cwd,
            Some("/bin/sh"),
            vec!["-c".into(), "echo hi".into()],
            80,
            24,
        )
        .await;

    // The command writes immediately and exits. The grid keeps the output even
    // though no viewer was attached when it happened.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let attached = client.attach(&terminal.terminal_id, 80, 24).await;
    assert!(
        attached.snapshot.contains("hi"),
        "snapshot was {:?}",
        attached.snapshot
    );
    assert!(!attached.terminal.alive, "the shell should have exited");
    let exit = attached.terminal.exit.expect("exit status");
    assert_eq!(exit.status, 0);
    assert!(exit.signal.is_none());

    match client
        .request(ClientRequest::TerminalList { thread_id: None })
        .await
        .expect("list terminals")
    {
        ResponseBody::Terminals { terminals } => {
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].terminal_id, terminal.terminal_id);
        }
        other => panic!("unexpected list response: {other:?}"),
    }
    daemon.stop().await;
}

#[tokio::test]
async fn interactive_process_echoes_bytes() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let cwd = tempfile::tempdir().unwrap();
    let cwd = cwd.path().to_string_lossy().to_string();

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let terminal = client
        .open(host, &cwd, Some("/bin/cat"), Vec::new(), 80, 24)
        .await;
    client.attach(&terminal.terminal_id, 80, 24).await;

    client
        .send_input(terminal.stream_id, b"hello terminal\n")
        .await;
    let output = client.output_until("hello terminal").await;
    assert!(
        output.contains("hello terminal"),
        "interactive echo was {output:?}"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn resize_uses_the_smallest_viewer() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let cwd = tempfile::tempdir().unwrap();
    let cwd = cwd.path().to_string_lossy().to_string();

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let terminal = client
        .open(host, &cwd, Some("/bin/sh"), Vec::new(), 80, 24)
        .await;
    client.attach(&terminal.terminal_id, 80, 24).await;

    client
        .request(ClientRequest::TerminalResize {
            terminal_id: terminal.terminal_id.clone(),
            cols: 40,
            rows: 12,
        })
        .await
        .expect("resize");
    client.send_input(terminal.stream_id, b"stty size\n").await;
    let output = client.output_until("12 40").await;
    assert!(output.contains("12 40"), "stty size output was {output:?}");

    // The PTY really shrank: the attached metadata follows the smallest viewer.
    match client
        .request(ClientRequest::TerminalList { thread_id: None })
        .await
        .expect("list")
    {
        ResponseBody::Terminals { terminals } => {
            assert_eq!(terminals[0].cols, 40);
            assert_eq!(terminals[0].rows, 12);
        }
        other => panic!("unexpected list response: {other:?}"),
    }
    daemon.stop().await;
}

#[tokio::test]
async fn open_records_the_device_in_the_audit_log() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let cwd = tempfile::tempdir().unwrap();
    let cwd_string = cwd.path().to_string_lossy().to_string();

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let terminal = client
        .open(host, &cwd_string, Some("/bin/sh"), Vec::new(), 80, 24)
        .await;

    let audit = std::fs::read_to_string(daemon.dir.path().join("terminal-audit.log"))
        .expect("audit log exists");
    assert!(audit.contains(&terminal.terminal_id.to_string()), "{audit}");
    assert!(audit.contains("dev_test"), "{audit}");
    assert!(audit.contains(&cwd_string), "{audit}");
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_viewer_resyncs_from_a_snapshot() {
    let manager = Arc::new(TerminalManager::new(
        HostId::new("host_test"),
        Arc::new(TracingAudit),
    ));
    let cwd = tempfile::tempdir().unwrap();
    let terminal = manager
        .open(
            OpenSpec {
                thread_id: None,
                host: HostId::new("host_test"),
                cwd: cwd.path().to_string_lossy().to_string(),
                shell: Some("/bin/sh".into()),
                args: vec![
                    "-c".into(),
                    // 32 MiB of NULs: enough to wrap the 256-chunk ring many
                    // times while staying cheap for the VT parser.
                    "head -c 33554432 /dev/zero".into(),
                ],
                cols: 80,
                rows: 24,
            },
            &device(),
        )
        .await
        .expect("open");

    // Two outbound slots: the viewer blocks on the second chunk while the ring
    // keeps wrapping, exactly the slow-client case ADR-007 describes.
    let (tx, mut rx) = mpsc::channel::<Outbound>(2);
    let viewer_id = manager.next_viewer_id();
    let attached = manager
        .attach(
            &terminal.terminal_id,
            viewer_id,
            tx,
            Some(80),
            Some(24),
            true,
        )
        .expect("attach");
    let start_seq = attached.snapshot.seq;
    tokio::spawn(run_viewer(
        Arc::clone(&attached.session),
        viewer_id,
        Arc::clone(&attached.viewer),
        attached.output,
        start_seq,
    ));

    // Let the PTY outrun the blocked viewer.
    tokio::time::sleep(Duration::from_millis(300)).await;
    manager
        .ack(&terminal.terminal_id, viewer_id, u64::MAX)
        .expect("ack");

    let mut saw_snapshot = false;
    for _ in 0..500 {
        let Ok(Some(message)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await
        else {
            break;
        };
        if let Outbound::Binary(bytes) = message {
            let frame = BinaryFrame::decode(&bytes).unwrap();
            assert!(bytes.len() >= BINARY_HEADER_LEN);
            manager
                .ack(&terminal.terminal_id, viewer_id, frame.seq)
                .expect("ack frame");
            if frame.kind == BinaryKind::PtySnapshot {
                saw_snapshot = true;
                break;
            }
        }
    }

    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    assert!(
        saw_snapshot,
        "a viewer that falls outside the ring must be resynced from a snapshot"
    );
}

/// Binary PTY input is authorised like a request: it needs `Hello` and a
/// viewer this connection attached. A stream id alone (ids are sequential)
/// must not let another connection type into somebody else's shell.
#[tokio::test]
async fn pty_input_needs_hello_and_an_attached_viewer() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().to_string_lossy().to_string();

    let mut owner = Client::connect(&daemon).await;
    owner.hello().await;
    let terminal = owner
        .open(host, &cwd, Some("/bin/sh"), Vec::new(), 80, 24)
        .await;
    owner.attach(&terminal.terminal_id, 80, 24).await;

    // A connection that never said Hello guesses the stream id.
    let mut stranger = {
        let url = format!("ws://{}/ws?token={}", daemon.addr, daemon.token);
        let (ws, _) = connect_async(url).await.expect("connect");
        Client {
            ws,
            next_id: 1,
            texts: VecDeque::new(),
            binaries: VecDeque::new(),
            attached: None,
        }
    };
    stranger
        .send_input(terminal.stream_id, b"touch no-hello\n")
        .await;
    stranger.pump().await;
    assert!(
        !stranger.texts.is_empty(),
        "a refused frame must be reported, not silently dropped"
    );

    // A connection that said Hello but never attached this terminal.
    let mut other = Client::connect(&daemon).await;
    other.hello().await;
    other
        .send_input(terminal.stream_id, b"touch not-attached\n")
        .await;
    other.pump().await;

    // The owner still works, and proves the shell processed input in order.
    owner
        .send_input(terminal.stream_id, b"echo done-$((40+2))\n")
        .await;
    owner.output_until("done-42").await;
    assert!(!work.path().join("no-hello").exists(), "no-Hello input ran");
    assert!(
        !work.path().join("not-attached").exists(),
        "unattached input ran"
    );
    owner
        .request(ClientRequest::TerminalClose {
            terminal_id: terminal.terminal_id,
        })
        .await
        .expect("close terminal");
    daemon.stop().await;
}

/// Two viewers answer the same output position. The first marked reply is
/// written to the PTY and the second is dropped; typed input is always written.
#[tokio::test]
async fn only_one_marked_reply_per_position_reaches_the_pty() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let work = tempfile::tempdir().unwrap();
    let cwd = work.path().to_string_lossy().to_string();

    let mut first = Client::connect(&daemon).await;
    first.hello().await;
    let terminal = first
        .open(
            host,
            &cwd,
            Some("/bin/sh"),
            vec!["-c".into(), "stty raw -echo; exec cat".into()],
            80,
            24,
        )
        .await;
    first.attach(&terminal.terminal_id, 80, 24).await;

    let mut second = Client::connect(&daemon).await;
    second.hello().await;
    second.attach(&terminal.terminal_id, 80, 24).await;

    // Let the shell switch the PTY to raw, echo-free mode before writing.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // A reply may only name output the viewer was sent. Three round trips give
    // both viewers chunks 1 through 3, so the mode reply can name position 3.
    for _ in 0..3 {
        first.send_input(terminal.stream_id, b"x").await;
        first.output_until("x").await;
        second.output_until("x").await;
    }

    // A mode report both viewers' xterm would answer identically.
    let mode_reply: &[u8] = b"\x1b[?12;2$y";
    let written = "\u{1b}[?12;2$y";
    let position = pty_input_reply_seq(3, 0);
    first
        .send_input_frame(terminal.stream_id, position, mode_reply)
        .await;
    second
        .send_input_frame(terminal.stream_id, position, mode_reply)
        .await;
    second
        .send_input_frame(terminal.stream_id, 0, b"iHELLO")
        .await;

    let output = second.output_until("iHELLO").await;
    assert_eq!(
        output.matches(written).count(),
        1,
        "two viewers' replies must reach the shell once: {output:?}"
    );

    first
        .request(ClientRequest::TerminalClose {
            terminal_id: terminal.terminal_id,
        })
        .await
        .expect("close terminal");
    daemon.stop().await;
}

/// Reads one viewer's output until `done` accepts it, acknowledging chunks.
async fn read_until(
    rx: &mut mpsc::Receiver<Outbound>,
    manager: &TerminalManager,
    terminal_id: &TerminalId,
    viewer_id: u64,
    done: impl FnMut(&str) -> bool,
) -> String {
    read_until_before(
        rx,
        manager,
        terminal_id,
        viewer_id,
        std::time::Instant::now() + Duration::from_secs(5),
        done,
    )
    .await
}

/// Reads one viewer's output against an overall deadline, acknowledging chunks.
///
/// The deadline covers the whole wait rather than each frame, so a long batch
/// is not cut short by per-frame timing as long as it makes progress.
async fn read_until_before(
    rx: &mut mpsc::Receiver<Outbound>,
    manager: &TerminalManager,
    terminal_id: &TerminalId,
    viewer_id: u64,
    deadline: std::time::Instant,
    mut done: impl FnMut(&str) -> bool,
) -> String {
    let mut output = String::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            panic!("terminal output deadline passed; got {output:?}");
        }
        let Ok(Some(message)) = tokio::time::timeout(left, rx.recv()).await else {
            panic!("terminal output timed out; got {output:?}");
        };
        let Outbound::Binary(bytes) = message else {
            continue;
        };
        let frame = BinaryFrame::decode(&bytes).expect("decode output");
        manager
            .ack(terminal_id, viewer_id, frame.seq)
            .expect("ack output");
        if matches!(frame.kind, BinaryKind::PtyOutput | BinaryKind::PtySnapshot) {
            output.push_str(&String::from_utf8_lossy(&frame.payload));
            if done(&output) {
                return output;
            }
        }
    }
}

/// A raw, echo-free `cat` terminal: the test counts the reply bytes `cat`
/// writes, not the PTY's caret-notation echo of them.
async fn raw_cat_terminal() -> (Arc<TerminalManager>, TerminalInfo, tempfile::TempDir) {
    let manager = Arc::new(TerminalManager::new(
        HostId::new("host_test"),
        Arc::new(TracingAudit),
    ));
    let cwd = tempfile::tempdir().unwrap();
    let terminal = manager
        .open(
            OpenSpec {
                thread_id: None,
                host: HostId::new("host_test"),
                cwd: cwd.path().to_string_lossy().to_string(),
                shell: Some("/bin/sh".into()),
                args: vec!["-c".into(), "stty raw -echo; exec cat".into()],
                cols: 80,
                rows: 24,
            },
            &device(),
        )
        .await
        .expect("open");
    // Let the shell switch the PTY to raw, echo-free mode before writing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    (manager, terminal, cwd)
}

/// Attaches a viewer and returns its id, the output receiver and the handle
/// that keeps it alive. The viewer task is started only by callers that read.
fn attach_viewer(
    manager: &Arc<TerminalManager>,
    terminal: &TerminalInfo,
    read: bool,
) -> (
    u64,
    mpsc::Receiver<Outbound>,
    h4rn_daemon::terminal::Attached,
) {
    attach_viewer_declaring(manager, terminal, read, true)
}

/// Attaches a viewer that declares (or does not declare) positioned replies.
fn attach_viewer_declaring(
    manager: &Arc<TerminalManager>,
    terminal: &TerminalInfo,
    read: bool,
    reply_positions_v2: bool,
) -> (
    u64,
    mpsc::Receiver<Outbound>,
    h4rn_daemon::terminal::Attached,
) {
    let viewer_id = manager.next_viewer_id();
    let (tx, rx) = mpsc::channel::<Outbound>(4);
    let attached = manager
        .attach(
            &terminal.terminal_id,
            viewer_id,
            tx,
            Some(80),
            Some(24),
            reply_positions_v2,
        )
        .expect("attach viewer");
    if read {
        tokio::spawn(run_viewer(
            Arc::clone(&attached.session),
            viewer_id,
            Arc::clone(&attached.viewer),
            attached.output.resubscribe(),
            attached.snapshot.seq,
        ));
    }
    (viewer_id, rx, attached)
}

const MODE_REPLY: &[u8] = b"\x1b[?12;2$y";
const WRITTEN_MODE_REPLY: &str = "\u{1b}[?12;2$y";

fn at(position: u64, ordinal: u64) -> QueryReply {
    QueryReply::At { position, ordinal }
}

/// Pretends the daemon has already sent output up to `seq` to this viewer.
///
/// A reply may only name output the viewer was sent; tests that do not read
/// real output state that here. The synthetic chunk is acknowledged at once so
/// it does not consume the viewer's credit window.
fn mark_sent(attached: &h4rn_daemon::terminal::Attached, seq: u64) {
    attached.viewer.record_sent(seq, 1);
    attached.viewer.ack(seq);
}

/// Two viewers answer the same query position: the first reply is written and
/// the second is dropped, while typed input still goes through.
#[tokio::test]
async fn two_viewers_answering_the_same_position_write_once() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (first_id, _first_rx, first) = attach_viewer(&manager, &terminal, false);
    mark_sent(&first, 10);
    let (second_id, mut second_rx, second) = attach_viewer(&manager, &terminal, true);
    mark_sent(&second, 10);

    manager
        .input_by_stream(terminal.stream_id, first_id, MODE_REPLY, at(3, 0))
        .expect("first reply");
    manager
        .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, at(3, 0))
        .expect("duplicate reply is dropped");
    manager
        .input_by_stream(terminal.stream_id, second_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut second_rx,
        &manager,
        &terminal.terminal_id,
        second_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches(WRITTEN_MODE_REPLY).count(),
        1,
        "two viewers' replies for one position must reach the shell once: {output:?}"
    );

    drop(first);
    drop(second);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// One chunk can carry several queries (two mode reports, a batched palette
/// query) and xterm answers each separately. Every distinct answer is written
/// once; only exact duplicates are dropped.
#[tokio::test]
async fn every_answer_in_one_chunk_is_written_once() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (first_id, _first_rx, first) = attach_viewer(&manager, &terminal, false);
    mark_sent(&first, 10);
    let (second_id, mut second_rx, second) = attach_viewer(&manager, &terminal, true);
    mark_sent(&second, 10);

    // Two expected answers in chunk 3, each answered by both viewers.
    for ordinal in 0..2 {
        manager
            .input_by_stream(terminal.stream_id, first_id, MODE_REPLY, at(3, ordinal))
            .expect("first viewer answers");
        manager
            .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, at(3, ordinal))
            .expect("the other viewer's copy is dropped");
    }
    manager
        .input_by_stream(terminal.stream_id, second_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut second_rx,
        &manager,
        &terminal.terminal_id,
        second_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches(WRITTEN_MODE_REPLY).count(),
        2,
        "each answer in the chunk must reach the shell once: {output:?}"
    );

    drop(first);
    drop(second);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// A 40-entry `OSC 4` batch in one chunk earns 40 answers; each must be
/// written once, not just the first.
#[tokio::test]
async fn a_forty_slot_palette_batch_gets_one_answer_per_slot() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (first_id, _first_rx, first) = attach_viewer(&manager, &terminal, false);
    mark_sent(&first, 10);
    let (second_id, mut second_rx, second) = attach_viewer(&manager, &terminal, true);
    mark_sent(&second, 10);

    for ordinal in 0..40 {
        manager
            .input_by_stream(terminal.stream_id, first_id, MODE_REPLY, at(5, ordinal))
            .expect("first viewer answers a slot");
        manager
            .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, at(5, ordinal))
            .expect("the other viewer's copy is dropped");
    }
    manager
        .input_by_stream(terminal.stream_id, second_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until_before(
        &mut second_rx,
        &manager,
        &terminal.terminal_id,
        second_id,
        std::time::Instant::now() + Duration::from_secs(60),
        |output| output.contains("iHELLO") && output.matches(WRITTEN_MODE_REPLY).count() >= 40,
    )
    .await;
    assert_eq!(
        output.matches(WRITTEN_MODE_REPLY).count(),
        40,
        "every slot in the batch must be answered once: {output:?}"
    );

    drop(first);
    drop(second);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// A viewer that receives a query but never answers blocks nobody: another
/// viewer's reply for that position is written.
#[tokio::test]
async fn a_stalled_viewer_does_not_block_the_reply() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (_first_id, _first_rx, first) = attach_viewer(&manager, &terminal, false);
    let (second_id, mut second_rx, second) = attach_viewer(&manager, &terminal, true);
    mark_sent(&second, 10);

    // The first viewer stays silent, as a backgrounded tab would.
    manager
        .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, at(4, 0))
        .expect("reply from the second viewer");
    manager
        .input_by_stream(terminal.stream_id, second_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut second_rx,
        &manager,
        &terminal.terminal_id,
        second_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches(WRITTEN_MODE_REPLY).count(),
        1,
        "the silent viewer must not leave the query unanswered: {output:?}"
    );

    drop(first);
    drop(second);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// The viewer that answered first does not own the terminal. It may stay
/// attached and silent while another viewer answers a later query.
#[tokio::test]
async fn an_early_answer_does_not_claim_later_queries() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (first_id, _first_rx, first) = attach_viewer(&manager, &terminal, false);
    mark_sent(&first, 10);
    let (second_id, mut second_rx, second) = attach_viewer(&manager, &terminal, true);
    mark_sent(&second, 10);

    manager
        .input_by_stream(terminal.stream_id, first_id, MODE_REPLY, at(3, 0))
        .expect("first viewer answers position 3");
    manager
        .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, at(9, 0))
        .expect("second viewer answers position 9");
    manager
        .input_by_stream(terminal.stream_id, second_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut second_rx,
        &manager,
        &terminal.terminal_id,
        second_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches(WRITTEN_MODE_REPLY).count(),
        2,
        "both answers must be written; the first answerer owns no later query: {output:?}"
    );

    drop(first);
    drop(second);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// A reply naming output the viewer was never sent is ignored and leaves the
/// answered record untouched, so it cannot suppress another viewer's valid
/// reply, whether it comes before or after that viewer detaches.
#[tokio::test]
async fn a_reply_for_output_never_sent_is_ignored() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (first_id, _first_rx, first) = attach_viewer(&manager, &terminal, false);
    mark_sent(&first, 10);
    let (second_id, mut second_rx, second) = attach_viewer(&manager, &terminal, true);
    mark_sent(&second, 10);

    // The first viewer claims a position far beyond anything it was sent.
    manager
        .input_by_stream(terminal.stream_id, first_id, MODE_REPLY, at(99, 0))
        .expect("a bogus position is ignored");

    manager
        .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, at(3, 0))
        .expect("the real answer is written");
    manager
        .input_by_stream(terminal.stream_id, second_id, b"iHELLO", QueryReply::No)
        .expect("typed input");
    let output = read_until(
        &mut second_rx,
        &manager,
        &terminal.terminal_id,
        second_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches(WRITTEN_MODE_REPLY).count(),
        1,
        "the bogus claim must not suppress the valid answer: {output:?}"
    );

    // After the bogus viewer detaches, the valid viewer still answers.
    manager.detach_viewer(first_id);
    manager
        .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, at(4, 0))
        .expect("another real answer is written");
    manager
        .input_by_stream(terminal.stream_id, second_id, b"iEND", QueryReply::No)
        .expect("typed input");
    let output = read_until(
        &mut second_rx,
        &manager,
        &terminal.terminal_id,
        second_id,
        |output| output.contains("iEND"),
    )
    .await;
    assert_eq!(
        output.matches(WRITTEN_MODE_REPLY).count(),
        1,
        "the valid viewer keeps answering after the bogus one detaches: {output:?}"
    );

    drop(first);
    drop(second);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// A viewer that attached after a chunk was produced never parsed that chunk,
/// so it has no answer for a query that started there. The queries it does
/// see keep the same identity as an earlier viewer's, so both name the next
/// chunk's query alike and it is written once.
#[tokio::test]
async fn a_viewer_that_attached_mid_query_names_the_next_query_the_same_way() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (early_id, _early_rx, early) = attach_viewer(&manager, &terminal, false);
    mark_sent(&early, 10);
    let (late_id, mut late_rx, late) = attach_viewer(&manager, &terminal, true);
    // The late viewer attaches with chunk 1 already folded into its snapshot:
    // it parses chunks 2 onward, so a query that started in chunk 1 is not in
    // its numbering at all.
    late.viewer.reset_credit(1);
    mark_sent(&late, 10);

    // Both viewers answered the query that starts in chunk 2 the same way, so
    // the first reply is the one written and the other is a duplicate. The
    // early viewer also answers the query that started in chunk 1.
    manager
        .input_by_stream(terminal.stream_id, late_id, b"iLATE", at(2, 0))
        .expect("the late viewer's answer to the chunk-2 query is written");
    manager
        .input_by_stream(terminal.stream_id, early_id, b"iEARLY", at(2, 0))
        .expect("the early viewer's duplicate is dropped");
    manager
        .input_by_stream(terminal.stream_id, early_id, b"iSPLIT", at(1, 0))
        .expect("the early viewer answers the query that started in chunk 1");
    manager
        .input_by_stream(terminal.stream_id, late_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut late_rx,
        &manager,
        &terminal.terminal_id,
        late_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches("iSPLIT").count(),
        1,
        "the query that started before the late snapshot is answered once: {output:?}"
    );
    assert_eq!(
        output.matches("iLATE").count(),
        1,
        "the late viewer's answer to the chunk-2 query is the one written: {output:?}"
    );
    assert!(
        !output.contains("iEARLY"),
        "both viewers name the chunk-2 query alike, so the second answer is dropped: {output:?}"
    );

    drop(early);
    drop(late);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// With no other viewer able to answer the position, a viewer that attached
/// at a snapshot is the only parser and may answer its first raw chunk.
#[tokio::test]
async fn a_sole_viewer_that_attached_at_a_snapshot_answers_its_first_chunk() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (viewer_id, mut rx, attached) = attach_viewer(&manager, &terminal, true);
    // The snapshot covers chunk 5; this viewer parses chunk 6 onward.
    attached.viewer.reset_credit(5);
    mark_sent(&attached, 6);

    manager
        .input_by_stream(terminal.stream_id, viewer_id, b"iFIRST", at(6, 0))
        .expect("the sole viewer answers its first raw chunk");
    manager
        .input_by_stream(terminal.stream_id, viewer_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut rx,
        &manager,
        &terminal.terminal_id,
        viewer_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches("iFIRST").count(),
        1,
        "a sole viewer's first answer has no other parser to collide with: {output:?}"
    );

    drop(attached);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// A viewer never parsed the chunk its snapshot replaced raw. xterm can still
/// report a query that the snapshot's leading escape completed, so the viewer
/// may name that chunk; but with another viewer attached holding the raw
/// chunk, only that viewer's answer is real. The snapshot claim is dropped and
/// never suppresses it.
#[tokio::test]
async fn a_snapshot_chunk_claim_is_dropped_when_another_viewer_can_answer() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (raw_id, mut raw_rx, raw) = attach_viewer(&manager, &terminal, true);
    // The raw viewer saw chunk 5 as output.
    mark_sent(&raw, 5);
    let (snapshot_id, _snapshot_rx, snapshot) = attach_viewer(&manager, &terminal, false);
    // This viewer attached with chunk 5 already folded into its snapshot.
    snapshot.viewer.reset_credit(5);
    mark_sent(&snapshot, 5);

    manager
        .input_by_stream(terminal.stream_id, snapshot_id, b"iSNAPSHOT", at(5, 0))
        .expect("a snapshot-chunk claim is dropped without failing the connection");
    manager
        .input_by_stream(terminal.stream_id, raw_id, b"iRAW", at(5, 0))
        .expect("the viewer that parsed the chunk raw answers it");
    manager
        .input_by_stream(terminal.stream_id, raw_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut raw_rx,
        &manager,
        &terminal.terminal_id,
        raw_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches("iRAW").count(),
        1,
        "the raw viewer's answer is written exactly once: {output:?}"
    );
    assert!(
        !output.contains("iSNAPSHOT"),
        "a claim for a chunk the snapshot replaced cannot suppress the real answer: {output:?}"
    );

    drop(raw);
    drop(snapshot);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// With no other parser attached, the snapshot viewer is the only one that can
/// answer a query whose start the snapshot replaced, so its claim on the
/// snapshot chunk itself is written. The shell is never left unanswered.
#[tokio::test]
async fn a_sole_viewer_answers_a_query_the_snapshot_completed() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (viewer_id, mut rx, attached) = attach_viewer(&manager, &terminal, true);
    // The snapshot covers chunk 5. xterm completes a query that started in
    // that chunk with the snapshot's leading bytes, naming it (5, 0).
    attached.viewer.reset_credit(5);
    mark_sent(&attached, 5);

    // A second viewer is registered but still taking its attach snapshot: it
    // has no output and cannot answer, so it must not make the ready viewer a
    // non-sole one and leave the query unanswered.
    let attaching_id = manager.next_viewer_id();
    let (attaching_tx, _attaching_rx) = mpsc::channel::<Outbound>(4);
    attached.session.add_viewer(
        attaching_id,
        80,
        24,
        true,
        Arc::new(Viewer::new(attaching_tx)),
    );

    manager
        .input_by_stream(terminal.stream_id, viewer_id, b"iSNAPSHOT", at(5, 0))
        .expect("the sole viewer answers the query its snapshot completed");
    manager
        .input_by_stream(terminal.stream_id, viewer_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut rx,
        &manager,
        &terminal.terminal_id,
        viewer_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches("iSNAPSHOT").count(),
        1,
        "a sole viewer's snapshot answer reaches the shell once: {output:?}"
    );

    drop(attached);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// A viewer that is registered but has not been handed its attach snapshot
/// yet has received nothing, so it cannot answer and must not hold back the
/// viewer that can.
#[tokio::test]
async fn a_viewer_still_attaching_does_not_block_the_answer() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (viewer_id, mut rx, attached) = attach_viewer(&manager, &terminal, true);
    // This viewer's snapshot covers chunk 5; chunk 6 is its first raw chunk.
    attached.viewer.reset_credit(5);
    mark_sent(&attached, 6);

    // A second viewer is registered while its attach is still working: it has
    // no snapshot position yet and has been sent no output.
    let second_id = manager.next_viewer_id();
    let (second_tx, _second_rx) = mpsc::channel::<Outbound>(4);
    attached
        .session
        .add_viewer(second_id, 80, 24, true, Arc::new(Viewer::new(second_tx)));

    manager
        .input_by_stream(terminal.stream_id, viewer_id, b"iFIRST", at(6, 0))
        .expect("the ready viewer answers its first raw chunk");
    manager
        .input_by_stream(terminal.stream_id, viewer_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut rx,
        &manager,
        &terminal.terminal_id,
        viewer_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches("iFIRST").count(),
        1,
        "a viewer that has received nothing yet cannot block the answer: {output:?}"
    );

    drop(attached);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// A viewer that attached before the chunk but is stalled below it never saw
/// the query, so it does not hold back a late-attached viewer's answer.
#[tokio::test]
async fn a_stalled_viewer_below_the_chunk_does_not_block_the_answer() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (_early_id, _early_rx, early) = attach_viewer(&manager, &terminal, false);
    // The early viewer attached at 0 but its output stalled at chunk 3.
    mark_sent(&early, 3);
    let (late_id, mut late_rx, late) = attach_viewer(&manager, &terminal, true);
    // The late viewer attached with chunk 6 in its snapshot: chunk 7 is its
    // first raw chunk, and it is the only viewer that received it.
    late.viewer.reset_credit(6);
    mark_sent(&late, 7);

    manager
        .input_by_stream(terminal.stream_id, late_id, b"iLATE", at(7, 0))
        .expect("the stalled viewer never saw chunk 7 and cannot block the answer");
    manager
        .input_by_stream(terminal.stream_id, late_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut late_rx,
        &manager,
        &terminal.terminal_id,
        late_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches("iLATE").count(),
        1,
        "a viewer stalled below the chunk cannot leave the query unanswered: {output:?}"
    );

    drop(early);
    drop(late);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// A resync snapshot moves the answering window exactly like an attach
/// snapshot, and a sole viewer is still the only parser that can answer.
#[tokio::test]
async fn a_sole_viewer_answers_after_a_resync_snapshot() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (viewer_id, mut rx, attached) = attach_viewer(&manager, &terminal, true);
    // Resync at position 5: the snapshot replaces chunk 5.
    attached.viewer.begin_resync();
    attached.viewer.record_snapshot_sent(5);
    mark_sent(&attached, 6);

    manager
        .input_by_stream(terminal.stream_id, viewer_id, b"iRESYNC", at(6, 0))
        .expect("the sole viewer answers after a resync snapshot");
    manager
        .input_by_stream(terminal.stream_id, viewer_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut rx,
        &manager,
        &terminal.terminal_id,
        viewer_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches("iRESYNC").count(),
        1,
        "a sole viewer still answers after a resync snapshot: {output:?}"
    );

    drop(attached);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// A viewer that never received chunk 5 cannot claim its first answer and
/// suppress the viewer that did receive it.
#[tokio::test]
async fn a_bogus_position_cannot_claim_a_real_answer() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (bogus_id, _bogus_rx, bogus) = attach_viewer(&manager, &terminal, false);
    // It attached after chunk 3 (its snapshot holds it), then received 4.
    bogus.viewer.reset_credit(3);
    mark_sent(&bogus, 4);
    let (real_id, mut real_rx, real) = attach_viewer(&manager, &terminal, true);
    mark_sent(&real, 5);

    manager
        .input_by_stream(terminal.stream_id, bogus_id, b"iBOGUS", at(5, 0))
        .expect("a position it was never sent is ignored");
    manager
        .input_by_stream(terminal.stream_id, real_id, b"iREAL", at(5, 0))
        .expect("the legitimate answer is written");
    manager
        .input_by_stream(terminal.stream_id, real_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut real_rx,
        &manager,
        &terminal.terminal_id,
        real_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches("iREAL").count(),
        1,
        "the legitimate answer is written once: {output:?}"
    );
    assert!(
        !output.contains("iBOGUS"),
        "a bogus claim must not suppress the real answer: {output:?}"
    );

    drop(bogus);
    drop(real);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// An old client is known to be old when it attaches, so the terminal is in
/// the legacy single-owner mode from that moment, before any reply arrives.
/// The first marked reply owns every answer; positioned replies from the owner
/// are still deduplicated.
#[tokio::test]
async fn an_old_client_keeps_one_reply_owner_until_it_detaches() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (first_id, _first_rx, first) = attach_viewer_declaring(&manager, &terminal, false, false);
    mark_sent(&first, 10);
    let (second_id, mut second_rx, second) = attach_viewer(&manager, &terminal, true);
    mark_sent(&second, 10);

    manager
        .input_by_stream(
            terminal.stream_id,
            first_id,
            MODE_REPLY,
            QueryReply::Unpositioned,
        )
        .expect("the old client owns the first reply");
    manager
        .input_by_stream(
            terminal.stream_id,
            second_id,
            MODE_REPLY,
            QueryReply::Unpositioned,
        )
        .expect("the other client's reply is dropped");
    // A positioned reply from the newcomer is dropped too while the old
    // client owns the terminal: one rule for both kinds of reply.
    manager
        .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, at(3, 0))
        .expect("positioned reply from a non-owner is dropped");
    manager
        .input_by_stream(terminal.stream_id, second_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut second_rx,
        &manager,
        &terminal.terminal_id,
        second_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches(WRITTEN_MODE_REPLY).count(),
        1,
        "an old client's replies must keep one owner: {output:?}"
    );

    // Detaching the owner returns the terminal to the per-answer rule.
    manager.detach_viewer(first_id);
    manager
        .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, at(3, 0))
        .expect("the new client answers after hand-over");
    manager
        .input_by_stream(terminal.stream_id, second_id, b"iEND", QueryReply::No)
        .expect("typed input");
    let output = read_until(
        &mut second_rx,
        &manager,
        &terminal.terminal_id,
        second_id,
        |output| output.contains("iEND"),
    )
    .await;
    assert_eq!(
        output.matches(WRITTEN_MODE_REPLY).count(),
        1,
        "the per-answer rule resumes when the old client leaves: {output:?}"
    );

    drop(first);
    drop(second);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// Both arrival orders with an old client attached write exactly one answer.
/// The old client answers first here.
#[tokio::test]
async fn mixed_versions_old_client_answers_first_writes_once() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (first_id, _first_rx, first) = attach_viewer_declaring(&manager, &terminal, false, false);
    mark_sent(&first, 10);
    let (second_id, mut second_rx, second) = attach_viewer(&manager, &terminal, true);
    mark_sent(&second, 10);

    manager
        .input_by_stream(
            terminal.stream_id,
            first_id,
            MODE_REPLY,
            QueryReply::Unpositioned,
        )
        .expect("old client answers first");
    manager
        .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, at(3, 0))
        .expect("new client's positioned reply is dropped");
    manager
        .input_by_stream(terminal.stream_id, second_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut second_rx,
        &manager,
        &terminal.terminal_id,
        second_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches(WRITTEN_MODE_REPLY).count(),
        1,
        "only the old client's answer is written while it is attached: {output:?}"
    );

    drop(first);
    drop(second);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// Both arrival orders with an old client attached write exactly one answer.
/// The new client answers first here: it owns the legacy role too, so the old
/// client's reply is the one dropped.
#[tokio::test]
async fn mixed_versions_new_client_answers_first_writes_once() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (first_id, _first_rx, first) = attach_viewer_declaring(&manager, &terminal, false, false);
    mark_sent(&first, 10);
    let (second_id, mut second_rx, second) = attach_viewer(&manager, &terminal, true);
    mark_sent(&second, 10);

    manager
        .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, at(3, 0))
        .expect("new client claims the legacy role");
    manager
        .input_by_stream(
            terminal.stream_id,
            first_id,
            MODE_REPLY,
            QueryReply::Unpositioned,
        )
        .expect("old client's reply is dropped");
    manager
        .input_by_stream(terminal.stream_id, second_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut second_rx,
        &manager,
        &terminal.terminal_id,
        second_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches(WRITTEN_MODE_REPLY).count(),
        1,
        "the first answer reaches the shell once, whichever client sent it: {output:?}"
    );

    drop(first);
    drop(second);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// In mixed mode a rejected positioned claim must not take the legacy role.
/// The old client's next valid unpositioned reply is still written.
#[tokio::test]
async fn a_rejected_reply_does_not_claim_the_legacy_owner() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (old_id, _old_rx, old) = attach_viewer_declaring(&manager, &terminal, false, false);
    mark_sent(&old, 10);
    let (new_id, mut new_rx, new) = attach_viewer(&manager, &terminal, true);
    mark_sent(&new, 10);

    // The new client claims output it was never sent; the reply is rejected,
    // so it must not own the terminal's answers.
    manager
        .input_by_stream(terminal.stream_id, new_id, b"iBOGUS", at(99, 0))
        .expect("a bogus claim is rejected");
    manager
        .input_by_stream(
            terminal.stream_id,
            old_id,
            b"iOLD",
            QueryReply::Unpositioned,
        )
        .expect("the old client's valid reply is written");
    manager
        .input_by_stream(terminal.stream_id, new_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut new_rx,
        &manager,
        &terminal.terminal_id,
        new_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches("iOLD").count(),
        1,
        "a rejected claim must not suppress the old client's reply: {output:?}"
    );
    assert!(
        !output.contains("iBOGUS"),
        "the rejected claim must not reach the shell: {output:?}"
    );

    drop(old);
    drop(new);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// A legacy viewer attaching or detaching elsewhere must not move the reply
/// role while an answer is in flight.
#[tokio::test]
async fn another_legacy_viewer_cannot_reset_the_owner() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (owner_id, _owner_rx, owner) = attach_viewer_declaring(&manager, &terminal, false, false);
    mark_sent(&owner, 10);
    let (new_id, mut new_rx, new) = attach_viewer(&manager, &terminal, true);
    mark_sent(&new, 10);

    // The old client owns the replies after a written answer.
    manager
        .input_by_stream(
            terminal.stream_id,
            owner_id,
            b"iOWNER",
            QueryReply::Unpositioned,
        )
        .expect("the old client owns the first reply");

    // Another old client comes and goes. Neither event may hand the role to
    // the new client or clear it while the owner is still attached.
    let (other_id, _other_rx, other) = attach_viewer_declaring(&manager, &terminal, false, false);
    mark_sent(&other, 10);
    manager.detach_viewer(other_id);

    // The new client's reply is the one that must be dropped: the owner is
    // unchanged and can still answer the query.
    manager
        .input_by_stream(
            terminal.stream_id,
            new_id,
            b"iNEW",
            QueryReply::Unpositioned,
        )
        .expect("a non-owner's reply is dropped");
    manager
        .input_by_stream(terminal.stream_id, new_id, b"iHELLO", QueryReply::No)
        .expect("typed input");

    let output = read_until(
        &mut new_rx,
        &manager,
        &terminal.terminal_id,
        new_id,
        |output| output.contains("iHELLO"),
    )
    .await;
    assert_eq!(
        output.matches("iOWNER").count(),
        1,
        "the owner's reply stays the only one written: {output:?}"
    );
    assert!(
        !output.contains("iNEW"),
        "attaching and detaching an old client must not reset the owner: {output:?}"
    );

    drop(owner);
    drop(new);
    drop(other);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// Closing a terminal hangs up the shell, which forwards SIGHUP to its jobs.
/// A job that put itself in another session survives, and so does one whose
/// `nohup` explicitly ignores the hangup.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn closing_a_terminal_ends_background_jobs() {
    use std::path::Path;

    async fn read_pid(path: &Path) -> i32 {
        for _ in 0..1000 {
            if let Ok(text) = std::fs::read_to_string(path)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{} never appeared", path.display());
    }

    fn process_running(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // A zombie is already gone; its parent just has not reaped it.
            Ok(stat) => {
                stat.rsplit(')')
                    .next()
                    .and_then(|rest| rest.split_whitespace().next())
                    != Some("Z")
            }
            Err(_) => false,
        }
    }

    async fn wait_until_gone(what: &str, pid: i32) {
        for _ in 0..1000 {
            if !process_running(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{what} (pid {pid}) still runs after the terminal closed");
    }

    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let work = tempfile::tempdir().unwrap();
    let cwd_path = work.path().to_path_buf();
    let cwd = cwd_path.to_string_lossy().to_string();

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let terminal = client
        .open(
            host,
            &cwd,
            Some("/bin/bash"),
            vec!["--noprofile".into(), "--norc".into(), "-i".into()],
            80,
            24,
        )
        .await;
    client.attach(&terminal.terminal_id, 80, 24).await;

    // Wait for the shell itself rather than a fixed startup delay. The `''`
    // keeps the echoed command line from containing the marker the command
    // prints, so the wait cannot match its own echo.
    client
        .send_input(terminal.stream_id, b"echo h4rn''-ready\n")
        .await;
    client.output_until("h4rn-ready").await;

    // Keep the shell itself out of the SIGTERM path, so the fixtures below
    // exercise their own traps rather than the shell's exit.
    client
        .send_input(terminal.stream_id, b"trap '' TERM\n")
        .await;

    // `set -m` gives the job its own process group, the case a group-only
    // signal misses; the shell's SIGHUP forwarding reaches it anyway.
    client
        .send_input(
            terminal.stream_id,
            b"set -m; sleep 1000 & echo $! > bg.pid\n",
        )
        .await;
    let bg = read_pid(&cwd_path.join("bg.pid")).await;
    assert!(process_running(bg), "background job did not start");

    // `nohup` ignores the hangup, so only the person may end it.
    client
        .send_input(
            terminal.stream_id,
            b"nohup sleep 1000 >/dev/null 2>&1 & echo $! > nohup.pid\n",
        )
        .await;
    let nohup_job = read_pid(&cwd_path.join("nohup.pid")).await;
    assert!(process_running(nohup_job), "nohup job did not start");

    // A job that moved itself into another session has left the terminal.
    let has_setsid = std::process::Command::new("setsid")
        .arg("--version")
        .output()
        .is_ok();
    let detached = if has_setsid {
        client
            .send_input(
                terminal.stream_id,
                b"setsid sh -c 'echo $$ > detached.pid; exec sleep 1000' &\n",
            )
            .await;
        Some(read_pid(&cwd_path.join("detached.pid")).await)
    } else {
        None
    };

    // A job that only dies when the shell forwards SIGHUP before SIGTERM.
    // `set +m` leaves it in the shell's process group, so the TERM-first order
    // reaches it first; that trap then ignores HUP and moves the job into a new
    // session, which the group-wide KILL also misses. With HUP first the job
    // exits at once, before TERM can run the trap.
    let hup_first = if has_setsid {
        client.send_input(terminal.stream_id, b"set +m\n").await;
        client
            .send_input(
                terminal.stream_id,
                b"( trap 'trap \"\" HUP; exec setsid sleep 1000' TERM; trap 'exit 0' HUP; echo hup''-ready; while :; do sleep 0.05; done ) & echo $! > hup-first.pid\n",
            )
            .await;
        let pid = read_pid(&cwd_path.join("hup-first.pid")).await;
        // The marker is printed after both traps are installed, so the job is
        // ready to receive the hangup without a timing guess.
        client.output_until("hup-ready").await;
        Some(pid)
    } else {
        None
    };

    client
        .request(ClientRequest::TerminalClose {
            terminal_id: terminal.terminal_id,
        })
        .await
        .expect("close terminal");

    wait_until_gone("the background job", bg).await;
    if let Some(pid) = hup_first {
        wait_until_gone("the HUP-before-TERM job", pid).await;
    }
    for survivor in [Some(nohup_job), detached].into_iter().flatten() {
        assert!(
            process_running(survivor),
            "a job that ignored or left the hangup must survive the terminal"
        );
        let _ = std::process::Command::new("kill")
            .args(["-9", &survivor.to_string()])
            .status();
        wait_until_gone("a detached job", survivor).await;
    }
    daemon.stop().await;
}

/// Closing a terminal keeps a grace period after SIGTERM, so a job whose TERM
/// handler needs a moment to clean up finishes before the group is killed.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn closing_a_terminal_waits_for_term_cleanup() {
    let daemon = boot().await;
    let host = host_id_for(&daemon).await;
    let work = tempfile::tempdir().unwrap();
    let cwd_path = work.path().to_path_buf();
    let cwd = cwd_path.to_string_lossy().to_string();

    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    let terminal = client
        .open(
            host,
            &cwd,
            Some("/bin/bash"),
            vec!["--noprofile".into(), "--norc".into(), "-i".into()],
            80,
            24,
        )
        .await;
    client.attach(&terminal.terminal_id, 80, 24).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // `set +m` keeps the job in the shell's process group, so the group-wide
    // SIGTERM reaches it. It ignores SIGHUP and spends ~80 ms writing its
    // marker after TERM; without the grace period the group is killed first.
    client.send_input(terminal.stream_id, b"set +m\n").await;
    client
        .send_input(
            terminal.stream_id,
            b"( trap '' HUP; trap 'sleep 0.08; echo cleaned > cleanup.txt; exit 0' TERM; while :; do sleep 0.01; done ) & echo $! > worker.pid\n",
        )
        .await;
    let worker = cwd_path.join("worker.pid");
    for _ in 0..500 {
        if worker.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(worker.exists(), "the cleanup worker never started");
    // Give the loop a moment to install its traps.
    tokio::time::sleep(Duration::from_millis(100)).await;

    client
        .request(ClientRequest::TerminalClose {
            terminal_id: terminal.terminal_id,
        })
        .await
        .expect("close terminal");

    let marker = cwd_path.join("cleanup.txt");
    for _ in 0..100 {
        if marker.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        marker.exists(),
        "the TERM handler was killed before it could write its marker"
    );
    daemon.stop().await;
}
