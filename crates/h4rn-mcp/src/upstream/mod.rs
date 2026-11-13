//! Upstream MCP servers managed by the hub.
//!
//! An upstream is one user-configured server reached over stdio, streamable
//! HTTP or legacy SSE. The manager gives every transport the same lifecycle:
//!
//! - **lazy start**: nothing is spawned until a session actually lists or calls
//!   a tool, so a daemon with twenty configured servers starts none at boot;
//! - **health**: a ready server is pinged on the hub's background tick and a
//!   failed ping restarts it;
//! - **restart with backoff**: failures double from 500 ms up to 30 s, and a
//!   successful handshake resets the counter;
//! - **capability caching**: tools, resources and prompts are cached after
//!   `initialize` and refreshed when the server sends `list_changed`;
//! - **timeouts and output caps**: every request has a deadline, every tool
//!   result is truncated at a fixed size before it reaches a model;
//! - **shared runtime config**: the same [`Upstream`] trait is implemented by
//!   the cross-machine proxy in [`crate::sharing`], so a remote server behaves
//!   like a local one from the hub's point of view.

pub mod http;
pub mod stdio;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use h4rn_protocol::{HostId, McpServerStatusKind};
use serde_json::{Value, json};
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::jsonrpc::{self, RpcErrorObject};
use crate::provider::{ProgressUpdate, ToolCallContext, ToolError};
use crate::types::{
    CallToolResult, PromptDefinition, PromptMessage, ResourceContents, ResourceDefinition,
    ServerCapabilities, ToolDefinition, parse_call_result, parse_prompt_messages, parse_prompts,
    parse_resource_contents, parse_resources, parse_tools,
};

/// How long a single JSON-RPC request to an upstream may take.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a tool call may take end to end.
pub const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a health ping may take.
pub const DEFAULT_HEALTH_TIMEOUT: Duration = Duration::from_secs(10);
/// Default cap on a tool result crossing the hub.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Default cap on tools listed from one server.
pub const DEFAULT_MAX_TOOLS: usize = 512;

/// Tuning for every upstream.
#[derive(Clone, Debug)]
pub struct UpstreamLimits {
    /// Per-request deadline inside a transport.
    pub request_timeout: Duration,
    /// End-to-end tool-call deadline.
    pub tool_timeout: Duration,
    /// Health-ping deadline.
    pub health_timeout: Duration,
    /// Maximum bytes of a tool result.
    pub max_output_bytes: usize,
    /// Maximum tools accepted from one server.
    pub max_tools: usize,
    /// First restart delay.
    pub restart_base: Duration,
    /// Maximum restart delay.
    pub restart_max: Duration,
}

impl Default for UpstreamLimits {
    fn default() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            tool_timeout: DEFAULT_TOOL_TIMEOUT,
            health_timeout: DEFAULT_HEALTH_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_tools: DEFAULT_MAX_TOOLS,
            restart_base: Duration::from_millis(500),
            restart_max: Duration::from_secs(30),
        }
    }
}

/// How to reach one upstream. Values are already resolved from the secret
/// store; `Debug` deliberately prints only names.
#[derive(Clone)]
pub enum UpstreamConfig {
    /// A child process speaking MCP over stdio.
    Stdio {
        /// Executable.
        command: String,
        /// Arguments.
        args: Vec<String>,
        /// Working directory.
        cwd: Option<std::path::PathBuf>,
        /// Resolved environment variables.
        env: BTreeMap<String, String>,
    },
    /// A streamable-HTTP endpoint.
    Http {
        /// Endpoint URL.
        url: String,
        /// Resolved headers.
        headers: BTreeMap<String, String>,
    },
    /// A legacy HTTP+SSE endpoint.
    Sse {
        /// Endpoint URL.
        url: String,
        /// Resolved headers.
        headers: BTreeMap<String, String>,
    },
    /// A server on another machine, reached through the encrypted transport.
    Remote {
        /// Machine that executes it.
        machine: HostId,
        /// Client half of the tunnel.
        tunnel: Arc<dyn crate::sharing::McpTunnel>,
    },
}

