//! The MCP server outside agents connect to, and its tools.
//!
//! Outside agents are MCP clients h4rn did not start. They authenticate with an
//! OAuth bearer token whose scope is the level the person approved. The level
//! is enforced here, on the daemon, for every call: read-only cannot start or
//! message anything, and a supervised agent's threads always run at
//! approval-required regardless of the mode it asks for.
//!
//! The endpoint is deliberately separate from `/mcp`, the hub harnesses use:
//! an outside agent sees only this small tool set, never the person's own
//! upstream servers.

use std::sync::{Arc, RwLock};

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use h4rn_mcp::jsonrpc::{self, Incoming, RpcErrorObject};
use h4rn_mcp::types::{CallToolResult, Content, ToolAnnotations, ToolDefinition};
use h4rn_protocol::{
    HarnessKind, HostId, OutsideAgentLevel, RuntimeMode, ThreadId, ThreadSummary, WorkGrant,
};
use serde_json::{Value, json};

use super::oauth::{Grant, OAuthStore};
use super::rate::{EndpointLimits, RateLimiter};
use super::redact::Redactor;
use crate::auth::TokenStore;
use crate::hosts::HostInventory;
use crate::projects::ProjectService;
use crate::threads::{NewThread, ThreadManager};

/// How much of one message `get_thread` returns, after redaction.
const MESSAGE_TEXT_CHARS: usize = 2_000;

/// What an outside agent may call.
const READ_TOOLS: &[&str] = &[
    "list_projects",
    "list_threads",
    "get_thread",
    "list_providers",
];
const WRITE_TOOLS: &[&str] = &["start_thread", "send_thread_message", "stop_thread"];

/// The daemon half of outside-agent access.
pub struct OutsideAgents {
    store: Arc<OAuthStore>,
    manager: Arc<ThreadManager>,
    projects: Arc<ProjectService>,
    hosts: Arc<HostInventory>,
    proxy: Arc<h4rn_proxy::ProxyService>,
    host: HostId,
    base_url: RwLock<Option<String>>,
    /// Request rate limits. One aggregate bucket covers the unauthenticated
    /// OAuth mutations; per-key buckets are keyed by validated identifiers.
    limits: RwLock<EndpointLimits>,
    /// Redacts every value this server returns. Shared with h4rn's own agent
    /// tools, so both seats answer with the same redacted title.
    redactor: Arc<Redactor>,
}

impl OutsideAgents {
    /// Builds the server over the daemon's live services.
    pub fn new(
        store: Arc<OAuthStore>,
        manager: Arc<ThreadManager>,
        projects: Arc<ProjectService>,
        hosts: Arc<HostInventory>,
        proxy: Arc<h4rn_proxy::ProxyService>,
        redactor: Arc<Redactor>,
        host: HostId,
    ) -> Self {
        Self {
            store,
            manager,
            projects,
            hosts,
            proxy,
            host,
            base_url: RwLock::new(None),
            limits: RwLock::new(EndpointLimits::default()),
            redactor,
        }
    }

    /// Replaces the limiter (tests use a small window to prove the refusal).
    pub fn with_limits(self, limits: RateLimiter) -> Self {
        self.set_limits(limits);
        self
    }

    /// Replaces the per-key limiter on a live server.
    pub fn set_limits(&self, limits: RateLimiter) {
        self.limits
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .per_key = limits;
    }

    /// Replaces the aggregate limiter on a live server.
    pub fn set_aggregate_limits(&self, limits: RateLimiter) {
        self.limits
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .aggregate = limits;
    }

    /// The OAuth store.
    pub fn store(&self) -> Arc<OAuthStore> {
        Arc::clone(&self.store)
    }

    /// Names the store that holds the owner's access token.
    ///
    /// The local access token is the owner's own credential: a thread may quote
    /// it, and an outside agent must never read it back.
    pub fn set_owner_token(&self, tokens: &TokenStore) {
        self.redactor.set_owner_token(tokens);
    }

    /// The base URL of this daemon's loopback listener.
    pub fn set_base_url(&self, url: impl Into<String>) {
        *self
            .base_url
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(url.into().trim_end_matches('/').to_owned());
    }

