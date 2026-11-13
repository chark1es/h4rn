//! The Claude Code adapter (ADR-005).
//!
//! # Transport
//!
//! One long-lived process per session:
//!
//! ```text
//! claude -p --input-format stream-json --output-format stream-json \
//!        --verbose --include-partial-messages [--model <m>] [--resume <id>] \
//!        [--mcp-config <0600 tempfile> --strict-mcp-config] \
//!        [--permission-mode <m>] [--permission-prompt-tool <mcp tool>]
//! ```
//!
//! Answers arrive as newline-delimited JSON on stdout and are normalised by
//! [`protocol::ClaudeStreamParser`]. User turns are written to stdin as
//! `{"type":"user", ...}` stream-json frames; the process stays alive between
//! turns, so no `--resume` is needed until the daemon restarts.
//!
//! # MCP injection
//!
//! The hub endpoint is written to a `0600` temp file that is deleted when the
//! session shuts down. Only the `h4rn` entry is present and
//! `--strict-mcp-config` keeps the user's own servers out (ADR-006). The
//! bearer token is read from the `H4RN_MCP_AUTH_TOKEN` (or
//! `H4RN_MCP_BEARER_TOKEN`) entry of [`SessionSpec::env_overrides`] and never
//! appears in argv or the child's environment.
//!
//! # Approvals
//!
//! Claude Code in print mode has no interactive prompt of its own: anything
//! that would prompt is auto-denied unless a permission-prompt tool is
//! configured. The daemon therefore sets
//! `H4RN_CLAUDE_PERMISSION_PROMPT_TOOL=mcp__h4rn__approval_prompt` once the
//! hub exposes that tool, which the CLI calls on the h4rn MCP server; the
//! hub owns the pending approval, so [`HarnessSession::respond_approval`]
//! reports `Unsupported` rather than pretending it can answer. Without the
//! override, Claude auto-denies the action and the adapter reports the denied
//! `ToolCall` plus a diagnostic, which is honest about what happened.
//!
//! # Interrupt and shutdown
//!
//! Interrupt uses the SDK control channel: `{"type":"control_request",
//! "request":{"subtype":"interrupt"}}` on stdin, which the CLI acknowledges
//! with a `control_response`. Shutdown closes stdin, sends `SIGTERM` to the
//! child's process group, waits five seconds, then `SIGKILL`s the group. The
//! child is spawned into its own process group so `Bash` tools die with it
//! (ADR-021).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use base64::Engine as _;
use futures::stream::{BoxStream, StreamExt};
use h4rn_protocol::{AgentState, ApprovalDecision, HarnessKind, RuntimeMode, TurnStatus};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    AttachmentRef, Harness, HarnessError, HarnessEvent, HarnessInstall, HarnessSession,
    SessionSpec, UserInput,
};

mod limit;
pub(crate) mod process;
mod protocol;

pub use limit::{LimitSignal, detect_limit, parse_reset_time};
pub use process::StderrRing;
pub use protocol::{ClaudeStreamParser, TurnReport, state_for_tool, tool_paths, tool_summary};

use process::{GroupSignal, signal_group};

/// Extra environment key the daemon may set to give the MCP hub a bearer token.
///
/// It is consumed by the adapter and removed from the child's environment.
pub const ENV_MCP_AUTH_TOKEN: &str = "H4RN_MCP_AUTH_TOKEN";
/// Alias for [`ENV_MCP_AUTH_TOKEN`].
pub const ENV_MCP_BEARER_TOKEN: &str = "H4RN_MCP_BEARER_TOKEN";
/// Overrides `--permission-mode` (`manual`, `acceptEdits`, `plan`, ...).
pub const ENV_PERMISSION_MODE: &str = "H4RN_CLAUDE_PERMISSION_MODE";
/// Names the MCP tool the CLI calls for permission prompts.
pub const ENV_PERMISSION_PROMPT_TOOL: &str = "H4RN_CLAUDE_PERMISSION_PROMPT_TOOL";
/// Claude Code gateway base URL, **without** a trailing `/v1`.
pub const ENV_ANTHROPIC_BASE_URL: &str = "ANTHROPIC_BASE_URL";
/// Bearer token Claude Code sends to a gateway.
pub const ENV_ANTHROPIC_AUTH_TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";
/// Main model id.
pub const ENV_ANTHROPIC_MODEL: &str = "ANTHROPIC_MODEL";
/// Small/fast model id.
pub const ENV_ANTHROPIC_SMALL_FAST_MODEL: &str = "ANTHROPIC_SMALL_FAST_MODEL";
/// Enables `/model` discovery from the gateway's `/v1/models`.
pub const ENV_GATEWAY_MODEL_DISCOVERY: &str = "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY";