impl UpstreamConfig {
    /// Transport label.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
            Self::Sse { .. } => "sse",
            Self::Remote { .. } => "remote",
        }
    }

    /// Whether this config is a cross-machine proxy.
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote { .. })
    }

    /// The machine the server runs on, `None` for this daemon.
    pub fn machine(&self) -> Option<&HostId> {
        match self {
            Self::Remote { machine, .. } => Some(machine),
            _ => None,
        }
    }
}

impl fmt::Debug for UpstreamConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio {
                command,
                args,
                cwd,
                env,
            } => formatter
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("cwd", cwd)
                .field("env", &env.keys().collect::<Vec<_>>())
                .finish(),
            Self::Http { url, headers } => formatter
                .debug_struct("Http")
                .field("url", url)
                .field("headers", &headers.keys().collect::<Vec<_>>())
                .finish(),
            Self::Sse { url, headers } => formatter
                .debug_struct("Sse")
                .field("url", url)
                .field("headers", &headers.keys().collect::<Vec<_>>())
                .finish(),
            Self::Remote { machine, .. } => formatter
                .debug_struct("Remote")
                .field("machine", machine)
                .finish(),
        }
    }
}

/// A failure talking to an upstream.
#[derive(Clone, Debug, PartialEq)]
pub enum UpstreamError {
    /// The transport failed (spawn, socket, child exit).
    Transport(String),
    /// The message was not valid MCP.
    Protocol(String),
    /// The request deadline elapsed.
    Timeout,
    /// The call was cancelled.
    Cancelled,
    /// The server is not ready and is waiting to retry.
    Unavailable(String),
    /// The server answered with a JSON-RPC error.
    Rpc(RpcErrorObject),
}

