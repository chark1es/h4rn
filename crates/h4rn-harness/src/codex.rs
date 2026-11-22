//! The Codex adapter (ADR-005).
//!
//! # Transport
//!
//! One long-lived `codex app-server` process per session, spoken to over
//! newline-delimited JSON-RPC on stdio:
//!
//! ```text
//! codex app-server -c thread_unload_delay_secs=0 [provider overrides]
//! ```
//!
//! The adapter performs the `initialize` handshake, then `thread/start` (or
//! `thread/resume` with [`SessionSpec::resume_id`]) carrying the workspace,
//! model, approval policy, sandbox mode and the per-thread `mcp_servers.h4rn`
//! configuration. Turns are `turn/start` requests; Codex streams notifications
//! until `turn/completed`. The process stays alive between turns, so the only
//! time `thread/resume` runs is after the daemon restarts.
//!
//! # MCP injection
//!
//! Codex reads MCP servers from the thread's `config`, not from a file, so the
//! hub endpoint and bearer token ride the `thread/start` request and never
//! appear in argv or the child's environment (ADR-006).
//!
//! # Approvals
//!
//! Codex asks for permission with server-initiated JSON-RPC requests. The
//! stdout task decodes `item/commandExecution/requestApproval`,
//! `item/fileChange/requestApproval`, `item/permissions/requestApproval`,
//! `item/tool/requestUserInput` and the legacy `execCommandApproval` /
//! `applyPatchApproval` methods into [`HarnessEvent::ApprovalRequested`].
//! [`HarnessSession::respond_approval`] sends the matching JSON-RPC response.
//!
//! # Interrupt and shutdown
//!
//! Interrupt is `turn/interrupt`. If the request cannot be delivered, the
//! adapter falls back to `SIGINT` on the child's process group. Shutdown closes
//! stdin, sends `SIGTERM` to the process group, waits five seconds, then
//! `SIGKILL`s the group (ADR-021).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use h4rn_protocol::{AgentState, ApprovalDecision, HarnessKind, RuntimeMode, TurnStatus};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

use crate::attachments::{self, AttachmentClass, AttachmentDelivery};
use crate::claude_code::process::{GroupSignal, signal_group};
use crate::{
    AttachmentRef, Harness, HarnessError, HarnessEvent, HarnessInstall, HarnessSession,
    SessionSpec, TurnReport, UserInput,
};

mod limit;
mod protocol;

pub use protocol::{ApprovalRequest, CodexParser, approval_from_request, approval_response};

/// Environment key for the MCP hub bearer token.
///
/// The same key is accepted by the Claude Code adapter; it is consumed here and
/// removed from the child's environment.
pub const ENV_MCP_AUTH_TOKEN: &str = "H4RN_MCP_AUTH_TOKEN";
/// Alias for [`ENV_MCP_AUTH_TOKEN`].
pub const ENV_MCP_BEARER_TOKEN: &str = "H4RN_MCP_BEARER_TOKEN";
/// Selects a Codex `model_providers` entry (for CLIProxyAPI or a gateway).
pub const ENV_MODEL_PROVIDER: &str = "H4RN_CODEX_MODEL_PROVIDER";
/// Overrides the provider base URL (also accepts `OPENAI_BASE_URL`).
pub const ENV_BASE_URL: &str = "H4RN_CODEX_BASE_URL";
/// Names the environment variable that holds the provider key.
///
/// The key itself rides the child environment under the named variable, never
/// argv; Codex resolves `model_providers.<id>.env_key` at request time.
pub const ENV_API_KEY_ENV: &str = "H4RN_CODEX_API_KEY_ENV";
/// Overrides `wire_api` (only `responses` is accepted by Codex today).
pub const ENV_WIRE_API: &str = "H4RN_CODEX_WIRE_API";

const MCP_TOOL_TIMEOUT_SECS: u64 = 3_900;
/// The service tier Codex labels "Fast" (2x speed, increased usage).
const FAST_SERVICE_TIER: &str = "priority";
const EVENT_CHANNEL_CAPACITY: usize = 512;
const STDIN_CHANNEL_CAPACITY: usize = 64;
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Environment variables a nested Codex must not inherit.
const SCRUBBED_ENV: &[&str] = &["CODEX_THREAD_ID", "CODEX_SESSION_ID", "CODEX_CI"];

const H4RN_ONLY_ENV: &[&str] = &[
    ENV_MCP_AUTH_TOKEN,
    ENV_MCP_BEARER_TOKEN,
    ENV_MODEL_PROVIDER,
    ENV_BASE_URL,
    ENV_API_KEY_ENV,
    ENV_WIRE_API,
];

/// The Codex adapter.
#[derive(Clone, Debug, Default)]
pub struct CodexHarness {
    install: Option<HarnessInstall>,
}

impl CodexHarness {
    /// Creates an adapter that discovers `codex` on this host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an adapter bound to a known installation.
    pub fn with_install(install: HarnessInstall) -> Self {
        Self {
            install: Some(install),
        }
    }
}

#[async_trait]
impl Harness for CodexHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    fn detect(&self) -> Option<HarnessInstall> {
        self.install.clone().or_else(detect_codex)
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

        let mut command = std::process::Command::new(&install.path);
        command
            .arg("app-server")
            .current_dir(&spec.workspace)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // T3 Code's own delay, disabled here so a thread unsubscribes at once.
        command.args(["-c", "thread_unload_delay_secs=0"]);
        for argument in provider_arguments(&spec.env_overrides) {
            command.args(["-c", &argument]);
        }