const MCP_TIMEOUT_MS: u64 = 65 * 60 * 1000;
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 80 * 1024 * 1024;
const EVENT_CHANNEL_CAPACITY: usize = 512;
const STDIN_CHANNEL_CAPACITY: usize = 64;
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Environment variables a nested Claude Code must not inherit.
///
/// A daemon started from a Claude Code session would otherwise make the child
/// believe it is part of that terminal session (T3 Code's `ClaudeHome`).
const SCRUBBED_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "ELECTRON_RUN_AS_NODE",
];

const H4RN_ONLY_ENV: &[&str] = &[
    ENV_MCP_AUTH_TOKEN,
    ENV_MCP_BEARER_TOKEN,
    ENV_PERMISSION_MODE,
    ENV_PERMISSION_PROMPT_TOOL,
];

const PERMISSION_MODES: &[&str] = &[
    "default",
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "manual",
    "dontAsk",
    "plan",
];

/// The Claude Code adapter.
#[derive(Clone, Debug, Default)]
pub struct ClaudeCodeHarness {
    install: Option<HarnessInstall>,
}

impl ClaudeCodeHarness {
    /// Creates an adapter that discovers `claude` on this host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an adapter bound to a known installation.
    ///
    /// Tests and the daemon's inventory use this after running
    /// [`detect_claude`] once; nothing else should hard-code a path.
    pub fn with_install(install: HarnessInstall) -> Self {
        Self {
            install: Some(install),
        }
    }
}