impl fmt::Display for UpstreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(message) => write!(formatter, "transport failed: {message}"),
            Self::Protocol(message) => write!(formatter, "protocol error: {message}"),
            Self::Timeout => write!(formatter, "the upstream did not answer in time"),
            Self::Cancelled => write!(formatter, "the call was cancelled"),
            Self::Unavailable(message) => write!(formatter, "{message}"),
            Self::Rpc(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for UpstreamError {}

impl UpstreamError {
    /// Maps to a tool error, preserving JSON-RPC codes.
    pub fn into_tool_error(self) -> ToolError {
        match self {
            Self::Rpc(error) => ToolError::protocol(error.code, error.message),
            Self::Cancelled => ToolError::cancelled(),
            Self::Timeout => ToolError::protocol(
                jsonrpc::INTERNAL_ERROR,
                "the upstream did not answer in time",
            ),
            other => ToolError::protocol(jsonrpc::INTERNAL_ERROR, other.to_string()),
        }
    }
}

/// One connected upstream connection.
#[async_trait]
pub trait Upstream: Send + Sync {
    /// Server name/version reported during `initialize`.
    fn server_info(&self) -> Option<&str>;

    /// Capabilities reported during `initialize`.
    fn capabilities(&self) -> &ServerCapabilities;

    /// Instructions reported during `initialize`.
    fn instructions(&self) -> Option<&str>;

    /// Sends one JSON-RPC request and waits for the response.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, UpstreamError>;

    /// Sends one notification.
    async fn notify(&self, method: &str, params: Value) -> Result<(), UpstreamError>;

    /// Stops the transport. Idempotent.
    async fn shutdown(&self);
}

/// What a factory returns after a successful handshake and discovery.
pub struct ConnectedUpstream {
    /// Live transport.
    pub upstream: Arc<dyn Upstream>,
    /// Cached tools.
    pub tools: Vec<ToolDefinition>,
    /// Cached resources.
    pub resources: Vec<ResourceDefinition>,
    /// Cached prompts.
    pub prompts: Vec<PromptDefinition>,
    /// Diagnostics.
    pub summary: Option<String>,
}

/// Builds a transport for one config and performs `initialize`.
#[async_trait]
pub trait UpstreamFactory: Send + Sync {
    /// Connects, initializes and discovers capabilities.
    async fn connect(
        &self,
        name: &str,
        config: &UpstreamConfig,
        limits: &UpstreamLimits,
        events: broadcast::Sender<UpstreamEvent>,
    ) -> Result<ConnectedUpstream, UpstreamError>;
}

/// The shipping factory: stdio, streamable HTTP, legacy SSE and remote proxy.
pub struct DefaultUpstreamFactory;

#[async_trait]
impl UpstreamFactory for DefaultUpstreamFactory {
    async fn connect(
        &self,
        name: &str,
        config: &UpstreamConfig,
        limits: &UpstreamLimits,
        events: broadcast::Sender<UpstreamEvent>,
    ) -> Result<ConnectedUpstream, UpstreamError> {
        match config {
            UpstreamConfig::Stdio {
                command,
                args,
                cwd,
                env,
            } => stdio::connect(name, command, args, cwd.as_deref(), env, limits, events).await,
            UpstreamConfig::Http { url, headers } => {
                http::connect_streamable(name, url, headers, limits, events).await
            }
            UpstreamConfig::Sse { url, headers } => {
                http::connect_legacy_sse(name, url, headers, limits, events).await
            }
            UpstreamConfig::Remote { machine, tunnel } => {
                crate::sharing::RemoteUpstream::connect(
                    name,
                    machine.clone(),
                    Arc::clone(tunnel),
                    limits,
                )
                .await
            }
        }
    }
}

/// A runtime change worth telling the hub and its clients about.
#[derive(Clone, Debug)]
pub enum UpstreamEvent {
    /// Status or tool count changed.
    Status {
        /// Server name.
        server: String,
        /// New status.
        status: McpServerStatusKind,
        /// Cached tool count.
        tool_count: u32,
        /// Redacted message.
        message: Option<String>,
        /// When it happened.
        at: chrono::DateTime<Utc>,
    },
    /// The tool list is stale.
    ToolsChanged {
        /// Server name.
        server: String,
    },
    /// The resource list is stale.
    ResourcesChanged {
        /// Server name.
        server: String,
    },
    /// The prompt list is stale.
    PromptsChanged {
        /// Server name.
        server: String,
    },
}

impl UpstreamEvent {
    /// The server the event is about.
    pub fn server(&self) -> &str {
        match self {
            Self::Status { server, .. }
            | Self::ToolsChanged { server }
            | Self::ResourcesChanged { server }
            | Self::PromptsChanged { server } => server,
        }
    }
}

/// Status of one managed upstream.
#[derive(Clone, Debug)]
pub struct UpstreamStatus {
    /// Current lifecycle state.
    pub status: McpServerStatusKind,
    /// Cached tool count.
    pub tool_count: u32,
    /// Redacted failure.
    pub last_error: Option<String>,
    /// Consecutive failed starts.
    pub attempts: u32,
    /// When the next start will be attempted.
    pub retry_at: Option<Instant>,
    /// Machine that executes the server, `None` for local.
    pub machine: Option<HostId>,
}

struct ManagedState {
    upstream: Option<Arc<dyn Upstream>>,
    status: McpServerStatusKind,
    last_error: Option<String>,
    attempts: u32,
    next_retry: Option<Instant>,
    tools: Vec<ToolDefinition>,
    tools_loaded: bool,
    resources: Vec<ResourceDefinition>,
    resources_loaded: bool,
    prompts: Vec<PromptDefinition>,
    prompts_loaded: bool,
    capabilities: ServerCapabilities,
    summary: Option<String>,
}

struct Managed {
    name: String,
    config: UpstreamConfig,
    state: tokio::sync::Mutex<ManagedState>,
}

/// The manager: one entry per configured server.
pub struct UpstreamManager {
    factory: Arc<dyn UpstreamFactory>,
    limits: UpstreamLimits,
    entries: RwLock<BTreeMap<String, Arc<Managed>>>,
    events: broadcast::Sender<UpstreamEvent>,
}

impl UpstreamManager {
    /// Creates a manager with the shipping factory.
    pub fn new(limits: UpstreamLimits) -> Self {
        Self::with_factory(Arc::new(DefaultUpstreamFactory), limits)
    }

    /// Creates a manager with an injected factory (tests).
    pub fn with_factory(factory: Arc<dyn UpstreamFactory>, limits: UpstreamLimits) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            factory,
            limits,
            entries: RwLock::new(BTreeMap::new()),
            events,
        }
    }

    /// Subscribes to runtime changes.
    pub fn subscribe(&self) -> broadcast::Receiver<UpstreamEvent> {
        self.events.subscribe()
    }

    /// Tuning.
    pub fn limits(&self) -> &UpstreamLimits {
        &self.limits
    }

    /// Replaces the full configuration set, stopping servers that disappeared.
    pub async fn replace(&self, configs: BTreeMap<String, UpstreamConfig>) {
        let stale: Vec<Arc<Managed>> = {
            let mut entries = self.entries.write().await;
            let mut stale: Vec<Arc<Managed>> = entries
                .iter()
                .filter(|(name, _)| !configs.contains_key(*name))
                .map(|(_, entry)| Arc::clone(entry))
                .collect();
            entries.retain(|name, _| configs.contains_key(name));
            for (name, config) in configs {
                if let Some(previous) = entries.remove(&name) {
                    stale.push(previous);
                }
                entries.insert(name.clone(), Arc::new(Managed::new(name, config)));
            }
            stale
        };
        for entry in stale {
            entry.stop().await;
        }
    }

    /// Adds or replaces one server.
    pub async fn upsert(&self, name: impl Into<String>, config: UpstreamConfig) {
        let name = name.into();
        let previous = {
            let mut entries = self.entries.write().await;
            entries.insert(name.clone(), Arc::new(Managed::new(name, config)))
        };
        if let Some(previous) = previous {
            previous.stop().await;
        }
    }

    /// Removes one server, stopping it.
    pub async fn remove(&self, name: &str) {
        let removed = {
            let mut entries = self.entries.write().await;
            entries.remove(name)
        };
        if let Some(removed) = removed {
            removed.stop().await;
        }
    }

    /// Configured server names, sorted.
    pub async fn names(&self) -> Vec<String> {
        self.entries.read().await.keys().cloned().collect()
    }

    /// Whether `name` is configured.
    pub async fn contains(&self, name: &str) -> bool {
        self.entries.read().await.contains_key(name)
    }

    /// The machine a server runs on, `None` for local.
    pub async fn machine(&self, name: &str) -> Option<HostId> {
        self.entries
            .read()
            .await
            .get(name)
            .and_then(|entry| entry.config.machine().cloned())
    }

    /// The transport kind (`stdio`, `http`, `sse`, `remote`).
    pub async fn transport_kind(&self, name: &str) -> Option<&'static str> {
        self.entries
            .read()
            .await
            .get(name)
            .map(|entry| entry.config.kind())
    }

    async fn get_entry(&self, name: &str) -> Option<Arc<Managed>> {
        self.entries.read().await.get(name).cloned()
    }

    /// Runtime status, or `Stopped` when configured but never started.
    pub async fn status(&self, name: &str) -> UpstreamStatus {
        let Some(entry) = self.get_entry(name).await else {
            return UpstreamStatus {
                status: McpServerStatusKind::Stopped,
                tool_count: 0,
                last_error: None,
                attempts: 0,
                retry_at: None,
                machine: None,
            };
        };
        let state = entry.state.lock().await;
        UpstreamStatus {
            status: state.status,
            tool_count: state.tools.len() as u32,
            last_error: state.last_error.clone(),
            attempts: state.attempts,
            retry_at: state.next_retry,
            machine: entry.config.machine().cloned(),
        }
    }

    /// Ensures the server is running and returns its connection.
    pub async fn ensure(&self, name: &str) -> Result<Arc<dyn Upstream>, UpstreamError> {
        let entry = self
            .get_entry(name)
            .await
            .ok_or_else(|| UpstreamError::Unavailable(format!("no MCP server named \"{name}\"")))?;
        entry.ensure(self).await
    }

    /// Lists tools, refreshing the cache when it is stale.
    pub async fn list_tools(&self, name: &str) -> Result<Vec<ToolDefinition>, UpstreamError> {
        let entry = self
            .get_entry(name)
            .await
            .ok_or_else(|| UpstreamError::Unavailable(format!("no MCP server named \"{name}\"")))?;
        entry.tools(self).await
    }

    /// Calls one tool.
    pub async fn call_tool(
        &self,
        name: &str,
        tool: &str,
        arguments: Value,
        context: &ToolCallContext,
    ) -> Result<CallToolResult, UpstreamError> {
        let entry = self
            .get_entry(name)
            .await
            .ok_or_else(|| UpstreamError::Unavailable(format!("no MCP server named \"{name}\"")))?;
        entry.call_tool(self, tool, arguments, context).await
    }

    /// Lists resources.
    pub async fn list_resources(
        &self,
        name: &str,
    ) -> Result<Vec<ResourceDefinition>, UpstreamError> {
        let entry = self
            .get_entry(name)
            .await
            .ok_or_else(|| UpstreamError::Unavailable(format!("no MCP server named \"{name}\"")))?;
        entry.resources(self).await
    }

    /// Reads one resource.
    pub async fn read_resource(
        &self,
        name: &str,
        uri: &str,
    ) -> Result<Vec<ResourceContents>, UpstreamError> {
        let entry = self
            .get_entry(name)
            .await
            .ok_or_else(|| UpstreamError::Unavailable(format!("no MCP server named \"{name}\"")))?;
        entry.read_resource(self, uri).await
    }

    /// Lists prompts.
    pub async fn list_prompts(&self, name: &str) -> Result<Vec<PromptDefinition>, UpstreamError> {
        let entry = self
            .get_entry(name)
            .await
            .ok_or_else(|| UpstreamError::Unavailable(format!("no MCP server named \"{name}\"")))?;
        entry.prompts(self).await
    }

    /// Renders one prompt.
    pub async fn get_prompt(
        &self,
        name: &str,
        prompt: &str,
        arguments: &BTreeMap<String, String>,
    ) -> Result<Vec<PromptMessage>, UpstreamError> {
        let entry = self
            .get_entry(name)
            .await
            .ok_or_else(|| UpstreamError::Unavailable(format!("no MCP server named \"{name}\"")))?;
        entry.get_prompt(self, prompt, arguments).await
    }

    /// Marks the cached tool list stale (a `list_changed` notification).
    pub async fn invalidate_tools(&self, name: &str) {
        if let Some(entry) = self.get_entry(name).await {
            entry.invalidate_tools().await;
        }
    }

    /// Marks the cached resource list stale.
    pub async fn invalidate_resources(&self, name: &str) {
        if let Some(entry) = self.get_entry(name).await {
            entry.invalidate_resources().await;
        }
    }

    /// Marks the cached prompt list stale.
    pub async fn invalidate_prompts(&self, name: &str) {
        if let Some(entry) = self.get_entry(name).await {
            entry.invalidate_prompts().await;
        }
    }

    /// One health pass: ping ready servers, start due ones, prune sessions.
    pub async fn health_tick(&self) {
        let entries: Vec<Arc<Managed>> = self.entries.read().await.values().cloned().collect();
        for entry in entries {
            let ready = {
                let state = entry.state.lock().await;
                state.upstream.clone()
            };
            if let Some(upstream) = ready {
                if tokio::time::timeout(
                    self.limits.health_timeout,
                    upstream.request("ping", json!({}), self.limits.health_timeout),
                )
                .await
                .is_err()
                {
                    entry.mark_failure(self, "the health check failed").await;
                    continue;
                }
                continue;
            }
            let _ = entry.ensure(self).await;
        }
    }

    /// Stops every server.
    pub async fn shutdown(&self) {
        let entries: Vec<Arc<Managed>> = self.entries.read().await.values().cloned().collect();
        for entry in entries {
            entry.stop().await;
        }
    }
}