        for key in SCRUBBED_ENV {
            command.env_remove(key);
        }
        for (key, value) in &spec.env_overrides {
            if !H4RN_ONLY_ENV.contains(&key.as_str()) {
                command.env(key, value);
            }
        }

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
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
            message: "codex exited before it reported a process id".into(),
        })?;

        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            signal_group(pid, GroupSignal::Kill);
            return Err(HarnessError::Spawn {
                harness: self.kind(),
                message: "codex was spawned without piped stdio".into(),
            });
        };

        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (stdin_tx, stdin_rx) = mpsc::channel(STDIN_CHANNEL_CAPACITY);
        let shared = Arc::new(Mutex::new(SharedState::new()));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let approvals = Arc::new(Mutex::new(HashMap::new()));
        let stderr_ring = crate::claude_code::process::StderrRing::new();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let interrupt_requested = Arc::new(AtomicBool::new(false));

        spawn_stdout_reader(
            stdout,
            shared.clone(),
            pending.clone(),
            approvals.clone(),
            interrupt_requested.clone(),
            stdin_tx.clone(),
            events_tx.clone(),
        );
        spawn_stderr_reader(stderr, stderr_ring.clone());
        let writer = spawn_stdin_writer(stdin, stdin_rx, events_tx.clone());
        let supervisor =
            spawn_supervisor(child, stderr_ring.clone(), shutting_down.clone(), events_tx);

        let mut session = CodexSession {
            kind: self.kind(),
            stdin_tx: Some(stdin_tx),
            events_rx: Some(events_rx),
            supervisor: Some(supervisor),
            writer: Some(writer),
            reader: None,
            pid,
            shutting_down,
            interrupt_requested,
            stderr: stderr_ring,
            shared,
            pending,
            approvals,
            next_id: AtomicU64::new(1),
            first_attachments: spec.attachments.clone(),
            first_send: true,
            closed: false,
            settings: CodexSettings {
                model: spec.model.clone(),
                runtime_mode: spec.runtime_mode,
                effort: spec.reasoning_effort.clone(),
                fast: spec.fast,
            },
            catalog: None,
        };

        session.handshake(&spec).await?;
        Ok(Box::new(session))
    }
}

/// The session returned by [`CodexHarness`].
pub struct CodexSession {
    kind: HarnessKind,
    stdin_tx: Option<mpsc::Sender<Vec<u8>>>,
    events_rx: Option<mpsc::Receiver<HarnessEvent>>,
    supervisor: Option<tokio::task::JoinHandle<()>>,
    writer: Option<tokio::task::JoinHandle<()>>,
    reader: Option<tokio::task::JoinHandle<()>>,
    pid: u32,
    shutting_down: Arc<AtomicBool>,
    interrupt_requested: Arc<AtomicBool>,
    stderr: crate::claude_code::process::StderrRing,
    shared: Arc<Mutex<SharedState>>,
    pending: Pending,
    approvals: Approvals,
    next_id: AtomicU64,
    first_attachments: Vec<AttachmentRef>,
    first_send: bool,
    closed: bool,
    /// Settings the next `turn/start` carries.
    ///
    /// Codex applies model, approval policy, sandbox and reasoning effort per
    /// turn ("this turn and subsequent turns"), so a live thread switch is one
    /// field change followed by the next turn.
    settings: CodexSettings,
    /// What the current model's catalogue says it supports, once fetched.
    catalog: Option<CodexCatalog>,
}

/// The per-model options Codex's `model/list` advertised.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CodexCatalog {
    /// Reasoning efforts this model accepts.
    efforts: Vec<String>,
    /// The effort the model runs at unless the thread overrides it.
    default_effort: Option<String>,
    /// Whether this model has a fast service tier.
    fast: bool,
}

/// The thread settings a Codex session applies to each turn.
#[derive(Clone, Debug)]
struct CodexSettings {
    model: String,
    runtime_mode: RuntimeMode,
    effort: Option<String>,
    fast: bool,
}

/// State the session exposes from the parser running on the stdout task.
#[derive(Debug)]
struct SharedState {
    session_id: Option<String>,
    model: Option<String>,
    state: AgentState,
    last_turn: Option<TurnReport>,
    touched_paths: Vec<String>,
    turn_id: Option<String>,
}

impl SharedState {
    fn new() -> Self {
        Self {
            session_id: None,
            model: None,
            state: AgentState::Idle,
            last_turn: None,
            touched_paths: Vec::new(),
            turn_id: None,
        }
    }
}

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;
type Approvals = Arc<Mutex<HashMap<String, PendingApproval>>>;

#[derive(Clone, Debug)]
struct PendingApproval {
    rpc_id: Value,
    method: String,
    params: Value,
}

impl CodexSession {
    /// The Codex thread id, for `thread/resume` after a daemon restart.
    pub fn session_id(&self) -> Option<String> {
        lock(&self.shared).session_id.clone()
    }

    /// The model reported by the thread.
    pub fn model(&self) -> Option<String> {
        lock(&self.shared).model.clone()
    }

    /// The current agent-state hint.
    pub fn agent_state(&self) -> AgentState {
        lock(&self.shared).state
    }

    /// The most recent completed turn's fine-grained accounting.
    pub fn last_turn(&self) -> Option<TurnReport> {
        lock(&self.shared).last_turn.clone()
    }

    /// Paths touched by file-change items in this session.
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

    async fn handshake(&mut self, spec: &SessionSpec) -> Result<(), HarnessError> {
        self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "h4rn",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )
        .await?;

