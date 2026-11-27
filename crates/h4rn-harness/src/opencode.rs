//! The OpenCode adapter (ADR-005).
//!
//! # Transport
//!
//! One `opencode serve` process per session, on a random loopback port:
//!
//! ```text
//! opencode serve --hostname 127.0.0.1 --port <n>
//! ```
//!
//! The process is authenticated with a per-session `OPENCODE_SERVER_PASSWORD`
//! (`Basic opencode:<password>`), spawned into its own process group and
//! reaped by the same watchdog as the other adapters (ADR-021). Session
//! creation, prompts, model selection, permission replies and interrupts are
//! HTTP calls; the event stream is consumed from `GET /api/event` as SSE and
//! normalised by [`OpenCodeParser`].
//!
//! # Why one server per session
//!
//! OpenCode v2.2.7 binds a server's *location* (project, config, permission
//! defaults) to its working directory, and `POST /api/session` has no
//! directory parameter. Two workspaces therefore cannot safely share one
//! server: the second session would be created in the first workspace's
//! project. One server per session (equivalently, per workspace) is the only
//! shape the API guarantees today. A per-workspace pool is a future
//! optimisation once OpenCode exposes a location-scoped create call.
//!
//! # Configuration
//!
//! h4rn never edits the user's `opencode.json`. The child receives a generated
//! `OPENCODE_CONFIG_CONTENT` document, which OpenCode merges over the user's
//! global config. A CLIProxyAPI/custom provider is declared there from
//! [`ENV_OPENCODE_BASE_URL`], [`ENV_OPENCODE_API_KEY`],
//! [`ENV_OPENCODE_PROVIDER`], [`ENV_OPENCODE_PACKAGE`] and
//! [`ENV_OPENCODE_MODELS`]. The API key rides the environment, never argv.
//! Model selection is `provider/model`.
//!
//! # MCP injection
//!
//! The h4rn hub is added at runtime with
//! `PUT /api/experimental/mcp/h4rn?location[directory]=<workspace>` and a
//! `remote` config whose `Authorization: Bearer` comes from the
//! `H4RN_MCP_AUTH_TOKEN` entry of [`SessionSpec::env_overrides`] (ADR-006).
//! Nothing is written to the user's config; the server loses the entry when
//! the session shuts down.
//!
//! # Approvals
//!
//! OpenCode emits `permission.asked`; the adapter answers with
//! `POST /api/session/{id}/permission/{request}/reply` using `once`, `always`
//! or `reject`. The session's permission ruleset is set from
//! [`SessionSpec::runtime_mode`] immediately after creation.
//!
//! # Interrupt and shutdown
//!
//! Interrupt is `POST /api/session/{id}/interrupt`. Shutdown aborts the SSE
//! consumer, sends `SIGTERM` to the process group, waits five seconds, then
//! `SIGKILL`s the group.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use futures::stream::{BoxStream, StreamExt};
use h4rn_protocol::{AgentState, ApprovalDecision, HarnessKind, RuntimeMode};
use rand::RngCore;
use reqwest::header::ACCEPT;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::attachments::{self, AttachmentClass, AttachmentDelivery};
use crate::claude_code::process::{GroupSignal, StderrRing, signal_group};
use crate::{
    AttachmentRef, Harness, HarnessError, HarnessEvent, HarnessInstall, HarnessSession,
    SessionSpec, TurnReport, UserInput,
};

mod limit;
mod protocol;

pub use protocol::OpenCodeParser;

/// The h4rn MCP hub bearer token, consumed by the adapter.
pub const ENV_MCP_AUTH_TOKEN: &str = "H4RN_MCP_AUTH_TOKEN";
/// Alias for [`ENV_MCP_AUTH_TOKEN`].
pub const ENV_MCP_BEARER_TOKEN: &str = "H4RN_MCP_BEARER_TOKEN";
/// Base URL of a custom OpenAI-compatible provider (for CLIProxyAPI).
pub const ENV_OPENCODE_BASE_URL: &str = "H4RN_OPENCODE_BASE_URL";
/// API key for the custom provider.
pub const ENV_OPENCODE_API_KEY: &str = "H4RN_OPENCODE_API_KEY";
/// Provider id used as the model prefix. Defaults to `cliproxy`.
pub const ENV_OPENCODE_PROVIDER: &str = "H4RN_OPENCODE_PROVIDER";
/// OpenCode provider package. Defaults to `@ai-sdk/openai-compatible`.
pub const ENV_OPENCODE_PACKAGE: &str = "H4RN_OPENCODE_PACKAGE";
/// Comma-separated model ids to publish on the generated provider.
pub const ENV_OPENCODE_MODELS: &str = "H4RN_OPENCODE_MODELS";
/// Explicit `OPENCODE_CONFIG_CONTENT`; when set, it wins over generation.
pub const ENV_OPENCODE_CONFIG: &str = "OPENCODE_CONFIG_CONTENT";
/// Alias for the CLIProxyAPI base URL.
pub const ENV_CLIPROXY_BASE_URL: &str = "H4RN_CLIPROXY_BASE_URL";
/// Alias for the CLIProxyAPI key.
pub const ENV_CLIPROXY_API_KEY: &str = "H4RN_CLIPROXY_API_KEY";

/// OpenCode changed the location-scoped MCP API in 2.2.6; older servers can
/// still start, but h4rn cannot scope the hub to a workspace.
pub const MINIMUM_OPENCODE_VERSION: &str = "2.2.6";

const SERVER_PASSWORD: &str = "OPENCODE_SERVER_PASSWORD";
const EVENT_CHANNEL_CAPACITY: usize = 512;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const KILL_GRACE: Duration = Duration::from_secs(2);
const MCP_SETTLE: Duration = Duration::from_secs(5);
const DIAGNOSTIC_CHARS: usize = 400;

/// Environment a nested OpenCode must not inherit.
///
/// A daemon started from an OpenCode session would otherwise pass that
/// session's id, password or config content into every agent it launches.
const SCRUBBED_ENV: &[&str] = &[
    "OPENCODE_PASSWORD",
    SERVER_PASSWORD,
    "OPENCODE_SESSION_ID",
    "OPENCODE_TERMINAL",
    "OPENCODE",
    "AI_AGENT",
    ENV_OPENCODE_CONFIG,
];

const H4RN_ONLY_ENV: &[&str] = &[
    ENV_MCP_AUTH_TOKEN,
    ENV_MCP_BEARER_TOKEN,
    ENV_OPENCODE_BASE_URL,
    ENV_OPENCODE_API_KEY,
    ENV_OPENCODE_PROVIDER,
    ENV_OPENCODE_PACKAGE,
    ENV_OPENCODE_MODELS,
    ENV_CLIPROXY_BASE_URL,
    ENV_CLIPROXY_API_KEY,
];