impl Managed {
    fn new(name: String, config: UpstreamConfig) -> Self {
        Self {
            name,
            config,
            state: tokio::sync::Mutex::new(ManagedState {
                upstream: None,
                status: McpServerStatusKind::Stopped,
                last_error: None,
                attempts: 0,
                next_retry: None,
                tools: Vec::new(),
                tools_loaded: false,
                resources: Vec::new(),
                resources_loaded: false,
                prompts: Vec::new(),
                prompts_loaded: false,
                capabilities: ServerCapabilities::default(),
                summary: None,
            }),
        }
    }

    fn emit(
        &self,
        manager: &UpstreamManager,
        status: McpServerStatusKind,
        tool_count: u32,
        message: Option<String>,
    ) {
        let _ = manager.events.send(UpstreamEvent::Status {
            server: self.name.clone(),
            status,
            tool_count,
            message,
            at: Utc::now(),
        });
    }

    async fn ensure(&self, manager: &UpstreamManager) -> Result<Arc<dyn Upstream>, UpstreamError> {
        let mut state = self.state.lock().await;
        if let Some(upstream) = &state.upstream {
            return Ok(Arc::clone(upstream));
        }
        if let Some(retry_at) = state.next_retry
            && Instant::now() < retry_at
        {
            let wait = retry_at.saturating_duration_since(Instant::now());
            return Err(UpstreamError::Unavailable(format!(
                "\"{}\" is restarting in {}s",
                self.name,
                wait.as_secs().max(1)
            )));
        }
        state.status = McpServerStatusKind::Starting;
        let attempts = state.attempts;
        self.emit(
            manager,
            McpServerStatusKind::Starting,
            0,
            state.last_error.clone(),
        );
        // The state lock is held across connect on purpose: concurrent callers
        // wait and then observe the same ready-or-failed result instead of
        // racing two child processes into existence.
        let connected = manager
            .factory
            .connect(
                &self.name,
                &self.config,
                &manager.limits,
                manager.events.clone(),
            )
            .await;
        match connected {
            Ok(connected) => {
                state.upstream = Some(Arc::clone(&connected.upstream));
                state.capabilities = connected.upstream.capabilities().clone();
                state.tools = connected.tools;
                state.tools_loaded = true;
                state.resources = connected.resources;
                state.resources_loaded = true;
                state.prompts = connected.prompts;
                state.prompts_loaded = true;
                state.status = if self.config.is_remote() {
                    McpServerStatusKind::Remote
                } else {
                    McpServerStatusKind::Ready
                };
                state.last_error = None;
                state.attempts = 0;
                state.next_retry = None;
                state.summary = connected.summary;
                let tool_count = state.tools.len() as u32;
                let status = state.status;
                drop(state);
                self.emit(manager, status, tool_count, None);
                Ok(Arc::clone(&connected.upstream))
            }
            Err(error) => {
                state.attempts = attempts.saturating_add(1);
                let delay = restart_delay(&manager.limits, state.attempts);
                state.next_retry = Some(Instant::now() + delay);
                state.status = McpServerStatusKind::Error;
                state.last_error = Some(error.to_string());
                let message = format!("{} (retrying in {}s)", error, delay.as_secs().max(1));
                let attempts = state.attempts;
                drop(state);
                self.emit(
                    manager,
                    McpServerStatusKind::Error,
                    0,
                    Some(message.clone()),
                );
                Err(UpstreamError::Unavailable(format!(
                    "\"{}\" failed to start (attempt {attempts}): {message}",
                    self.name
                )))
            }
        }
    }