        let config = mcp_config(spec);
        let mut params = json!({
            "cwd": spec.workspace,
            "approvalPolicy": approval_policy(spec.runtime_mode),
            "approvalsReviewer": "user",
            "sandbox": sandbox_mode(spec.runtime_mode),
        });
        if let Some(model) = model_argument(&spec.model) {
            params["model"] = Value::String(model.to_string());
        }
        if spec.fast {
            params["serviceTier"] = Value::String(FAST_SERVICE_TIER.to_string());
        }
        if let Some(config) = config {
            params["config"] = config;
        }
        if let Some(effort) = spec
            .reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|effort| !effort.is_empty())
        {
            // Thread creation has no top-level effort field; the config key is
            // Codex's own `model_reasoning_effort`.
            let config = params
                .as_object_mut()
                .and_then(|params| params.get_mut("config"))
                .and_then(Value::as_object_mut);
            match config {
                Some(config) => {
                    config.insert(
                        "model_reasoning_effort".into(),
                        Value::String(effort.to_string()),
                    );
                }
                None => {
                    params["config"] = json!({ "model_reasoning_effort": effort });
                }
            }
        }
        if let Some(resume_id) = spec.resume_id.as_deref().filter(|id| !id.is_empty()) {
            params["threadId"] = Value::String(resume_id.to_string());
        }

        let method = if spec.resume_id.as_deref().is_some_and(|id| !id.is_empty()) {
            "thread/resume"
        } else {
            "thread/start"
        };
        let result = self.request(method, params).await?;

        let thread_id = result["thread"]["id"]
            .as_str()
            .or_else(|| result["thread"]["sessionId"].as_str())
            .ok_or_else(|| HarnessError::Protocol {
                harness: self.kind,
                message: "codex did not return a thread id".into(),
            })?
            .to_string();
        let model = result["model"]
            .as_str()
            .or_else(|| result["thread"]["model"].as_str())
            .map(str::to_string);

        {
            let mut shared = lock(&self.shared);
            shared.session_id = Some(thread_id);
            shared.model = model;
            shared.state = AgentState::Idle;
        }

        // The model catalogue says which efforts and speed tiers this thread
        // can actually use. A failed fetch leaves it unknown; the adapter then
        // does not guess, and the thread keeps its requested values.
        self.refresh_catalog().await;
        if let Some(catalog) = &self.catalog
            && let Some(effort) = self.settings.effort.as_deref()
            && !catalog.efforts.is_empty()
            && !catalog
                .efforts
                .iter()
                .any(|value| value.eq_ignore_ascii_case(effort))
        {
            return Err(HarnessError::Unsupported {
                harness: self.kind,
                feature: format!(
                    "reasoning effort `{effort}`; this model offers: {}",
                    catalog.efforts.join(", ")
                ),
            });
        }
        if let Some(catalog) = &self.catalog
            && self.settings.fast
            && !catalog.fast
        {
            return Err(HarnessError::Unsupported {
                harness: self.kind,
                feature: "a fast tier for this model".into(),
            });
        }
        Ok(())
    }

    /// Reads `model/list` and records what the current model supports.
    ///
    /// Best effort: an older Codex without the method, or a provider whose
    /// catalogue is unavailable, leaves `catalog` as it was (usually `None`),
    /// so no valid setting is rejected on a guess.
    async fn refresh_catalog(&mut self) {
        let requested = {
            let named = model_argument(&self.settings.model).map(str::to_string);
            match named {
                Some(model) => Some(model),
                None => {
                    let shared = lock(&self.shared);
                    shared.model.clone()
                }
            }
        };
        let Ok(result) = self.request("model/list", json!({})).await else {
            return;
        };
        self.catalog = catalog_from_model_list(&result, requested.as_deref());
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, HarnessError> {
        let id = Value::from(self.next_id.fetch_add(1, Ordering::SeqCst));
        let key = id_key(&id);
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(key.clone(), sender);

        let frame = json!({ "id": id, "method": method, "params": params });
        if let Err(error) = self.write_frame(frame).await {
            lock(&self.pending).remove(&key);
            return Err(error);
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => Err(HarnessError::Protocol {
                harness: self.kind,
                message: format!("codex {method} failed: {message}"),
            }),
            Ok(Err(_)) => Err(HarnessError::Transport {
                harness: self.kind,
                message: format!("codex {method} response channel closed"),
            }),
            Err(_) => {
                lock(&self.pending).remove(&key);
                Err(HarnessError::Transport {
                    harness: self.kind,
                    message: format!("codex {method} timed out"),
                })
            }
        }
    }

    async fn write_frame(&mut self, frame: Value) -> Result<(), HarnessError> {
        let Some(sender) = &self.stdin_tx else {
            return Err(HarnessError::Transport {
                harness: self.kind,
                message: "codex stdin is closed".into(),
            });
        };
        let bytes = encode_frame(frame)?;
        sender
            .send(bytes)
            .await
            .map_err(|_| HarnessError::Transport {
                harness: self.kind,
                message: "codex writer task has exited".into(),
            })
    }

    fn mark_state(&self, state: AgentState) {
        lock(&self.shared).state = state;
    }
}

#[async_trait]
impl HarnessSession for CodexSession {
    fn kind(&self) -> HarnessKind {
        self.kind
    }

    async fn set_runtime_mode(&mut self, mode: RuntimeMode) -> Result<(), HarnessError> {
        self.settings.runtime_mode = mode;
        Ok(())
    }