/// The OpenCode adapter.
#[derive(Clone, Debug, Default)]
pub struct OpenCodeHarness {
    install: Option<HarnessInstall>,
}

impl OpenCodeHarness {
    /// Creates an adapter that discovers `opencode` on this host.
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
impl Harness for OpenCodeHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Opencode
    }

    fn detect(&self) -> Option<HarnessInstall> {
        self.install.clone().or_else(detect_opencode)
    }

    async fn list_models(
        &self,
        install: &HarnessInstall,
        cwd: &std::path::Path,
    ) -> Result<Vec<String>, std::io::Error> {
        let mut command = cli_command(&install.path);
        command.current_dir(cwd).arg("models");
        for key in SCRUBBED_ENV {
            command.env_remove(key);
        }
        let output = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .stderr(std::process::Stdio::null())
            .output()
            .await?;
        if !output.status.success() {
            return Err(std::io::Error::other("OpenCode model listing failed"));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| {
                !line.contains(char::is_whitespace)
                    && line
                        .split_once('/')
                        .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty())
            })
            .map(str::to_owned)
            .collect())
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
        if let Some(version) = parse_semver(&install.version) {
            if version < parse_semver(MINIMUM_OPENCODE_VERSION).unwrap_or_default() {
                return Err(HarnessError::Spawn {
                    harness: self.kind(),
                    message: format!(
                        "opencode {} is too old; h4rn needs {MINIMUM_OPENCODE_VERSION} or newer",
                        install.version
                    ),
                });
            }
        }

        let port = reserve_port().map_err(|message| HarnessError::Spawn {
            harness: self.kind(),
            message,
        })?;
        let password = random_password();
        let base_url = format!("http://127.0.0.1:{port}");
        let stderr_ring = StderrRing::new();

        let mut command = std::process::Command::new(&install.path);
        command
            .arg("serve")
            .arg("--hostname")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(port.to_string())
            .current_dir(&spec.workspace)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());

        for key in SCRUBBED_ENV {
            command.env_remove(key);
        }
        for (key, value) in &spec.env_overrides {
            if !H4RN_ONLY_ENV.contains(&key.as_str()) {
                command.env(key, value);
            }
        }
        command.env(SERVER_PASSWORD, &password);
        command.env(ENV_OPENCODE_CONFIG, config_content_for(&spec));

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
            message: "opencode exited before it reported a process id".into(),
        })?;

        let Some(stderr) = child.stderr.take() else {
            signal_group(pid, GroupSignal::Kill);
            return Err(HarnessError::Spawn {
                harness: self.kind(),
                message: "opencode was spawned without a stderr pipe".into(),
            });
        };
        let stderr_task = spawn_stderr_reader(stderr, stderr_ring.clone());

        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|error| HarnessError::Spawn {
                harness: self.kind(),
                message: format!("failed to build the OpenCode HTTP client: {error}"),
            })?;

        if let Err(error) = wait_ready(&http, &base_url, &password, &mut child, &stderr_ring).await
        {
            stderr_task.abort();
            signal_group(pid, GroupSignal::Kill);
            return Err(error);
        }

        let parser = Arc::new(Mutex::new(OpenCodeParser::new()));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let (events_tx, events_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let sse_task = spawn_sse(
            http.clone(),
            base_url.clone(),
            password.clone(),
            parser.clone(),
            events_tx.clone(),
            shutting_down.clone(),
        );

        if let Err(error) = handshake(&http, &base_url, &password, &spec, &parser, &events_tx).await
        {
            shutting_down.store(true, Ordering::SeqCst);
            sse_task.abort();
            signal_group(pid, GroupSignal::Kill);
            return Err(error);
        }

        let supervisor = spawn_supervisor(child, shutting_down.clone(), events_tx.clone());

        Ok(Box::new(OpenCodeSession {
            kind: self.kind(),
            http,
            base_url,
            password,
            pid,
            parser,
            events_rx: Some(events_rx),
            sse_task: Some(sse_task),
            supervisor: Some(supervisor),
            stderr: stderr_ring,
            shutting_down,
            first_attachments: spec.attachments.clone(),
            first_send: true,
            closed: false,
        }))
    }
}

/// The session returned by [`OpenCodeHarness`].
pub struct OpenCodeSession {
    kind: HarnessKind,
    http: reqwest::Client,
    base_url: String,
    password: String,
    pid: u32,
    parser: Arc<Mutex<OpenCodeParser>>,
    events_rx: Option<mpsc::Receiver<HarnessEvent>>,
    sse_task: Option<tokio::task::JoinHandle<()>>,
    supervisor: Option<tokio::task::JoinHandle<()>>,
    stderr: StderrRing,
    shutting_down: Arc<AtomicBool>,
    first_attachments: Vec<AttachmentRef>,
    first_send: bool,
    closed: bool,
}

impl OpenCodeSession {
    /// The child's process id (also its process-group id on Unix).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The loopback base URL of this session's server.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The model reported by the event stream (`provider/model`).
    pub fn model(&self) -> Option<String> {
        lock(&self.parser).model().map(str::to_string)
    }

    fn current_session_id(&self) -> Option<String> {
        lock(&self.parser).session_id().map(str::to_string)
    }