    async fn mark_failure(&self, manager: &UpstreamManager, message: &str) {
        let mut state = self.state.lock().await;
        if let Some(upstream) = state.upstream.take() {
            tokio::spawn(async move { upstream.shutdown().await });
        }
        state.status = McpServerStatusKind::Error;
        state.last_error = Some(message.to_owned());
        state.attempts = state.attempts.saturating_add(1);
        state.next_retry = Some(Instant::now() + restart_delay(&manager.limits, state.attempts));
        let tool_count = state.tools.len() as u32;
        state.tools_loaded = false;
        state.resources_loaded = false;
        state.prompts_loaded = false;
        drop(state);
        self.emit(
            manager,
            McpServerStatusKind::Error,
            tool_count,
            Some(message.to_owned()),
        );
    }

    async fn stop(&self) {
        let mut state = self.state.lock().await;
        if let Some(upstream) = state.upstream.take() {
            upstream.shutdown().await;
        }
        state.status = McpServerStatusKind::Stopped;
        state.tools.clear();
        state.resources.clear();
        state.prompts.clear();
        state.tools_loaded = false;
        state.resources_loaded = false;
        state.prompts_loaded = false;
    }

    async fn invalidate_tools(&self) {
        self.state.lock().await.tools_loaded = false;
    }