    async fn set_model(&mut self, model: &str) -> Result<(), HarnessError> {
        let model = model.trim();
        if model.is_empty() {
            return Err(HarnessError::Unsupported {
                harness: self.kind,
                feature: "an empty model".into(),
            });
        }
        let previous = self.settings.model.clone();
        self.settings.model = model.to_string();
        self.refresh_catalog().await;
        let catalog = self.catalog.clone();
        let effort = self.settings.effort.clone();
        // A model switch can leave the thread's effort unsupported by the new
        // model; refuse it here rather than send a turn Codex will reject.
        if let Some(catalog) = &catalog
            && let Some(effort) = effort.as_deref()
            && !catalog.efforts.is_empty()
            && !catalog
                .efforts
                .iter()
                .any(|value| value.eq_ignore_ascii_case(effort))
        {
            self.settings.model = previous;
            self.refresh_catalog().await;
            return Err(HarnessError::Unsupported {
                harness: self.kind,
                feature: format!(
                    "model `{model}` with reasoning effort `{effort}`; the model offers: {}",
                    catalog.efforts.join(", ")
                ),
            });
        }
        if let Some(catalog) = &catalog
            && self.settings.fast
            && !catalog.fast
        {
            self.settings.model = previous;
            self.refresh_catalog().await;
            return Err(HarnessError::Unsupported {
                harness: self.kind,
                feature: format!("model `{model}` with fast mode; it has no fast tier"),
            });
        }
        Ok(())
    }

    async fn set_reasoning_effort(&mut self, effort: Option<&str>) -> Result<(), HarnessError> {
        if let Some(effort) = effort
            && let Some(catalog) = &self.catalog
            && !catalog.efforts.is_empty()
            && !catalog
                .efforts
                .iter()
                .any(|value| value.eq_ignore_ascii_case(effort))
        {
            return Err(HarnessError::Unsupported {
                harness: self.kind,
                feature: format!(
                    "reasoning effort `{effort}`; this model offers: {}",
                    catalog.efforts.join(", ")
                ),
            });
        }
        self.settings.effort = effort.map(str::to_string);
        Ok(())
    }

    async fn set_fast(&mut self, fast: bool) -> Result<(), HarnessError> {
        if fast
            && let Some(catalog) = &self.catalog
            && !catalog.fast
        {
            return Err(HarnessError::Unsupported {
                harness: self.kind,
                feature: "a fast tier for this model".into(),
            });
        }
        self.settings.fast = fast;
        Ok(())
    }

    fn controls(&self) -> Option<crate::SessionControls> {
        let catalog = self.catalog.as_ref()?;
        let base = crate::thread_controls(self.kind);
        Some(crate::SessionControls {
            modes: base.modes,
            // The catalogue lists selectable models, but a provider can run a
            // custom slug too; the model itself is left unconstrained.
            models: None,
            efforts: catalog.efforts.clone(),
            fast: catalog.fast,
            switch_mode: true,
            switch_model: true,
            switch_effort: true,
            switch_fast: true,
        })
    }

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

        let thread_id = self.session_id().ok_or_else(|| HarnessError::Transport {
            harness: self.kind,
            message: "codex session has no thread id".into(),
        })?;
        let turn_input = build_turn_input(&input.text, &attachments);
        // The turn carries the thread's current settings: a switch made
        // between turns is in effect from this prompt on, without restarting
        // the conversation.
        let default_effort = self
            .catalog
            .as_ref()
            .and_then(|catalog| catalog.default_effort.clone());
        let params = turn_params(
            &self.settings,
            default_effort.as_deref(),
            thread_id,
            turn_input,
        );
        let result = self.request("turn/start", params).await?;

        if let Some(turn_id) = result["turn"]["id"].as_str() {
            lock(&self.shared).turn_id = Some(turn_id.to_string());
        }
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
        let (thread_id, turn_id) = {
            let shared = lock(&self.shared);
            (shared.session_id.clone(), shared.turn_id.clone())
        };
        let (Some(thread_id), Some(turn_id)) = (thread_id, turn_id) else {
            return Ok(());
        };

        if self
            .request(
                "turn/interrupt",
                json!({ "threadId": thread_id, "turnId": turn_id }),
            )
            .await
            .is_err()
        {
            signal_group(self.pid, GroupSignal::Interrupt);
        }
        Ok(())
    }

    async fn respond_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
        option_id: Option<&str>,
    ) -> Result<(), HarnessError> {
        let pending = lock(&self.approvals).remove(approval_id);
        let Some(pending) = pending else {
            return Err(HarnessError::Unsupported {
                harness: self.kind,
                feature: format!(
                    "answering approval {approval_id}; it is unknown or already answered"
                ),
            });
        };
        let frame = approval_response(
            &pending.rpc_id,
            &pending.method,
            &pending.params,
            decision,
            option_id,
        );
        self.write_frame(frame).await
    }

    fn events(&mut self) -> BoxStream<'static, HarnessEvent> {
        match self.events_rx.take() {
            Some(receiver) => ReceiverStream::new(receiver).boxed(),
            None => futures::stream::empty().boxed(),
        }
    }

    fn session_id(&self) -> Option<String> {
        CodexSession::session_id(self)
    }

    fn agent_state(&self) -> AgentState {
        CodexSession::agent_state(self)
    }

    fn last_turn(&self) -> Option<TurnReport> {
        CodexSession::last_turn(self)
    }

    fn touched_paths(&self) -> Vec<String> {
        CodexSession::touched_paths(self)
    }

    fn stderr_tail(&self) -> String {
        CodexSession::stderr_tail(self)
    }

    async fn shutdown(&mut self) -> Result<(), HarnessError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.shutting_down.store(true, Ordering::SeqCst);

        self.stdin_tx.take();
        signal_group(self.pid, GroupSignal::Terminate);

        if let Some(mut supervisor) = self.supervisor.take() {
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut supervisor).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(target: "h4rn_harness::codex", %error, "codex supervisor task failed");
                }
                Err(_) => {
                    signal_group(self.pid, GroupSignal::Kill);
                    match tokio::time::timeout(KILL_GRACE, &mut supervisor).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(target: "h4rn_harness::codex", %error, "codex supervisor task failed after kill");
                        }
                        Err(_) => supervisor.abort(),
                    }
                }
            }
        }
        if let Some(reader) = self.reader.take() {
            reader.abort();
        }
        if let Some(writer) = self.writer.take() {
            writer.abort();
        }
        Ok(())
    }
}