    fn ensure_open(&self) -> Result<(), HarnessError> {
        if self.closed || self.shutting_down.load(Ordering::SeqCst) {
            return Err(HarnessError::Transport {
                harness: self.kind,
                message: "session is shut down".into(),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl HarnessSession for OpenCodeSession {
    fn kind(&self) -> HarnessKind {
        self.kind
    }

    async fn set_runtime_mode(&mut self, mode: RuntimeMode) -> Result<(), HarnessError> {
        self.ensure_open()?;
        let session_id = self
            .current_session_id()
            .ok_or_else(|| HarnessError::Transport {
                harness: self.kind,
                message: "opencode session has no server session id".into(),
            })?;
        let response = self
            .http
            .patch(format!("{}/api/session/{session_id}", self.base_url))
            .basic_auth("opencode", Some(self.password.as_str()))
            .timeout(REQUEST_TIMEOUT)
            .json(&json!({ "permissions": permissions_for(mode) }))
            .send()
            .await
            .map_err(|error| HarnessError::Transport {
                harness: self.kind,
                message: format!("opencode permission switch failed: {error}"),
            })?;
        ensure_success(response, "permission switch", self.kind).await?;
        Ok(())
    }

    async fn set_model(&mut self, model: &str) -> Result<(), HarnessError> {
        self.ensure_open()?;
        let Some((provider, model)) = parse_model(model) else {
            return Err(HarnessError::Unsupported {
                harness: self.kind,
                feature: unsupported_model(model),
            });
        };
        let session_id = self
            .current_session_id()
            .ok_or_else(|| HarnessError::Transport {
                harness: self.kind,
                message: "opencode session has no server session id".into(),
            })?;
        let response = self
            .http
            .post(format!("{}/api/session/{session_id}/model", self.base_url))
            .basic_auth("opencode", Some(self.password.as_str()))
            .timeout(REQUEST_TIMEOUT)
            .json(&json!({ "model": { "id": model, "providerID": provider } }))
            .send()
            .await
            .map_err(|error| HarnessError::Transport {
                harness: self.kind,
                message: format!("opencode model switch failed: {error}"),
            })?;
        ensure_success(response, "model switch", self.kind).await?;
        Ok(())
    }

    async fn send(&mut self, input: UserInput) -> Result<(), HarnessError> {
        self.ensure_open()?;
        let session_id = self
            .current_session_id()
            .ok_or_else(|| HarnessError::Transport {
                harness: self.kind,
                message: "opencode session has no id".into(),
            })?;

        let mut attachments = input.attachments;
        if self.first_send {
            self.first_send = false;
            attachments.extend(self.first_attachments.iter().cloned());
        }
        let (text, files) = build_prompt(&input.text, &attachments);
        let mut body = json!({ "text": text });
        if !files.is_empty() {
            body["files"] = Value::Array(files);
        }

        let response = self
            .http
            .post(format!("{}/api/session/{session_id}/prompt", self.base_url))
            .basic_auth("opencode", Some(&self.password))
            .timeout(REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(|error| HarnessError::Transport {
                harness: self.kind,
                message: format!("opencode prompt failed: {error}"),
            })?;
        ensure_success(response, "prompt", self.kind).await?;
        lock(&self.parser).note_thinking();
        Ok(())
    }

    async fn interrupt(&mut self) -> Result<(), HarnessError> {
        self.ensure_open()?;
        let Some(session_id) = self.current_session_id() else {
            return Ok(());
        };
        let response = self
            .http
            .post(format!(
                "{}/api/session/{session_id}/interrupt",
                self.base_url
            ))
            .basic_auth("opencode", Some(&self.password))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| HarnessError::Transport {
                harness: self.kind,
                message: format!("opencode interrupt failed: {error}"),
            })?;
        ensure_success(response, "interrupt", self.kind).await?;
        Ok(())
    }

    async fn respond_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
        option_id: Option<&str>,
    ) -> Result<(), HarnessError> {
        self.ensure_open()?;
        let session_id = self
            .current_session_id()
            .ok_or_else(|| HarnessError::Transport {
                harness: self.kind,
                message: "opencode session has no id".into(),
            })?;
        let reply = permission_decision(decision, option_id);
        let response = self
            .http
            .post(format!(
                "{}/api/session/{session_id}/permission/{approval_id}/reply",
                self.base_url
            ))
            .basic_auth("opencode", Some(&self.password))
            .timeout(REQUEST_TIMEOUT)
            .json(&json!({ "decision": reply }))
            .send()
            .await
            .map_err(|error| HarnessError::Transport {
                harness: self.kind,
                message: format!("opencode permission reply failed: {error}"),
            })?;
        ensure_success(response, "permission reply", self.kind).await?;
        Ok(())
    }

    fn events(&mut self) -> BoxStream<'static, HarnessEvent> {
        match self.events_rx.take() {
            Some(receiver) => ReceiverStream::new(receiver).boxed(),
            None => futures::stream::empty().boxed(),
        }
    }

    async fn shutdown(&mut self) -> Result<(), HarnessError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.shutting_down.store(true, Ordering::SeqCst);
        if let Some(task) = self.sse_task.take() {
            task.abort();
        }
        signal_group(self.pid, GroupSignal::Terminate);

        if let Some(mut supervisor) = self.supervisor.take() {
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut supervisor).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(
                        target: "h4rn_harness::opencode",
                        %error,
                        "opencode supervisor task failed"
                    );
                }
                Err(_) => {
                    signal_group(self.pid, GroupSignal::Kill);
                    match tokio::time::timeout(KILL_GRACE, &mut supervisor).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(
                                target: "h4rn_harness::opencode",
                                %error,
                                "opencode supervisor task failed after kill"
                            );
                        }
                        Err(_) => supervisor.abort(),
                    }
                }
            }
        }
        Ok(())
    }

    fn session_id(&self) -> Option<String> {
        self.current_session_id()
    }

    fn agent_state(&self) -> AgentState {
        lock(&self.parser).agent_state()
    }

    fn last_turn(&self) -> Option<TurnReport> {
        lock(&self.parser).last_turn().cloned()
    }

    fn touched_paths(&self) -> Vec<String> {
        lock(&self.parser).touched_paths().to_vec()
    }

    fn stderr_tail(&self) -> String {
        self.stderr.snapshot()
    }
}

impl Drop for OpenCodeSession {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.shutting_down.store(true, Ordering::SeqCst);
        if let Some(task) = self.sse_task.take() {
            task.abort();
        }
        signal_group(self.pid, GroupSignal::Kill);
    }
}

/// Parses a `provider/model` selection.
///
/// Empty values and the sentinels `default`/`auto` mean "no explicit
/// selection": a session start lets the server pick, which is why they are not
/// a `(provider, model)` pair. A live session cannot switch back to that choice
/// (`[`unsupported_model`]` says so), only to a named model.
pub fn parse_model(model: &str) -> Option<(String, String)> {
    let model = model.trim();
    if model.is_empty()
        || model.eq_ignore_ascii_case("default")
        || model.eq_ignore_ascii_case("auto")
    {
        return None;
    }
    let (provider, id) = model.split_once('/')?;
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    Some((provider.to_string(), id.to_string()))
}

/// Why [`parse_model`] returned nothing, in the person's terms.
fn unsupported_model(model: &str) -> String {
    let model = model.trim();
    if model.is_empty()
        || model.eq_ignore_ascii_case("default")
        || model.eq_ignore_ascii_case("auto")
    {
        "the server's default model; a session start lets OpenCode pick it, while a live session can only switch to a `provider/model`".to_string()
    } else {
        format!("model `{model}`; OpenCode models are `provider/model`")
    }
}