#[async_trait]
impl Harness for ClaudeCodeHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::ClaudeCode
    }

    fn detect(&self) -> Option<HarnessInstall> {
        self.install.clone().or_else(detect_claude)
    }

    async fn start_session(
        &self,
        spec: SessionSpec,
    ) -> Result<Box<dyn HarnessSession>, HarnessError> {
        let install = self
            .detect()
            .ok_or(HarnessError::NotDetected { kind: self.kind() })?;
        if !spec.workspace.is_dir() {
            return Err(HarnessError::Spawn {
                harness: self.kind(),
                message: format!(
                    "workspace {} is not a directory on this host",
                    spec.workspace.display()
                ),
            });
        }

        let mcp_token = spec
            .env_overrides
            .get(ENV_MCP_AUTH_TOKEN)
            .or_else(|| spec.env_overrides.get(ENV_MCP_BEARER_TOKEN))
            .map(String::as_str);
        let mcp_config = spec
            .mcp_endpoint
            .as_deref()
            .filter(|endpoint| !endpoint.trim().is_empty())
            .map(|endpoint| write_mcp_config(endpoint, mcp_token))
            .transpose()?;

        let mut command = std::process::Command::new(&install.path);
        command
            .arg("-p")
            .args([
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
            ])
            .current_dir(&spec.workspace)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if !spec.model.trim().is_empty() && spec.model != "default" {
            command.args(["--model", spec.model.trim()]);
        }
        if let Some(resume_id) = spec.resume_id.as_deref().filter(|id| !id.is_empty()) {
            command.args(["--resume", resume_id]);
        }
        if let Some(config) = &mcp_config {
            command.arg("--mcp-config").arg(config.path());
            command.arg("--strict-mcp-config");
        }
        if let Some(mode) = permission_mode(&spec.env_overrides, spec.runtime_mode) {
            command.args(["--permission-mode", &mode]);
        }
        if let Some(tool) = spec
            .env_overrides
            .get(ENV_PERMISSION_PROMPT_TOOL)
            .filter(|tool| !tool.trim().is_empty())
        {
            command.args(["--permission-prompt-tool", tool.trim()]);
        }

        for key in SCRUBBED_ENV {
            command.env_remove(key);
        }
        for (key, value) in &spec.env_overrides {
            if !H4RN_ONLY_ENV.contains(&key.as_str()) {
                command.env(key, value);
            }
        }
        command.env("CLAUDE_CODE_ENTRYPOINT", "h4rn");

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // New process group whose id is the child's pid.
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }

        let mut child = tokio::process::Command::from(command)
            .spawn()
            .map_err(|error| HarnessError::Spawn {
                harness: self.kind(),
                message: format!("failed to spawn {}: {error}", install.path.display()),
            })?;
        let pid = child.id().ok_or_else(|| HarnessError::Spawn {
            harness: self.kind(),
            message: "claude exited before it reported a process id".into(),
        })?;

        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            signal_group(pid, GroupSignal::Kill);
            return Err(HarnessError::Spawn {
                harness: self.kind(),
                message: "claude was spawned without piped stdio".into(),
            });
        };

        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (stdin_tx, stdin_rx) = mpsc::channel(STDIN_CHANNEL_CAPACITY);
        let shared = Arc::new(Mutex::new(SharedState::new()));
        let stderr_ring = StderrRing::new();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let interrupt_requested = Arc::new(AtomicBool::new(false));

        spawn_stdout_reader(
            stdout,
            shared.clone(),
            interrupt_requested.clone(),
            events_tx.clone(),
        );
        spawn_stderr_reader(stderr, stderr_ring.clone());
        let writer = spawn_stdin_writer(stdin, stdin_rx, events_tx.clone());
        let supervisor =
            spawn_supervisor(child, stderr_ring.clone(), shutting_down.clone(), events_tx);

        Ok(Box::new(ClaudeCodeSession {
            kind: self.kind(),
            stdin_tx: Some(stdin_tx),
            events_rx: Some(events_rx),
            supervisor: Some(supervisor),
            writer: Some(writer),
            pid,
            shutting_down,
            interrupt_requested,
            stderr: stderr_ring,
            shared,
            mcp_config,
            first_attachments: spec.attachments,
            first_send: true,
            closed: false,
            control_seq: 0,
        }))
    }
}

/// The session returned by [`ClaudeCodeHarness`].
pub struct ClaudeCodeSession {
    kind: HarnessKind,
    stdin_tx: Option<mpsc::Sender<Vec<u8>>>,
    events_rx: Option<mpsc::Receiver<HarnessEvent>>,
    // The tasks are detached on purpose: the supervisor must outlive a dropped
    // session so it can reap the child after `Drop` signals it.
    supervisor: Option<tokio::task::JoinHandle<()>>,
    writer: Option<tokio::task::JoinHandle<()>>,
    pid: u32,
    shutting_down: Arc<AtomicBool>,
    interrupt_requested: Arc<AtomicBool>,
    stderr: StderrRing,
    shared: Arc<Mutex<SharedState>>,
    mcp_config: Option<tempfile::NamedTempFile>,
    first_attachments: Vec<AttachmentRef>,
    first_send: bool,
    closed: bool,
    control_seq: u64,
}

/// State the session exposes from the parser running on the stdout task.
#[derive(Debug)]
struct SharedState {
    session_id: Option<String>,
    model: Option<String>,
    tools: Vec<String>,
    state: AgentState,
    last_turn: Option<TurnReport>,
    touched_paths: Vec<String>,
}

impl SharedState {
    fn new() -> Self {
        Self {
            session_id: None,
            model: None,
            tools: Vec::new(),
            state: AgentState::Idle,
            last_turn: None,
            touched_paths: Vec::new(),
        }
    }
}

impl ClaudeCodeSession {
    /// The harness-native session id, for `--resume` after a daemon restart.
    pub fn session_id(&self) -> Option<String> {
        lock(&self.shared).session_id.clone()
    }