impl Drop for CodexSession {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.shutting_down.store(true, Ordering::SeqCst);
        signal_group(self.pid, GroupSignal::Kill);
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_stdout_reader(
    stdout: tokio::process::ChildStdout,
    shared: Arc<Mutex<SharedState>>,
    pending: Pending,
    approvals: Approvals,
    interrupt_requested: Arc<AtomicBool>,
    stdin_tx: mpsc::Sender<Vec<u8>>,
    events_tx: mpsc::Sender<HarnessEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut parser = CodexParser::new();
        let mut lines = BufReader::new(stdout).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => {
                    let _ = events_tx
                        .send(HarnessEvent::Error {
                            message: format!("failed reading codex stdout: {error}"),
                            retryable: false,
                        })
                        .await;
                    break;
                }
            };

            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                // Let the parser turn this into a diagnostic event.
                let mut events = parser.push_line(&line);
                if publish(&events_tx, &interrupt_requested, &mut events).await {
                    return;
                }
                continue;
            };

            if let Some(method) = value["method"].as_str() {
                if let Some(rpc_id) = value.get("id") {
                    let params = value.get("params").cloned().unwrap_or(Value::Null);
                    if let Some(request) = approval_from_request(method, &params) {
                        lock(&approvals).insert(
                            request.approval_id.clone(),
                            PendingApproval {
                                rpc_id: rpc_id.clone(),
                                method: request.method.clone(),
                                params,
                            },
                        );
                        lock(&shared).state = AgentState::AwaitingApproval;
                        if events_tx.send(request.event()).await.is_err() {
                            return;
                        }
                    } else {
                        let frame = json!({
                            "id": rpc_id,
                            "error": {
                                "code": -32601,
                                "message": format!("h4rn does not implement {method}"),
                            }
                        });
                        if let Ok(bytes) = encode_frame(frame) {
                            let _ = stdin_tx.send(bytes).await;
                        }
                    }
                    continue;
                }

                let mut events = parser.push_value(&value);
                {
                    let mut state = lock(&shared);
                    if let Some(session_id) = parser.session_id() {
                        state.session_id = Some(session_id.to_string());
                    }
                    if let Some(model) = parser.model() {
                        state.model = Some(model.to_string());
                    }
                    if let Some(turn_id) = parser.turn_id() {
                        state.turn_id = Some(turn_id.to_string());
                    }
                    state.state = parser.agent_state();
                    state.touched_paths = parser.touched_paths().to_vec();
                    state.last_turn = parser.last_turn().cloned();
                }
                if publish(&events_tx, &interrupt_requested, &mut events).await {
                    return;
                }
                continue;
            }

            if value.get("id").is_some() {
                resolve_response(&pending, &value);
            }
        }
    })
}

async fn publish(
    events_tx: &mpsc::Sender<HarnessEvent>,
    interrupt_requested: &AtomicBool,
    events: &mut [HarnessEvent],
) -> bool {
    let completed = events
        .iter()
        .any(|event| matches!(event, HarnessEvent::TurnCompleted { .. }));
    if completed && interrupt_requested.swap(false, Ordering::SeqCst) {
        for event in events.iter_mut() {
            if let HarnessEvent::TurnCompleted { status, .. } = event {
                *status = TurnStatus::Interrupted;
            }
        }
    }
    for event in events.iter_mut() {
        if events_tx.send(event.clone()).await.is_err() {
            return true;
        }
    }
    false
}

fn resolve_response(pending: &Pending, value: &Value) {
    let Some(id) = value.get("id") else {
        return;
    };
    let Some(sender) = lock(pending).remove(&id_key(id)) else {
        return;
    };
    if let Some(error) = value.get("error") {
        let message = error["message"]
            .as_str()
            .unwrap_or("unspecified JSON-RPC error")
            .to_string();
        let _ = sender.send(Err(message));
    } else {
        let _ = sender.send(Ok(value.get("result").cloned().unwrap_or(Value::Null)));
    }
}

fn spawn_stderr_reader(
    stderr: tokio::process::ChildStderr,
    ring: crate::claude_code::process::StderrRing,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => ring.push(&line),
                Ok(None) => break,
                Err(error) => {
                    ring.push(&format!("failed reading codex stderr: {error}"));
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
                        message: format!("failed writing to codex stdin: {error}"),
                        retryable: false,
                    })
                    .await;
                break;
            }
            if let Err(error) = stdin.flush().await {
                let _ = events_tx
                    .send(HarnessEvent::Error {
                        message: format!("failed flushing codex stdin: {error}"),
                        retryable: false,
                    })
                    .await;
                break;
            }
        }
    })
}

fn spawn_supervisor(
    mut child: tokio::process::Child,
    stderr: crate::claude_code::process::StderrRing,
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
                        format!("codex exited with {status}")
                    } else {
                        format!("codex exited with {status}: {tail}")
                    };
                    Some(HarnessEvent::Error {
                        message,
                        retryable: false,
                    })
                }
                Err(error) => Some(HarnessEvent::Error {
                    message: format!("failed to wait for codex: {error}"),
                    retryable: false,
                }),
                _ => None,
            };
            if let Some(event) = event {
                let _ = events_tx.send(event).await;
            }
        }
    })
}