/// Builds the generated `OPENCODE_CONFIG_CONTENT` document for a session.
///
/// Returns `None` when no custom provider override was supplied. Callers then
/// send `{}`, which leaves the user's global config intact.
pub fn provider_config(spec: &SessionSpec) -> Option<Value> {
    let base_url = env_override(spec, ENV_OPENCODE_BASE_URL)
        .or_else(|| env_override(spec, ENV_CLIPROXY_BASE_URL));
    let api_key = env_override(spec, ENV_OPENCODE_API_KEY)
        .or_else(|| env_override(spec, ENV_CLIPROXY_API_KEY));
    if base_url.is_none() && api_key.is_none() {
        return None;
    }

    let provider = env_override(spec, ENV_OPENCODE_PROVIDER).unwrap_or_else(|| "cliproxy".into());
    let package = env_override(spec, ENV_OPENCODE_PACKAGE)
        .unwrap_or_else(|| "@ai-sdk/openai-compatible".into());

    let mut settings = serde_json::Map::new();
    if let Some(base_url) = base_url {
        settings.insert("baseURL".into(), Value::String(base_url));
    }
    if let Some(api_key) = api_key {
        settings.insert("apiKey".into(), Value::String(api_key));
    }

    let mut models = serde_json::Map::new();
    if let Some(list) = env_override(spec, ENV_OPENCODE_MODELS) {
        for model in list
            .split(',')
            .map(str::trim)
            .filter(|model| !model.is_empty())
        {
            models.insert(model.to_string(), json!({}));
        }
    }
    if let Some((model_provider, model_id)) = parse_model(&spec.model) {
        if model_provider == provider {
            models.insert(model_id, json!({}));
        }
    }

    let mut providers = serde_json::Map::new();
    providers.insert(
        provider,
        json!({
            "name": "CLIProxyAPI",
            "package": package,
            "settings": Value::Object(settings),
            "models": Value::Object(models),
        }),
    );
    Some(json!({ "providers": Value::Object(providers) }))
}

/// The `OPENCODE_CONFIG_CONTENT` value a session's child receives.
pub fn config_content_for(spec: &SessionSpec) -> String {
    if let Some(explicit) = spec.env_overrides.get(ENV_OPENCODE_CONFIG) {
        if !explicit.trim().is_empty() {
            return explicit.clone();
        }
    }
    provider_config(spec)
        .map(|config| config.to_string())
        .unwrap_or_else(|| "{}".into())
}

/// Builds the prompt text and the `files` array for the prompt API.
///
/// Images become `file://` parts, which OpenCode reads and inlines on the
/// server; every other attachment degrades to a path line in the prompt so it
/// is never silently dropped (ADR-008).
pub fn build_prompt(text: &str, attachments: &[AttachmentRef]) -> (String, Vec<Value>) {
    let mut text = text.to_string();
    let mut files = Vec::new();
    for attachment in attachments {
        let media_type = attachments::media_type(attachment);
        let class = AttachmentClass::of(&media_type, &attachment.path);
        let name = attachment
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("attachment");
        // OpenCode reads a `file://` part on the server, so images ride that
        // channel when the file is local. Everything else degrades to a path
        // mention, matching the ADR-008 matrix.
        if attachments::strategy(HarnessKind::Opencode, class) == AttachmentDelivery::FileUrl
            && attachment.path.is_file()
            && let Ok(url) = reqwest::Url::from_file_path(&attachment.path)
        {
            let uri = url.as_str().to_string();
            files.push(json!({
                "uri": uri,
                "source": { "type": "uri", "uri": uri },
                "name": name,
            }));
            continue;
        }
        text.push('\n');
        text.push_str(&attachments::path_line(attachment));
    }
    (text, files)
}

/// The permission rules OpenCode should apply to a session.
pub fn permissions_for(mode: RuntimeMode) -> Value {
    let (shell, edit) = match mode {
        RuntimeMode::ApprovalRequired | RuntimeMode::Plan => ("ask", "ask"),
        RuntimeMode::AutoAcceptEdits => ("ask", "allow"),
        RuntimeMode::FullAccess => ("allow", "allow"),
    };
    json!([
        { "action": "shell", "resource": "*", "effect": shell },
        { "action": "edit", "resource": "*", "effect": edit },
    ])
}

/// Maps an approval decision to an OpenCode permission reply.
pub fn permission_decision(decision: ApprovalDecision, option_id: Option<&str>) -> &'static str {
    match decision {
        ApprovalDecision::Approve => match option_id {
            Some("always") => "always",
            _ => "once",
        },
        ApprovalDecision::Deny | ApprovalDecision::Cancel => "reject",
    }
}

/// The MCP add payload for the h4rn hub, when the spec supplies an endpoint.
pub fn mcp_payload(spec: &SessionSpec) -> Option<Value> {
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

    let mut config = json!({
        "type": "remote",
        "url": endpoint,
        "oauth": false,
    });
    if let Some(token) = token {
        config["headers"] = json!({ "Authorization": format!("Bearer {token}") });
    }
    Some(json!({ "config": config }))
}