    /// The model reported by the init frame.
    pub fn model(&self) -> Option<String> {
        lock(&self.shared).model.clone()
    }

    /// The tool inventory reported by the init frame.
    pub fn tools(&self) -> Vec<String> {
        lock(&self.shared).tools.clone()
    }

    /// The current agent-state hint derived from tool names.
    pub fn agent_state(&self) -> AgentState {
        lock(&self.shared).state
    }

    /// The most recent completed turn's fine-grained accounting.
    pub fn last_turn(&self) -> Option<TurnReport> {
        lock(&self.shared).last_turn.clone()
    }

    /// Paths touched by `Edit`/`Write`/`MultiEdit` in this session.
    pub fn touched_paths(&self) -> Vec<String> {
        lock(&self.shared).touched_paths.clone()
    }

    /// The tail of the child's stderr, for error surfaces.
    pub fn stderr_tail(&self) -> String {
        self.stderr.snapshot()
    }

    /// The child's process id (also its process-group id on Unix).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    async fn write_frame(&mut self, frame: Vec<u8>) -> Result<(), HarnessError> {
        let Some(sender) = &self.stdin_tx else {
            return Err(HarnessError::Transport {
                harness: self.kind,
                message: "claude stdin is closed".into(),
            });
        };
        // stream-json input is newline-delimited; a frame without a newline is
        // buffered by the CLI and the turn appears to hang.
        sender
            .send(frame)
            .await
            .map_err(|_| HarnessError::Transport {
                harness: self.kind,
                message: "claude writer task has exited".into(),
            })
    }

    fn mark_state(&self, state: AgentState) {
        lock(&self.shared).state = state;
    }

    fn next_request_id(&mut self) -> String {
        self.control_seq = self.control_seq.wrapping_add(1);
        format!("h4rn-{}", self.control_seq)
    }
}

#[async_trait]
impl HarnessSession for ClaudeCodeSession {
    async fn send(&mut self, input: UserInput) -> Result<(), HarnessError> {
        if self.closed || self.shutting_down.load(Ordering::SeqCst) {
            return Err(HarnessError::Transport {
                harness: self.kind,
                message: "session is shut down".into(),
            });
        }

        let mut attachments = input.attachments;
        if self.first_send {
            self.first_send = false;
            attachments.extend(self.first_attachments.iter().cloned());
        }

        let frame = build_user_frame(&input.text, &attachments)?;
        self.write_frame(frame).await?;
        self.interrupt_requested.store(false, Ordering::SeqCst);
        self.mark_state(AgentState::Thinking);
        Ok(())
    }