fn encode_frame(frame: Value) -> Result<Vec<u8>, HarnessError> {
    let mut bytes = serde_json::to_vec(&frame).map_err(|error| HarnessError::Protocol {
        harness: HarnessKind::Codex,
        message: format!("failed to encode a codex frame: {error}"),
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn id_key(id: &Value) -> String {
    match id {
        Value::String(text) => format!("s:{text}"),
        Value::Number(number) => format!("n:{number}"),
        other => format!("o:{other}"),
    }
}

fn model_argument(model: &str) -> Option<&str> {
    let model = model.trim();
    if model.is_empty() || model == "default" {
        None
    } else {
        Some(model)
    }
}

/// The Codex `approvalPolicy` value for a thread's autonomy setting.
fn approval_policy(runtime_mode: RuntimeMode) -> &'static str {
    match runtime_mode {
        RuntimeMode::ApprovalRequired | RuntimeMode::Plan => "untrusted",
        RuntimeMode::AutoAcceptEdits => "on-request",
        RuntimeMode::FullAccess => "never",
    }
}

/// The Codex `sandbox` value for a thread's autonomy setting.
fn sandbox_mode(runtime_mode: RuntimeMode) -> &'static str {
    match runtime_mode {
        RuntimeMode::ApprovalRequired => "read-only",
        RuntimeMode::AutoAcceptEdits => "workspace-write",
        // Codex has no plan mode; the daemon refuses it for this harness.
        RuntimeMode::Plan => "read-only",
        RuntimeMode::FullAccess => "danger-full-access",
    }
}

/// The catalogue entry for one thread's model, from a `model/list` reply.
///
/// `requested` is the model the thread named; without one the catalogue's
/// default entry is the thread's model. An unknown id leaves the catalogue
/// unknown rather than guessing from another model.
fn catalog_from_model_list(result: &Value, requested: Option<&str>) -> Option<CodexCatalog> {
    let entries = result["data"].as_array()?;
    let entry = match requested {
        Some(id) => entries
            .iter()
            .find(|entry| entry["id"].as_str() == Some(id))?,
        None => entries.iter().find(|entry| entry["isDefault"] == true)?,
    };
    let efforts = entry["supportedReasoningEfforts"]
        .as_array()
        .map(|efforts| {
            efforts
                .iter()
                .filter_map(|option| option["reasoningEffort"].as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let fast = entry["serviceTiers"]
        .as_array()
        .is_some_and(|tiers| tiers.iter().any(|tier| tier["id"] == FAST_SERVICE_TIER))
        || entry["additionalSpeedTiers"]
            .as_array()
            .is_some_and(|tiers| tiers.iter().any(|tier| tier == "fast"));
    let default_effort = entry["defaultReasoningEffort"].as_str().map(str::to_string);
    Some(CodexCatalog {
        efforts,
        default_effort,
        fast,
    })
}

/// The `turn/start` parameters for one prompt.
///
/// Codex applies model, approval policy, sandbox, effort and service tier per
/// turn ("this turn and subsequent turns"), so a live thread switch is a field
/// change here and nothing more.
fn turn_params(
    settings: &CodexSettings,
    default_effort: Option<&str>,
    thread_id: String,
    input: Vec<Value>,
) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "input": input,
        "approvalPolicy": approval_policy(settings.runtime_mode),
        "sandboxPolicy": sandbox_policy(settings.runtime_mode),
        "serviceTierForTurn": if settings.fast {
            FAST_SERVICE_TIER
        } else {
            "default"
        },
    });
    if let Some(model) = model_argument(&settings.model) {
        params["model"] = Value::String(model.to_string());
    }
    // An unset effort returns the model to its own default explicitly; simply
    // omitting the field would keep the previous turn's override.
    if let Some(effort) = settings
        .effort
        .as_deref()
        .or(default_effort)
        .map(str::trim)
        .filter(|effort| !effort.is_empty())
    {
        params["effort"] = Value::String(effort.to_string());
    }
    params
}

/// The `turn/start` sandbox policy object for a permission mode.
fn sandbox_policy(runtime_mode: RuntimeMode) -> Value {
    match runtime_mode {
        RuntimeMode::ApprovalRequired | RuntimeMode::Plan => {
            json!({ "type": "readOnly", "networkAccess": false })
        }
        RuntimeMode::AutoAcceptEdits => json!({
            "type": "workspaceWrite",
            "networkAccess": false,
        }),
        RuntimeMode::FullAccess => json!({ "type": "dangerFullAccess" }),
    }
}

/// Builds the per-thread `config` object that injects the h4rn MCP server.
fn mcp_config(spec: &SessionSpec) -> Option<Value> {
    let endpoint = spec
        .mcp_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())?;
    let token = spec
        .env_overrides
        .get(ENV_MCP_AUTH_TOKEN)
        .or_else(|| spec.env_overrides.get(ENV_MCP_BEARER_TOKEN))
        .map(String::as_str)
        .filter(|token| !token.is_empty());

    let mut server = json!({
        "url": endpoint,
        "tool_timeout_sec": MCP_TOOL_TIMEOUT_SECS,
    });
    if let Some(token) = token {
        server["http_headers"] = json!({ "Authorization": format!("Bearer {token}") });
    }
    Some(json!({ "mcp_servers.h4rn": server }))
}

