//! The Google Antigravity adapter (ADR-026).
//!
//! # Transport
//!
//! ACP is the primary surface. One long-lived child per session:
//!
//! ```text
//! agy acp --process-per-session --sandbox \
//!     [--app-data-dir <dir>] [--gemini-dir <dir>] [--cloud_code_endpoint <url>]
//! ```
//!
//! The child speaks JSON-RPC 2.0 over NDJSON on stdio and is driven through
//! [`crate::acp::AcpClient`]. The adapter negotiates capabilities, opens (or
//! loads) a session, selects the account-scoped model from the list
//! `session/new` returned, sets an explicit mode, and injects the h4rn MCP hub
//! over HTTP when the agent advertises `mcpCapabilities.http` (otherwise over
//! the documented `mcp-remote` stdio bridge).
//!
//! # Approvals
//!
//! ACP puts the decision on the client, so [`HarnessSession::respond_approval`]
//! answers the agent's `session/request_permission` request directly. The
//! adapter never passes `--dangerously-skip-permissions`: an approval-required
//! thread stays in `default` mode and the client decides.
//!
//! # Fallback
//!
//! When the ACP child cannot be spawned or its handshake fails, the adapter
//! drops to the CLI's turn-shaped `stream-json` mode (see [`stream`]). That
//! transport cannot raise approvals, so it runs an explicit conservative mode
//! and reports [`HarnessError::Unsupported`] if a decision is requested.
//!
//! # Detection
//!
//! [`detect_agy`] looks for `agy` on `PATH` and in `~/.local/bin` (the
//! documented manual install location), then asks it for `--version`. Nothing
//! in the daemon hard-codes a path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use futures::stream::{BoxStream, StreamExt};
use h4rn_protocol::{
    AgentState, ApprovalDecision, ApprovalKind, ApprovalOption, HarnessKind, RuntimeMode, ToolCall,
    ToolCallStatus, TurnStatus, Usage,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::acp::{
    AcpClient, AcpClientConfig, AcpError, AcpEvent, AcpMcpServer, AcpPermissionOption,
    AcpPermissionRequest, AcpServerInfo, AcpSessionInfo, AcpToolCall, AcpToolKind, AcpToolStatus,
    AcpUpdate, AcpUsage, AgentCapabilities, ENV_MCP_STDIO_BRIDGE, PromptContent,
};
use crate::attachments::{self, AttachmentClass, AttachmentDelivery};
use crate::{
    AttachmentRef, ENV_MCP_AUTH_TOKEN, ENV_MCP_BEARER_TOKEN, Harness, HarnessError, HarnessEvent,
    HarnessInstall, HarnessSession, SessionSpec, TurnReport, UserInput, detect_limit,
};

mod stream;

pub use stream::AgyStreamParser;

const EVENT_CHANNEL_CAPACITY: usize = 512;
const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES: usize = 80 * 1024 * 1024;
const MAX_TEXT_RESOURCE_BYTES: u64 = 2 * 1024 * 1024;
const TOUCHED_PATH_LIMIT: usize = 512;
const TURN_TEXT_LIMIT: usize = 16 * 1024;

/// Overrides the `agy` executable used by the adapter.
pub const ENV_AGY_PATH: &str = "H4RN_AGY_PATH";
/// Passes `--app-data-dir` to `agy acp` (one profile per Google account).
pub const ENV_APP_DATA_DIR: &str = "H4RN_AGY_APP_DATA_DIR";
/// Passes `--gemini-dir` to `agy acp`.
pub const ENV_GEMINI_DIR: &str = "H4RN_AGY_GEMINI_DIR";
/// Passes `--cloud_code_endpoint` to `agy acp` for gateway deployments.
pub const ENV_CLOUD_CODE_ENDPOINT: &str = "H4RN_AGY_CLOUD_CODE_ENDPOINT";
/// Overrides the execution mode (`default`, `accept-edits`, `auto`, `plan`).
pub const ENV_AGY_MODE: &str = "H4RN_AGY_MODE";
/// Forces one transport: `acp`, `stream-json`, or `auto` (the default).
pub const ENV_AGY_TRANSPORT: &str = "H4RN_AGY_TRANSPORT";
/// The only API-key variable Antigravity's CLI accepts.
pub const ENV_GEMINI_API_KEY: &str = "GEMINI_API_KEY";
/// Documented custom Gemini endpoint override.
pub const ENV_GOOGLE_GEMINI_BASE_URL: &str = "GOOGLE_GEMINI_BASE_URL";
/// Model id Antigravity should request.
pub const ENV_GEMINI_MODEL: &str = "GEMINI_MODEL";

/// Environment variables a nested Antigravity must not inherit.
const SCRUBBED_ENV: &[&str] = &[
    "AGY_SESSION_ID",
    "ANTIGRAVITY_SESSION_ID",
    "GEMINI_CLI_SESSION_ID",
];

/// The Google Antigravity adapter.
#[derive(Clone, Debug, Default)]
pub struct AntigravityHarness {
    install: Option<HarnessInstall>,
}

impl AntigravityHarness {
    /// Creates an adapter that discovers `agy` on this host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an adapter bound to a known installation.
    ///
    /// Tests and the daemon's inventory use this after running [`detect_agy`]
    /// once; nothing else should hard-code a path.
    pub fn with_install(install: HarnessInstall) -> Self {
        Self {
            install: Some(install),
        }
    }
}

#[async_trait]
impl Harness for AntigravityHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Antigravity
    }

    fn detect(&self) -> Option<HarnessInstall> {
        self.install.clone().or_else(detect_agy)
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

        let transport = transport_preference(&spec);
        if transport != TransportPreference::StreamJson {
            match AcpSession::start(&install, &spec).await {
                Ok(session) => return Ok(Box::new(session)),
                // An explicit mode/model failure is a caller error, not a
                // transport failure: the fallback would fail the same way.
                Err(error @ HarnessError::Unsupported { .. }) => return Err(error),
                Err(error) if transport == TransportPreference::Acp => return Err(error),
                Err(error) => {
                    tracing::warn!(
                        target: "h4rn_harness::antigravity",
                        %error,
                        "agy ACP transport failed; falling back to stream-json"
                    );
                }
            }
        }

        let mode = stream_mode(&spec);
        let session = stream::spawn(&install, &spec, &mode)?;
        Ok(Box::new(session))
    }
}

