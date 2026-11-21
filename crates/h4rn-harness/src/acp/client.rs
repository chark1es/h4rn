//! The ACP transport: spawn, JSON-RPC correlation, scoped `fs/*`, and events.
//!
//! One [`AcpClient`] owns one agent process. Requests are correlated by
//! monotonic JSON-RPC ids; notifications are parsed into [`AcpEvent`]; inbound
//! agent requests are either answered here (`fs/read_text_file`,
//! `fs/write_text_file`) or surfaced to the adapter
//! (`session/request_permission`). Unknown client methods get a JSON-RPC
//! method-not-found error instead of being silently dropped.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::process::{GroupSignal, StderrRing, signal_group, spawn_agent, spawn_stderr_reader};
use super::protocol::{
    ACP_PROTOCOL_VERSION, AcpPermissionRequest, AcpPromptResult, AcpServerInfo, AcpSessionInfo,
    AcpUpdate, AgentCapabilities, JSONRPC_VERSION, PromptContent, parse_session_update,
};
use crate::TransportHealth;

const DEFAULT_EVENT_CAPACITY: usize = 1_024;
const FRAME_CHANNEL_CAPACITY: usize = 256;
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
const SESSION_TIMEOUT: Duration = Duration::from_secs(120);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// How long a prompt may wait for the agent's stdin to accept it.
///
/// The daemon holds the session while `send` runs, so an agent that stops
/// reading must not hold Stop and shutdown forever. On timeout the process is
/// killed, which also guarantees a half-written frame is never delivered. The
/// same deadline bounds every other frame's enqueue (notifications, cancels,
/// mode and model changes, `session/close`), so a full writer queue can never
/// stall a Stop.
const PROMPT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const KILL_GRACE: Duration = Duration::from_secs(2);
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;

/// Environment variables an agent process should not inherit from h4rn.
///
/// Nested agent sessions can otherwise believe they were launched from
/// another agent's terminal. The adapter adds harness-specific entries.
const DEFAULT_SCRUBBED_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_ENTRYPOINT",
];

/// How to spawn one ACP agent.
#[derive(Clone, Debug)]
pub struct AcpClientConfig {
    /// Absolute path to the agent executable.
    pub program: PathBuf,
    /// Arguments passed to the executable.
    pub args: Vec<String>,
    /// Workspace root; `fs/*` requests are confined to this directory.
    pub workspace: PathBuf,
    /// Extra environment variables for the child.
    pub env: BTreeMap<String, String>,
    /// Environment variables removed from the inherited environment.
    pub scrubbed_env: Vec<String>,
    /// Client name reported in `initialize`.
    pub client_name: String,
    /// Client version reported in `initialize`.
    pub client_version: String,
    /// Advertise `fs/read_text_file` support.
    pub fs_read: bool,
    /// Advertise `fs/write_text_file` support.
    pub fs_write: bool,
    /// Capacity of the event channel.
    pub event_capacity: usize,
}

impl AcpClientConfig {
    /// Creates a configuration for `program` rooted at `workspace`.
    pub fn new(program: impl Into<PathBuf>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            workspace: workspace.into(),
            env: BTreeMap::new(),
            scrubbed_env: DEFAULT_SCRUBBED_ENV
                .iter()
                .map(|key| (*key).into())
                .collect(),
            client_name: "h4rn".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            fs_read: true,
            fs_write: true,
            event_capacity: DEFAULT_EVENT_CAPACITY,
        }
    }

    /// Appends one argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends several arguments.
    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets one environment variable.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Adds one environment variable to scrub.
    pub fn scrub(mut self, key: impl Into<String>) -> Self {
        self.scrubbed_env.push(key.into());
        self
    }
}

/// ACP transport or protocol failure.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    /// The agent process could not be spawned.
    #[error("failed to spawn {program}: {message}")]
    Spawn {
        /// Executable that failed.
        program: String,
        /// Underlying failure.
        message: String,
    },
    /// The transport exited or the request channel closed.
    #[error("ACP transport closed: {message}")]
    Closed {
        /// What closed.
        message: String,
    },
    /// A frame did not match the protocol.
    #[error("ACP protocol error: {message}")]
    Protocol {
        /// What was wrong.
        message: String,
    },
    /// The agent answered with a JSON-RPC error.
    #[error("ACP agent error {code}: {message}")]
    Rpc {
        /// JSON-RPC error code.
        code: i64,
        /// Error message.
        message: String,
        /// Optional error data.
        data: Option<Value>,
    },
    /// A request did not receive an answer in time.
    #[error("ACP request `{method}` timed out after {seconds}s")]
    Timeout {
        /// Method that timed out.
        method: String,
        /// Timeout in seconds.
        seconds: u64,
    },
    /// A request was abandoned before it was answered.
    #[error("ACP request `{method}` was cancelled")]
    Cancelled {
        /// Method that was cancelled.
        method: String,
    },
    /// An agent `fs/*` request escaped the workspace.
    #[error("path {path} is outside the session workspace")]
    OutsideWorkspace {
        /// Requested path.
        path: String,
    },
    /// A feature is not supported by this agent or client.
    #[error("ACP does not support {feature}")]
    Unsupported {
        /// Feature that was requested.
        feature: String,
    },
}

/// One normalized event from an ACP client.
#[derive(Clone, Debug, PartialEq)]
pub enum AcpEvent {
    /// A `session/update` notification.
    SessionUpdate {
        /// Session the update belongs to.
        session_id: String,
        /// Parsed update.
        update: AcpUpdate,
    },
    /// The agent is asking the client to approve a tool call.
    PermissionRequest {
        /// JSON-RPC id to echo back with the decision.
        request_id: Value,
        /// Parsed permission request.
        request: AcpPermissionRequest,
    },
    /// A notification this client does not normalize.
    Notification {
        /// Method name.
        method: String,
        /// Raw params.
        params: Value,
    },
    /// The agent's stdout closed.
    Closed {
        /// Human-readable reason.
        message: String,
    },
    /// The agent process exited.
    Exited {
        /// Human-readable exit status.
        status: String,
        /// The tail of stderr at exit.
        stderr: String,
        /// Whether this client killed the agent itself (a timeout, a shutdown,
        /// a crash the daemon caused). A deliberate teardown is not an error to
        /// report on top of the failure that caused it.
        deliberate: bool,
    },
    /// A transport or protocol error.
    Error {
        /// What happened.
        message: String,
        /// Whether a retry is safe.
        retryable: bool,
    },
    /// The write transport died (a writer I/O error, or a teardown).
    ///
    /// Distinct from [`Self::Error`] because the daemon reports this same death
    /// once from the failed prompt write: the two are one row, while an
    /// unrelated diagnostic must never be mistaken for it.
    TransportClosed {
        /// What happened.
        message: String,
        /// Whether a retry is safe.
        retryable: bool,
    },
}

type PendingMap = Mutex<HashMap<String, oneshot::Sender<Result<Value, AcpError>>>>;

struct OutgoingFrame {
    frame: Value,
    /// Signalled once the writer has tried to write the frame to the agent.
    ///
    /// The payload carries the writer's own I/O error when it could not write
    /// or flush the frame, so the caller can name what actually went wrong
    /// instead of a generic "the writer exited". A dropped sender (an aborted
    /// writer) is the same generic case.
    written: Option<oneshot::Sender<Result<(), String>>>,
}

struct StartedCall {
    method: String,
    key: String,
    receiver: oneshot::Receiver<Result<Value, AcpError>>,
    shared: Arc<Shared>,
}

/// A `session/prompt` request that is on the wire and awaiting the agent.
///
/// The reply only resolves when the writer has flushed the frame, so a caller
/// that starts a cancel afterwards can never beat the prompt it cancels.
pub struct PendingPrompt {
    started: StartedCall,
}

impl PendingPrompt {
    /// Waits for the turn result.
    pub async fn wait(self) -> Result<AcpPromptResult, AcpError> {
        let value = self.started.wait(None).await?;
        Ok(AcpPromptResult::from_value(&value))
    }
}