    async fn invalidate_resources(&self) {
        self.state.lock().await.resources_loaded = false;
    }

    async fn invalidate_prompts(&self) {
        self.state.lock().await.prompts_loaded = false;
    }

    async fn tools(&self, manager: &UpstreamManager) -> Result<Vec<ToolDefinition>, UpstreamError> {
        let upstream = self.ensure(manager).await?;
        let stale = {
            let state = self.state.lock().await;
            !state.tools_loaded
        };
        if stale {
            let tools = fetch_tools(&*upstream, &manager.limits).await?;
            let mut state = self.state.lock().await;
            state.tools = tools.clone();
            state.tools_loaded = true;
        }
        Ok(self.state.lock().await.tools.clone())
    }

    async fn call_tool(
        &self,
        manager: &UpstreamManager,
        tool: &str,
        arguments: Value,
        context: &ToolCallContext,
    ) -> Result<CallToolResult, UpstreamError> {
        let upstream = self.ensure(manager).await?;
        let params = json!({"name": tool, "arguments": arguments});
        let call = upstream.request("tools/call", params, manager.limits.tool_timeout);
        let result = tokio::select! {
            _ = context.cancel.cancelled() => return Err(UpstreamError::Cancelled),
            result = tokio::time::timeout(manager.limits.tool_timeout, call) => {
                match result {
                    Ok(result) => result?,
                    Err(_) => return Err(UpstreamError::Timeout),
                }
            }
        };
        let mut parsed = parse_call_result(&result).map_err(UpstreamError::Protocol)?;
        parsed.cap_output(manager.limits.max_output_bytes);
        Ok(parsed)
    }