/// Detects an `opencode` installation on this host.
pub fn detect_opencode() -> Option<HarnessInstall> {
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

async fn handshake(
    http: &reqwest::Client,
    base_url: &str,
    password: &str,
    spec: &SessionSpec,
    parser: &Arc<Mutex<OpenCodeParser>>,
    events_tx: &mpsc::Sender<HarnessEvent>,
) -> Result<(), HarnessError> {
    let kind = HarnessKind::Opencode;
    let session_id = match spec.resume_id.as_deref().filter(|id| !id.is_empty()) {
        Some(resume_id) => {
            let response = http
                .get(format!("{base_url}/api/session/{resume_id}"))
                .basic_auth("opencode", Some(password))
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await
                .map_err(|error| HarnessError::Transport {
                    harness: kind,
                    message: format!("opencode session lookup failed: {error}"),
                })?;
            let value = ensure_success(response, "session lookup", kind).await?;
            let value: Value = value.json().await.map_err(|error| HarnessError::Protocol {
                harness: kind,
                message: format!("opencode session lookup returned invalid JSON: {error}"),
            })?;
            value["data"]["id"]
                .as_str()
                .unwrap_or(resume_id)
                .to_string()
        }
        None => {
            let response = http
                .post(format!("{base_url}/api/session"))
                .basic_auth("opencode", Some(password))
                .timeout(REQUEST_TIMEOUT)
                .json(&json!({}))
                .send()
                .await
                .map_err(|error| HarnessError::Transport {
                    harness: kind,
                    message: format!("opencode session creation failed: {error}"),
                })?;
            let value = ensure_success(response, "session creation", kind).await?;
            let value: Value = value.json().await.map_err(|error| HarnessError::Protocol {
                harness: kind,
                message: format!("opencode session creation returned invalid JSON: {error}"),
            })?;
            value["data"]["id"]
                .as_str()
                .ok_or_else(|| HarnessError::Protocol {
                    harness: kind,
                    message: "opencode did not return a session id".into(),
                })?
                .to_string()
        }
    };

    let announce = lock(parser).adopt_session(&session_id);
    if announce {
        let _ = events_tx
            .send(HarnessEvent::SessionStarted {
                session_id: session_id.clone(),
            })
            .await;
    }

    if let Some((provider, model)) = parse_model(&spec.model) {
        let response = http
            .post(format!("{base_url}/api/session/{session_id}/model"))
            .basic_auth("opencode", Some(password))
            .timeout(REQUEST_TIMEOUT)
            .json(&json!({ "model": { "id": model, "providerID": provider } }))
            .send()
            .await
            .map_err(|error| HarnessError::Transport {
                harness: kind,
                message: format!("opencode model selection failed: {error}"),
            })?;
        ensure_success(response, "model selection", kind).await?;
    }

    let response = http
        .patch(format!("{base_url}/api/session/{session_id}"))
        .basic_auth("opencode", Some(password))
        .timeout(REQUEST_TIMEOUT)
        .json(&json!({ "permissions": permissions_for(spec.runtime_mode) }))
        .send()
        .await
        .map_err(|error| HarnessError::Transport {
            harness: kind,
            message: format!("opencode permission setup failed: {error}"),
        })?;
    ensure_success(response, "permission setup", kind).await?;

    if let Some(payload) = mcp_payload(spec) {
        let directory = spec.workspace.to_string_lossy().to_string();
        let response = http
            .put(format!("{base_url}/api/experimental/mcp/h4rn"))
            .query(&[("location[directory]", directory.as_str())])
            .basic_auth("opencode", Some(password))
            .timeout(REQUEST_TIMEOUT)
            .json(&payload)
            .send()
            .await
            .map_err(|error| HarnessError::Transport {
                harness: kind,
                message: format!("opencode MCP injection failed: {error}"),
            })?;
        ensure_success(response, "MCP injection", kind).await?;
        wait_for_mcp(http, base_url, password, &directory).await?;
    }

    Ok(())
}

async fn wait_for_mcp(
    http: &reqwest::Client,
    base_url: &str,
    password: &str,
    directory: &str,
) -> Result<(), HarnessError> {
    let kind = HarnessKind::Opencode;
    let deadline = tokio::time::Instant::now() + MCP_SETTLE;
    loop {
        let response = http
            .get(format!("{base_url}/api/mcp"))
            .query(&[("location[directory]", directory)])
            .basic_auth("opencode", Some(password))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| HarnessError::Transport {
                harness: kind,
                message: format!("opencode MCP status failed: {error}"),
            })?;
        let value = ensure_success(response, "MCP status", kind).await?;
        let value: Value = value.json().await.map_err(|error| HarnessError::Protocol {
            harness: kind,
            message: format!("opencode MCP status returned invalid JSON: {error}"),
        })?;
        if let Some(servers) = value["data"].as_array() {
            if let Some(server) = servers.iter().find(|server| server["name"] == "h4rn") {
                match server["status"]["status"].as_str().unwrap_or_default() {
                    "connected" => return Ok(()),
                    "failed" | "needs_auth" => {
                        let message = server["status"]["error"]
                            .as_str()
                            .unwrap_or("the h4rn MCP hub did not connect");
                        return Err(HarnessError::Protocol {
                            harness: kind,
                            message: format!("opencode MCP injection failed: {message}"),
                        });
                    }
                    _ => {}
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                target: "h4rn_harness::opencode",
                "h4rn MCP server did not report connected before the settle deadline"
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_ready(
    http: &reqwest::Client,
    base_url: &str,
    password: &str,
    child: &mut tokio::process::Child,
    stderr: &StderrRing,
) -> Result<(), HarnessError> {
    let kind = HarnessKind::Opencode;
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().map_err(|error| HarnessError::Spawn {
            harness: kind,
            message: format!("failed to inspect the opencode process: {error}"),
        })? {
            return Err(HarnessError::Spawn {
                harness: kind,
                message: format!(
                    "opencode exited during startup ({status}): {}",
                    truncate(stderr.snapshot().trim(), DIAGNOSTIC_CHARS)
                ),
            });
        }

        match http
            .get(format!("{base_url}/api/info"))
            .basic_auth("opencode", Some(password))
            .timeout(Duration::from_secs(1))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
                return Err(HarnessError::Protocol {
                    harness: kind,
                    message: "opencode rejected the generated server password".into(),
                });
            }
            _ => {}
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(HarnessError::Spawn {
                harness: kind,
                message: format!(
                    "opencode did not become ready within {}s: {}",
                    READY_TIMEOUT.as_secs(),
                    truncate(stderr.snapshot().trim(), DIAGNOSTIC_CHARS)
                ),
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn ensure_success(
    response: reqwest::Response,
    operation: &str,
    kind: HarnessKind,
) -> Result<reqwest::Response, HarnessError> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(HarnessError::Protocol {
        harness: kind,
        message: format!(
            "opencode {operation} failed ({status}): {}",
            truncate(body.trim(), DIAGNOSTIC_CHARS)
        ),
    })
}

fn spawn_stderr_reader(
    stderr: tokio::process::ChildStderr,
    ring: StderrRing,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            ring.push(&line);
        }
    })
}

/// Decodes streamed bytes without turning a split UTF-8 sequence into U+FFFD.
///
/// Chunks end at arbitrary byte boundaries; an incomplete trailing sequence is
/// carried into the next chunk, while genuinely invalid bytes decode lossily.
#[derive(Default)]
struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    fn push(&mut self, bytes: &[u8]) -> String {
        // Only the incomplete suffix of the previous chunk is carried over, so
        // walking a chunk of invalid bytes never shifts the whole buffer.
        self.pending.extend_from_slice(bytes);
        let mut text = String::with_capacity(self.pending.len());
        let mut cursor = 0;
        loop {
            match std::str::from_utf8(&self.pending[cursor..]) {
                Ok(complete) => {
                    text.push_str(complete);
                    cursor = self.pending.len();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if let Ok(prefix) = std::str::from_utf8(&self.pending[cursor..cursor + valid]) {
                        text.push_str(prefix);
                    }
                    cursor += valid;
                    match error.error_len() {
                        Some(len) => {
                            text.push(char::REPLACEMENT_CHARACTER);
                            cursor += len;
                        }
                        None => break,
                    }
                }
            }
        }
        // Keep only the bytes that could start a sequence continued by the
        // next chunk.
        self.pending.drain(..cursor);
        text
    }

    /// Flushes the end of a stream: leftover partial bytes become one U+FFFD
    /// instead of being dropped silently.
    fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        self.pending.clear();
        String::from(char::REPLACEMENT_CHARACTER)
    }
}

/// Turns one OpenCode event-stream chunk into complete lines.
///
/// The UTF-8 decoder and the line buffer are one step, so this is the function
/// the stream loop calls: a character split across two chunks is decoded once,
/// not two replacement characters, and a line split across chunks is joined
/// before the parser sees it. The unfinished line stays buffered for the next
/// chunk.
#[derive(Default)]
struct SseLines {
    utf8: Utf8Stream,
    buffer: String,
}

impl SseLines {
    /// Decodes a chunk and returns the lines it completed.
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let text = self.utf8.push(chunk);
        self.split(&text)
    }

    /// Finishes the stream. A truncated trailing character becomes one
    /// replacement character; an unterminated last line is not dispatched, just
    /// as it never was on a live stream.
    fn finish(&mut self) -> Vec<String> {
        let text = self.utf8.finish();
        self.split(&text)
    }

    fn split(&mut self, text: &str) -> Vec<String> {
        self.buffer.push_str(text);
        let mut lines = Vec::new();
        while let Some(position) = self.buffer.find('\n') {
            let line = self.buffer[..position].to_string();
            self.buffer.drain(..=position);
            lines.push(line);
        }
        lines
    }
}