struct Shared {
    config: AcpClientConfig,
    workspace: PathBuf,
    next_id: AtomicU64,
    pending: PendingMap,
    stdin_tx: Mutex<Option<mpsc::Sender<OutgoingFrame>>>,
    events_tx: mpsc::Sender<AcpEvent>,
    events_rx: Mutex<Option<mpsc::Receiver<AcpEvent>>>,
    server: Mutex<Option<AcpServerInfo>>,
    stderr: StderrRing,
    pid: Option<u32>,
    shutting_down: AtomicBool,
    /// Whether the transport is dead, in a form the daemon's event pump can
    /// read without taking this client's session lock. Set as soon as the
    /// agent's stdin or stdout fails, so a pending prompt that fails because of
    /// it can see the transport is dead even before the process is reaped.
    health: TransportHealth,
    writer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// A running ACP agent.
#[derive(Clone)]
pub struct AcpClient {
    shared: Arc<Shared>,
}

impl AcpClient {
    /// Spawns an agent and returns a client that has not handshaken yet.
    ///
    /// Call [`initialize`](Self::initialize) before opening a session. The
    /// adapter uses the two-step form so a failed handshake can fall back to a
    /// different transport without leaving a process behind.
    pub fn spawn(config: AcpClientConfig) -> Result<Self, AcpError> {
        if !config.workspace.is_dir() {
            return Err(AcpError::Spawn {
                program: config.program.display().to_string(),
                message: format!(
                    "workspace {} is not a directory",
                    config.workspace.display()
                ),
            });
        }
        let workspace =
            std::fs::canonicalize(&config.workspace).unwrap_or_else(|_| config.workspace.clone());
        let scrubbed: Vec<&str> = config.scrubbed_env.iter().map(String::as_str).collect();

        let mut child = spawn_agent(
            &config.program,
            &config.args,
            &workspace,
            &config.env,
            &scrubbed,
        )
        .map_err(|error| AcpError::Spawn {
            program: config.program.display().to_string(),
            message: error.to_string(),
        })?;
        let pid = child.id().ok_or_else(|| AcpError::Spawn {
            program: config.program.display().to_string(),
            message: "the agent exited before it reported a process id".into(),
        })?;

        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            signal_group(pid, GroupSignal::Kill);
            return Err(AcpError::Spawn {
                program: config.program.display().to_string(),
                message: "the agent was spawned without piped stdio".into(),
            });
        };

        let stderr_ring = StderrRing::new();
        let stderr_task = spawn_stderr_reader(stderr, stderr_ring.clone());
        // Keep the task alive for the process lifetime; the client owns the
        // process, not the reader handle.
        std::mem::forget(stderr_task);

        let client = Self::start(config, workspace, stdout, stdin, Some(pid), stderr_ring);
        {
            let shared = client.shared.clone();
            let supervisor = tokio::spawn(async move {
                let status = child.wait().await;
                // A flag that was already set when the process was reaped means
                // this client ended it on purpose (a write deadline, a
                // shutdown): that exit is not a second failure to report.
                let deliberate = shared.shutting_down.swap(true, Ordering::SeqCst);
                shared.health.close();
                fail_pending(
                    &shared,
                    AcpError::Closed {
                        message: "the ACP agent exited".into(),
                    },
                );
                let (status_text, stderr_tail) = match status {
                    Ok(status) => (status.to_string(), shared.stderr.snapshot()),
                    Err(error) => (
                        format!("failed to wait for the agent: {error}"),
                        shared.stderr.snapshot(),
                    ),
                };
                let _ = shared
                    .events_tx
                    .send(AcpEvent::Exited {
                        status: status_text,
                        stderr: stderr_tail,
                        deliberate,
                    })
                    .await;
            });
            *lock(&client.shared.supervisor) = Some(supervisor);
        }
        Ok(client)
    }