/// Provider overrides translated into `codex -c key=value` arguments.
///
/// The provider name comes from [`ENV_MODEL_PROVIDER`]; its base URL from
/// [`ENV_BASE_URL`] or `OPENAI_BASE_URL`. When only a base URL is supplied the
/// built-in OpenAI provider is redirected with `openai_base_url`, which is the
/// shape Codex documents.
fn provider_arguments(env: &BTreeMap<String, String>) -> Vec<String> {
    let provider = env
        .get(ENV_MODEL_PROVIDER)
        .map(String::as_str)
        .map(str::trim)
        .filter(|provider| !provider.is_empty());
    let base_url = env
        .get(ENV_BASE_URL)
        .or_else(|| env.get("OPENAI_BASE_URL"))
        .map(String::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty());
    let api_key_env = env
        .get(ENV_API_KEY_ENV)
        .map(String::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let wire_api = env
        .get(ENV_WIRE_API)
        .map(String::as_str)
        .map(str::trim)
        .filter(|api| !api.is_empty())
        .unwrap_or("responses");

    let mut arguments = Vec::new();
    match (provider, base_url) {
        (Some(provider), Some(base_url)) => {
            arguments.push(format!("model_provider={provider}"));
            arguments.push(format!("model_providers.{provider}.base_url={base_url}"));
            arguments.push(format!("model_providers.{provider}.wire_api={wire_api}"));
            arguments.push(format!(
                "model_providers.{provider}.requires_openai_auth=false"
            ));
            if let Some(api_key_env) = api_key_env {
                arguments.push(format!("model_providers.{provider}.env_key={api_key_env}"));
            }
        }
        (Some(provider), None) => arguments.push(format!("model_provider={provider}")),
        (None, Some(base_url)) => arguments.push(format!("openai_base_url={base_url}")),
        (None, None) => {}
    }
    arguments
}

/// Builds the `turn/start` input array from text and attachments.
///
/// Images become `localImage` items (Codex reads the path itself); every other
/// attachment becomes a `mention` item, and the prompt also states the absolute
/// path so the model can refer to the file even if the mention is ignored.
pub fn build_turn_input(text: &str, attachments: &[AttachmentRef]) -> Vec<Value> {
    let mut input = Vec::new();
    let mut full_text = text.to_string();
    let mut path_lines = Vec::new();

    for attachment in attachments {
        let name = attachment
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| attachment.id.to_string());
        let path = attachment.path.display().to_string();
        let media_type = attachments::media_type(attachment);
        let class = AttachmentClass::of(&media_type, &attachment.path);
        match attachments::strategy(HarnessKind::Codex, class) {
            // Codex reads the file itself; the path rides the item.
            AttachmentDelivery::LocalImagePath => {
                input.push(json!({ "type": "localImage", "path": path }));
            }
            // Everything else becomes an @path mention.
            _ => {
                input.push(json!({
                    "type": "mention",
                    "name": name,
                    "path": path,
                }));
            }
        }
        path_lines.push(attachments::path_line(attachment));
    }

    if !path_lines.is_empty() {
        if !full_text.is_empty() {
            full_text.push('\n');
        }
        full_text.push_str(&path_lines.join("\n"));
    }
    if !full_text.is_empty() {
        input.insert(0, json!({ "type": "text", "text": full_text }));
    }
    input
}