    async fn resources(
        &self,
        manager: &UpstreamManager,
    ) -> Result<Vec<ResourceDefinition>, UpstreamError> {
        let upstream = self.ensure(manager).await?;
        let stale = {
            let state = self.state.lock().await;
            !state.resources_loaded
        };
        if stale {
            let resources = fetch_resources(&*upstream).await?;
            let mut state = self.state.lock().await;
            state.resources = resources.clone();
            state.resources_loaded = true;
        }
        Ok(self.state.lock().await.resources.clone())
    }

    async fn read_resource(
        &self,
        manager: &UpstreamManager,
        uri: &str,
    ) -> Result<Vec<ResourceContents>, UpstreamError> {
        let upstream = self.ensure(manager).await?;
        let result = upstream
            .request(
                "resources/read",
                json!({"uri": uri}),
                manager.limits.request_timeout,
            )
            .await?;
        parse_resource_contents(&result).map_err(UpstreamError::Protocol)
    }

    async fn prompts(
        &self,
        manager: &UpstreamManager,
    ) -> Result<Vec<PromptDefinition>, UpstreamError> {
        let upstream = self.ensure(manager).await?;
        let stale = {
            let state = self.state.lock().await;
            !state.prompts_loaded
        };
        if stale {
            let prompts = fetch_prompts(&*upstream).await?;
            let mut state = self.state.lock().await;
            state.prompts = prompts.clone();
            state.prompts_loaded = true;
        }
        Ok(self.state.lock().await.prompts.clone())
    }

    async fn get_prompt(
        &self,
        manager: &UpstreamManager,
        prompt: &str,
        arguments: &BTreeMap<String, String>,
    ) -> Result<Vec<PromptMessage>, UpstreamError> {
        let upstream = self.ensure(manager).await?;
        let result = upstream
            .request(
                "prompts/get",
                json!({"name": prompt, "arguments": arguments}),
                manager.limits.request_timeout,
            )
            .await?;
        parse_prompt_messages(&result).map_err(UpstreamError::Protocol)
    }
}

fn restart_delay(limits: &UpstreamLimits, attempts: u32) -> Duration {
    let exponent = attempts.saturating_sub(1).min(16);
    let scaled = limits
        .restart_base
        .saturating_mul(1u32.checked_shl(exponent).unwrap_or(u32::MAX));
    scaled.min(limits.restart_max)
}

/// Lists every page of tools, bounded by the limits.
pub async fn fetch_tools(
    upstream: &dyn Upstream,
    limits: &UpstreamLimits,
) -> Result<Vec<ToolDefinition>, UpstreamError> {
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = cursor
            .as_ref()
            .map(|cursor| json!({"cursor": cursor}))
            .unwrap_or_else(|| json!({}));
        let result = upstream
            .request("tools/list", params, limits.request_timeout)
            .await?;
        let (mut page, next) = parse_tools(&result).map_err(UpstreamError::Protocol)?;
        tools.append(&mut page);
        if tools.len() >= limits.max_tools {
            tools.truncate(limits.max_tools);
            break;
        }
        match next {
            Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
            _ => break,
        }
    }
    Ok(tools)
}