    /// Builds a client over an arbitrary reader/writer pair.
    ///
    /// This is what the in-process fake agent uses: a duplex pipe pair behaves
    /// exactly like the child's stdin/stdout, without spawning a process.
    fn start<W, R>(
        config: AcpClientConfig,
        workspace: PathBuf,
        stdout: R,
        stdin: W,
        pid: Option<u32>,
        stderr: StderrRing,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (events_tx, events_rx) = mpsc::channel(config.event_capacity.max(16));
        let (stdin_tx, stdin_rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let shared = Arc::new(Shared {
            config,
            workspace,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            stdin_tx: Mutex::new(Some(stdin_tx)),
            events_tx,
            events_rx: Mutex::new(Some(events_rx)),
            server: Mutex::new(None),
            stderr,
            pid,
            shutting_down: AtomicBool::new(false),
            health: TransportHealth::new(),
            writer: Mutex::new(None),
            reader: Mutex::new(None),
            supervisor: Mutex::new(None),
        });

        let writer = spawn_writer(stdin, stdin_rx, shared.clone());
        let reader = spawn_reader(stdout, shared.clone());
        *lock(&shared.writer) = Some(writer);
        *lock(&shared.reader) = Some(reader);

        Self { shared }
    }

    /// Builds a client over a test's pipe pair without spawning a process.
    #[cfg(test)]
    pub(crate) fn start_for_test<R, W>(config: AcpClientConfig, stdout: R, stdin: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let workspace = config.workspace.clone();
        Self::start(config, workspace, stdout, stdin, None, StderrRing::new())
    }

    /// Performs the `initialize` handshake and returns the agent's reply.
    pub async fn initialize(&self) -> Result<AcpServerInfo, AcpError> {
        let params = json!({
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": {
                    "readTextFile": self.shared.config.fs_read,
                    "writeTextFile": self.shared.config.fs_write,
                },
                "terminal": false,
            },
            "clientInfo": {
                "name": self.shared.config.client_name,
                "version": self.shared.config.client_version,
            },
        });
        let result = self
            .call("initialize", params, Some(INITIALIZE_TIMEOUT))
            .await?;
        let info = AcpServerInfo::from_value(&result);
        *lock(&self.shared.server) = Some(info.clone());
        Ok(info)
    }

    /// The handshake reply, once [`initialize`](Self::initialize) succeeded.
    pub fn server_info(&self) -> Option<AcpServerInfo> {
        lock(&self.shared.server).clone()
    }

    /// The negotiated capability block.
    pub fn capabilities(&self) -> AgentCapabilities {
        self.server_info()
            .map(|info| info.capabilities)
            .unwrap_or_default()
    }

    /// Takes the event receiver. A client has exactly one consumer.
    pub fn take_events(&self) -> Option<mpsc::Receiver<AcpEvent>> {
        lock(&self.shared.events_rx).take()
    }

    /// Opens a new session and returns its models, modes and id.
    pub async fn new_session(
        &self,
        cwd: &Path,
        mcp_servers: &[super::protocol::AcpMcpServer],
    ) -> Result<AcpSessionInfo, AcpError> {
        let params = json!({
            "cwd": cwd.display().to_string(),
            "mcpServers": mcp_servers.iter().map(|server| server.to_json()).collect::<Vec<_>>(),
        });
        let result = self
            .call("session/new", params, Some(SESSION_TIMEOUT))
            .await?;
        let info = AcpSessionInfo::from_value(&result, "");
        if info.session_id.is_empty() {
            return Err(AcpError::Protocol {
                message: "session/new returned no sessionId".into(),
            });
        }
        Ok(info)
    }

    /// Loads an existing session (`session/load`).
    pub async fn load_session(
        &self,
        session_id: &str,
        cwd: &Path,
        mcp_servers: &[super::protocol::AcpMcpServer],
    ) -> Result<AcpSessionInfo, AcpError> {
        let params = json!({
            "sessionId": session_id,
            "cwd": cwd.display().to_string(),
            "mcpServers": mcp_servers.iter().map(|server| server.to_json()).collect::<Vec<_>>(),
        });
        let result = self
            .call("session/load", params, Some(SESSION_TIMEOUT))
            .await?;
        Ok(AcpSessionInfo::from_value(&result, session_id))
    }

    /// Sends one prompt and waits for the agent's turn result.
    pub async fn prompt(
        &self,
        session_id: &str,
        content: &[PromptContent],
    ) -> Result<AcpPromptResult, AcpError> {
        self.start_prompt(session_id, content).await?.wait().await
    }

    /// Writes one prompt and returns a handle that waits for the turn result.
    ///
    /// This function only resolves after the agent's stdin has flushed the
    /// `session/prompt` frame, which is what makes a later
    /// [`cancel`](Self::cancel) ordered after the prompt it cancels.
    pub async fn start_prompt(
        &self,
        session_id: &str,
        content: &[PromptContent],
    ) -> Result<PendingPrompt, AcpError> {
        let params = json!({
            "sessionId": session_id,
            "prompt": content.iter().map(PromptContent::to_json).collect::<Vec<_>>(),
        });
        let started = self.begin_call("session/prompt", params, true).await?;
        Ok(PendingPrompt { started })
    }

    /// Asks the agent to cancel the running turn (`session/cancel`).
    pub async fn cancel(&self, session_id: &str) -> Result<(), AcpError> {
        self.notify("session/cancel", json!({"sessionId": session_id}))
            .await
    }

    /// Switches the session model (`session/set_model`).
    pub async fn set_model(&self, session_id: &str, model_id: &str) -> Result<(), AcpError> {
        self.call(
            "session/set_model",
            json!({"sessionId": session_id, "modelId": model_id}),
            Some(SESSION_TIMEOUT),
        )
        .await
        .map(|_| ())
    }

    /// Switches the session mode (`session/set_mode`).
    pub async fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<(), AcpError> {
        self.call(
            "session/set_mode",
            json!({"sessionId": session_id, "modeId": mode_id}),
            Some(SESSION_TIMEOUT),
        )
        .await
        .map(|_| ())
    }

    /// Closes the session (`session/close`).
    pub async fn close_session(&self, session_id: &str) -> Result<(), AcpError> {
        self.call(
            "session/close",
            json!({"sessionId": session_id}),
            Some(Duration::from_secs(5)),
        )
        .await
        .map(|_| ())
    }

    /// Answers a `session/request_permission` request.
    ///
    /// `option_id` is the agent's own option id (`allow`, `deny`, ...). Passing
    /// `None` sends the ACP `cancelled` outcome.
    pub async fn respond_permission(
        &self,
        request_id: &Value,
        option_id: Option<&str>,
    ) -> Result<(), AcpError> {
        let result = match option_id {
            Some(option_id) => json!({
                "outcome": {"outcome": "selected", "optionId": option_id}
            }),
            None => json!({"outcome": {"outcome": "cancelled"}}),
        };
        self.send_frame(
            json!({
                "jsonrpc": JSONRPC_VERSION,
                "id": request_id,
                "result": result,
            }),
            "session/request_permission",
        )
        .await
    }

    /// The child's process id, when this client owns a process.
    pub fn pid(&self) -> Option<u32> {
        self.shared.pid
    }

    /// The tail of the agent's stderr.
    pub fn stderr_tail(&self) -> String {
        self.shared.stderr.snapshot()
    }

    /// Whether the transport has closed.
    pub fn is_closed(&self) -> bool {
        self.shared.health.is_closed()
    }

    /// A lock-free view of the transport's health, for readers that must never
    /// wait on the client's session lock (the daemon's event pump).
    pub fn transport_health(&self) -> TransportHealth {
        self.shared.health.clone()
    }

    /// Kills the process group immediately. Safe to call from `Drop`.
    pub fn kill(&self) {
        self.shared.shutting_down.store(true, Ordering::SeqCst);
        self.shared.health.close();
        if let Some(pid) = self.shared.pid {
            signal_group(pid, GroupSignal::Kill);
        }
    }

    /// Stops the agent: close stdin, ask it to exit, then kill the group.
    pub async fn shutdown(&self) -> Result<(), AcpError> {
        if self.shared.shutting_down.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.shared.health.close();
        // Closing the frame channel ends the writer task, which drops the
        // child's stdin and lets a cooperative agent exit between turns.
        *lock(&self.shared.stdin_tx) = None;
        if let Some(pid) = self.shared.pid {
            signal_group(pid, GroupSignal::Terminate);
        }

        let supervisor = lock(&self.shared.supervisor).take();
        if let Some(mut supervisor) = supervisor {
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut supervisor).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(target: "h4rn_harness::acp", %error, "ACP supervisor task failed");
                }
                Err(_) => {
                    if let Some(pid) = self.shared.pid {
                        signal_group(pid, GroupSignal::Kill);
                    }
                    match tokio::time::timeout(KILL_GRACE, &mut supervisor).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(target: "h4rn_harness::acp", %error, "ACP supervisor task failed after kill");
                        }
                        Err(_) => supervisor.abort(),
                    }
                }
            }
        }
        if let Some(writer) = lock(&self.shared.writer).take() {
            writer.abort();
        }
        if let Some(reader) = lock(&self.shared.reader).take() {
            reader.abort();
        }
        fail_pending(
            &self.shared,
            AcpError::Closed {
                message: "the ACP client was shut down".into(),
            },
        );
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), AcpError> {
        if self.is_closed() {
            return Err(AcpError::Closed {
                message: "the ACP client is shut down".into(),
            });
        }
        self.send_frame(
            json!({
                "jsonrpc": JSONRPC_VERSION,
                "method": method,
                "params": params,
            }),
            method,
        )
        .await
    }

    async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, AcpError> {
        let started = self.begin_call(method, params, false).await?;
        started.wait(timeout).await
    }

    /// Registers a request and hands it to the writer.
    ///
    /// When `track_write` is set, this does not return until the writer has
    /// flushed the frame to the agent.
    async fn begin_call(
        &self,
        method: &str,
        params: Value,
        track_write: bool,
    ) -> Result<StartedCall, AcpError> {
        if self.is_closed() {
            return Err(AcpError::Closed {
                message: "the ACP client is shut down".into(),
            });
        }
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let key = id_key(&json!(id));
        let (sender, receiver) = oneshot::channel();
        lock(&self.shared.pending).insert(key.clone(), sender);

        let frame = json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": id,
            "method": method,
            "params": params,
        });
        let deadline = tokio::time::Instant::now() + PROMPT_WRITE_TIMEOUT;
        let written = if track_write {
            let (written_tx, written_rx) = oneshot::channel();
            // Enqueueing and flushing share one deadline. A full writer queue
            // must not postpone the clock, or an agent that stopped reading
            // stdin could hold the daemon for as long as the queue drains.
            match tokio::time::timeout_at(
                deadline,
                self.send_frame_tracked(frame, written_tx, method),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    lock(&self.shared.pending).remove(&key);
                    return Err(error);
                }
                Err(_) => {
                    lock(&self.shared.pending).remove(&key);
                    // The frame never reached the writer; tear the transport
                    // down so a later send cannot slip it out.
                    teardown_transport(
                        &self.shared,
                        "the ACP agent stopped accepting frames".into(),
                    );
                    return Err(AcpError::Timeout {
                        method: method.to_string(),
                        seconds: PROMPT_WRITE_TIMEOUT.as_secs(),
                    });
                }
            }
            Some(written_rx)
        } else {
            if let Err(error) = self.send_frame(frame, method).await {
                lock(&self.shared.pending).remove(&key);
                return Err(error);
            }
            None
        };
        if let Some(written) = written {
            match tokio::time::timeout_at(deadline, written).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(detail))) => {
                    lock(&self.shared.pending).remove(&key);
                    // The writer saw a real I/O failure on this frame. Keep
                    // what it said: the daemon's one failure row is built from
                    // this error, and "the writer exited" alone would lose the
                    // reason the agent's stdin is gone.
                    let message =
                        format!("the ACP writer failed before the request was written: {detail}");
                    teardown_transport(&self.shared, message.clone());
                    return Err(AcpError::Closed { message });
                }
                Ok(Err(_)) => {
                    lock(&self.shared.pending).remove(&key);
                    // The writer that held this frame is gone without writing
                    // it, so no reply can ever come: the transport is dead even
                    // if the reader is still open. Tear it down before waking
                    // anyone, so a later send cannot find an open client.
                    teardown_transport(
                        &self.shared,
                        "the ACP writer exited before the request was written".into(),
                    );
                    return Err(AcpError::Closed {
                        message: "the ACP writer exited before the request was written".into(),
                    });
                }
                Err(_) => {
                    lock(&self.shared.pending).remove(&key);
                    // The agent stopped reading stdin mid-frame. Tear the
                    // transport down so the blocked frame can never be written
                    // later, and kill the process it was talking to.
                    teardown_transport(&self.shared, "the ACP agent stopped reading frames".into());
                    return Err(AcpError::Timeout {
                        method: method.to_string(),
                        seconds: PROMPT_WRITE_TIMEOUT.as_secs(),
                    });
                }
            }
        }

        Ok(StartedCall {
            method: method.to_string(),
            key,
            receiver,
            shared: self.shared.clone(),
        })
    }

    /// Enqueues one frame, bounded by the write deadline.
    ///
    /// Every frame goes through here, not only prompts: a full writer queue
    /// must never hold a Stop (a cancel), a mode change or a shutdown forever.
    /// On expiry the writer is aborted so the blocked frame cannot be written
    /// later, and the agent is killed like a timed-out prompt.
    async fn send_frame(&self, frame: Value, method: &str) -> Result<(), AcpError> {
        self.send_outgoing(
            OutgoingFrame {
                frame,
                written: None,
            },
            method,
        )
        .await
    }

    async fn send_frame_tracked(
        &self,
        frame: Value,
        written: oneshot::Sender<Result<(), String>>,
        method: &str,
    ) -> Result<(), AcpError> {
        self.send_outgoing(
            OutgoingFrame {
                frame,
                written: Some(written),
            },
            method,
        )
        .await
    }

    async fn send_outgoing(&self, outgoing: OutgoingFrame, method: &str) -> Result<(), AcpError> {
        let sender = lock(&self.shared.stdin_tx).clone();
        let Some(sender) = sender else {
            return Err(AcpError::Closed {
                message: "the ACP agent stdin is closed".into(),
            });
        };
        match tokio::time::timeout(PROMPT_WRITE_TIMEOUT, sender.send(outgoing)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(AcpError::Closed {
                message: "the ACP writer task has exited".into(),
            }),
            Err(_) => {
                teardown_transport(&self.shared, "the ACP agent stopped reading frames".into());
                Err(AcpError::Timeout {
                    method: method.to_string(),
                    seconds: PROMPT_WRITE_TIMEOUT.as_secs(),
                })
            }
        }
    }
}