    async fn interrupt(&mut self) -> Result<(), HarnessError> {
        if self.closed || self.shutting_down.load(Ordering::SeqCst) {
            return Err(HarnessError::Transport {
                harness: self.kind,
                message: "session is shut down".into(),
            });
        }

        self.interrupt_requested.store(true, Ordering::SeqCst);
        let request_id = self.next_request_id();
        let frame = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": { "subtype": "interrupt" }
        });
        let bytes = match encode_frame(&frame) {
            Ok(bytes) => bytes,
            Err(error) => {
                signal_group(self.pid, GroupSignal::Interrupt);
                return Err(error);
            }
        };

        if self.write_frame(bytes).await.is_ok() {
            // The control request is the supported path; verified against
            // Claude Code 2.1.323, which answers with control_response.
            Ok(())
        } else {
            signal_group(self.pid, GroupSignal::Interrupt);
            Ok(())
        }
    }

    async fn respond_approval(
        &mut self,
        _approval_id: &str,
        _decision: ApprovalDecision,
        _option_id: Option<&str>,
    ) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported {
            harness: self.kind,
            feature: format!(
                "answering approvals in-process; configure {ENV_PERMISSION_PROMPT_TOOL} \
                 with an MCP tool on the h4rn hub so the hub owns the pending decision"
            ),
        })
    }

    fn events(&mut self) -> BoxStream<'static, HarnessEvent> {
        match self.events_rx.take() {
            Some(receiver) => ReceiverStream::new(receiver).boxed(),
            None => futures::stream::empty().boxed(),
        }
    }

    fn session_id(&self) -> Option<String> {
        ClaudeCodeSession::session_id(self)
    }

    fn agent_state(&self) -> AgentState {
        ClaudeCodeSession::agent_state(self)
    }

    fn last_turn(&self) -> Option<TurnReport> {
        ClaudeCodeSession::last_turn(self)
    }

    fn touched_paths(&self) -> Vec<String> {
        ClaudeCodeSession::touched_paths(self)
    }

    fn stderr_tail(&self) -> String {
        ClaudeCodeSession::stderr_tail(self)
    }

    async fn shutdown(&mut self) -> Result<(), HarnessError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.shutting_down.store(true, Ordering::SeqCst);

        // Closing the stdin channel ends the writer task, which drops the
        // child's stdin and lets Claude Code exit between turns.
        self.stdin_tx.take();
        signal_group(self.pid, GroupSignal::Terminate);

        if let Some(mut supervisor) = self.supervisor.take() {
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut supervisor).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(target: "h4rn_harness::claude", %error, "claude supervisor task failed");
                }
                Err(_) => {
                    signal_group(self.pid, GroupSignal::Kill);
                    match tokio::time::timeout(KILL_GRACE, &mut supervisor).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(target: "h4rn_harness::claude", %error, "claude supervisor task failed after kill");
                        }
                        Err(_) => supervisor.abort(),
                    }
                }
            }
        }
        if let Some(writer) = self.writer.take() {
            writer.abort();
        }
        // Dropping the temp file removes the MCP config from disk.
        self.mcp_config.take();
        Ok(())
    }
}

impl Drop for ClaudeCodeSession {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        // Best effort: never leave an orphaned claude (ADR-021). The detached
        // supervisor reaps the child, so no zombie remains.
        self.shutting_down.store(true, Ordering::SeqCst);
        signal_group(self.pid, GroupSignal::Kill);
    }
}

fn spawn_stdout_reader(
    stdout: tokio::process::ChildStdout,
    shared: Arc<Mutex<SharedState>>,
    interrupt_requested: Arc<AtomicBool>,
    events_tx: mpsc::Sender<HarnessEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut parser = ClaudeStreamParser::new();
        let mut lines = BufReader::new(stdout).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => {
                    let _ = events_tx
                        .send(HarnessEvent::Error {
                            message: format!("failed reading claude stdout: {error}"),
                            retryable: false,
                        })
                        .await;
                    break;
                }
            };

            let mut events = parser.push_line(&line);
            {
                let mut state = lock(&shared);
                state.session_id = parser.session_id().map(str::to_string);
                state.model = parser.model().map(str::to_string);
                state.tools = parser.tools().to_vec();
                state.state = parser.agent_state();
                state.touched_paths = parser.touched_paths().to_vec();
                state.last_turn = parser.last_turn().cloned();
            }

            let completed = events
                .iter()
                .any(|event| matches!(event, HarnessEvent::TurnCompleted { .. }));
            if completed && interrupt_requested.swap(false, Ordering::SeqCst) {
                for event in &mut events {
                    if let HarnessEvent::TurnCompleted { status, .. } = event {
                        // The CLI reports an interrupted turn as an error
                        // result; the user's intent wins on the wire.
                        *status = TurnStatus::Interrupted;
                    }
                }
            }

            for event in events {
                if events_tx.send(event).await.is_err() {
                    return;
                }
            }
        }
    })
}

fn spawn_stderr_reader(
    stderr: tokio::process::ChildStderr,
    ring: StderrRing,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => ring.push(&line),
                Ok(None) => break,
                Err(error) => {
                    ring.push(&format!("failed reading claude stderr: {error}"));
                    break;
                }
            }
        }
    })
}