/// Detects a `codex` installation on this host.
///
/// Search order: every `PATH` entry, then the common per-user install
/// locations (`~/.local/bin`, `~/.bun/bin`, `~/.npm-global/bin`, and on Windows
/// `%APPDATA%\npm`). The first candidate that answers `codex --version` wins.
pub fn detect_codex() -> Option<HarnessInstall> {
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
    let mut dirs = std::collections::BTreeSet::new();
    if let Some(path) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path) {
            if !entry.as_os_str().is_empty() {
                dirs.insert(entry);
            }
        }
    }

    if let Some(home) = home_dir() {
        for suffix in [".local/bin", ".bun/bin", ".npm-global/bin", "bin"] {
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
        vec!["codex.exe", "codex.cmd", "codex.bat", "codex"]
    } else {
        vec!["codex"]
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
        if !token.starts_with(|character: char| character.is_ascii_digit()) {
            continue;
        }
        let version: String = token
            .chars()
            .take_while(|character| {
                character.is_ascii_digit() || *character == '.' || *character == '-'
            })
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
    fn parses_the_codex_version_banner() {
        assert_eq!(
            parse_version("codex-cli 0.168.0").as_deref(),
            Some("0.168.0")
        );
        assert_eq!(parse_version("v1.2.3\n").as_deref(), Some("1.2.3"));
        assert_eq!(parse_version("\n\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn probes_an_executable_and_reads_its_version() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("codex");
        std::fs::write(&path, "#!/bin/sh\necho 'codex-cli 0.168.0'\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(probe_version(&path).as_deref(), Some("0.168.0"));
    }

    #[test]
    fn runtime_modes_map_to_codex_policy_and_sandbox() {
        assert_eq!(approval_policy(RuntimeMode::ApprovalRequired), "untrusted");
        assert_eq!(sandbox_mode(RuntimeMode::ApprovalRequired), "read-only");
        assert_eq!(approval_policy(RuntimeMode::AutoAcceptEdits), "on-request");
        assert_eq!(
            sandbox_mode(RuntimeMode::AutoAcceptEdits),
            "workspace-write"
        );
        assert_eq!(approval_policy(RuntimeMode::FullAccess), "never");
        assert_eq!(sandbox_mode(RuntimeMode::FullAccess), "danger-full-access");
    }

    #[test]
    fn mcp_config_uses_the_dotted_thread_key() {
        let mut spec = crate::test_spec();
        spec.mcp_endpoint = Some("https://hub.example/mcp".into());
        spec.env_overrides
            .insert(ENV_MCP_AUTH_TOKEN.into(), "secret".into());
        let config = mcp_config(&spec).expect("config");
        assert_eq!(config["mcp_servers.h4rn"]["url"], "https://hub.example/mcp");
        assert_eq!(
            config["mcp_servers.h4rn"]["http_headers"]["Authorization"],
            "Bearer secret"
        );
        assert_eq!(
            config["mcp_servers.h4rn"]["tool_timeout_sec"],
            MCP_TOOL_TIMEOUT_SECS
        );
    }

    #[test]
    fn provider_overrides_become_codex_config_arguments() {
        let mut env = BTreeMap::new();
        env.insert(ENV_MODEL_PROVIDER.into(), "cliproxyapi".into());
        env.insert(ENV_BASE_URL.into(), "http://127.0.0.1:8317/v1".into());
        let arguments = provider_arguments(&env);
        assert!(arguments.contains(&"model_provider=cliproxyapi".to_string()));
        assert!(arguments.contains(
            &"model_providers.cliproxyapi.base_url=http://127.0.0.1:8317/v1".to_string()
        ));
        assert!(
            arguments
                .contains(&"model_providers.cliproxyapi.requires_openai_auth=false".to_string())
        );
    }

    #[test]
    fn a_named_key_env_and_wire_api_are_passed_to_codex() {
        let mut env = BTreeMap::new();
        env.insert(ENV_MODEL_PROVIDER.into(), "h4rn-proxy_0".into());
        env.insert(ENV_BASE_URL.into(), "https://proxy.example/v1".into());
        env.insert(ENV_API_KEY_ENV.into(), "CLIPROXY_API_KEY".into());
        env.insert(ENV_WIRE_API.into(), "responses".into());
        let arguments = provider_arguments(&env);
        assert!(
            arguments
                .contains(&"model_providers.h4rn-proxy_0.env_key=CLIPROXY_API_KEY".to_string())
        );
        assert!(arguments.contains(&"model_providers.h4rn-proxy_0.wire_api=responses".to_string()));
        // The key itself stays in the child environment, never in argv.
        assert!(!arguments.iter().any(|argument| argument.contains("sk-")));
    }

    #[test]
    fn openai_base_url_redirects_the_builtin_provider() {
        let mut env = BTreeMap::new();
        env.insert("OPENAI_BASE_URL".into(), "http://127.0.0.1:8317/v1".into());
        assert_eq!(
            provider_arguments(&env),
            vec!["openai_base_url=http://127.0.0.1:8317/v1".to_string()]
        );
    }

    #[test]
    fn turn_input_uses_text_images_and_mentions() {
        let image = AttachmentRef {
            id: AttachmentId::from_content(b"png"),
            path: PathBuf::from("/tmp/shot.png"),
            media_type: "image/png".into(),
        };
        let file = AttachmentRef {
            id: AttachmentId::from_content(b"csv"),
            path: PathBuf::from("/tmp/report.csv"),
            media_type: "text/csv".into(),
        };
        let input = build_turn_input("look", &[image, file]);
        assert_eq!(input[0]["type"], "text");
        assert!(
            input[0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("[Attached image \"shot.png\""))
        );
        assert_eq!(input[1]["type"], "localImage");
        assert_eq!(input[1]["path"], "/tmp/shot.png");
        assert_eq!(input[2]["type"], "mention");
        assert_eq!(input[2]["name"], "report.csv");
        assert_eq!(input[2]["path"], "/tmp/report.csv");
    }

    #[test]
    fn model_argument_skips_the_default_slug() {
        assert_eq!(model_argument("default"), None);
        assert_eq!(model_argument("  "), None);
        assert_eq!(model_argument("gpt-6.1-sol"), Some("gpt-6.1-sol"));
    }

    #[test]
    fn a_turn_carries_the_threads_model_effort_and_fast_tier() {
        let settings = CodexSettings {
            model: "gpt-6.2-sol".into(),
            runtime_mode: RuntimeMode::FullAccess,
            effort: Some("xhigh".into()),
            fast: true,
        };
        let params = turn_params(&settings, None, "thread-1".into(), vec![]);
        assert_eq!(params["model"], "gpt-6.2-sol");
        assert_eq!(params["effort"], "xhigh");
        assert_eq!(params["serviceTierForTurn"], "priority");
        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(params["sandboxPolicy"]["type"], "dangerFullAccess");
    }

    #[test]
    fn a_turn_without_effort_or_fast_omits_them() {
        let settings = CodexSettings {
            model: "default".into(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            effort: None,
            fast: false,
        };
        let params = turn_params(&settings, None, "thread-1".into(), vec![]);
        assert!(params.get("model").is_none());
        assert!(params.get("effort").is_none());
        assert_eq!(params["serviceTierForTurn"], "default");
        assert_eq!(params["sandboxPolicy"]["type"], "readOnly");

        // With no thread override the model's own default is sent explicitly:
        // omitting the field would keep the previous turn's effort.
        let params = turn_params(&settings, Some("medium"), "thread-1".into(), vec![]);
        assert_eq!(params["effort"], "medium");
    }

    #[test]
    fn the_model_catalogue_names_the_efforts_and_fast_tier_it_supports() {
        let result = json!({
            "data": [
                {
                    "id": "gpt-6.2-sol",
                    "isDefault": true,
                    "defaultReasoningEffort": "low",
                    "supportedReasoningEfforts": [
                        {"reasoningEffort": "low"},
                        {"reasoningEffort": "high"}
                    ],
                    "serviceTiers": [{"id": "priority"}, {"id": "flex"}]
                },
                {
                    "id": "gpt-plain",
                    "isDefault": false,
                    "supportedReasoningEfforts": [],
                    "serviceTiers": []
                }
            ]
        });
        let catalog = catalog_from_model_list(&result, Some("gpt-6.2-sol")).expect("catalogue");
        assert_eq!(catalog.efforts, vec!["low", "high"]);
        assert_eq!(catalog.default_effort.as_deref(), Some("low"));
        assert!(catalog.fast);
        let plain = catalog_from_model_list(&result, Some("gpt-plain")).expect("catalogue");
        assert!(plain.efforts.is_empty());
        assert!(!plain.fast);
        // An unknown id does not borrow another model's options.
        assert_eq!(catalog_from_model_list(&result, Some("missing")), None);
    }
}