impl StartedCall {
    async fn wait(self, timeout: Option<Duration>) -> Result<Value, AcpError> {
        let outcome = match timeout {
            Some(timeout) => match tokio::time::timeout(timeout, self.receiver).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    lock(&self.shared.pending).remove(&self.key);
                    return Err(AcpError::Timeout {
                        method: self.method,
                        seconds: timeout.as_secs(),
                    });
                }
            },
            None => self.receiver.await,
        };
        match outcome {
            Ok(result) => result,
            Err(_) => Err(AcpError::Cancelled {
                method: self.method,
            }),
        }
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        // The shared state may still be held by the reader/writer tasks, so a
        // Drop on the last clone is not guaranteed to run here. The adapter
        // also calls `kill` explicitly; this is a best-effort backstop.
    }
}

fn spawn_writer<W>(
    mut stdin: W,
    mut frames: mpsc::Receiver<OutgoingFrame>,
    shared: Arc<Shared>,
) -> tokio::task::JoinHandle<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(outgoing) = frames.recv().await {
            let OutgoingFrame { frame, written } = outgoing;
            let mut bytes = match serde_json::to_vec(&frame) {
                Ok(bytes) => bytes,
                Err(error) => {
                    let _ = shared
                        .events_tx
                        .send(AcpEvent::Error {
                            message: format!("failed to encode an ACP frame: {error}"),
                            retryable: false,
                        })
                        .await;
                    continue;
                }
            };
            bytes.push(b'\n');
            if let Err(error) = stdin.write_all(&bytes).await {
                let detail = error.to_string();
                // Mark the transport closed and stop the process group before
                // waking any caller: a prompt that observes this failure must
                // never find an open client or a live agent behind it.
                close_transport(&shared);
                fail_pending(
                    &shared,
                    AcpError::Closed {
                        message: format!("the ACP agent stdin stopped accepting frames: {detail}"),
                    },
                );
                let _ = shared
                    .events_tx
                    .send(AcpEvent::TransportClosed {
                        message: format!("failed to write to the ACP agent: {detail}"),
                        retryable: false,
                    })
                    .await;
                // The caller's one failure row is built from this: name the
                // writer's own I/O error, not just that the writer left.
                if let Some(written) = written {
                    let _ = written.send(Err(detail));
                }
                break;
            }
            if let Err(error) = stdin.flush().await {
                let detail = error.to_string();
                close_transport(&shared);
                fail_pending(
                    &shared,
                    AcpError::Closed {
                        message: format!("the ACP agent stdin stopped accepting frames: {detail}"),
                    },
                );
                let _ = shared
                    .events_tx
                    .send(AcpEvent::TransportClosed {
                        message: format!("failed to flush the ACP agent stdin: {detail}"),
                        retryable: false,
                    })
                    .await;
                if let Some(written) = written {
                    let _ = written.send(Err(detail));
                }
                break;
            }
            if let Some(written) = written {
                let _ = written.send(Ok(()));
            }
        }
        // Dropping `stdin` sends EOF.
    })
}

fn spawn_reader<R>(stdout: R, shared: Arc<Shared>) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => {
                    let _ = shared
                        .events_tx
                        .send(AcpEvent::TransportClosed {
                            message: format!("failed to read the ACP agent stdout: {error}"),
                            retryable: false,
                        })
                        .await;
                    break;
                }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let frame: Value = match serde_json::from_str(line) {
                Ok(frame) => frame,
                Err(error) => {
                    let _ = shared
                        .events_tx
                        .send(AcpEvent::Error {
                            message: format!(
                                "unparseable ACP frame ({error}): {}",
                                truncate(line, 320)
                            ),
                            retryable: false,
                        })
                        .await;
                    continue;
                }
            };

            if let Some(method) = frame.get("method").and_then(Value::as_str) {
                let method = method.to_string();
                let params = frame.get("params").cloned().unwrap_or(Value::Null);
                match frame.get("id").cloned() {
                    Some(id) => handle_inbound(&shared, id, &method, params).await,
                    None => handle_notification(&shared, &method, params).await,
                }
            } else if let Some(id) = frame.get("id") {
                resolve_response(&shared, id.clone(), &frame);
            } else {
                let _ = shared
                    .events_tx
                    .send(AcpEvent::Error {
                        message: "ACP frame had neither method nor id".into(),
                        retryable: false,
                    })
                    .await;
            }
        }

        // The agent's stdout is gone: no reply can ever come back. Mark the
        // transport dead before failing the pending calls, so a prompt that
        // fails because of this can see why.
        shared.health.close();
        fail_pending(
            &shared,
            AcpError::Closed {
                message: "the ACP agent stdout closed".into(),
            },
        );
        if !shared.shutting_down.load(Ordering::SeqCst) {
            let _ = shared
                .events_tx
                .send(AcpEvent::Closed {
                    message: "the ACP agent stdout closed".into(),
                })
                .await;
        }
    })
}

async fn handle_notification(shared: &Arc<Shared>, method: &str, params: Value) {
    if method == "session/update" {
        if let Some((session_id, update)) = parse_session_update(&params) {
            let _ = shared
                .events_tx
                .send(AcpEvent::SessionUpdate { session_id, update })
                .await;
            return;
        }
    }
    let _ = shared
        .events_tx
        .send(AcpEvent::Notification {
            method: method.to_string(),
            params,
        })
        .await;
}

async fn handle_inbound(shared: &Arc<Shared>, id: Value, method: &str, params: Value) {
    match method {
        "fs/read_text_file" => {
            let result = read_text_file(shared, &params);
            match result {
                Ok(content) => {
                    send_response(shared, id, json!({"content": content})).await;
                }
                Err(error) => {
                    send_rpc_error(shared, id, -32_000, &error.to_string()).await;
                }
            }
        }
        "fs/write_text_file" => {
            let result = write_text_file(shared, &params);
            match result {
                Ok(()) => send_response(shared, id, json!({})).await,
                Err(error) => {
                    send_rpc_error(shared, id, -32_000, &error.to_string()).await;
                }
            }
        }
        "session/request_permission" => {
            let request = AcpPermissionRequest::from_value(&params);
            let _ = shared
                .events_tx
                .send(AcpEvent::PermissionRequest {
                    request_id: id,
                    request,
                })
                .await;
        }
        "terminal/create"
        | "terminal/output"
        | "terminal/wait_for_exit"
        | "terminal/kill"
        | "terminal/release" => {
            send_rpc_error(
                shared,
                id,
                -32_601,
                "this client did not advertise terminal support",
            )
            .await;
        }
        other => {
            send_rpc_error(shared, id, -32_601, &format!("method not found: {other}")).await;
        }
    }
}

async fn send_response(shared: &Arc<Shared>, id: Value, result: Value) {
    let _ = send_from_reader(
        shared,
        json!({"jsonrpc": JSONRPC_VERSION, "id": id, "result": result}),
    )
    .await;
}

async fn send_rpc_error(shared: &Arc<Shared>, id: Value, code: i64, message: &str) {
    let _ = send_from_reader(
        shared,
        json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": id,
            "error": {"code": code, "message": message},
        }),
    )
    .await;
}