fn spawn_stdin_writer(
    mut stdin: tokio::process::ChildStdin,
    mut frames: mpsc::Receiver<Vec<u8>>,
    events_tx: mpsc::Sender<HarnessEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(frame) = frames.recv().await {
            if let Err(error) = stdin.write_all(&frame).await {
                let _ = events_tx
                    .send(HarnessEvent::Error {
                        message: format!("failed writing to claude stdin: {error}"),
                        retryable: false,
                    })
                    .await;
                break;
            }
            if let Err(error) = stdin.flush().await {
                let _ = events_tx
                    .send(HarnessEvent::Error {
                        message: format!("failed flushing claude stdin: {error}"),
                        retryable: false,
                    })
                    .await;
                break;
            }
        }
        // Dropping `stdin` sends EOF, which is how a clean shutdown starts.
    })
}

fn spawn_supervisor(
    mut child: tokio::process::Child,
    stderr: StderrRing,
    shutting_down: Arc<AtomicBool>,
    events_tx: mpsc::Sender<HarnessEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let status = child.wait().await;
        if !shutting_down.load(Ordering::SeqCst) {
            let event = match &status {
                Ok(status) if !status.success() => {
                    let tail = stderr.snapshot();
                    let message = if tail.is_empty() {
                        format!("claude exited with {status}")
                    } else {
                        format!("claude exited with {status}: {tail}")
                    };
                    Some(HarnessEvent::Error {
                        message,
                        retryable: false,
                    })
                }
                Err(error) => Some(HarnessEvent::Error {
                    message: format!("failed to wait for claude: {error}"),
                    retryable: false,
                }),
                _ => None,
            };
            if let Some(event) = event {
                let _ = events_tx.send(event).await;
            }
        }
        // Dropping the last sender ends the event stream.
    })
}

/// Builds the stream-json user frame for one turn.
///
/// Images become native `base64` blocks (with the absolute path also stated in
/// the prompt text, so the model can refer to the file); every other
/// attachment is delivered as a path reference, which is the matrix in
/// ADR-008. An unreadable or oversized image silently degrades to the path
/// reference rather than failing the turn.
pub fn build_user_frame(
    text: &str,
    attachments: &[AttachmentRef],
) -> Result<Vec<u8>, HarnessError> {
    let mut blocks = Vec::new();
    let mut path_lines = Vec::new();
    let mut native_blocks = Vec::new();
    let mut total_image_bytes = 0usize;

    for attachment in attachments {
        let media_type = crate::attachments::media_type(attachment);
        if media_type.starts_with("image/")
            && let Ok(bytes) = std::fs::read(&attachment.path)
            && bytes.len() <= MAX_IMAGE_BYTES
            && total_image_bytes.saturating_add(bytes.len()) <= MAX_TOTAL_IMAGE_BYTES
        {
            total_image_bytes = total_image_bytes.saturating_add(bytes.len());
            native_blocks.push(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": media_type,
                    "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
                }
            }));
        }
        path_lines.push(crate::attachments::path_line(attachment));
    }

    let mut full_text = text.to_string();
    if !path_lines.is_empty() {
        if !full_text.is_empty() {
            full_text.push('\n');
        }
        full_text.push_str(&path_lines.join("\n"));
    }
    if !full_text.is_empty() {
        blocks.push(json!({"type": "text", "text": full_text}));
    }
    blocks.extend(native_blocks);

    let frame = json!({
        "type": "user",
        "message": { "role": "user", "content": blocks }
    });
    encode_frame(&frame)
}