    /// The base URL, when the daemon has bound a listener.
    pub fn base_url(&self) -> Option<String> {
        self.base_url
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The Streamable-HTTP URL an outside agent puts in its MCP client.
    pub fn mcp_url(&self) -> String {
        match self.base_url() {
            Some(base) => format!("{base}/mcp"),
            None => String::new(),
        }
    }

    /// The daemon's local host id.
    pub fn host(&self) -> &HostId {
        &self.host
    }

    /// Checks the per-key request limit. `Err(seconds)` becomes a 429.
    pub fn check_limit(&self, key: &str) -> Result<(), u64> {
        self.limits
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .per_key
            .check(key)
    }

    /// Checks the aggregate limit that covers every unauthenticated OAuth
    /// mutation, before a body is parsed or any per-client state is allocated.
    pub fn check_aggregate(&self) -> Result<(), u64> {
        self.limits
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .aggregate
            .check("oauth")
    }

    /// The RFC 8707 resource this daemon serves, when it has bound a listener.
    fn resource(&self) -> Option<String> {
        let url = self.mcp_url();
        (!url.is_empty()).then_some(url)
    }

    /// Verifies a bearer token against this endpoint's resource.
    pub fn verify_grant(&self, token: &str) -> Option<Grant> {
        self.store.verify_access(token, self.resource().as_deref())
    }

    /// Sign-ins waiting for the person's decision.
    pub fn pending_sign_ins(&self) -> Vec<h4rn_protocol::OutsideAgentSignIn> {
        self.store.pending_sign_ins()
    }

    /// Answers one waiting sign-in from the authenticated h4rn session.
    ///
    /// This is the only path that turns a client's request into a grant. The
    /// page the client opened has no way in.
    pub fn respond_sign_in(
        &self,
        request_id: &str,
        decision: h4rn_protocol::ApprovalDecision,
        level: Option<OutsideAgentLevel>,
    ) -> Result<(), super::oauth::OAuthError> {
        match decision {
            h4rn_protocol::ApprovalDecision::Approve => {
                let level = level.ok_or_else(|| {
                    super::oauth::OAuthError::InvalidRequest(
                        "choose read-only, supervised or full access".into(),
                    )
                })?;
                self.store.approve(request_id, level).map(|_| ())
            }
            h4rn_protocol::ApprovalDecision::Deny | h4rn_protocol::ApprovalDecision::Cancel => {
                self.store.deny(request_id).map(|_| ())
            }
        }
    }

    /// Handles `POST /mcp` when the bearer token is an outside-agent token.
    pub async fn handle_mcp(&self, headers: HeaderMap, body: Bytes) -> Response {
        let Some(token) = bearer_token(&headers) else {
            return unauthorized(self);
        };
        // The token is verified before anything is counted, so a bucket is
        // never created for a value that is not a live grant.
        let Some(grant) = self.verify_grant(token) else {
            return unauthorized(self);
        };
        if let Err(retry) = self.check_limit(&format!("mcp:{}", grant.client_id)) {
            return too_many(retry);
        }
        let value: Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(error) => {
                return rpc_http(
                    StatusCode::BAD_REQUEST,
                    RpcErrorObject::new(jsonrpc::PARSE_ERROR, format!("invalid JSON: {error}"))
                        .response(Value::Null),
                );
            }
        };
        if value.is_array() {
            // The 2025-11-25 transport carries one message per POST. A batch
            // would run several tool calls for one rate-limit hit and one
            // scope check, so it is refused instead of half-supported.
            return rpc_http(
                StatusCode::BAD_REQUEST,
                RpcErrorObject::new(
                    jsonrpc::INVALID_REQUEST,
                    "h4rn accepts one JSON-RPC message per request",
                )
                .response(Value::Null),
            );
        }
        match self.dispatch(&grant, value).await {
            Some(response) => rpc_http(StatusCode::OK, response),
            None => (StatusCode::ACCEPTED, "").into_response(),
        }
    }