/// Enqueues a frame the reader produced (`fs/*` replies, permission answers).
///
/// The reply goes through the same bounded frame queue as every other frame:
/// a full writer queue must not park the reader forever, because the reader is
/// also what notices the agent's stdout closing. On expiry the transport is
/// torn down like a timed-out prompt, so a half-written reply is never sent.
async fn send_from_reader(shared: &Arc<Shared>, frame: Value) -> Result<(), AcpError> {
    let sender = lock(&shared.stdin_tx).clone();
    let Some(sender) = sender else {
        return Err(AcpError::Closed {
            message: "the ACP agent stdin is closed".into(),
        });
    };
    match tokio::time::timeout(
        PROMPT_WRITE_TIMEOUT,
        sender.send(OutgoingFrame {
            frame,
            written: None,
        }),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(AcpError::Closed {
            message: "the ACP writer task has exited".into(),
        }),
        Err(_) => {
            teardown_transport(
                shared,
                "the ACP agent stopped accepting frames from the client".into(),
            );
            Err(AcpError::Timeout {
                method: "client reply".into(),
                seconds: PROMPT_WRITE_TIMEOUT.as_secs(),
            })
        }
    }
}

/// Marks a dead transport closed and kills the agent's process group.
///
/// Splitting this from [`fail_pending`] lets a caller publish the closed state
/// and the process cleanup before any waiting call can observe its error. The
/// writer task calls it on itself, so it never aborts the writer here.
fn close_transport(shared: &Arc<Shared>) {
    shared.shutting_down.store(true, Ordering::SeqCst);
    shared.health.close();
    *lock(&shared.stdin_tx) = None;
    if let Some(pid) = shared.pid {
        signal_group(pid, GroupSignal::Kill);
    }
}

/// Tears a stuck transport down: no later frame may reach the agent, and the
/// process is killed so nothing keeps talking to a pipe nobody drains.
fn teardown_transport(shared: &Arc<Shared>, message: String) {
    close_transport(shared);
    if let Some(writer) = lock(&shared.writer).take() {
        writer.abort();
    }
    fail_pending(
        shared,
        AcpError::Closed {
            message: message.clone(),
        },
    );
    if let Err(error) = shared.events_tx.try_send(AcpEvent::TransportClosed {
        message,
        retryable: false,
    }) {
        // A full event queue already has plenty for the daemon to act on; the
        // closed transport is visible through `is_closed` either way.
        tracing::debug!(
            target: "h4rn_harness::acp",
            %error,
            "could not report the torn-down transport"
        );
    }
}

fn resolve_response(shared: &Arc<Shared>, id: Value, frame: &Value) {
    let key = id_key(&id);
    let Some(sender) = lock(&shared.pending).remove(&key) else {
        return;
    };
    let outcome = if let Some(error) = frame.get("error") {
        Err(AcpError::Rpc {
            code: error.get("code").and_then(Value::as_i64).unwrap_or(-32_603),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown ACP error")
                .to_string(),
            data: error.get("data").cloned(),
        })
    } else {
        Ok(frame.get("result").cloned().unwrap_or(Value::Null))
    };
    let _ = sender.send(outcome);
}

fn fail_pending(shared: &Arc<Shared>, error: AcpError) {
    let pending: Vec<_> = lock(&shared.pending).drain().map(|(_, tx)| tx).collect();
    for sender in pending {
        let _ = sender.send(Err(clone_error(&error)));
    }
}

fn clone_error(error: &AcpError) -> AcpError {
    match error {
        AcpError::Spawn { program, message } => AcpError::Spawn {
            program: program.clone(),
            message: message.clone(),
        },
        AcpError::Closed { message } => AcpError::Closed {
            message: message.clone(),
        },
        AcpError::Protocol { message } => AcpError::Protocol {
            message: message.clone(),
        },
        AcpError::Rpc {
            code,
            message,
            data,
        } => AcpError::Rpc {
            code: *code,
            message: message.clone(),
            data: data.clone(),
        },
        AcpError::Timeout { method, seconds } => AcpError::Timeout {
            method: method.clone(),
            seconds: *seconds,
        },
        AcpError::Cancelled { method } => AcpError::Cancelled {
            method: method.clone(),
        },
        AcpError::OutsideWorkspace { path } => AcpError::OutsideWorkspace { path: path.clone() },
        AcpError::Unsupported { feature } => AcpError::Unsupported {
            feature: feature.clone(),
        },
    }
}

fn id_key(id: &Value) -> String {
    match id {
        Value::String(text) => format!("s:{text}"),
        other => format!("n:{other}"),
    }
}

fn read_text_file(shared: &Arc<Shared>, params: &Value) -> Result<String, AcpError> {
    if !shared.config.fs_read {
        return Err(AcpError::Unsupported {
            feature: "fs/read_text_file".into(),
        });
    }
    let requested =
        params
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| AcpError::Protocol {
                message: "fs/read_text_file requires a path".into(),
            })?;
    let path = scoped_path(&shared.workspace, requested)?;
    let metadata = std::fs::metadata(&path).map_err(|error| AcpError::Protocol {
        message: format!("failed to stat {}: {error}", path.display()),
    })?;
    if metadata.len() > MAX_READ_BYTES {
        return Err(AcpError::Protocol {
            message: format!(
                "{} is {} bytes, above the {} byte read cap",
                path.display(),
                metadata.len(),
                MAX_READ_BYTES
            ),
        });
    }
    let text = std::fs::read_to_string(&path).map_err(|error| AcpError::Protocol {
        message: format!("failed to read {}: {error}", path.display()),
    })?;

    let line = params.get("line").and_then(Value::as_u64).unwrap_or(0);
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(0);
    if line == 0 && limit == 0 {
        return Ok(text);
    }
    let start = line.saturating_sub(1) as usize;
    let selected = text
        .lines()
        .skip(start)
        .take(if limit == 0 {
            usize::MAX
        } else {
            limit as usize
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(selected)
}

fn write_text_file(shared: &Arc<Shared>, params: &Value) -> Result<(), AcpError> {
    if !shared.config.fs_write {
        return Err(AcpError::Unsupported {
            feature: "fs/write_text_file".into(),
        });
    }
    let requested =
        params
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| AcpError::Protocol {
                message: "fs/write_text_file requires a path".into(),
            })?;
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| AcpError::Protocol {
            message: "fs/write_text_file requires content".into(),
        })?;
    let path = scoped_path(&shared.workspace, requested)?;
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent).map_err(|error| AcpError::Protocol {
            message: format!("failed to create {}: {error}", parent.display()),
        })?;
    }
    std::fs::write(&path, content).map_err(|error| AcpError::Protocol {
        message: format!("failed to write {}: {error}", path.display()),
    })
}