/// Detects an `agy` installation on this host.
///
/// Candidates are the `H4RN_AGY_PATH` override, `agy` on `PATH`, and the
/// documented manual install location `~/.local/bin/agy`. The version comes
/// from `agy --version`; an install that cannot be queried still counts as
/// detected and reports `unknown`.
pub fn detect_agy() -> Option<HarnessInstall> {
    let path = std::env::var_os(ENV_AGY_PATH)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| find_executable("agy"))?;
    let version = version_of(&path).unwrap_or_else(|| "unknown".to_string());
    Some(HarnessInstall { path, version })
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path_var) {
        for candidate in [name.to_string(), format!("{name}.exe")] {
            let path = directory.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    for suffix in [".local/bin", "bin"] {
        let path = home.join(suffix).join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

fn version_of(path: &Path) -> Option<String> {
    let output = Command::new(path).arg("--version").output().ok()?;
    let bytes = if output.stdout.is_empty() {
        output.stderr
    } else {
        output.stdout
    };
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.trim_start_matches('v').to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransportPreference {
    Auto,
    Acp,
    StreamJson,
}

fn transport_preference(spec: &SessionSpec) -> TransportPreference {
    match spec
        .env_overrides
        .get(ENV_AGY_TRANSPORT)
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("acp") => TransportPreference::Acp,
        Some("stream-json" | "stream_json" | "stream") => TransportPreference::StreamJson,
        _ => TransportPreference::Auto,
    }
}

/// ACP mode for the thread's autonomy.
fn wanted_mode(spec: &SessionSpec) -> String {
    if let Some(mode) = spec
        .env_overrides
        .get(ENV_AGY_MODE)
        .map(|mode| mode.trim())
        .filter(|mode| !mode.is_empty())
    {
        return mode.to_string();
    }
    match spec.runtime_mode {
        RuntimeMode::ApprovalRequired => "default",
        RuntimeMode::AutoAcceptEdits => "accept-edits",
        RuntimeMode::FullAccess => "auto",
    }
    .to_string()
}

/// Stream-json mode for the fallback, which cannot answer approvals.
fn stream_mode(spec: &SessionSpec) -> String {
    if let Some(mode) = spec
        .env_overrides
        .get(ENV_AGY_MODE)
        .map(|mode| mode.trim())
        .filter(|mode| !mode.is_empty())
    {
        return mode.to_string();
    }
    match spec.runtime_mode {
        // No client-side approval path exists here, so the safest fallback is
        // plan mode rather than silently auto-approving actions.
        RuntimeMode::ApprovalRequired => "plan",
        RuntimeMode::AutoAcceptEdits => "accept-edits",
        RuntimeMode::FullAccess => "auto",
    }
    .to_string()
}

fn is_h4rn_only_env(key: &str) -> bool {
    key.starts_with("H4RN_")
}

fn acp_config(install: &HarnessInstall, spec: &SessionSpec) -> AcpClientConfig {
    let mut config = AcpClientConfig::new(&install.path, &spec.workspace).args([
        "acp",
        "--process-per-session",
        "--sandbox",
    ]);
    for key in SCRUBBED_ENV {
        config = config.scrub(*key);
    }
    if let Some(dir) = spec
        .env_overrides
        .get(ENV_APP_DATA_DIR)
        .filter(|dir| !dir.trim().is_empty())
    {
        config = config.arg("--app-data-dir").arg(dir.trim());
    }
    if let Some(dir) = spec
        .env_overrides
        .get(ENV_GEMINI_DIR)
        .filter(|dir| !dir.trim().is_empty())
    {
        config = config.arg("--gemini-dir").arg(dir.trim());
    }
    if let Some(endpoint) = spec
        .env_overrides
        .get(ENV_CLOUD_CODE_ENDPOINT)
        .filter(|endpoint| !endpoint.trim().is_empty())
    {
        config = config.arg("--cloud_code_endpoint").arg(endpoint.trim());
    }
    for (key, value) in &spec.env_overrides {
        if !is_h4rn_only_env(key) {
            config = config.env(key, value);
        }
    }
    config
}

fn mcp_servers(spec: &SessionSpec, capabilities: &AgentCapabilities) -> Vec<AcpMcpServer> {
    let Some(endpoint) = spec
        .mcp_endpoint
        .as_deref()
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
    else {
        return Vec::new();
    };
    let token = spec
        .env_overrides
        .get(ENV_MCP_AUTH_TOKEN)
        .or_else(|| spec.env_overrides.get(ENV_MCP_BEARER_TOKEN))
        .map(String::as_str);
    if capabilities.supports_http_mcp() {
        return vec![AcpMcpServer::hub_http(endpoint, token)];
    }
    let bridge = spec
        .env_overrides
        .get(ENV_MCP_STDIO_BRIDGE)
        .map(String::as_str);
    vec![AcpMcpServer::hub_stdio(endpoint, bridge)]
}

fn map_acp_error(error: AcpError) -> HarnessError {
    let harness = HarnessKind::Antigravity;
    match error {
        AcpError::Spawn { message, .. } => HarnessError::Spawn { harness, message },
        AcpError::Closed { message } => HarnessError::Transport { harness, message },
        AcpError::Protocol { message } => HarnessError::Protocol { harness, message },
        AcpError::Rpc { code, message, .. } => HarnessError::Protocol {
            harness,
            message: format!("agent error {code}: {message}"),
        },
        AcpError::Timeout { method, seconds } => HarnessError::Transport {
            harness,
            message: format!("`{method}` timed out after {seconds}s"),
        },
        AcpError::Cancelled { method } => HarnessError::Transport {
            harness,
            message: format!("`{method}` was cancelled"),
        },
        AcpError::OutsideWorkspace { path } => HarnessError::Protocol {
            harness,
            message: format!("agent requested a path outside the workspace: {path}"),
        },
        AcpError::Unsupported { feature } => HarnessError::Unsupported { harness, feature },
    }
}

/// A live ACP session.
struct AcpSession {
    client: AcpClient,
    session_id: String,
    events_rx: Option<mpsc::Receiver<HarnessEvent>>,
    events_tx: mpsc::Sender<HarnessEvent>,
    bridge: tokio::task::JoinHandle<()>,
    prompt_task: Option<tokio::task::JoinHandle<()>>,
    shared: Arc<Mutex<AcpShared>>,
    prompt_running: Arc<AtomicBool>,
    interrupt_requested: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    capabilities: AgentCapabilities,
    close_session: bool,
    first_attachments: Vec<AttachmentRef>,
    first_send: bool,
    closed: bool,
}

impl AcpSession {
    async fn start(install: &HarnessInstall, spec: &SessionSpec) -> Result<Self, HarnessError> {
        let client = AcpClient::spawn(acp_config(install, spec)).map_err(map_acp_error)?;
        match Self::start_with(&client, spec).await {
            Ok(session) => Ok(session),
            Err(error) => {
                client.kill();
                Err(error)
            }
        }
    }

    async fn start_with(client: &AcpClient, spec: &SessionSpec) -> Result<Self, HarnessError> {
        let info: AcpServerInfo = client.initialize().await.map_err(map_acp_error)?;
        if info.protocol_version == 0 {
            return Err(HarnessError::Protocol {
                harness: HarnessKind::Antigravity,
                message: "initialize reply did not carry a protocolVersion".into(),
            });
        }
        let capabilities = info.capabilities.clone();
        let servers = mcp_servers(spec, &capabilities);

        let session: AcpSessionInfo =
            match spec.resume_id.as_deref().filter(|id| !id.trim().is_empty()) {
                Some(session_id) => {
                    if !capabilities.supports_load() {
                        return Err(HarnessError::Unsupported {
                            harness: HarnessKind::Antigravity,
                            feature: "resuming with session/load; this agent does not advertise it"
                                .into(),
                        });
                    }
                    client
                        .load_session(session_id, &spec.workspace, &servers)
                        .await
                        .map_err(map_acp_error)?
                }
                None => client
                    .new_session(&spec.workspace, &servers)
                    .await
                    .map_err(map_acp_error)?,
            };

        let requested = spec.model.trim();
        if !requested.is_empty()
            && requested != "default"
            && requested != "antigravity-default"
            && !requested.eq_ignore_ascii_case("auto")
        {
            match session.resolve_model(requested) {
                Some(model_id) => {
                    if session.current_model_id.as_deref() != Some(model_id.as_str()) {
                        client
                            .set_model(&session.session_id, &model_id)
                            .await
                            .map_err(map_acp_error)?;
                    }
                }
                None => {
                    return Err(HarnessError::Unsupported {
                        harness: HarnessKind::Antigravity,
                        feature: format!(
                            "model `{requested}`; the account offers: {}",
                            available_models(&session)
                        ),
                    });
                }
            }
        }

        let wanted_mode = wanted_mode(spec);
        if let Some(mode) = session.modes.iter().find(|mode| mode.id == wanted_mode) {
            if session.current_mode_id.as_deref() != Some(mode.id.as_str()) {
                client
                    .set_mode(&session.session_id, &mode.id)
                    .await
                    .map_err(map_acp_error)?;
            }
        } else if spec.env_overrides.contains_key(ENV_AGY_MODE) {
            return Err(HarnessError::Unsupported {
                harness: HarnessKind::Antigravity,
                feature: format!(
                    "mode `{wanted_mode}`; the agent offers: {}",
                    session
                        .modes
                        .iter()
                        .map(|mode| mode.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        } else {
            tracing::warn!(
                target: "h4rn_harness::antigravity",
                mode = %wanted_mode,
                "agy did not offer the requested mode; keeping the agent's current mode"
            );
        }

        let events = client.take_events().ok_or(HarnessError::Protocol {
            harness: HarnessKind::Antigravity,
            message: "the ACP event stream was already consumed".into(),
        })?;
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let shared = Arc::new(Mutex::new(AcpShared::new()));
        let prompt_running = Arc::new(AtomicBool::new(false));
        let interrupt_requested = Arc::new(AtomicBool::new(false));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let bridge = spawn_event_bridge(
            events,
            events_tx.clone(),
            shared.clone(),
            shutting_down.clone(),
        );

        events_tx
            .send(HarnessEvent::SessionStarted {
                session_id: session.session_id.clone(),
            })
            .await
            .map_err(|_| HarnessError::Transport {
                harness: HarnessKind::Antigravity,
                message: "the harness event channel closed during startup".into(),
            })?;

        Ok(Self {
            client: client.clone(),
            session_id: session.session_id,
            events_rx: Some(events_rx),
            events_tx,
            bridge,
            prompt_task: None,
            shared,
            prompt_running,
            interrupt_requested,
            shutting_down,
            capabilities,
            close_session: info.capabilities.close_session,
            first_attachments: spec.attachments.clone(),
            first_send: true,
            closed: false,
        })
    }

    fn set_state(&self, state: AgentState) {
        lock(&self.shared).state = state;
    }
}

fn available_models(session: &AcpSessionInfo) -> String {
    session
        .models
        .iter()
        .map(|model| model.model_id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

#[async_trait]
impl HarnessSession for AcpSession {
    async fn send(&mut self, input: UserInput) -> Result<(), HarnessError> {
        if self.closed || self.shutting_down.load(Ordering::SeqCst) {
            return Err(HarnessError::Transport {
                harness: HarnessKind::Antigravity,
                message: "session is shut down".into(),
            });
        }
        if self.prompt_running.swap(true, Ordering::SeqCst) {
            return Err(HarnessError::Transport {
                harness: HarnessKind::Antigravity,
                message: "a turn is already running".into(),
            });
        }

        let mut attachments = input.attachments;
        if self.first_send {
            self.first_send = false;
            attachments.extend(self.first_attachments.iter().cloned());
        }
        let content = match build_prompt(&input.text, &attachments, &self.capabilities) {
            Ok(content) => content,
            Err(error) => {
                self.prompt_running.store(false, Ordering::SeqCst);
                return Err(error);
            }
        };

        {
            let mut shared = lock(&self.shared);
            shared.turn_text.clear();
            shared.tool_labels.clear();
            shared.state = AgentState::Thinking;
        }

        let client = self.client.clone();
        let session_id = self.session_id.clone();
        let events_tx = self.events_tx.clone();
        let shared = self.shared.clone();
        let prompt_running = self.prompt_running.clone();
        let interrupt_requested = self.interrupt_requested.clone();
        let started = Instant::now();

        // Wait until `session/prompt` is on the agent's stdin before returning:
        // an interrupt after this point can safely be sent as a cancel.
        let pending = match client.start_prompt(&session_id, &content).await {
            Ok(pending) => pending,
            Err(error) => {
                self.prompt_running.store(false, Ordering::SeqCst);
                return Err(map_acp_error(error));
            }
        };

        self.prompt_task = Some(tokio::spawn(async move {
            let result = pending.wait().await;
            let duration_ms = millis(started.elapsed());
            prompt_running.store(false, Ordering::SeqCst);
            match result {
                Ok(prompt) => {
                    let usage = usage_from_acp(&prompt.usage, duration_ms);
                    let turn_text = lock(&shared).turn_text.clone();
                    let limit = detect_limit(&turn_text, Utc::now());
                    let interrupted =
                        interrupt_requested.swap(false, Ordering::SeqCst) || prompt.cancelled();
                    let status = if interrupted {
                        TurnStatus::Interrupted
                    } else if prompt.completed() {
                        TurnStatus::Completed
                    } else if prompt.token_limited() {
                        TurnStatus::TokenLimit
                    } else if prompt.turn_limited() {
                        TurnStatus::TurnLimit
                    } else {
                        TurnStatus::Failed
                    };
                    {
                        let mut shared = lock(&shared);
                        shared.last_turn = Some(TurnReport {
                            num_turns: 1,
                            duration_ms,
                            duration_api_ms: 0,
                            cost_micros: 0,
                            is_error: status == TurnStatus::Failed,
                            stop_reason: Some(prompt.stop_reason.clone()),
                            subtype: None,
                        });
                        shared.state = turn_state(status, limit.is_some());
                    }
                    if let Some(signal) = limit {
                        let _ = events_tx
                            .send(HarnessEvent::LimitHit {
                                reset_at: signal.reset_at,
                                auto_resume: signal.reset_at.is_some(),
                            })
                            .await;
                    }
                    if !prompt.usage.is_empty() {
                        let _ = events_tx
                            .send(HarnessEvent::Usage {
                                usage: usage.clone(),
                            })
                            .await;
                    }
                    if status == TurnStatus::Failed {
                        let _ = events_tx
                            .send(HarnessEvent::Error {
                                message: format!(
                                    "Antigravity stopped the turn: {}",
                                    prompt.stop_reason
                                ),
                                retryable: true,
                            })
                            .await;
                    }
                    let _ = events_tx
                        .send(HarnessEvent::TurnCompleted { status, usage })
                        .await;
                }
                Err(error) => {
                    let mut detail = error.to_string();
                    let stderr = client.stderr_tail();
                    if !stderr.is_empty() {
                        detail.push('\n');
                        detail.push_str(&stderr);
                    }
                    let limit = detect_limit(&detail, Utc::now());
                    let usage = Usage {
                        duration_ms,
                        ..Usage::default()
                    };
                    {
                        let mut shared = lock(&shared);
                        shared.state = if limit.is_some() {
                            AgentState::RateLimited
                        } else {
                            AgentState::Error
                        };
                        shared.last_turn = Some(TurnReport {
                            num_turns: 0,
                            duration_ms,
                            duration_api_ms: 0,
                            cost_micros: 0,
                            is_error: true,
                            stop_reason: None,
                            subtype: Some("error".into()),
                        });
                    }
                    if let Some(signal) = limit {
                        let _ = events_tx
                            .send(HarnessEvent::LimitHit {
                                reset_at: signal.reset_at,
                                auto_resume: signal.reset_at.is_some(),
                            })
                            .await;
                    } else {
                        let _ = events_tx
                            .send(HarnessEvent::Error {
                                message: detail.trim().to_string(),
                                retryable: !matches!(
                                    error,
                                    AcpError::Protocol { .. } | AcpError::OutsideWorkspace { .. }
                                ),
                            })
                            .await;
                    }
                    let _ = events_tx
                        .send(HarnessEvent::TurnCompleted {
                            status: TurnStatus::Failed,
                            usage,
                        })
                        .await;
                }
            }
        }));

        Ok(())
    }

    async fn interrupt(&mut self) -> Result<(), HarnessError> {
        if self.closed || self.shutting_down.load(Ordering::SeqCst) {
            return Err(HarnessError::Transport {
                harness: HarnessKind::Antigravity,
                message: "session is shut down".into(),
            });
        }
        self.interrupt_requested.store(true, Ordering::SeqCst);
        // `send` only returns once `session/prompt` was flushed, so a running
        // prompt is always on the wire by the time a cancel can reach here.
        if self.prompt_running.load(Ordering::SeqCst) {
            self.client
                .cancel(&self.session_id)
                .await
                .map_err(map_acp_error)?;
        }
        Ok(())
    }

    async fn respond_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
        option_id: Option<&str>,
    ) -> Result<(), HarnessError> {
        if self.closed || self.shutting_down.load(Ordering::SeqCst) {
            return Err(HarnessError::Transport {
                harness: HarnessKind::Antigravity,
                message: "session is shut down".into(),
            });
        }
        let pending = lock(&self.shared).pending.remove(approval_id);
        let Some(pending) = pending else {
            return Err(HarnessError::Protocol {
                harness: HarnessKind::Antigravity,
                message: format!("approval `{approval_id}` is not pending"),
            });
        };
        let selected = select_permission_option(&pending.options, decision, option_id);
        if decision == ApprovalDecision::Approve && selected.is_none() {
            return Err(HarnessError::Protocol {
                harness: HarnessKind::Antigravity,
                message: "the agent offered no allow option for this request".into(),
            });
        }
        self.client
            .respond_permission(&pending.request_id, selected.as_deref())
            .await
            .map_err(map_acp_error)?;
        self.set_state(AgentState::Thinking);
        Ok(())
    }

    fn events(&mut self) -> BoxStream<'static, HarnessEvent> {
        match self.events_rx.take() {
            Some(receiver) => ReceiverStream::new(receiver).boxed(),
            None => futures::stream::empty().boxed(),
        }
    }

    fn session_id(&self) -> Option<String> {
        Some(self.session_id.clone())
    }

    fn agent_state(&self) -> AgentState {
        lock(&self.shared).state
    }

    fn last_turn(&self) -> Option<TurnReport> {
        lock(&self.shared).last_turn.clone()
    }

    fn touched_paths(&self) -> Vec<String> {
        lock(&self.shared).touched.clone()
    }

    fn stderr_tail(&self) -> String {
        self.client.stderr_tail()
    }

    async fn shutdown(&mut self) -> Result<(), HarnessError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.shutting_down.store(true, Ordering::SeqCst);

        if self.close_session
            && let Err(error) = self.client.close_session(&self.session_id).await
        {
            tracing::debug!(
                target: "h4rn_harness::antigravity",
                %error,
                "session/close failed; shutdown continues"
            );
        }
        if let Some(task) = self.prompt_task.take() {
            task.abort();
        }
        let outcome = self.client.shutdown().await.map_err(map_acp_error);
        self.bridge.abort();
        outcome
    }
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.shutting_down.store(true, Ordering::SeqCst);
        if let Some(task) = self.prompt_task.take() {
            task.abort();
        }
        self.bridge.abort();
        // Best effort: never leave an orphaned agy (ADR-021).
        self.client.kill();
    }
}

#[derive(Debug)]
struct PendingApproval {
    request_id: Value,
    options: Vec<AcpPermissionOption>,
}

#[derive(Debug)]
struct AcpShared {
    state: AgentState,
    touched: Vec<String>,
    tool_started: HashMap<String, DateTime<Utc>>,
    tool_labels: HashMap<String, ToolLabel>,
    pending: HashMap<String, PendingApproval>,
    turn_text: String,
    last_turn: Option<TurnReport>,
}

#[derive(Debug, Default)]
struct ToolLabel {
    name: String,
    summary: String,
}

impl AcpShared {
    fn new() -> Self {
        Self {
            state: AgentState::Idle,
            touched: Vec::new(),
            tool_started: HashMap::new(),
            tool_labels: HashMap::new(),
            pending: HashMap::new(),
            turn_text: String::new(),
            last_turn: None,
        }
    }
}

fn spawn_event_bridge(
    mut events: mpsc::Receiver<AcpEvent>,
    events_tx: mpsc::Sender<HarnessEvent>,
    shared: Arc<Mutex<AcpShared>>,
    shutting_down: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let mapped: Vec<HarnessEvent> = match event {
                AcpEvent::SessionUpdate { update, .. } => map_update(&update, &shared),
                AcpEvent::PermissionRequest {
                    request_id,
                    request,
                } => vec![register_approval(&request_id, &request, &shared)],
                AcpEvent::Notification { params, .. } => {
                    detect_limit(&params.to_string(), Utc::now())
                        .map(|signal| {
                            vec![HarnessEvent::LimitHit {
                                reset_at: signal.reset_at,
                                auto_resume: signal.reset_at.is_some(),
                            }]
                        })
                        .unwrap_or_default()
                }
                AcpEvent::Closed { message } => {
                    if shutting_down.load(Ordering::SeqCst) {
                        Vec::new()
                    } else {
                        vec![HarnessEvent::Error {
                            message,
                            retryable: true,
                        }]
                    }
                }
                AcpEvent::Exited { status, stderr } => {
                    if shutting_down.load(Ordering::SeqCst) {
                        Vec::new()
                    } else if let Some(signal) = detect_limit(&stderr, Utc::now()) {
                        vec![HarnessEvent::LimitHit {
                            reset_at: signal.reset_at,
                            auto_resume: signal.reset_at.is_some(),
                        }]
                    } else {
                        let message = if stderr.trim().is_empty() {
                            format!("agy exited ({status})")
                        } else {
                            format!("agy exited ({status}): {}", stderr.trim())
                        };
                        vec![HarnessEvent::Error {
                            message,
                            retryable: false,
                        }]
                    }
                }
                AcpEvent::Error { message, retryable } => {
                    vec![HarnessEvent::Error { message, retryable }]
                }
            };
            for event in mapped {
                if events_tx.send(event).await.is_err() {
                    return;
                }
            }
        }
    })
}

fn register_approval(
    request_id: &Value,
    request: &AcpPermissionRequest,
    shared: &Arc<Mutex<AcpShared>>,
) -> HarnessEvent {
    let approval_id = approval_id(request_id);
    let kind = approval_kind(request);
    let prompt = approval_prompt(request);
    let options = request
        .options
        .iter()
        .map(|option| ApprovalOption {
            id: option.option_id.clone(),
            label: option.name.clone(),
        })
        .collect();
    {
        let mut shared = lock(shared);
        shared.pending.insert(
            approval_id.clone(),
            PendingApproval {
                request_id: request_id.clone(),
                options: request.options.clone(),
            },
        );
        shared.state = AgentState::AwaitingApproval;
    }
    HarnessEvent::ApprovalRequested {
        approval_id,
        kind,
        prompt,
        options,
    }
}

fn approval_id(request_id: &Value) -> String {
    match request_id {
        Value::String(id) => id.clone(),
        other => other.to_string(),
    }
}

fn approval_kind(request: &AcpPermissionRequest) -> ApprovalKind {
    let Some(call) = &request.tool_call else {
        return ApprovalKind::Permission;
    };
    let name = tool_name(call)
        .unwrap_or_else(|| call.kind.label().to_string())
        .to_ascii_lowercase();
    if name.contains("ask_question") {
        return ApprovalKind::Question;
    }
    match call.kind {
        AcpToolKind::Execute => ApprovalKind::Command,
        AcpToolKind::Edit => ApprovalKind::FileChange,
        _ => ApprovalKind::Permission,
    }
}

fn approval_prompt(request: &AcpPermissionRequest) -> String {
    let Some(call) = &request.tool_call else {
        return "Antigravity is requesting permission".into();
    };
    let name = tool_name(call).unwrap_or_else(|| call.kind.label().to_string());
    let summary = call.summary();
    if summary.is_empty() {
        format!("Allow `{name}`?")
    } else {
        format!("Allow `{name}` to {summary}?")
    }
}

fn select_permission_option(
    options: &[AcpPermissionOption],
    decision: ApprovalDecision,
    requested: Option<&str>,
) -> Option<String> {
    if let Some(requested) = requested
        && options.iter().any(|option| option.option_id == requested)
    {
        return Some(requested.to_string());
    }
    match decision {
        ApprovalDecision::Approve => options
            .iter()
            .find(|option| is_allow_option(option))
            .map(|option| option.option_id.clone()),
        ApprovalDecision::Deny => options
            .iter()
            .find(|option| is_reject_option(option))
            .map(|option| option.option_id.clone()),
        ApprovalDecision::Cancel => None,
    }
}

fn is_allow_option(option: &AcpPermissionOption) -> bool {
    option.kind.starts_with("allow")
        || option.option_id.eq_ignore_ascii_case("allow")
        || option.option_id.to_ascii_lowercase().contains("approve")
}

fn is_reject_option(option: &AcpPermissionOption) -> bool {
    option.kind.starts_with("reject")
        || option.kind.starts_with("deny")
        || option.option_id.eq_ignore_ascii_case("deny")
        || option.option_id.to_ascii_lowercase().contains("reject")
}

fn map_update(update: &AcpUpdate, shared: &Arc<Mutex<AcpShared>>) -> Vec<HarnessEvent> {
    match update {
        AcpUpdate::AgentMessageChunk { .. } => match update.text() {
            Some(text) => {
                remember_turn_text(shared, &text);
                lock(shared).state = AgentState::Thinking;
                vec![HarnessEvent::TextDelta { text }]
            }
            None => Vec::new(),
        },
        AcpUpdate::AgentThoughtChunk { .. } => {
            if let Some(text) = update.text() {
                remember_turn_text(shared, &text);
            }
            lock(shared).state = AgentState::Thinking;
            Vec::new()
        }
        AcpUpdate::ToolCall { tool_call } | AcpUpdate::ToolCallUpdate { tool_call } => {
            vec![tool_event(tool_call, shared)]
        }
        AcpUpdate::Plan { .. } => {
            lock(shared).state = AgentState::Thinking;
            Vec::new()
        }
        AcpUpdate::Usage { usage } => {
            if usage.is_empty() {
                Vec::new()
            } else {
                vec![HarnessEvent::Usage {
                    usage: usage_from_acp(usage, 0),
                }]
            }
        }
        _ => Vec::new(),
    }
}

fn remember_turn_text(shared: &Arc<Mutex<AcpShared>>, text: &str) {
    let mut shared = lock(shared);
    if shared.turn_text.len().saturating_add(text.len()) > TURN_TEXT_LIMIT {
        let keep = TURN_TEXT_LIMIT.saturating_sub(text.len());
        let mut split = shared.turn_text.len().saturating_sub(keep);
        while split < shared.turn_text.len() && !shared.turn_text.is_char_boundary(split) {
            split += 1;
        }
        shared.turn_text = shared.turn_text[split..].to_string();
    }
    shared.turn_text.push_str(text);
}

fn tool_event(call: &AcpToolCall, shared: &Arc<Mutex<AcpShared>>) -> HarnessEvent {
    let tool_call_id = call.tool_call_id.clone();
    let summary = call.summary();
    let input = call.raw_input.clone();
    let output = call.output_preview();
    let status = tool_status(&call.status);
    let finished = matches!(
        call.status,
        AcpToolStatus::Completed | AcpToolStatus::Failed | AcpToolStatus::Cancelled
    );
    let mut shared = lock(shared);
    // `tool_call_update` often carries only a status or output; keep the last
    // title and summary the agent sent so an update never empties the row.
    let label = shared.tool_labels.entry(tool_call_id.clone()).or_default();
    if let Some(name) = call
        .raw
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    {
        label.name = name.to_string();
    } else if label.name.is_empty() {
        if let Some(title) = call.title.as_deref().filter(|title| !title.is_empty()) {
            label.name = title.to_string();
        }
    }
    if !summary.is_empty() {
        label.summary = summary;
    }
    let name = if label.name.is_empty() {
        call.kind.label().to_string()
    } else {
        label.name.clone()
    };
    let summary = label.summary.clone();
    let started_at = shared
        .tool_started
        .entry(tool_call_id.clone())
        .or_insert_with(Utc::now)
        .to_owned();
    let completed_at = finished.then(Utc::now);
    if is_mutating(call, &name) {
        for path in call.paths() {
            if shared.touched.len() < TOUCHED_PATH_LIMIT && !shared.touched.contains(&path) {
                shared.touched.push(path);
            }
        }
    }
    if finished {
        shared.tool_labels.remove(&tool_call_id);
    }
    shared.state = tool_state(&call.kind, &status);
    HarnessEvent::ToolCall {
        tool_call: ToolCall {
            tool_call_id,
            name,
            status,
            summary,
            input,
            output,
            started_at,
            completed_at,
            stats: call.stats(),
        },
    }
}

fn tool_name(call: &AcpToolCall) -> Option<String> {
    call.raw
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| call.title.clone().filter(|title| !title.is_empty()))
}

fn is_mutating(call: &AcpToolCall, name: &str) -> bool {
    if matches!(call.kind, AcpToolKind::Edit) {
        return true;
    }
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "write_to_file"
            | "replace_file_content"
            | "multi_replace_file_content"
            | "notebook_edit"
            | "sed_file"
            | "apply_patch"
            | "edit"
            | "write"
    )
}