/// Encodes one stream-json frame, including its trailing newline.
fn encode_frame(frame: &serde_json::Value) -> Result<Vec<u8>, HarnessError> {
    let mut bytes = serde_json::to_vec(frame).map_err(|error| HarnessError::Protocol {
        harness: HarnessKind::ClaudeCode,
        message: format!("failed to encode a stream-json frame: {error}"),
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn permission_mode(env: &BTreeMap<String, String>, runtime_mode: RuntimeMode) -> Option<String> {
    let requested = match env.get(ENV_PERMISSION_MODE) {
        Some(requested) => requested,
        None => return Some(mode_for_runtime(runtime_mode).to_string()),
    };
    PERMISSION_MODES
        .iter()
        .find(|mode| mode.eq_ignore_ascii_case(requested.trim()))
        .map(|mode| (*mode).to_string())
}

/// The Claude `--permission-mode` value for a thread's autonomy setting.
fn mode_for_runtime(runtime_mode: RuntimeMode) -> &'static str {
    match runtime_mode {
        RuntimeMode::ApprovalRequired => "default",
        RuntimeMode::AutoAcceptEdits => "acceptEdits",
        RuntimeMode::FullAccess => "bypassPermissions",
    }
}

fn write_mcp_config(
    endpoint: &str,
    token: Option<&str>,
) -> Result<tempfile::NamedTempFile, HarnessError> {
    use std::io::Write as _;

    let mut server = json!({
        "type": "http",
        "url": endpoint,
        "timeout": MCP_TIMEOUT_MS,
    });
    if let Some(token) = token.filter(|token| !token.is_empty()) {
        server["headers"] = json!({ "Authorization": format!("Bearer {token}") });
    }
    let body = json!({ "mcpServers": { "h4rn": server } });

    let mut file = tempfile::Builder::new()
        .prefix("h4rn-claude-mcp-")
        .suffix(".json")
        .tempfile()
        .map_err(|error| HarnessError::Spawn {
            harness: HarnessKind::ClaudeCode,
            message: format!("failed to create the MCP config temp file: {error}"),
        })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| HarnessError::Spawn {
                harness: HarnessKind::ClaudeCode,
                message: format!("failed to restrict the MCP config file: {error}"),
            })?;
    }

    serde_json::to_writer(file.as_file_mut(), &body).map_err(|error| HarnessError::Spawn {
        harness: HarnessKind::ClaudeCode,
        message: format!("failed to write the MCP config: {error}"),
    })?;
    file.as_file_mut()
        .flush()
        .map_err(|error| HarnessError::Spawn {
            harness: HarnessKind::ClaudeCode,
            message: format!("failed to flush the MCP config: {error}"),
        })?;
    Ok(file)
}

/// Detects a `claude` installation on this host.
///
/// Search order: every `PATH` entry, then the common per-user install
/// locations (`~/.local/bin`, `~/.claude/local`, `~/.npm-global/bin`,
/// `~/.bun/bin`, and on Windows `%APPDATA%\npm`). The first candidate that
/// answers `claude --version` wins; a binary that cannot report a version is
/// treated as broken rather than usable.
pub fn detect_claude() -> Option<HarnessInstall> {
    for directory in candidate_dirs() {
        for name in candidate_names() {
            let path = directory.join(name);
            if !path.is_file() {
                continue;
            }
            if let Some(version) = probe_version(&path) {
                return Some(HarnessInstall { path, version });
            }
        }
    }
    None
}

fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = BTreeSet::new();
    if let Some(path) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path) {
            if !entry.as_os_str().is_empty() {
                dirs.insert(entry);
            }
        }
    }

    if let Some(home) = home_dir() {
        for suffix in [
            ".local/bin",
            ".claude/local",
            ".npm-global/bin",
            ".bun/bin",
            "bin",
        ] {
            dirs.insert(home.join(suffix));
        }
    }
    if cfg!(windows) {
        for key in ["APPDATA", "LOCALAPPDATA", "USERPROFILE"] {
            if let Some(base) = std::env::var_os(key) {
                let base = PathBuf::from(base);
                dirs.insert(base.join("npm"));
                dirs.insert(base.join(".local").join("bin"));
            }
        }
    } else {
        for prefix in [
            "/usr/local/bin",
            "/opt/homebrew/bin",
            "/usr/bin",
            "/snap/bin",
        ] {
            dirs.insert(PathBuf::from(prefix));
        }
    }

    dirs.into_iter().collect()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn candidate_names() -> Vec<&'static str> {
    if cfg!(windows) {
        vec!["claude.exe", "claude.cmd", "claude.bat", "claude"]
    } else {
        vec!["claude"]
    }
}

fn probe_version(path: &Path) -> Option<String> {
    let output = version_command(path).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(version) = parse_version(&stdout) {
        return Some(version);
    }
    parse_version(&String::from_utf8_lossy(&output.stderr))
}