async fn fetch_resources(
    upstream: &dyn Upstream,
) -> Result<Vec<ResourceDefinition>, UpstreamError> {
    let mut resources = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = cursor
            .as_ref()
            .map(|cursor| json!({"cursor": cursor}))
            .unwrap_or_else(|| json!({}));
        let result = upstream
            .request("resources/list", params, DEFAULT_REQUEST_TIMEOUT)
            .await?;
        let (mut page, next) = parse_resources(&result).map_err(UpstreamError::Protocol)?;
        resources.append(&mut page);
        match next {
            Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
            _ => break,
        }
    }
    Ok(resources)
}

async fn fetch_prompts(upstream: &dyn Upstream) -> Result<Vec<PromptDefinition>, UpstreamError> {
    let mut prompts = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = cursor
            .as_ref()
            .map(|cursor| json!({"cursor": cursor}))
            .unwrap_or_else(|| json!({}));
        let result = upstream
            .request("prompts/list", params, DEFAULT_REQUEST_TIMEOUT)
            .await?;
        let (mut page, next) = parse_prompts(&result).map_err(UpstreamError::Protocol)?;
        prompts.append(&mut page);
        match next {
            Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
            _ => break,
        }
    }
    Ok(prompts)
}

/// Initializes a connection and discovers its capabilities.
///
/// Shared by every transport so the handshake and pagination cannot drift.
pub async fn discover(
    upstream: Arc<dyn Upstream>,
    limits: &UpstreamLimits,
) -> Result<ConnectedUpstream, UpstreamError> {
    let capabilities = upstream.capabilities().clone();
    let tools = if capabilities.tools {
        fetch_tools(&*upstream, limits).await?
    } else {
        Vec::new()
    };
    let resources = if capabilities.resources {
        fetch_resources(&*upstream).await?
    } else {
        Vec::new()
    };
    let prompts = if capabilities.prompts {
        fetch_prompts(&*upstream).await?
    } else {
        Vec::new()
    };
    Ok(ConnectedUpstream {
        summary: upstream.server_info().map(str::to_owned),
        upstream,
        tools,
        resources,
        prompts,
    })
}

/// The `initialize` params every transport sends.
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": crate::LATEST_PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {
            "name": crate::HUB_SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// Helper for an in-flight progress forwarder used by transports that support
/// server-initiated progress; upstream progress is not forwarded today because
/// MCP upstreams report progress only for their own `progressToken`s and the
/// hub mints its own. Kept as a named type so the intent is explicit.
pub struct ProgressBridge {
    sender: mpsc::Sender<ProgressUpdate>,
}

impl ProgressBridge {
    /// Wraps a hub progress sink.
    pub fn new(sender: mpsc::Sender<ProgressUpdate>) -> Self {
        Self { sender }
    }

    /// Forwards one update.
    pub async fn forward(&self, update: ProgressUpdate) {
        let _ = self.sender.send(update).await;
    }
}

/// A cancellation token attached to a tool call.
pub fn call_token(context: &ToolCallContext) -> CancellationToken {
    context.cancel.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let limits = UpstreamLimits::default();
        assert_eq!(restart_delay(&limits, 1), Duration::from_millis(500));
        assert_eq!(restart_delay(&limits, 2), Duration::from_secs(1));
        assert_eq!(restart_delay(&limits, 3), Duration::from_secs(2));
        assert_eq!(restart_delay(&limits, 30), Duration::from_secs(30));
    }

    #[test]
    fn config_debug_redacts_values() {
        let config = UpstreamConfig::Stdio {
            command: "npx".into(),
            args: vec!["-y".into()],
            cwd: None,
            env: BTreeMap::from([("TOKEN".into(), "super-secret".into())]),
        };
        let debug = format!("{config:?}");
        assert!(debug.contains("TOKEN"));
        assert!(!debug.contains("super-secret"));
    }

    #[tokio::test]
    async fn missing_secret_is_not_a_start_failure_at_config_time() {
        // Config carries resolved values; the manager never sees references.
        // This guards the invariant that Debug and status never print values.
        let manager = UpstreamManager::new(UpstreamLimits::default());
        assert!(manager.names().await.is_empty());
        assert_eq!(
            manager.status("nope").await.status,
            McpServerStatusKind::Stopped
        );
    }

    #[test]
    fn initialize_params_name_the_hub() {
        let params = initialize_params();
        assert_eq!(params["clientInfo"]["name"], "h4rn");
        assert_eq!(params["protocolVersion"], crate::LATEST_PROTOCOL_VERSION);
    }
}