fn tool_status(status: &AcpToolStatus) -> ToolCallStatus {
    match status {
        AcpToolStatus::Pending | AcpToolStatus::InProgress => ToolCallStatus::Running,
        AcpToolStatus::Completed => ToolCallStatus::Completed,
        AcpToolStatus::Failed => ToolCallStatus::Failed,
        AcpToolStatus::Cancelled => ToolCallStatus::Failed,
        AcpToolStatus::Other(_) => ToolCallStatus::Running,
    }
}

fn tool_state(kind: &AcpToolKind, status: &ToolCallStatus) -> AgentState {
    match status {
        ToolCallStatus::Running => match kind {
            AcpToolKind::Edit => AgentState::Editing,
            AcpToolKind::Read | AcpToolKind::Fetch => AgentState::Reading,
            AcpToolKind::Search => AgentState::Searching,
            AcpToolKind::Think => AgentState::Thinking,
            _ => AgentState::RunningTool,
        },
        ToolCallStatus::Completed => AgentState::Thinking,
        ToolCallStatus::Failed | ToolCallStatus::Denied => AgentState::Error,
    }
}

fn turn_state(status: TurnStatus, limited: bool) -> AgentState {
    if limited {
        return AgentState::RateLimited;
    }
    match status {
        TurnStatus::Completed => AgentState::Done,
        TurnStatus::Failed | TurnStatus::TokenLimit | TurnStatus::TurnLimit => AgentState::Error,
        TurnStatus::Interrupted => AgentState::Idle,
    }
}