    async fn dispatch(&self, grant: &Grant, message: Value) -> Option<Value> {
        let incoming = match jsonrpc::parse_message(message) {
            Ok(incoming) => incoming,
            Err(error) => {
                return Some(
                    RpcErrorObject::new(jsonrpc::PARSE_ERROR, error.to_string())
                        .response(Value::Null),
                );
            }
        };
        let (id, method, params) = match incoming {
            Incoming::Notification { .. } | Incoming::Response { .. } => return None,
            Incoming::Request { id, method, params } => (id, method, params),
        };
        let outcome = self.request(grant, &method, params).await;
        Some(match outcome {
            Ok(result) => jsonrpc::success(id, result),
            Err(error) => error.response(id),
        })
    }

    async fn request(
        &self,
        grant: &Grant,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcErrorObject> {
        match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str);
                let version = h4rn_mcp::negotiate_protocol_version(requested);
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {
                        "name": "h4rn",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "instructions": format!(
                        "Connected to h4rn as {}. {}",
                        grant.client_name,
                        match grant.level {
                            OutsideAgentLevel::ReadOnly => "This grant is read-only: list projects, threads, providers and models, and read a thread.",
                            OutsideAgentLevel::Supervised => "This grant may start and message threads; they always run at approval-required.",
                            OutsideAgentLevel::FullAccess => "This grant may start and message threads at the mode it asks for.",
                        }
                    ),
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.tool_definitions(grant.level)})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).ok_or_else(|| {
                    RpcErrorObject::new(jsonrpc::INVALID_PARAMS, "tools/call needs a name")
                })?;
                if !grant.level.can_write() && WRITE_TOOLS.contains(&name) {
                    return Err(RpcErrorObject::new(
                        jsonrpc::INVALID_PARAMS,
                        format!(
                            "`{name}` needs a supervised or full-access grant; this one is read-only"
                        ),
                    ));
                }
                if !READ_TOOLS.contains(&name) && !WRITE_TOOLS.contains(&name) {
                    return Err(RpcErrorObject::new(
                        jsonrpc::METHOD_NOT_FOUND,
                        format!("h4rn has no outside tool named `{name}`"),
                    ));
                }
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match self.call(grant, name, arguments).await {
                    Ok(result) => Ok(serde_json::to_value(result)
                        .unwrap_or_else(|_| json!({"content": [], "isError": true}))),
                    Err(error) => Ok(serde_json::to_value(CallToolResult::error(error)).unwrap()),
                }
            }
            other => Err(RpcErrorObject::new(
                jsonrpc::METHOD_NOT_FOUND,
                format!("method \"{other}\" is not served by h4rn's outside MCP endpoint"),
            )),
        }
    }

    fn tool_definitions(&self, level: OutsideAgentLevel) -> Vec<Value> {
        let mut tools = vec![
            tool(
                "list_projects",
                "List the projects registered with h4rn. Read-only.",
                json!({"type": "object", "properties": {}, "additionalProperties": false}),
                true,
            ),
            tool(
                "list_threads",
                "List threads, newest first, with their state and workspace. Read-only.",
                json!({
                    "type": "object",
                    "properties": {
                        "project_id": {"type": "string", "description": "Limit to one project."},
                        "all_projects": {"type": "boolean", "description": "Include every project. Defaults to true."},
                        "include_children": {"type": "boolean", "description": "Include subagent child threads. Defaults to false."}
                    },
                    "additionalProperties": false
                }),
                true,
            ),
            tool(
                "get_thread",
                "Read one thread: its summary and its most recent messages. Read-only.",
                json!({
                    "type": "object",
                    "properties": {
                        "thread_id": {"type": "string"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Recent messages to include. Defaults to 30."}
                    },
                    "required": ["thread_id"],
                    "additionalProperties": false
                }),
                true,
            ),
            tool(
                "list_providers",
                "List the harnesses this machine can run, the models recent threads used, and the models its proxy profiles report. Read-only.",
                json!({"type": "object", "properties": {}, "additionalProperties": false}),
                true,
            ),
        ];
        if level.can_write() {
            tools.push(tool(
                "start_thread",
                "Start a thread and send its first prompt. Choose the project, harness and model from list_projects and list_providers.",
                json!({
                    "type": "object",
                    "properties": {
                        "project_id": {"type": "string", "description": "Registered project to run in."},
                        "workspace_path": {"type": "string", "description": "Absolute folder, when no project id is given."},
                        "harness": {"type": "string", "enum": ["claude_code", "codex", "opencode", "antigravity", "acp"]},
                        "agent_id": {"type": "string", "description": "Configured ACP agent, when harness is acp."},
                        "model": {"type": "string"},
                        "prompt": {"type": "string"},
                        "runtime_mode": {"type": "string", "enum": ["approval_required", "auto_accept_edits", "plan", "full_access"], "description": "Defaults to approval_required. Supervised grants always run at approval_required."},
                        "reasoning_effort": {"type": "string"},
                        "fast": {"type": "boolean"}
                    },
                    "required": ["harness", "model", "prompt"],
                    "additionalProperties": false
                }),
                false,
            ));
            tools.push(tool(
                "send_thread_message",
                "Send a message to a thread. A busy thread queues it for its next turn; it never interrupts.",
                json!({
                    "type": "object",
                    "properties": {
                        "thread_id": {"type": "string"},
                        "text": {"type": "string"}
                    },
                    "required": ["thread_id", "text"],
                    "additionalProperties": false
                }),
                false,
            ));
            tools.push(tool(
                "stop_thread",
                "Stop the turn a thread is running. The thread stays open.",
                json!({
                    "type": "object",
                    "properties": {"thread_id": {"type": "string"}},
                    "required": ["thread_id"],
                    "additionalProperties": false
                }),
                false,
            ));
        }
        tools
    }

    async fn call(
        &self,
        grant: &Grant,
        name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, String> {
        match name {
            "list_projects" => {
                let projects = self.projects.list().await;
                Ok(structured(
                    if projects.is_empty() {
                        "No projects are registered.".to_owned()
                    } else {
                        projects
                            .iter()
                            .map(|project| {
                                format!("{} — {} ({})", project.name, project.path, project.id)
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    },
                    json!({"projects": projects}),
                ))
            }
            "list_threads" => {
                let projects = self.projects.list().await;
                let filter_project = arguments.get("project_id").and_then(Value::as_str);
                let workspace = filter_project.and_then(|id| {
                    projects
                        .iter()
                        .find(|project| project.id == id)
                        .map(|project| project.path.clone())
                });
                if filter_project.is_some() && workspace.is_none() {
                    return Err(format!(
                        "no project with id `{}` is registered",
                        filter_project.unwrap_or_default()
                    ));
                }
                let mut filter = h4rn_protocol::ThreadFilter {
                    include_children: arguments
                        .get("include_children")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    ..h4rn_protocol::ThreadFilter::default()
                };
                if arguments.get("all_projects").and_then(Value::as_bool) == Some(false)
                    && workspace.is_none()
                {
                    return Err("`all_projects: false` needs a `project_id`".into());
                }
                filter.search = None;
                let summaries = self.manager.list_threads(&filter).await;
                let summaries: Vec<ThreadSummary> = match &workspace {
                    Some(workspace) => summaries
                        .into_iter()
                        .filter(|thread| &thread.workspace_path == workspace)
                        .collect(),
                    None => summaries,
                };
                Ok(structured(
                    if summaries.is_empty() {
                        "No threads.".to_owned()
                    } else {
                        summaries
                            .iter()
                            .map(|thread| {
                                format!(
                                    "{} — {} [{:?}] {:?} · {}",
                                    thread.thread_id,
                                    self.redactor.title(&thread.title),
                                    thread.agent_state,
                                    thread.harness,
                                    thread.workspace_path
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    },
                    json!({
                        "threads": summaries.iter().map(|thread| self.thread_brief(thread)).collect::<Vec<_>>(),
                    }),
                ))
            }
            "get_thread" => {
                let thread_id = required_string(&arguments, "thread_id")?;
                let id = ThreadId::new(thread_id.clone());
                let limit = arguments
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|value| value.clamp(1, 100) as usize)
                    .unwrap_or(30);
                let snapshot = self
                    .manager
                    .get_thread(&id, 0)
                    .await
                    .map_err(|error| error.to_string())?;
                let events = snapshot
                    .events
                    .iter()
                    .map(|wire| wire.event.clone())
                    .collect::<Vec<_>>();
                let messages = recent_messages(&events, limit);
                let mut summary = snapshot.summary.clone();
                summary.title = self.redactor.title(&summary.title);
                let messages: Vec<MessageRow> = messages
                    .into_iter()
                    .map(|message| MessageRow {
                        role: message.role,
                        text: self.redactor.text_to(&message.text, MESSAGE_TEXT_CHARS),
                    })
                    .collect();
                let text = format!(
                    "{} [{:?}] · {}\n{}",
                    summary.title,
                    summary.agent_state,
                    summary.workspace_path,
                    messages
                        .iter()
                        .map(|message| format!("{}: {}", message.role, message.text))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                Ok(structured(
                    text,
                    json!({
                        "thread": self.thread_brief(&summary),
                        "messages": messages,
                        "next_seq": snapshot.next_seq,
                    }),
                ))
            }
            "list_providers" => {
                let harnesses = self.hosts.available_harnesses();
                let threads = self
                    .manager
                    .list_threads(&h4rn_protocol::ThreadFilter::default())
                    .await;
                let detections: Vec<Value> = harnesses
                    .iter()
                    .map(|kind| {
                        let install = self.hosts.install(*kind);
                        let mut models: Vec<String> = Vec::new();
                        for thread in threads.iter().filter(|thread| thread.harness == *kind) {
                            for model in std::iter::once(&thread.model).chain(thread.models.iter())
                            {
                                if !model.is_empty() && !models.contains(model) {
                                    models.push(model.clone());
                                }
                            }
                        }
                        json!({
                            "harness": kind.slug(),
                            "installed": install.is_some(),
                            "version": install.map(|install| install.version),
                            "models": models,
                        })
                    })
                    .collect();
                let profiles = self.proxy.profiles();
                let profile_models: Vec<Value> = profiles
                    .iter()
                    .map(|profile| {
                        let models = self
                            .proxy
                            .cached_models(&profile.id)
                            .map(|catalog| {
                                catalog
                                    .models
                                    .into_iter()
                                    .map(|model| model.id)
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        json!({
                            "id": profile.id.as_str(),
                            "name": profile.name,
                            "default": profile.default,
                            "models": models,
                        })
                    })
                    .collect();
                let text = format!(
                    "Harnesses: {}. Proxy profiles: {}.",
                    detections
                        .iter()
                        .map(|entry| entry["harness"].as_str().unwrap_or("").to_owned())
                        .collect::<Vec<_>>()
                        .join(", "),
                    profiles.len()
                );
                Ok(structured(
                    text,
                    json!({
                        "machine": self.host,
                        "harnesses": detections,
                        "proxy_profiles": profile_models,
                    }),
                ))
            }
            "start_thread" => {
                let harness = arguments
                    .get("harness")
                    .and_then(Value::as_str)
                    .and_then(parse_harness)
                    .ok_or_else(|| {
                        "`harness` must be claude_code, codex, opencode, antigravity or acp"
                            .to_owned()
                    })?;
                let model = required_string(&arguments, "model")?;
                let prompt = required_string(&arguments, "prompt")?;
                if prompt.trim().is_empty() {
                    return Err("`prompt` must not be empty".into());
                }
                let workspace = match arguments.get("project_id").and_then(Value::as_str) {
                    Some(project_id) => self
                        .projects
                        .list()
                        .await
                        .into_iter()
                        .find(|project| project.id == project_id)
                        .map(|project| project.path)
                        .ok_or_else(|| {
                            format!("no project with id `{project_id}` is registered")
                        })?,
                    None => required_string(&arguments, "workspace_path")?,
                };
                if !std::path::Path::new(&workspace).is_absolute() {
                    return Err("`workspace_path` must be absolute".into());
                }
                let requested = match arguments.get("runtime_mode").and_then(Value::as_str) {
                    None => RuntimeMode::ApprovalRequired,
                    Some(value) => parse_mode(value).ok_or_else(|| {
                        "`runtime_mode` must be approval_required, auto_accept_edits, plan or full_access"
                            .to_owned()
                    })?,
                };
                let mode = clamp_mode(grant.level, requested);
                // The first turn carries the grant's ceiling as well as the
                // clamped mode: a person raising the thread's mode while that
                // prompt runs must not lift it, and a resume after a usage
                // limit keeps it.
                let ceiling = ceiling_for(grant.level);
                let work_grant = WorkGrant {
                    client_id: grant.client_id.clone(),
                    family: grant.family.clone(),
                    ceiling,
                };
                let created = self
                    .manager
                    .create_thread_for_work(
                        NewThread {
                            thread_id: None,
                            parent_thread_id: None,
                            subagent_id: None,
                            host: self.host.clone(),
                            harness,
                            agent_id: arguments
                                .get("agent_id")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            workspace_path: workspace,
                            model,
                            prompt,
                            attachments: Vec::new(),
                            runtime_mode: mode,
                            reasoning_effort: arguments
                                .get("reasoning_effort")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            fast: arguments
                                .get("fast")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            worktree: None,
                        },
                        ceiling,
                        Some(work_grant),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(structured(
                    format!(
                        "Started thread {} at {}.",
                        created.thread_id,
                        match mode {
                            RuntimeMode::ApprovalRequired => "approval-required",
                            RuntimeMode::AutoAcceptEdits => "auto-accept edits",
                            RuntimeMode::Plan => "plan",
                            RuntimeMode::FullAccess => "full access",
                        }
                    ),
                    json!({
                        "thread_id": created.thread_id,
                        "turn_id": created.turn_id,
                        "runtime_mode": mode,
                        "thread": self.thread_brief(&created.summary),
                    }),
                ))
            }
            "send_thread_message" => {
                let thread_id = ThreadId::new(required_string(&arguments, "thread_id")?);
                let text = required_string(&arguments, "text")?;
                if text.trim().is_empty() {
                    return Err("`text` must not be empty".into());
                }
                // A supervised grant carries its ceiling with the message. The
                // thread's own mode is the person's and is never rewritten: the
                // turn this message starts is clamped while it runs, and raising
                // the mode afterwards cannot lift a ceiling already queued.
                let ceiling = ceiling_for(grant.level);
                let work_grant = WorkGrant {
                    client_id: grant.client_id.clone(),
                    family: grant.family.clone(),
                    ceiling,
                };
                let accepted = self
                    .manager
                    .send_message_for_work(&thread_id, text, Vec::new(), ceiling, Some(work_grant))
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(structured(
                    format!(
                        "Sent to {} for turn {}.",
                        accepted.thread_id, accepted.turn_id
                    ),
                    json!({
                        "thread_id": accepted.thread_id,
                        "turn_id": accepted.turn_id,
                        "runtime_mode_ceiling": ceiling,
                    }),
                ))
            }
            "stop_thread" => {
                let thread_id = ThreadId::new(required_string(&arguments, "thread_id")?);
                self.manager
                    .interrupt(&thread_id)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(structured(
                    format!("Stopped the running turn in {thread_id}."),
                    json!({"thread_id": thread_id, "stopped": true}),
                ))
            }
            other => Err(format!("h4rn has no outside tool named `{other}`")),
        }
    }

    /// One thread as an outside tool returns it.
    ///
    /// The title is the first characters of the person's first prompt, so it can
    /// hold anything they pasted — including a credential. Redacting here covers
    /// every output form: the structured brief, the listing text and a newly
    /// started thread all come through this one function.
    fn thread_brief(&self, summary: &ThreadSummary) -> Value {
        json!({
            "thread_id": summary.thread_id,
            "title": self.redactor.title(&summary.title),
            "state": summary.agent_state,
            "tab": summary.tab,
            "harness": summary.harness,
            "model": summary.model,
            "runtime_mode": summary.runtime_mode,
            "workspace_path": summary.workspace_path,
            "last_activity": summary.last_activity,
            "needs_attention": summary.needs_attention,
            "unread": summary.unread,
        })
    }
}

fn tool(name: &str, description: &str, schema: Value, read_only: bool) -> Value {
    let mut definition = ToolDefinition::new(name, Some(description.to_owned()), schema);
    definition.annotations = Some(ToolAnnotations {
        read_only_hint: Some(read_only),
        idempotent_hint: read_only.then_some(true),
        ..ToolAnnotations::default()
    });
    serde_json::to_value(definition).unwrap_or_else(|_| json!({"name": name}))
}

fn structured(text: impl Into<String>, value: Value) -> CallToolResult {
    CallToolResult {
        content: vec![Content::text(text.into())],
        structured_content: Some(value),
        is_error: false,
    }
}

fn required_string(arguments: &Value, key: &str) -> Result<String, String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("`{key}` is required"))
}

fn parse_harness(value: &str) -> Option<HarnessKind> {
    match value {
        "claude_code" | "claude-code" => Some(HarnessKind::ClaudeCode),
        "codex" => Some(HarnessKind::Codex),
        "opencode" => Some(HarnessKind::Opencode),
        "antigravity" => Some(HarnessKind::Antigravity),
        "acp" => Some(HarnessKind::Acp),
        _ => None,
    }
}

fn parse_mode(value: &str) -> Option<RuntimeMode> {
    match value {
        "approval_required" => Some(RuntimeMode::ApprovalRequired),
        "auto_accept_edits" => Some(RuntimeMode::AutoAcceptEdits),
        "plan" => Some(RuntimeMode::Plan),
        "full_access" => Some(RuntimeMode::FullAccess),
        _ => None,
    }
}

/// A supervised grant always runs its threads at approval-required.
fn clamp_mode(level: OutsideAgentLevel, requested: RuntimeMode) -> RuntimeMode {
    match level {
        OutsideAgentLevel::ReadOnly => RuntimeMode::ApprovalRequired,
        OutsideAgentLevel::Supervised => RuntimeMode::ApprovalRequired,
        OutsideAgentLevel::FullAccess => requested,
    }
}

/// The autonomy ceiling a grant puts on any turn it starts.
///
/// A supervised grant caps every turn at approval-required; a full-access one
/// caps nothing beyond the thread's own mode. Read-only cannot start work.
fn ceiling_for(level: OutsideAgentLevel) -> Option<RuntimeMode> {
    matches!(level, OutsideAgentLevel::Supervised).then_some(RuntimeMode::ApprovalRequired)
}

#[derive(Clone, serde::Serialize)]
struct MessageRow {
    role: &'static str,
    text: String,
}

fn recent_messages(events: &[h4rn_protocol::Event], limit: usize) -> Vec<MessageRow> {
    let mut messages = Vec::new();
    for event in events {
        let h4rn_protocol::Event::ItemCompleted {
            item: h4rn_protocol::Item::Message { role, text, .. },
            ..
        } = event
        else {
            continue;
        };
        messages.push(MessageRow {
            role: match role {
                h4rn_protocol::MessageRole::User => "user",
                h4rn_protocol::MessageRole::Assistant => "assistant",
                h4rn_protocol::MessageRole::System => "system",
            },
            text: text.clone(),
        });
    }
    let drop = messages.len().saturating_sub(limit);
    messages.drain(..drop);
    messages
}

/// Whether a request carries an outside-agent token rather than a hub token.
///
/// The two token populations have distinct prefixes, so `/mcp` can serve both
/// without a client ever choosing the wrong handler: harness tokens stay on
/// the hub, OAuth tokens reach the outside tool set.
pub fn is_outside_request(headers: &HeaderMap) -> bool {
    bearer_token(headers).is_some_and(|token| token.starts_with("h4rn_at_"))
}

pub(crate) fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.trim().split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    Some(token.trim()).filter(|value| !value.is_empty())
}

fn unauthorized(agents: &OutsideAgents) -> Response {
    let metadata = agents
        .base_url()
        .map(|base| format!("{base}/.well-known/oauth-protected-resource"))
        .unwrap_or_default();
    let body = json!({
        "error": "invalid_token",
        "error_description": "connect with OAuth first",
    });
    let mut response = (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response();
    if let Ok(value) = format!("Bearer resource_metadata=\"{metadata}\"").parse() {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

fn too_many(retry_after_secs: u64) -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("too many requests; try again in {retry_after_secs}s"),
    )
        .into_response();
    if let Ok(value) = retry_after_secs.to_string().parse() {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

fn rpc_http(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}
