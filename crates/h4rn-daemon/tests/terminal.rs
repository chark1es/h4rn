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
use h4rn_daemon::terminal::{OpenSpec, QueryReply, TerminalManager, TracingAudit, run_viewer};
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
    let daemon = DaemonBuilder::new(config).build().await.expect("build");
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
                self.binaries.push_back(frame);
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
        match self
            .request(ClientRequest::TerminalAttach {
                terminal_id: terminal_id.clone(),
                cols: Some(cols),
                rows: Some(rows),
            })
            .await
            .expect("attach terminal")
        {
            ResponseBody::TerminalAttached {
                terminal,
                snapshot,
                start_seq,
            } => AttachedView {
                terminal,
                snapshot,
                start_seq,
            },
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
        .attach(&terminal.terminal_id, viewer_id, tx, Some(80), Some(24))
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

    // A mode report both viewers' xterm would answer identically.
    let mode_reply: &[u8] = b"\x1b[?12;2$y";
    let written = "\u{1b}[?12;2$y";
    let position = pty_input_reply_seq(3);
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
    mut done: impl FnMut(&str) -> bool,
) -> String {
    let mut output = String::new();
    loop {
        let Ok(Some(message)) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await
        else {
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
    let viewer_id = manager.next_viewer_id();
    let (tx, rx) = mpsc::channel::<Outbound>(4);
    let attached = manager
        .attach(&terminal.terminal_id, viewer_id, tx, Some(80), Some(24))
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

/// Two viewers answer the same query position: the first reply is written and
/// the second is dropped, while typed input still goes through.
#[tokio::test]
async fn two_viewers_answering_the_same_position_write_once() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (first_id, _first_rx, first) = attach_viewer(&manager, &terminal, false);
    let (second_id, mut second_rx, second) = attach_viewer(&manager, &terminal, true);

    manager
        .input_by_stream(terminal.stream_id, first_id, MODE_REPLY, QueryReply::At(3))
        .expect("first reply");
    manager
        .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, QueryReply::At(3))
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

/// A viewer that receives a query but never answers blocks nobody: another
/// viewer's reply for that position is written.
#[tokio::test]
async fn a_stalled_viewer_does_not_block_the_reply() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (_first_id, _first_rx, first) = attach_viewer(&manager, &terminal, false);
    let (second_id, mut second_rx, second) = attach_viewer(&manager, &terminal, true);

    // The first viewer stays silent, as a backgrounded tab would.
    manager
        .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, QueryReply::At(4))
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

/// A reply for an already answered position, or for an older one, is dropped
/// even when it comes from another viewer.
#[tokio::test]
async fn replies_for_answered_or_older_positions_are_dropped() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (first_id, _first_rx, first) = attach_viewer(&manager, &terminal, false);
    let (second_id, mut second_rx, second) = attach_viewer(&manager, &terminal, true);

    manager
        .input_by_stream(terminal.stream_id, first_id, MODE_REPLY, QueryReply::At(6))
        .expect("reply for the newest position");
    manager
        .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, QueryReply::At(6))
        .expect("duplicate position is dropped");
    manager
        .input_by_stream(terminal.stream_id, second_id, MODE_REPLY, QueryReply::At(5))
        .expect("older position is dropped");
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
        "only the first reply for the newest position may be written: {output:?}"
    );

    drop(first);
    drop(second);
    manager.close(&terminal.terminal_id).await.expect("close");
    manager.shutdown().await;
    drop(cwd);
}

/// A reply with no position comes from an older client and keeps the previous
/// single-owner rule: the first viewer owns replies until it detaches.
#[tokio::test]
async fn an_unpositioned_reply_keeps_the_old_single_owner_rule() {
    let (manager, terminal, cwd) = raw_cat_terminal().await;
    let (first_id, _first_rx, first) = attach_viewer(&manager, &terminal, false);
    let (second_id, mut second_rx, second) = attach_viewer(&manager, &terminal, true);

    manager
        .input_by_stream(
            terminal.stream_id,
            first_id,
            MODE_REPLY,
            QueryReply::Unpositioned,
        )
        .expect("first reply");
    manager
        .input_by_stream(
            terminal.stream_id,
            second_id,
            MODE_REPLY,
            QueryReply::Unpositioned,
        )
        .expect("second reply is dropped");
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

    // Detaching the owner frees the role for the remaining viewer.
    manager.detach_viewer(first_id);
    manager
        .input_by_stream(
            terminal.stream_id,
            second_id,
            MODE_REPLY,
            QueryReply::Unpositioned,
        )
        .expect("reply after hand-over");
    let output = read_until(
        &mut second_rx,
        &manager,
        &terminal.terminal_id,
        second_id,
        |output| output.matches(WRITTEN_MODE_REPLY).count() >= 1,
    )
    .await;
    assert_eq!(
        output.matches(WRITTEN_MODE_REPLY).count(),
        1,
        "the surviving viewer must take over unpositioned replies: {output:?}"
    );

    drop(first);
    drop(second);
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
        for _ in 0..500 {
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
        for _ in 0..250 {
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
    // Let the prompt settle before typing.
    tokio::time::sleep(Duration::from_millis(200)).await;

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
                b"( trap 'trap \"\" HUP; exec setsid sleep 1000' TERM; trap 'exit 0' HUP; while :; do sleep 0.05; done ) & echo $! > hup-first.pid\n",
            )
            .await;
        let pid = read_pid(&cwd_path.join("hup-first.pid")).await;
        // Give the loop a moment to install its traps.
        tokio::time::sleep(Duration::from_millis(100)).await;
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