fn usage_from_acp(usage: &AcpUsage, duration_ms: u64) -> Usage {
    Usage {
        // The h4rn counter includes cached input; ACP reports it separately.
        input_tokens: usage
            .input_tokens
            .saturating_add(usage.cached_read_tokens)
            .saturating_add(usage.cached_write_tokens),
        output_tokens: usage.output_tokens,
        cached_input_tokens: usage.cached_read_tokens,
        cost_micros: 0,
        duration_ms,
        ..Usage::default()
    }
}

fn build_prompt(
    text: &str,
    attachments: &[AttachmentRef],
    capabilities: &AgentCapabilities,
) -> Result<Vec<PromptContent>, HarnessError> {
    let mut content = Vec::new();
    let mut notes = Vec::new();
    let mut total_image_bytes = 0usize;

    for attachment in attachments {
        let name = attachment
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| attachment.id.to_string());
        let media_type = attachments::media_type(attachment);
        let class = AttachmentClass::of(&media_type, &attachment.path);

        if attachments::strategy(HarnessKind::Antigravity, class)
            == AttachmentDelivery::AcpImageBlock
        {
            if capabilities.image
                && let Ok(bytes) = std::fs::read(&attachment.path)
                && bytes.len() <= MAX_IMAGE_BYTES
                && total_image_bytes.saturating_add(bytes.len()) <= MAX_TOTAL_IMAGE_BYTES
            {
                total_image_bytes = total_image_bytes.saturating_add(bytes.len());
                content.push(PromptContent::image(
                    base64::engine::general_purpose::STANDARD.encode(&bytes),
                    media_type,
                ));
                continue;
            }
            if capabilities.image {
                notes.push(format!(
                    "[Attached image \"{name}\" could not be inlined; it is saved at: {}]",
                    attachment.path.display()
                ));
            } else {
                notes.push(attachments::path_line(attachment));
            }
            continue;
        }

        if capabilities.embedded_context
            && is_text_media(&media_type)
            && std::fs::metadata(&attachment.path)
                .map(|metadata| metadata.len() <= MAX_TEXT_RESOURCE_BYTES)
                .unwrap_or(false)
            && let Some(body) = attachments::read_text(&attachment.path)
        {
            content.push(PromptContent::resource_text(
                file_uri(&attachment.path),
                media_type,
                body,
            ));
            continue;
        }

        if capabilities.embedded_context {
            let size = std::fs::metadata(&attachment.path)
                .ok()
                .map(|metadata| metadata.len());
            content.push(PromptContent::resource_link(
                file_uri(&attachment.path),
                name,
                (!media_type.is_empty()).then_some(media_type),
                size,
            ));
        } else {
            notes.push(attachments::path_line(attachment));
        }
    }

    let mut full_text = text.to_string();
    if !notes.is_empty() {
        if !full_text.is_empty() {
            full_text.push('\n');
        }
        full_text.push_str(&notes.join("\n"));
    }
    if !full_text.trim().is_empty() {
        content.insert(0, PromptContent::text(full_text));
    }
    if content.is_empty() {
        content.push(PromptContent::text(String::new()));
    }
    Ok(content)
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn is_text_media(media_type: &str) -> bool {
    media_type.starts_with("text/")
        || matches!(
            media_type,
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/toml"
                | "application/yaml"
                | "application/x-yaml"
        )
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;
    use h4rn_protocol::ApprovalDecision;
    use serde_json::json;

    fn attachment(path: &Path, media_type: &str) -> AttachmentRef {
        AttachmentRef {
            id: "blob-1".to_string().into(),
            path: path.to_path_buf(),
            media_type: media_type.to_string(),
        }
    }

    fn acp_pair(
        workspace: &Path,
    ) -> (
        AcpClient,
        BufReader<tokio::io::DuplexStream>,
        tokio::io::DuplexStream,
    ) {
        let (client_write, agent_read) = tokio::io::duplex(64 * 1024);
        let (agent_write, client_read) = tokio::io::duplex(64 * 1024);
        let client = AcpClient::start_for_test(
            AcpClientConfig::new("/usr/local/bin/fake-acp", workspace),
            client_read,
            client_write,
        );
        (client, BufReader::new(agent_read), agent_write)
    }

    fn acp_spec(workspace: &Path) -> SessionSpec {
        SessionSpec {
            workspace: workspace.to_path_buf(),
            model: "default".into(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            resume_id: None,
            mcp_endpoint: None,
            env_overrides: std::collections::BTreeMap::new(),
            attachments: Vec::new(),
        }
    }

    async fn next_frame(reader: &mut BufReader<tokio::io::DuplexStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read a frame");
        serde_json::from_str(line.trim()).expect("valid json")
    }

    async fn send_frame(writer: &mut tokio::io::DuplexStream, frame: Value) {
        let mut bytes = serde_json::to_vec(&frame).expect("encode");
        bytes.push(b'\n');
        writer.write_all(&bytes).await.expect("write");
        writer.flush().await.expect("flush");
    }

    #[test]
    fn approval_decisions_choose_the_matching_option() {
        let options = vec![
            AcpPermissionOption {
                option_id: "allow".into(),
                name: "Allow".into(),
                kind: "allow_once".into(),
            },
            AcpPermissionOption {
                option_id: "deny".into(),
                name: "Deny".into(),
                kind: "reject_once".into(),
            },
        ];
        assert_eq!(
            select_permission_option(&options, ApprovalDecision::Approve, None).as_deref(),
            Some("allow")
        );
        assert_eq!(
            select_permission_option(&options, ApprovalDecision::Deny, None).as_deref(),
            Some("deny")
        );
        assert_eq!(
            select_permission_option(&options, ApprovalDecision::Cancel, None),
            None
        );
    }

    #[test]
    fn prompt_building_embeds_small_text_attachments() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let path = workspace.path().join("notes.md");
        std::fs::write(&path, "# Notes\n").expect("write");
        let capabilities = AgentCapabilities {
            embedded_context: true,
            ..AgentCapabilities::default()
        };
        let content = build_prompt(
            "read this",
            &[attachment(&path, "text/markdown")],
            &capabilities,
        )
        .expect("prompt");
        assert!(matches!(&content[0], PromptContent::Text(text) if text == "read this"));
        assert!(matches!(
            &content[1],
            PromptContent::Resource { text: Some(body), .. } if body == "# Notes\n"
        ));
    }

    #[test]
    fn prompt_building_embeds_utf16_text_attachments() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let path = workspace.path().join("notes.txt");
        let mut encoded = vec![0xFF, 0xFE];
        for unit in "Über notes\n".encode_utf16() {
            encoded.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&path, encoded).expect("write");
        let capabilities = AgentCapabilities {
            embedded_context: true,
            ..AgentCapabilities::default()
        };
        let content =
            build_prompt("", &[attachment(&path, "text/plain")], &capabilities).expect("prompt");
        assert!(
            matches!(
                &content[0],
                PromptContent::Resource { text: Some(body), .. } if body == "Über notes\n"
            ),
            "the UTF-16 body must be embedded, got {content:?}"
        );
    }

    #[test]
    fn images_are_inlined_only_when_capable() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let path = workspace.path().join("pixel.png");
        std::fs::write(&path, [0x89, b'P', b'N', b'G']).expect("write");
        let content = build_prompt(
            "",
            &[attachment(&path, "image/png")],
            &AgentCapabilities::default(),
        )
        .expect("prompt");
        assert!(matches!(
            &content[0],
            PromptContent::Text(text) if text.contains("saved at")
        ));

        let content = build_prompt(
            "",
            &[attachment(&path, "image/png")],
            &AgentCapabilities {
                image: true,
                ..AgentCapabilities::default()
            },
        )
        .expect("prompt");
        assert!(matches!(&content[0], PromptContent::Image { .. }));
    }

    #[test]
    fn usage_includes_cached_input() {
        let usage = AcpUsage {
            input_tokens: 10,
            output_tokens: 2,
            cached_read_tokens: 3,
            cached_write_tokens: 4,
            thought_tokens: 1,
            total_tokens: 19,
        };
        let mapped = usage_from_acp(&usage, 1200);
        assert_eq!(mapped.input_tokens, 17);
        assert_eq!(mapped.output_tokens, 2);
        assert_eq!(mapped.cached_input_tokens, 3);
        assert_eq!(mapped.duration_ms, 1200);
    }

    #[test]
    fn tool_updates_track_paths_and_status() {
        let shared = Arc::new(Mutex::new(AcpShared::new()));
        let call = AcpToolCall::from_value(&json!({
            "toolCallId": "call-1",
            "name": "replace_file_content",
            "kind": "edit",
            "status": "in_progress",
            "title": "Edit src/lib.rs",
            "locations": [{"path": "/w/src/lib.rs", "line": 4}],
            "rawInput": {"path": "/w/src/lib.rs"}
        }));
        let event = tool_event(&call, &shared);
        match event {
            HarnessEvent::ToolCall { tool_call } => {
                assert_eq!(tool_call.name, "replace_file_content");
                assert_eq!(tool_call.status, ToolCallStatus::Running);
                assert_eq!(tool_call.summary, "Edit src/lib.rs");
                assert_eq!(tool_call.completed_at, None);
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
        assert_eq!(lock(&shared).touched, vec!["/w/src/lib.rs"]);
        assert_eq!(lock(&shared).state, AgentState::Editing);
    }

    async fn handshake(
        reader: &mut BufReader<tokio::io::DuplexStream>,
        writer: &mut tokio::io::DuplexStream,
    ) {
        let initialize = next_frame(reader).await;
        assert_eq!(initialize["method"], "initialize");
        send_frame(
            writer,
            json!({"jsonrpc": "2.0", "id": initialize["id"], "result": {"protocolVersion": 1}}),
        )
        .await;

        let new_session = next_frame(reader).await;
        assert_eq!(new_session["method"], "session/new");
        send_frame(
            writer,
            json!({"jsonrpc": "2.0", "id": new_session["id"], "result": {"sessionId": "s-1"}}),
        )
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interrupt_cancels_only_after_the_prompt_is_on_the_wire() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            handshake(&mut reader, &mut writer).await;
            let prompt = next_frame(&mut reader).await;
            assert_eq!(prompt["method"], "session/prompt");
            let cancel = next_frame(&mut reader).await;
            assert_eq!(cancel["method"], "session/cancel");
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": "cancelled"}}),
            )
            .await;
        });

        let mut session = AcpSession::start_with(&client, &acp_spec(workspace.path()))
            .await
            .expect("start session");
        session
            .send(UserInput {
                text: "hello".into(),
                attachments: Vec::new(),
            })
            .await
            .expect("send");
        session.interrupt().await.expect("interrupt");

        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_token_limited_turn_is_not_reported_as_completed() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let (client, mut reader, mut writer) = acp_pair(workspace.path());

        let agent = tokio::spawn(async move {
            handshake(&mut reader, &mut writer).await;
            let prompt = next_frame(&mut reader).await;
            assert_eq!(prompt["method"], "session/prompt");
            send_frame(
                &mut writer,
                json!({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": "max_tokens"}}),
            )
            .await;
        });

        let mut session = AcpSession::start_with(&client, &acp_spec(workspace.path()))
            .await
            .expect("start session");
        let mut events = session.events();
        session
            .send(UserInput {
                text: "hello".into(),
                attachments: Vec::new(),
            })
            .await
            .expect("send");

        let status = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.next().await {
                if let HarnessEvent::TurnCompleted { status, .. } = event {
                    return status;
                }
            }
            panic!("the event stream ended before the turn completed");
        })
        .await
        .expect("timed out waiting for the turn");
        assert_eq!(status, TurnStatus::TokenLimit);
        assert_eq!(
            session
                .last_turn()
                .expect("a turn report")
                .stop_reason
                .as_deref(),
            Some("max_tokens")
        );

        agent.await.expect("agent");
        session.shutdown().await.expect("shutdown");
    }

    #[test]
    fn titleless_tool_updates_keep_the_earlier_name_and_summary() {
        let shared = Arc::new(Mutex::new(AcpShared::new()));
        let started = AcpToolCall::from_value(&json!({
            "toolCallId": "call-1",
            "name": "read_file",
            "kind": "read",
            "status": "in_progress",
            "title": "Read package.json",
            "locations": [{"path": "/w/package.json"}]
        }));
        let HarnessEvent::ToolCall { tool_call } = tool_event(&started, &shared) else {
            panic!("expected a tool call");
        };
        assert_eq!(tool_call.name, "read_file");
        assert_eq!(tool_call.summary, "Read package.json");

        let update = AcpToolCall::from_value(&json!({
            "toolCallId": "call-1",
            "kind": "read",
            "status": "completed",
            "rawOutput": "{\"name\":\"h4rn\"}"
        }));
        let HarnessEvent::ToolCall { tool_call } = tool_event(&update, &shared) else {
            panic!("expected a tool call");
        };
        assert_eq!(tool_call.name, "read_file");
        assert_eq!(tool_call.summary, "Read package.json");
        assert!(tool_call.output.is_some());
        assert!(lock(&shared).tool_labels.is_empty(), "the id is forgotten");
    }
}