/// Confines an agent-requested path to the session workspace.
fn scoped_path(workspace: &Path, requested: &str) -> Result<PathBuf, AcpError> {
    let requested_path = Path::new(requested);
    let candidate = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        workspace.join(requested_path)
    };
    let normalized = normalize_path(&candidate);
    if !normalized.starts_with(workspace) {
        return Err(AcpError::OutsideWorkspace {
            path: requested.to_string(),
        });
    }

    // Resolve symlinks when the target exists; otherwise normalize the parent
    // so a symlinked directory cannot be used to escape the workspace.
    if normalized.exists() {
        let resolved = std::fs::canonicalize(&normalized).map_err(|error| AcpError::Protocol {
            message: format!("failed to resolve {}: {error}", normalized.display()),
        })?;
        if !resolved.starts_with(workspace) {
            return Err(AcpError::OutsideWorkspace {
                path: requested.to_string(),
            });
        }
        return Ok(resolved);
    }
    if let Some(parent) = normalized.parent()
        && let Ok(canonical_parent) = std::fs::canonicalize(parent)
        && !canonical_parent.starts_with(workspace)
    {
        return Err(AcpError::OutsideWorkspace {
            path: requested.to_string(),
        });
    }
    Ok(normalized)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            Component::RootDir | Component::Prefix(_) => output.push(component.as_os_str()),
            Component::Normal(part) => output.push(part),
        }
    }
    output
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::DuplexStream;

    struct FakeAgent {
        reader: BufReader<DuplexStream>,
        writer: DuplexStream,
    }

    impl FakeAgent {
        async fn next(&mut self) -> Value {
            let mut line = String::new();
            self.reader
                .read_line(&mut line)
                .await
                .expect("read a frame");
            serde_json::from_str(line.trim()).expect("valid json from the client")
        }

        async fn send(&mut self, frame: Value) {
            let mut bytes = serde_json::to_vec(&frame).expect("encode");
            bytes.push(b'\n');
            self.writer.write_all(&bytes).await.expect("write");
            self.writer.flush().await.expect("flush");
        }

        async fn reply(&mut self, id: Value, result: Value) {
            self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
                .await;
        }

        async fn error(&mut self, id: Value, code: i64, message: &str) {
            self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": code, "message": message}
            }))
            .await;
        }

        async fn notify(&mut self, method: &str, params: Value) {
            self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
                .await;
        }

        async fn request(&mut self, id: Value, method: &str, params: Value) {
            self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params
            }))
            .await;
        }
    }

    fn fake_pair(config: AcpClientConfig) -> (AcpClient, FakeAgent) {
        let (client_write, agent_read) = tokio::io::duplex(64 * 1024);
        let (agent_write, client_read) = tokio::io::duplex(64 * 1024);
        let client = AcpClient::start(
            config,
            std::env::temp_dir(),
            client_read,
            client_write,
            None,
            StderrRing::new(),
        );
        (
            client,
            FakeAgent {
                reader: BufReader::new(agent_read),
                writer: agent_write,
            },
        )
    }

    fn config(workspace: &Path) -> AcpClientConfig {
        AcpClientConfig::new(PathBuf::from("/usr/local/bin/fake-acp"), workspace)
    }

    #[tokio::test]
    async fn initialize_reads_the_negotiated_version_and_capabilities() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut agent) = fake_pair(config(workspace.path()));
        let agent_task = tokio::spawn(async move {
            let request = agent.next().await;
            assert_eq!(request["method"], "initialize");
            assert_eq!(request["params"]["protocolVersion"], ACP_PROTOCOL_VERSION);
            assert_eq!(
                request["params"]["clientCapabilities"]["fs"]["readTextFile"],
                true
            );
            agent
                .reply(
                    request["id"].clone(),
                    json!({
                        "protocolVersion": 1,
                        "agentInfo": {"name": "Fake ACP", "version": "9.9.9"},
                        "agentCapabilities": {
                            "loadSession": true,
                            "promptCapabilities": {"image": true, "embeddedContext": true},
                            "sessionCapabilities": {"load": {}},
                        },
                        "authMethods": [{"id": "oauth", "name": "Log in"}],
                    }),
                )
                .await;
        });

        let info = client.initialize().await.expect("initialize");
        agent_task.await.expect("agent");
        assert_eq!(info.protocol_version, 1);
        assert_eq!(info.agent_name, "Fake ACP");
        assert!(info.capabilities.load_session);
        assert!(info.capabilities.image);
        assert!(info.capabilities.embedded_context);
        assert_eq!(info.auth_methods[0].id, "oauth");
        assert_eq!(
            client.server_info().expect("server info").agent_version,
            "9.9.9"
        );

        client.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn session_prompt_streams_updates_and_reports_usage() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut agent) = fake_pair(config(workspace.path()));
        let events = client.take_events().expect("events");

        let agent_task = tokio::spawn(async move {
            let request = agent.next().await;
            assert_eq!(request["method"], "initialize");
            agent
                .reply(
                    request["id"].clone(),
                    json!({"protocolVersion": 1, "agentInfo": {"name": "Fake"}}),
                )
                .await;

            let request = agent.next().await;
            assert_eq!(request["method"], "session/new");
            assert_eq!(request["params"]["mcpServers"][0]["type"], "http");
            agent
                .reply(
                    request["id"].clone(),
                    json!({
                        "sessionId": "s-1",
                        "models": {
                            "availableModels": [{"modelId": "gemini-4-argon-medium", "name": "Argon"}],
                            "currentModelId": "gemini-4-argon-medium"
                        },
                        "modes": {
                            "currentModeId": "default",
                            "availableModes": [{"id": "default", "name": "Default"}]
                        }
                    }),
                )
                .await;

            let request = agent.next().await;
            assert_eq!(request["method"], "session/prompt");
            assert_eq!(request["params"]["prompt"][0]["type"], "text");
            agent
                .notify(
                    "session/update",
                    json!({
                        "sessionId": "s-1",
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": "pong"}
                        }
                    }),
                )
                .await;
            agent
                .reply(
                    request["id"].clone(),
                    json!({
                        "stopReason": "end_turn",
                        "usage": {"inputTokens": 10, "outputTokens": 2, "cachedReadTokens": 3}
                    }),
                )
                .await;
        });

        client.initialize().await.expect("initialize");
        let mcp = vec![super::super::protocol::AcpMcpServer::hub_http(
            "http://127.0.0.1:1/mcp",
            Some("secret"),
        )];
        let info = client
            .new_session(workspace.path(), &mcp)
            .await
            .expect("session");
        assert_eq!(info.session_id, "s-1");
        assert_eq!(
            info.resolve_model("gemini-4-argon").as_deref(),
            Some("gemini-4-argon-medium"),
            "a family prefix resolves to the currently selected concrete model"
        );
        assert_eq!(
            info.resolve_model("gemini-4-argon-medium").as_deref(),
            Some("gemini-4-argon-medium")
        );

        let result = client
            .prompt("s-1", &[PromptContent::text("ping")])
            .await
            .expect("prompt");
        assert_eq!(result.stop_reason, "end_turn");
        assert_eq!(result.usage.input_tokens, 10);

        agent_task.await.expect("agent");
        let mut events = events;
        let first = events.recv().await.expect("event");
        match first {
            AcpEvent::SessionUpdate { session_id, update } => {
                assert_eq!(session_id, "s-1");
                assert_eq!(update.text().as_deref(), Some("pong"));
            }
            other => panic!("unexpected event {other:?}"),
        }

        client.shutdown().await.expect("shutdown");
    }

    #[tokio::test(start_paused = true)]
    async fn a_prompt_the_agent_never_reads_times_out_and_kills_the_transport() {
        let workspace = tempfile::tempdir().expect("tempdir");
        // A tiny stdin buffer that nobody drains: the first frame fills it and
        // the writer blocks mid-write.
        let (client_write, _agent_read) = tokio::io::duplex(64);
        let (agent_write, client_read) = tokio::io::duplex(64);
        let client = AcpClient::start(
            config(workspace.path()),
            std::env::temp_dir(),
            client_read,
            client_write,
            None,
            StderrRing::new(),
        );
        let _agent_write = agent_write;

        let prompt = PromptContent::text("x".repeat(1024 * 1024));
        match client.start_prompt("s-1", &[prompt]).await {
            Err(AcpError::Timeout { method, seconds }) => {
                assert_eq!(method, "session/prompt");
                assert_eq!(seconds, PROMPT_WRITE_TIMEOUT.as_secs());
            }
            Err(other) => panic!("expected a prompt write timeout, got {other}"),
            Ok(_) => panic!("a stalled prompt write must not be reported as flushed"),
        }
        assert!(
            client.is_closed(),
            "the wedged transport is torn down so the half-written frame is never delivered"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_writer_queue_times_out_and_aborts_the_blocked_writer() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client_write, _agent_read) = tokio::io::duplex(64);
        let (agent_write, client_read) = tokio::io::duplex(64);
        let client = Arc::new(AcpClient::start(
            config(workspace.path()),
            std::env::temp_dir(),
            client_read,
            client_write,
            None,
            StderrRing::new(),
        ));
        let _agent_write = agent_write;

        // The writer blocks on the first frame (nobody drains the pipe) and
        // the next 256 frames fill its queue, so the following enqueue has to
        // wait. Before the fix its timer had not started yet.
        let filler = client.clone();
        let filling = tokio::spawn(async move {
            for _ in 0..(FRAME_CHANNEL_CAPACITY + 2) {
                let _ = filler.cancel("s-1").await;
            }
        });
        for _ in 0..1_000 {
            let full = lock(&client.shared.stdin_tx)
                .as_ref()
                .is_some_and(|sender| sender.capacity() == 0);
            if full {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            lock(&client.shared.stdin_tx)
                .as_ref()
                .map(mpsc::Sender::capacity),
            Some(0),
            "the writer queue is full"
        );

        let prompt_client = client.clone();
        let prompt = tokio::spawn(async move {
            prompt_client
                .start_prompt("s-1", &[PromptContent::text("x".repeat(1024))])
                .await
                .map(|_| ())
        });
        let outcome = tokio::time::timeout(Duration::from_secs(30), prompt)
            .await
            .expect("the enqueue must share the write deadline")
            .expect("prompt task");
        match outcome {
            Err(AcpError::Timeout { method, seconds }) => {
                assert_eq!(method, "session/prompt");
                assert_eq!(seconds, PROMPT_WRITE_TIMEOUT.as_secs());
            }
            Err(other) => panic!("expected a prompt write timeout, got {other}"),
            Ok(()) => panic!("a prompt that never reached the writer must not claim success"),
        }
        assert!(
            lock(&client.shared.writer).is_none(),
            "the writer blocked on the full queue is aborted so it cannot write the frame later"
        );
        assert!(client.is_closed(), "the wedged transport is torn down");
        filling.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_reply_under_a_full_writer_queue_times_out_instead_of_blocking() {
        let workspace = tempfile::tempdir().expect("tempdir");
        std::fs::write(workspace.path().join("hello.txt"), "hi there").expect("write");
        // A tiny stdin that nobody drains: the writer blocks on its first frame
        // and the queue behind it fills.
        let (client_write, agent_read) = tokio::io::duplex(64);
        let (agent_write, client_read) = tokio::io::duplex(4_096);
        let client = Arc::new(AcpClient::start(
            config(workspace.path()),
            std::env::temp_dir(),
            client_read,
            client_write,
            None,
            StderrRing::new(),
        ));
        let mut agent = FakeAgent {
            reader: BufReader::new(agent_read),
            writer: agent_write,
        };

        let sender = lock(&client.shared.stdin_tx).clone().expect("writer");
        sender
            .try_send(OutgoingFrame {
                frame: json!({
                    "jsonrpc": "2.0",
                    "method": "test/noop",
                    "params": {"pad": "x".repeat(4_096)},
                }),
                written: None,
            })
            .expect("the first frame fits");
        for _ in 0..1_000 {
            if sender.capacity() == FRAME_CHANNEL_CAPACITY {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sender.capacity(),
            FRAME_CHANNEL_CAPACITY,
            "the writer took the frame and is blocked writing it"
        );
        while sender
            .try_send(OutgoingFrame {
                frame: json!({"jsonrpc": "2.0", "method": "test/noop"}),
                written: None,
            })
            .is_ok()
        {}
        assert_eq!(sender.capacity(), 0, "the writer queue is full");

        // The agent asks for a file. The reader must answer through the same
        // bounded queue, so a full queue tears the transport down instead of
        // parking the reader forever.
        agent
            .request(
                json!("fs-1"),
                "fs/read_text_file",
                json!({"sessionId": "s", "path": workspace.path().join("hello.txt")}),
            )
            .await;
        for _ in 0..(PROMPT_WRITE_TIMEOUT.as_secs() as usize + 2) {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if client.is_closed() {
                break;
            }
        }
        assert!(
            client.is_closed(),
            "a reader reply that cannot be queued must not block the reader forever"
        );
        assert!(
            lock(&client.shared.writer).is_none(),
            "the blocked writer is aborted so the reply cannot slip out later"
        );
        assert!(
            lock(&client.shared.stdin_tx).is_none(),
            "later frames are refused instead of queued for a dead pipe"
        );
    }

    /// A writer whose stdin refuses every frame, like a pipe whose reader died.
    struct BrokenWriter;
    impl AsyncWrite for BrokenWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the agent's stdin is gone",
            )))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A writer that takes a frame and never finishes writing it.
    struct StalledWriter;
    impl AsyncWrite for StalledWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Pending
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A prompt whose frame its writer drops (the writer exited before it
    /// acknowledged the flush) has no reply coming: the transport is torn down
    /// rather than left open behind the error.
    #[tokio::test]
    async fn a_dropped_prompt_write_tears_the_transport_down() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (agent_write, client_read) = tokio::io::duplex(64);
        let client = Arc::new(AcpClient::start_for_test(
            config(workspace.path()),
            client_read,
            StalledWriter,
        ));
        let _agent_write = agent_write;
        let _events = client.take_events().expect("events");

        let prompting = client.clone();
        let prompt = tokio::spawn(async move {
            prompting
                .start_prompt("s-1", &[PromptContent::text("ping")])
                .await
                .map(|_| ())
        });
        let sender = lock(&client.shared.stdin_tx).clone().expect("writer");
        // The prompt is registered and its frame queued; the writer then takes
        // the frame and is stuck writing it, so the caller waits for the flush
        // acknowledgement.
        for _ in 0..1_000 {
            if !lock(&client.shared.pending).is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            !lock(&client.shared.pending).is_empty(),
            "the prompt write was enqueued"
        );
        for _ in 0..1_000 {
            if sender.capacity() == FRAME_CHANNEL_CAPACITY {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sender.capacity(),
            FRAME_CHANNEL_CAPACITY,
            "the writer took the prompt frame"
        );

        // The writer dies without acknowledging the frame it held.
        lock(&client.shared.writer).take().expect("writer").abort();
        let outcome = tokio::time::timeout(Duration::from_secs(5), prompt)
            .await
            .expect("the dropped write returns instead of waiting for its deadline")
            .expect("prompt task");
        match outcome {
            Err(AcpError::Closed { message }) => {
                assert_eq!(
                    message,
                    "the ACP writer exited before the request was written"
                );
            }
            other => panic!("expected a closed transport, got {other:?}"),
        }
        assert!(
            client.is_closed(),
            "no reply can come from a frame its writer dropped"
        );
        assert!(
            client.cancel("s-1").await.is_err(),
            "later frames are refused instead of queued for a dead writer"
        );
    }

    #[tokio::test]
    async fn a_writer_io_error_reports_the_client_closed() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (agent_write, client_read) = tokio::io::duplex(64);
        let client = AcpClient::start_for_test(config(workspace.path()), client_read, BrokenWriter);
        let _agent_write = agent_write;
        let mut events = client.take_events().expect("events");

        // The frame is enqueued; the writer fails on it. The failure is a dead
        // transport, not a session that merely had one bad prompt.
        let _ = client.cancel("s-1").await;
        let event = events.recv().await.expect("event");
        assert!(
            matches!(event, AcpEvent::TransportClosed { .. }),
            "the write failure is reported as the transport death it is: {event:?}"
        );
        assert!(
            client.is_closed(),
            "a half-written transport reports itself closed so the daemon detaches it"
        );
        assert!(
            client.cancel("s-1").await.is_err(),
            "later frames are refused instead of queued for a dead pipe"
        );
        client
            .shutdown()
            .await
            .expect("shutting down a dead writer is not an error");
    }

    /// A tracked write that fails carries the writer's own I/O error into the
    /// caller's error, instead of a generic "the writer exited".
    #[tokio::test]
    async fn a_tracked_write_failure_keeps_the_writer_io_error() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (agent_write, client_read) = tokio::io::duplex(64);
        let stderr = StderrRing::new();
        stderr.push("agy-fixture: closing stdin before the next prompt");
        let client = AcpClient::start(
            config(workspace.path()),
            workspace.path().to_path_buf(),
            client_read,
            BrokenWriter,
            None,
            stderr,
        );
        let _agent_write = agent_write;
        let _events = client.take_events().expect("events");

        let error = match client
            .start_prompt("s-1", &[PromptContent::text("ping")])
            .await
        {
            Ok(_) => panic!("the write failed, so the prompt cannot be pending"),
            Err(error) => error,
        };
        match error {
            AcpError::Closed { message } => assert!(
                message.contains("the agent's stdin is gone"),
                "the writer's I/O error is kept: {message}"
            ),
            other => panic!("expected a closed transport, got {other:?}"),
        }
        assert!(
            client
                .stderr_tail()
                .contains("closing stdin before the next prompt"),
            "the agent's stderr stays available for the one failure row"
        );
        assert!(client.is_closed(), "the failed write closed the transport");
    }

    /// A writer I/O error closes the client and kills the agent before it wakes
    /// a waiting caller: an agent that closed stdin but stayed alive must not
    /// survive, and the daemon must never see an open client behind the error.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_writer_io_error_closes_the_client_and_kills_the_agent() {
        let workspace = tempfile::tempdir().expect("tempdir");
        // The agent closes its own stdin but keeps running, so the client's
        // next write fails as a broken pipe while the process is still alive.
        let config =
            AcpClientConfig::new("/bin/sh", workspace.path()).args(["-c", "exec 0<&-; sleep 30"]);
        let client = AcpClient::spawn(config).expect("spawn the agent");
        let pid = client.pid().expect("a child process");
        let _events = client.take_events().expect("events");

        // Early frames fit in the pipe buffer, so keep writing until the shell
        // has closed stdin and the writer sees the failure.
        let mut failed = false;
        for _ in 0..200 {
            if client
                .start_prompt("s-1", &[PromptContent::text("ping")])
                .await
                .is_err()
            {
                failed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(failed, "the writer must notice the closed stdin");
        assert!(
            client.is_closed(),
            "the client is already closed when the caller sees the error"
        );

        // The process group is killed, not just the pipe abandoned.
        let gone = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // SAFETY: signal 0 only probes whether the process exists.
                if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            gone.is_ok(),
            "the agent process is gone after the writer I/O error"
        );
        client.shutdown().await.expect("shutdown");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_with_a_full_writer_queue_returns_within_the_deadline() {
        let workspace = tempfile::tempdir().expect("tempdir");
        // A tiny stdin buffer that nobody drains, so the writer blocks on its
        // first frame and the queue behind it fills.
        let (client_write, _agent_read) = tokio::io::duplex(64);
        let (agent_write, client_read) = tokio::io::duplex(64);
        let client = Arc::new(AcpClient::start(
            config(workspace.path()),
            std::env::temp_dir(),
            client_read,
            client_write,
            None,
            StderrRing::new(),
        ));
        let _agent_write = agent_write;

        let sender = lock(&client.shared.stdin_tx).clone().expect("writer");
        // One frame larger than the pipe: the writer takes it, blocks writing,
        // and stops draining the queue behind it.
        sender
            .try_send(OutgoingFrame {
                frame: json!({
                    "jsonrpc": "2.0",
                    "method": "test/noop",
                    "params": {"pad": "x".repeat(4_096)},
                }),
                written: None,
            })
            .expect("the first frame fits");
        for _ in 0..1_000 {
            if sender.capacity() == FRAME_CHANNEL_CAPACITY {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sender.capacity(),
            FRAME_CHANNEL_CAPACITY,
            "the writer took the frame and is blocked writing it"
        );
        while sender
            .try_send(OutgoingFrame {
                frame: json!({"jsonrpc": "2.0", "method": "test/noop"}),
                written: None,
            })
            .is_ok()
        {}
        assert_eq!(sender.capacity(), 0, "the writer queue is full");

        let stop_client = Arc::clone(&client);
        let stop = tokio::spawn(async move { stop_client.cancel("s-1").await });
        let outcome = tokio::time::timeout(Duration::from_secs(30), stop)
            .await
            .expect("a Stop must not wait for a full writer queue")
            .expect("stop task");
        match outcome {
            Err(AcpError::Timeout { method, seconds }) => {
                assert_eq!(method, "session/cancel");
                assert_eq!(seconds, PROMPT_WRITE_TIMEOUT.as_secs());
            }
            Err(other) => panic!("expected a cancel write timeout, got {other}"),
            Ok(()) => panic!("a cancel that never reached the writer must not claim success"),
        }
        assert!(
            lock(&client.shared.writer).is_none(),
            "the blocked writer is aborted so the cancel cannot slip out later"
        );
        assert!(client.is_closed(), "the wedged transport is torn down");
    }

    #[tokio::test]
    async fn a_prompt_started_before_a_cancel_reaches_the_agent_first() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut agent) = fake_pair(config(workspace.path()));
        let agent_task = tokio::spawn(async move {
            let prompt = agent.next().await;
            assert_eq!(prompt["method"], "session/prompt");
            let cancel = agent.next().await;
            assert_eq!(cancel["method"], "session/cancel");
            agent
                .reply(prompt["id"].clone(), json!({"stopReason": "cancelled"}))
                .await;
        });

        let pending = client
            .start_prompt("s-1", &[PromptContent::text("ping")])
            .await
            .expect("the prompt is flushed before this returns");
        client.cancel("s-1").await.expect("cancel");
        let result = pending.wait().await.expect("prompt result");
        assert!(result.cancelled());

        agent_task.await.expect("agent");
        client.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn inbound_fs_requests_are_scoped_to_the_workspace() {
        let workspace = tempfile::tempdir().expect("tempdir");
        std::fs::write(workspace.path().join("hello.txt"), "hi there\n").expect("write");
        let (client, mut agent) = fake_pair(config(workspace.path()));

        let workspace_path = workspace.path().to_path_buf();
        let agent_task = tokio::spawn(async move {
            let request = agent.next().await;
            agent
                .reply(request["id"].clone(), json!({"protocolVersion": 1}))
                .await;

            agent
                .request(
                    json!("fs-1"),
                    "fs/read_text_file",
                    json!({"sessionId": "s", "path": workspace_path.join("hello.txt")}),
                )
                .await;
            let response = agent.next().await;
            assert_eq!(response["id"], "fs-1");
            assert_eq!(response["result"]["content"], "hi there\n");

            agent
                .request(
                    json!("fs-2"),
                    "fs/read_text_file",
                    json!({"sessionId": "s", "path": "/etc/passwd"}),
                )
                .await;
            let response = agent.next().await;
            assert_eq!(response["id"], "fs-2");
            assert!(
                response["error"]["message"]
                    .as_str()
                    .is_some_and(|text| { text.contains("outside the session workspace") })
            );
        });

        client.initialize().await.expect("initialize");
        agent_task.await.expect("agent");
        client.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn permission_requests_are_surfaced_and_can_be_answered() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut agent) = fake_pair(config(workspace.path()));
        let mut events = client.take_events().expect("events");

        let agent_task = tokio::spawn(async move {
            let request = agent.next().await;
            agent
                .reply(request["id"].clone(), json!({"protocolVersion": 1}))
                .await;

            agent
                .request(
                    json!("perm-1"),
                    "session/request_permission",
                    json!({
                        "sessionId": "s",
                        "toolCall": {"toolCallId": "t-1", "title": "run it", "kind": "execute"},
                        "options": [
                            {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                            {"optionId": "deny", "name": "Deny", "kind": "reject_once"}
                        ]
                    }),
                )
                .await;
            let response = agent.next().await;
            assert_eq!(response["id"], "perm-1");
            assert_eq!(response["result"]["outcome"]["outcome"], "selected");
            assert_eq!(response["result"]["outcome"]["optionId"], "allow");
        });

        client.initialize().await.expect("initialize");
        let event = events.recv().await.expect("permission event");
        let (request_id, request) = match event {
            AcpEvent::PermissionRequest {
                request_id,
                request,
            } => (request_id, request),
            other => panic!("unexpected event {other:?}"),
        };
        assert_eq!(request.session_id, "s");
        assert_eq!(request.tool_call.expect("tool call").tool_call_id, "t-1");
        assert_eq!(request.options[0].option_id, "allow");

        client
            .respond_permission(&request_id, Some("allow"))
            .await
            .expect("respond");
        agent_task.await.expect("agent");
        client.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn cancel_is_a_notification() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut agent) = fake_pair(config(workspace.path()));
        let agent_task = tokio::spawn(async move {
            let request = agent.next().await;
            agent
                .reply(request["id"].clone(), json!({"protocolVersion": 1}))
                .await;
            let frame = agent.next().await;
            assert_eq!(frame["method"], "session/cancel");
            assert_eq!(frame["params"]["sessionId"], "s-1");
            assert!(frame.get("id").is_none());
        });

        client.initialize().await.expect("initialize");
        client.cancel("s-1").await.expect("cancel");
        agent_task.await.expect("agent");
        client.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn json_rpc_errors_surface_with_code_and_message() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut agent) = fake_pair(config(workspace.path()));
        let agent_task = tokio::spawn(async move {
            let request = agent.next().await;
            agent
                .error(request["id"].clone(), -32601, "no such method")
                .await;
        });

        let error = client.initialize().await.expect_err("initialize must fail");
        match error {
            AcpError::Rpc { code, message, .. } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "no such method");
            }
            other => panic!("unexpected error {other:?}"),
        }
        agent_task.await.expect("agent");
        client.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn jsonrpc_errors_are_returned_to_the_caller() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut agent) = fake_pair(config(workspace.path()));
        let agent_task = tokio::spawn(async move {
            let request = agent.next().await;
            agent
                .error(request["id"].clone(), -32601, "method not found")
                .await;
        });

        let error = client.initialize().await.expect_err("initialize must fail");
        match error {
            AcpError::Rpc {
                code,
                message,
                data,
                ..
            } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "method not found");
                assert!(data.is_none());
            }
            other => panic!("unexpected error {other:?}"),
        }
        agent_task.await.expect("agent");
        client.shutdown().await.expect("shutdown");
    }

    #[test]
    fn scoped_paths_reject_escapes_but_accept_relative_paths() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let workspace = std::fs::canonicalize(workspace.path()).expect("canonical");
        assert_eq!(
            scoped_path(&workspace, "a/b.txt").expect("inside"),
            workspace.join("a/b.txt")
        );
        assert!(scoped_path(&workspace, "../outside").is_err());
        assert!(scoped_path(&workspace, "/etc/passwd").is_err());
    }
}