fn version_command(path: &Path) -> std::process::Command {
    #[cfg(windows)]
    {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        if matches!(extension.as_deref(), Some("cmd" | "bat")) {
            let mut command = std::process::Command::new("cmd");
            command.arg("/C").arg(path).arg("--version");
            return command;
        }
    }
    let mut command = std::process::Command::new(path);
    command.arg("--version");
    command
}

fn parse_version(text: &str) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?.trim();
    for token in line.split_whitespace() {
        let token = token.trim_start_matches('v');
        if !token.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let version: String = token
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        let version = version.trim_end_matches(['.', '-']);
        if !version.is_empty() {
            return Some(version.to_string());
        }
    }
    None
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use h4rn_protocol::AttachmentId;

    #[test]
    fn parses_the_cli_version_banner() {
        assert_eq!(
            parse_version("2.1.323 (Claude Code)").as_deref(),
            Some("2.1.323")
        );
        assert_eq!(parse_version("v1.2.3\n").as_deref(), Some("1.2.3"));
        assert_eq!(parse_version("\n\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn probes_an_executable_and_reads_its_version() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("claude");
        std::fs::write(&path, "#!/bin/sh\necho '2.1.323 (Claude Code)'\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(probe_version(&path).as_deref(), Some("2.1.323"));
    }

    #[test]
    fn user_frames_use_stream_json_content_blocks() {
        let frame = build_user_frame("hello", &[]).expect("frame");
        let value: serde_json::Value = serde_json::from_slice(&frame).expect("json");
        assert_eq!(value["type"], "user");
        assert_eq!(value["message"]["role"], "user");
        assert_eq!(value["message"]["content"][0]["type"], "text");
        assert_eq!(value["message"]["content"][0]["text"], "hello");
    }

    #[test]
    fn image_attachments_become_native_base64_blocks() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("shot.png");
        std::fs::write(&path, [0x89, b'P', b'N', b'G']).expect("write");
        let attachment = AttachmentRef {
            id: AttachmentId::from_content(&[0x89, b'P', b'N', b'G']),
            path,
            media_type: "image/png".into(),
        };
        let frame = build_user_frame("look", &[attachment]).expect("frame");
        let value: serde_json::Value = serde_json::from_slice(&frame).expect("json");
        let blocks = value["message"]["content"].as_array().expect("blocks");
        assert_eq!(blocks[0]["type"], "text");
        assert!(
            blocks[0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("[Attached image \"shot.png\" is saved at:"))
        );
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "iVBORw==");
    }

    #[test]
    fn non_image_attachments_are_path_references() {
        let attachment = AttachmentRef {
            id: AttachmentId::from_content(b"csv"),
            path: PathBuf::from("/tmp/report.csv"),
            media_type: "text/csv".into(),
        };
        let frame = build_user_frame("summarise", &[attachment]).expect("frame");
        let value: serde_json::Value = serde_json::from_slice(&frame).expect("json");
        let blocks = value["message"]["content"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0]["text"].as_str().is_some_and(|text| {
            text.contains("[Attached file \"report.csv\" is saved at: /tmp/report.csv]")
        }));
    }

    #[test]
    fn permission_modes_are_validated() {
        let mut env = BTreeMap::new();
        env.insert(ENV_PERMISSION_MODE.into(), "acceptedits".into());
        assert_eq!(
            permission_mode(&env, RuntimeMode::ApprovalRequired).as_deref(),
            Some("acceptEdits")
        );
        env.insert(ENV_PERMISSION_MODE.into(), "definitely-not-a-mode".into());
        assert_eq!(permission_mode(&env, RuntimeMode::ApprovalRequired), None);
    }

    #[test]
    fn runtime_modes_map_to_claude_permission_modes() {
        let env = BTreeMap::new();
        assert_eq!(
            permission_mode(&env, RuntimeMode::ApprovalRequired).as_deref(),
            Some("default")
        );
        assert_eq!(
            permission_mode(&env, RuntimeMode::AutoAcceptEdits).as_deref(),
            Some("acceptEdits")
        );
        assert_eq!(
            permission_mode(&env, RuntimeMode::FullAccess).as_deref(),
            Some("bypassPermissions")
        );
    }
}