fn spawn_sse(
    http: reqwest::Client,
    base_url: String,
    password: String,
    parser: Arc<Mutex<OpenCodeParser>>,
    events_tx: mpsc::Sender<HarnessEvent>,
    shutting_down: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let url = format!("{base_url}/api/event");
        loop {
            if shutting_down.load(Ordering::SeqCst) || events_tx.is_closed() {
                break;
            }

            match http
                .get(&url)
                .basic_auth("opencode", Some(&password))
                .header(ACCEPT, "text/event-stream")
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    let mut stream = response.bytes_stream();
                    let mut lines = SseLines::default();
                    let mut finished = false;
                    'stream: loop {
                        let complete = match stream.next().await {
                            Some(Ok(bytes)) => lines.push(&bytes),
                            Some(Err(error)) => {
                                if !shutting_down.load(Ordering::SeqCst) {
                                    let _ = events_tx
                                        .send(HarnessEvent::Error {
                                            message: format!(
                                                "opencode event stream failed: {error}"
                                            ),
                                            retryable: true,
                                        })
                                        .await;
                                }
                                break 'stream;
                            }
                            None if !finished => {
                                finished = true;
                                lines.finish()
                            }
                            None => break 'stream,
                        };
                        for line in complete {
                            let events = {
                                let mut parser = lock(&parser);
                                parser.push_line(&line)
                            };
                            for event in events {
                                if events_tx.send(event).await.is_err() {
                                    break 'stream;
                                }
                            }
                        }
                    }
                }
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    if !shutting_down.load(Ordering::SeqCst) {
                        let _ = events_tx
                            .send(HarnessEvent::Error {
                                message: format!(
                                    "opencode event stream rejected ({status}): {}",
                                    truncate(body.trim(), DIAGNOSTIC_CHARS)
                                ),
                                retryable: true,
                            })
                            .await;
                    }
                }
                Err(error) => {
                    if !shutting_down.load(Ordering::SeqCst) {
                        let _ = events_tx
                            .send(HarnessEvent::Error {
                                message: format!("opencode event stream failed: {error}"),
                                retryable: true,
                            })
                            .await;
                    }
                }
            }

            if shutting_down.load(Ordering::SeqCst) || events_tx.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
}

fn spawn_supervisor(
    mut child: tokio::process::Child,
    shutting_down: Arc<AtomicBool>,
    events_tx: mpsc::Sender<HarnessEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let status = child.wait().await;
        if !shutting_down.load(Ordering::SeqCst) {
            shutting_down.store(true, Ordering::SeqCst);
            let message = match status {
                Ok(status) => format!("opencode server exited unexpectedly ({status})"),
                Err(error) => format!("failed waiting for opencode server: {error}"),
            };
            let _ = events_tx
                .send(HarnessEvent::Error {
                    message,
                    retryable: false,
                })
                .await;
        }
    })
}

fn env_override(spec: &SessionSpec, key: &str) -> Option<String> {
    spec.env_overrides
        .get(key)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn reserve_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| format!("failed to reserve a loopback port: {error}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| format!("failed to read the reserved loopback port: {error}"))
}

fn random_password() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
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
        vec!["opencode.exe", "opencode.cmd", "opencode.bat", "opencode"]
    } else {
        vec!["opencode"]
    }
}

fn probe_version(path: &Path) -> Option<String> {
    let output = cli_command(path).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(version) = parse_version(&stdout) {
        return Some(version);
    }
    parse_version(&String::from_utf8_lossy(&output.stderr))
}

fn cli_command(path: &Path) -> std::process::Command {
    #[cfg(windows)]
    {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        if matches!(extension.as_deref(), Some("cmd" | "bat")) {
            let mut command = std::process::Command::new("cmd");
            command.arg("/C").arg(path);
            return command;
        }
    }
    std::process::Command::new(path)
}

/// Extracts `2.2.7` from `opencode v2.2.7`.
pub fn parse_version(text: &str) -> Option<String> {
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

/// Parses a dotted version into comparable parts.
pub fn parse_semver(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split(['-', '+']).next()?.trim();
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let patch = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Some((major, minor, patch))
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
    use std::collections::BTreeMap;

    use h4rn_protocol::AttachmentId;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    fn spec() -> SessionSpec {
        crate::test_spec()
    }

    #[test]
    fn parses_provider_model_selections() {
        assert_eq!(
            parse_model("cliproxy/deepseek-v4.1-flash"),
            Some(("cliproxy".into(), "deepseek-v4.1-flash".into()))
        );
        assert_eq!(
            parse_model("synthetic/hf:deepseek-ai/DeepSeek-V4.1-Flash"),
            Some((
                "synthetic".into(),
                "hf:deepseek-ai/DeepSeek-V4.1-Flash".into()
            ))
        );
        assert_eq!(parse_model("default"), None);
        assert_eq!(parse_model("deepseek-v4.1-flash"), None);
        assert_eq!(parse_model(""), None);
    }

    #[test]
    fn a_default_model_is_refused_in_words_that_match_the_parser() {
        let message = unsupported_model("default");
        assert!(
            message.contains("server's default") && message.contains("session start"),
            "the reason explains what the sentinel means: {message}"
        );
        assert!(
            !message.contains("OpenCode models are"),
            "the sentinel is not a malformed provider/model: {message}"
        );
        assert!(unsupported_model("no-slash").contains("provider/model"));
    }

    #[test]
    fn generated_config_declares_the_custom_provider() {
        let mut spec = spec();
        spec.model = "cpa/gpt-5.1".into();
        spec.env_overrides.insert(
            ENV_OPENCODE_BASE_URL.into(),
            "http://127.0.0.1:8317/v1".into(),
        );
        spec.env_overrides
            .insert(ENV_OPENCODE_API_KEY.into(), "secret".into());
        spec.env_overrides
            .insert(ENV_OPENCODE_PROVIDER.into(), "cpa".into());

        let config = provider_config(&spec).expect("config");
        assert_eq!(
            config["providers"]["cpa"]["settings"]["baseURL"],
            "http://127.0.0.1:8317/v1"
        );
        assert_eq!(config["providers"]["cpa"]["settings"]["apiKey"], "secret");
        assert_eq!(config["providers"]["cpa"]["models"]["gpt-5.1"], json!({}));
        assert!(config_content_for(&spec).contains("\"cpa\""));
    }

    #[test]
    fn an_explicit_config_content_wins() {
        let mut spec = spec();
        spec.env_overrides.insert(
            ENV_OPENCODE_CONFIG.into(),
            "{\"model\":\"cliproxy/x\"}".into(),
        );
        assert_eq!(
            config_content_for(&spec),
            "{\"model\":\"cliproxy/x\"}".to_string()
        );
    }

    /// One HTTP request the adapter sent to the fake OpenCode server.
    struct CapturedRequest {
        method: String,
        path: String,
        authorization: Option<String>,
        body: Value,
    }

    /// Stands in for `opencode serve`: answers one request and returns it.
    async fn one_request_server() -> (String, tokio::task::JoinHandle<CapturedRequest>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let address = listener.local_addr().expect("the bound address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept a request");
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = socket.read(&mut chunk).await.expect("read the request");
                assert!(read > 0, "the request ended before its headers");
                buffer.extend_from_slice(&chunk[..read]);
                let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&buffer[..end]).to_string();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                while buffer.len() < end + 4 + content_length {
                    let read = socket.read(&mut chunk).await.expect("read the body");
                    assert!(read > 0, "the request ended before its body");
                    buffer.extend_from_slice(&chunk[..read]);
                }
                let mut lines = headers.lines();
                let request_line = lines.next().unwrap_or_default().to_string();
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let authorization = lines.find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("authorization")
                        .then(|| value.trim().to_string())
                });
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                    )
                    .await
                    .expect("answer the request");
                let body = String::from_utf8_lossy(&buffer[end + 4..]).to_string();
                return CapturedRequest {
                    method,
                    path,
                    authorization,
                    body: serde_json::from_str(&body).unwrap_or(Value::Null),
                };
            }
        });
        (format!("http://{address}"), server)
    }

    /// A live session pointed at a fake server, already on a session id.
    fn test_session(base_url: String) -> OpenCodeSession {
        let parser = Arc::new(Mutex::new(OpenCodeParser::new()));
        assert!(lock(&parser).adopt_session("ses_test"));
        OpenCodeSession {
            kind: HarnessKind::Opencode,
            http: reqwest::Client::new(),
            base_url,
            password: "test-password".into(),
            pid: 0,
            parser,
            events_rx: None,
            sse_task: None,
            supervisor: None,
            stderr: StderrRing::new(),
            shutting_down: Arc::new(AtomicBool::new(false)),
            first_attachments: Vec::new(),
            first_send: true,
            closed: false,
        }
    }

    #[tokio::test]
    async fn the_permission_switch_patches_the_sessions_ruleset() {
        let (base_url, server) = one_request_server().await;
        let mut session = test_session(base_url);
        session
            .set_runtime_mode(RuntimeMode::AutoAcceptEdits)
            .await
            .expect("switch the permission mode");
        let request = server.await.expect("the fake server");

        assert_eq!(request.method, "PATCH");
        assert_eq!(request.path, "/api/session/ses_test");
        let expected = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", "opencode", "test-password"))
        );
        assert_eq!(request.authorization.as_deref(), Some(expected.as_str()));
        assert_eq!(
            request.body,
            json!({ "permissions": permissions_for(RuntimeMode::AutoAcceptEdits) }),
            "the switch sends the new ruleset for this mode, nothing else"
        );
    }

    #[tokio::test]
    async fn the_model_switch_posts_the_provider_and_model() {
        let (base_url, server) = one_request_server().await;
        let mut session = test_session(base_url);
        session
            .set_model("cliproxy/deepseek-v4.1-flash")
            .await
            .expect("switch the model");
        let request = server.await.expect("the fake server");

        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/api/session/ses_test/model");
        assert_eq!(
            request.body,
            json!({
                "model": {
                    "id": "deepseek-v4.1-flash",
                    "providerID": "cliproxy",
                },
            }),
            "OpenCode takes the provider and the model id, split at the first slash"
        );
    }

    #[tokio::test]
    async fn a_model_switch_keeps_a_slash_inside_the_model_id() {
        let (base_url, server) = one_request_server().await;
        let mut session = test_session(base_url);
        session
            .set_model("synthetic/hf:deepseek-ai/DeepSeek-V4.1-Flash")
            .await
            .expect("switch the model");
        let request = server.await.expect("the fake server");

        assert_eq!(request.path, "/api/session/ses_test/model");
        assert_eq!(
            request.body,
            json!({
                "model": {
                    "id": "hf:deepseek-ai/DeepSeek-V4.1-Flash",
                    "providerID": "synthetic",
                },
            })
        );
    }

    #[test]
    fn permissions_follow_the_runtime_mode() {
        let manual = permissions_for(RuntimeMode::ApprovalRequired);
        assert_eq!(manual[0]["effect"], "ask");
        assert_eq!(manual[1]["effect"], "ask");

        let edits = permissions_for(RuntimeMode::AutoAcceptEdits);
        assert_eq!(edits[0]["effect"], "ask");
        assert_eq!(edits[1]["effect"], "allow");

        let full = permissions_for(RuntimeMode::FullAccess);
        assert_eq!(full[0]["effect"], "allow");
        assert_eq!(full[1]["effect"], "allow");

        // OpenCode has no plan mode; the daemon refuses it for this harness, so
        // the mapping only needs to stay on the safe side.
        let plan = permissions_for(RuntimeMode::Plan);
        assert_eq!(plan[0]["effect"], "ask");
        assert_eq!(plan[1]["effect"], "ask");
    }

    #[test]
    fn approval_options_map_to_opencode_replies() {
        assert_eq!(permission_decision(ApprovalDecision::Approve, None), "once");
        assert_eq!(
            permission_decision(ApprovalDecision::Approve, Some("always")),
            "always"
        );
        assert_eq!(permission_decision(ApprovalDecision::Deny, None), "reject");
        assert_eq!(
            permission_decision(ApprovalDecision::Cancel, None),
            "reject"
        );
    }

    #[test]
    fn the_mcp_payload_carries_the_bearer() {
        let mut spec = spec();
        spec.mcp_endpoint = Some("http://127.0.0.1:9100/mcp".into());
        spec.env_overrides
            .insert(ENV_MCP_AUTH_TOKEN.into(), "bearer".into());
        let payload = mcp_payload(&spec).expect("payload");
        assert_eq!(payload["config"]["type"], "remote");
        assert_eq!(payload["config"]["url"], "http://127.0.0.1:9100/mcp");
        assert_eq!(
            payload["config"]["headers"]["Authorization"],
            "Bearer bearer"
        );
    }

    #[test]
    fn images_become_file_uri_parts() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("pixel.png");
        std::fs::write(&path, [0x89, b'P', b'N', b'G']).expect("write");
        let attachment = AttachmentRef {
            id: AttachmentId::from_content(b"png"),
            path,
            media_type: "image/png".into(),
        };
        let (text, files) = build_prompt("read this", &[attachment]);
        assert_eq!(text, "read this");
        assert_eq!(files.len(), 1);
        assert!(
            files[0]["uri"]
                .as_str()
                .is_some_and(|uri| uri.starts_with("file://"))
        );
        assert_eq!(files[0]["name"], "pixel.png");
    }

    #[test]
    fn non_images_fall_back_to_a_path_mention() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("notes.txt");
        std::fs::write(&path, "hello").expect("write");
        let attachment = AttachmentRef {
            id: AttachmentId::from_content(b"hello"),
            path,
            media_type: "text/plain".into(),
        };
        let path_text = attachment.path.display().to_string();
        let (text, files) = build_prompt("read this", &[attachment]);
        assert!(files.is_empty());
        assert!(text.starts_with("read this\n[Attached file \"notes.txt\" is saved at: "));
        assert!(text.contains(&path_text));
    }

    #[test]
    fn a_missing_attachment_degrades_to_path_text() {
        let attachment = AttachmentRef {
            id: AttachmentId::from_content(b"x"),
            path: PathBuf::from("/missing/does-not-exist.txt"),
            media_type: "text/plain".into(),
        };
        let (text, files) = build_prompt("read this", &[attachment]);
        assert!(files.is_empty());
        assert!(text.contains("/missing/does-not-exist.txt"));
    }

    #[test]
    fn parses_the_opencode_version_banner() {
        assert_eq!(parse_version("opencode v2.2.7").as_deref(), Some("2.2.7"));
        assert_eq!(parse_version("2.2.7\n").as_deref(), Some("2.2.7"));
        assert_eq!(parse_version("\n\n"), None);
        assert_eq!(parse_semver("2.2.7"), Some((2, 2, 7)));
        assert!(parse_semver("2.1.0") < parse_semver("2.2.6"));
    }

    #[test]
    fn the_stream_loop_decodes_a_character_split_across_chunks() {
        // Exactly the line the OpenCode event loop parses, split inside the
        // emoji between two network chunks. The loop must join it and decode it
        // once: reverting the read loop to per-chunk lossy decoding would turn
        // the emoji into two replacement characters here.
        let payload = "data: {\"type\":\"session.text.delta\",\"data\":{\"sessionID\":\"s1\",\"delta\":\"🚀日本\"}}\n";
        let bytes = payload.as_bytes();
        let split = payload.find('🚀').expect("the emoji") + 1;
        assert!(split < payload.len());
        let mut lines = SseLines::default();
        let mut parser = OpenCodeParser::new();
        let mut text = String::new();
        let mut started = false;
        for chunk in [&bytes[..split], &bytes[split..]] {
            for line in lines.push(chunk) {
                for event in parser.push_line(&line) {
                    match event {
                        HarnessEvent::TextDelta { text: delta } => text.push_str(&delta),
                        HarnessEvent::SessionStarted { .. } => started = true,
                        _ => {}
                    }
                }
            }
        }
        assert!(started, "the joined line named the session");
        assert_eq!(text, "🚀日本", "the split character arrived intact");
    }

    #[test]
    fn streamed_utf8_survives_every_split_boundary() {
        let text = "a🚀日本z";
        let bytes = text.as_bytes();
        for split in 0..=bytes.len() {
            let mut decoder = Utf8Stream::default();
            let mut decoded = decoder.push(&bytes[..split]);
            decoded.push_str(&decoder.push(&bytes[split..]));
            assert_eq!(decoded, text, "split at byte {split}");
        }
    }

    #[test]
    fn utf8_stream_stays_linear_on_a_chunk_of_invalid_bytes() {
        let mut stream = Utf8Stream::default();
        let garbage = vec![0xFFu8; 512 * 1024];
        let text = stream.push(&garbage);
        assert_eq!(text.chars().count(), garbage.len());
        assert!(text.chars().all(|ch| ch == char::REPLACEMENT_CHARACTER));
        assert_eq!(stream.finish(), "", "nothing was left incomplete");
    }

    #[test]
    fn utf8_stream_finishes_a_truncated_character() {
        let mut stream = Utf8Stream::default();
        assert_eq!(stream.push(b"ok\xE2\x82"), "ok");
        assert_eq!(stream.finish(), "\u{FFFD}");
        assert_eq!(stream.finish(), "", "finish is idempotent");
    }

    #[test]
    fn h4rn_only_environment_is_consumed() {
        let mut spec = spec();
        spec.env_overrides = BTreeMap::from([
            (ENV_OPENCODE_BASE_URL.into(), "http://proxy".into()),
            (ENV_OPENCODE_API_KEY.into(), "secret".into()),
            ("OPENCODE_CONFIG".into(), "/tmp/config.json".into()),
        ]);
        let config = config_content_for(&spec);
        assert!(config.contains("http://proxy"));
        assert!(H4RN_ONLY_ENV.contains(&ENV_OPENCODE_API_KEY));
        assert!(!H4RN_ONLY_ENV.contains(&"OPENCODE_CONFIG"));
    }
}
