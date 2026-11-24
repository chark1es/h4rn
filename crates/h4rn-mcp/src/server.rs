//! The streamable-HTTP MCP endpoint exposed to harness sessions.
//!
//! One route, `/mcp`, on loopback. Every request carries the per-session bearer
//! token minted by the daemon; the token's scope decides which servers and
//! tools the session can see. The endpoint is deliberately stateless between
//! requests: Streamable HTTP allows a server to answer without a long-lived
//! stream, so a restart of the daemon only invalidates tokens, not streams.
//!
//! JSON-RPC carries one message per request (the 2025-11-25 Streamable HTTP
//! transport dropped batches). Notifications return `202 Accepted` with no
//! body. A malformed body returns a JSON-RPC parse error rather than an HTML
//! error page, because harnesses parse the body.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::hub::{HubError, McpHub};
use crate::jsonrpc::{self, RpcErrorObject};
use crate::names;
use crate::provider::ToolCallContext;
use crate::session::AuthorizedSession;

/// How long a single `tools/call` may take before the hub returns an error.
const TOOL_TIMEOUT: Duration = crate::upstream::DEFAULT_TOOL_TIMEOUT;

/// Supplies the protected-resource metadata URL for a `401`, when OAuth is on.
pub type ResourceMetadata = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Shared state for the MCP route.
#[derive(Clone)]
pub struct McpServerState {
    /// The hub that owns every operation.
    pub hub: Arc<McpHub>,
    /// Cancellation tokens for in-flight tool calls, keyed by request id.
    inflight: Arc<Mutex<HashMap<String, CancellationToken>>>,
    /// The protected-resource metadata to name on a `401`, when this daemon
    /// serves OAuth. The supplier reads the switch and the bound address when
    /// the request is answered, so it follows both.
    resource_metadata: Arc<RwLock<Option<ResourceMetadata>>>,
}

impl McpServerState {
    /// Creates the route state.
    pub fn new(hub: Arc<McpHub>) -> Self {
        Self {
            hub,
            inflight: Arc::new(Mutex::new(HashMap::new())),
            resource_metadata: Arc::new(RwLock::new(None)),
        }
    }

    /// Names the protected-resource metadata a `401` points at.
    pub fn set_resource_metadata(&self, supplier: ResourceMetadata) {
        *self
            .resource_metadata
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(supplier);
    }

    /// The protected-resource metadata URL, when one is configured and live.
    fn resource_metadata(&self) -> Option<String> {
        self.resource_metadata
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(|supplier| supplier())
    }

    /// Handles one Streamable-HTTP `POST /mcp` request.
    ///
    /// The daemon owns the outer router (it shares state with `/ws`), so it
    /// calls this instead of merging [`router`].
    pub async fn handle_post(&self, headers: HeaderMap, body: Bytes) -> Response {
        mcp_post(State(self.clone()), headers, body).await
    }

    /// Answers `GET /mcp`: the hub does not open a server-initiated stream.
    pub async fn handle_get(&self) -> Response {
        mcp_get(State(self.clone())).await
    }

    /// Handles `DELETE /mcp` by revoking the caller's session token.
    pub async fn handle_delete(&self, headers: HeaderMap) -> Response {
        mcp_delete(State(self.clone()), headers).await
    }
}

/// Builds the MCP router. The daemon merges it into its own router.
pub fn router(hub: Arc<McpHub>) -> Router {
    Router::new()
        .route("/mcp", post(mcp_post).get(mcp_get).delete(mcp_delete))
        .with_state(McpServerState::new(hub))
}

/// Hooks the configured hub path into an existing router.
pub fn mount(router: Router, hub: Arc<McpHub>) -> Router {
    router.merge(self::router(hub))
}

fn json_response(value: Value) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response()
}

fn rpc_error(status: StatusCode, id: Value, error: RpcErrorObject) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        error.response(id).to_string(),
    )
        .into_response()
}

fn unauthorized(message: &str, resource_metadata: Option<String>) -> Response {
    let mut response = rpc_error(
        StatusCode::UNAUTHORIZED,
        Value::Null,
        RpcErrorObject::new(jsonrpc::INVALID_REQUEST, message),
    );
    // MCP's authorization spec: a `401` names the protected-resource metadata
    // so a client that supports OAuth can discover where to sign in.
    if let Some(metadata) = resource_metadata
        && let Ok(value) = format!("Bearer resource_metadata=\"{metadata}\"").parse()
    {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    Some(token.trim())
}

fn authorize(
    state: &McpServerState,
    headers: &HeaderMap,
) -> Result<AuthorizedSession, Box<Response>> {
    let metadata = state.resource_metadata();
    let token = bearer_token(headers)
        .ok_or_else(|| Box::new(unauthorized("missing bearer token", metadata.clone())))?;
    let session = state.hub.authorize(token).ok_or_else(|| {
        Box::new(unauthorized(
            "the MCP token is invalid or expired",
            metadata.clone(),
        ))
    })?;
    if let Some(header) = headers
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        && header != session.session_id
    {
        return Err(Box::new(rpc_error(
            StatusCode::NOT_FOUND,
            Value::Null,
            RpcErrorObject::new(jsonrpc::INVALID_REQUEST, "unknown MCP session"),
        )));
    }
    Ok(session)
}

async fn mcp_post(
    State(state): State<McpServerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let session = match authorize(&state, &headers) {
        Ok(session) => session,
        Err(response) => return *response,
    };
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return rpc_error(
                StatusCode::BAD_REQUEST,
                Value::Null,
                RpcErrorObject::new(jsonrpc::PARSE_ERROR, format!("invalid JSON: {error}")),
            );
        }
    };
    if value.is_array() {
        // The 2025-11-25 transport carries one JSON-RPC message per POST. A
        // batch would run several tool calls for one authorization check and
        // one rate-limit hit, so it is refused rather than half-supported.
        return rpc_error(
            StatusCode::BAD_REQUEST,
            Value::Null,
            RpcErrorObject::new(
                jsonrpc::INVALID_REQUEST,
                "h4rn accepts one JSON-RPC message per request",
            ),
        );
    }
    match dispatch(&state, &session, value).await {
        Some(response) => {
            let mut http_response = json_response(response.body);
            if let Some(session_id) = response.session_id
                && let Ok(value) = session_id.parse()
            {
                http_response.headers_mut().insert("mcp-session-id", value);
            }
            http_response
        }
        None => (StatusCode::ACCEPTED, "").into_response(),
    }
}

async fn mcp_get(State(_state): State<McpServerState>) -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST, DELETE")],
        "the h4rn MCP hub does not open a server-initiated stream",
    )
        .into_response()
}

async fn mcp_delete(State(state): State<McpServerState>, headers: HeaderMap) -> Response {
    if let Some(token) = bearer_token(&headers) {
        state.hub.sessions().revoke(token);
    }
    (StatusCode::NO_CONTENT, "").into_response()
}

struct Dispatch {
    body: Value,
    session_id: Option<String>,
}

async fn dispatch(
    state: &McpServerState,
    session: &AuthorizedSession,
    message: Value,
) -> Option<Dispatch> {
    let incoming = match jsonrpc::parse_message(message) {
        Ok(incoming) => incoming,
        Err(error) => {
            return Some(Dispatch {
                body: RpcErrorObject::new(jsonrpc::PARSE_ERROR, error.to_string())
                    .response(Value::Null),
                session_id: None,
            });
        }
    };
    match incoming {
        jsonrpc::Incoming::Notification { method, params } => {
            if method == "notifications/cancelled" {
                cancel(&state.inflight, &params).await;
            }
            None
        }
        jsonrpc::Incoming::Response { .. } => None,
        jsonrpc::Incoming::Request { id, method, params } => {
            let outcome = handle_request(state, session, &id, &method, params).await;
            Some(Dispatch {
                body: match outcome {
                    Ok(result) => jsonrpc::success(id, result),
                    Err(error) => error.response(id),
                },
                session_id: (method == "initialize").then(|| session.session_id.clone()),
            })
        }
    }
}

async fn cancel(inflight: &Mutex<HashMap<String, CancellationToken>>, params: &Value) {
    let Some(request_id) = params.get("requestId") else {
        return;
    };
    let key = request_id.to_string();
    if let Some(token) = inflight.lock().await.remove(&key) {
        token.cancel();
    }
}

async fn handle_request(
    state: &McpServerState,
    session: &AuthorizedSession,
    id: &Value,
    method: &str,
    params: Value,
) -> Result<Value, RpcErrorObject> {
    let scope = session.scope.clone();
    match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str);
            let version = crate::negotiate_protocol_version(requested);
            state
                .hub
                .sessions()
                .set_protocol_version(&session.session_id, version);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {
                    "tools": {"listChanged": true},
                    "resources": {"listChanged": true, "subscribe": false},
                    "prompts": {"listChanged": true}
                },
                "serverInfo": {
                    "name": crate::HUB_SERVER_NAME,
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": "This is the h4rn MCP hub. Use the server__tool names it lists; h4rn__ tools are h4rn's own."
            }))
        }
        "ping" => Ok(json!({})),
        "logging/setLevel" => Ok(json!({})),
        "tools/list" => {
            let tools = state.hub.resolved_tools(&scope).await.map_err(hub_error)?;
            Ok(json!({
                "tools": tools.into_iter().map(|tool| {
                    let mut definition = serde_json::to_value(&tool.definition).unwrap_or_else(|_| json!({}));
                    definition["name"] = Value::String(tool.qualified);
                    definition
                }).collect::<Vec<_>>()
            }))
        }
        "tools/call" => call_tool(state, session, id, params).await,
        "resources/list" => {
            let resources = state.hub.list_resources(&scope).await.map_err(hub_error)?;
            Ok(json!({ "resources": resources }))
        }
        "resources/read" => {
            let uri = params.get("uri").and_then(Value::as_str).ok_or_else(|| {
                RpcErrorObject::new(jsonrpc::INVALID_PARAMS, "resources/read needs a uri")
            })?;
            let contents = state
                .hub
                .read_resource(&scope, uri)
                .await
                .map_err(hub_error)?;
            Ok(json!({ "contents": contents }))
        }
        "prompts/list" => {
            let prompts = state.hub.list_prompts(&scope).await.map_err(hub_error)?;
            Ok(json!({ "prompts": prompts }))
        }
        "prompts/get" => {
            let name = params.get("name").and_then(Value::as_str).ok_or_else(|| {
                RpcErrorObject::new(jsonrpc::INVALID_PARAMS, "prompts/get needs a name")
            })?;
            let arguments = match params.get("arguments") {
                Some(Value::Object(object)) => object
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect(),
                _ => Default::default(),
            };
            let messages = state
                .hub
                .get_prompt(&scope, name, &arguments)
                .await
                .map_err(hub_error)?;
            Ok(json!({
                "description": null,
                "messages": messages,
            }))
        }
        other => Err(RpcErrorObject::new(
            jsonrpc::METHOD_NOT_FOUND,
            format!("method \"{other}\" is not served by the h4rn MCP hub"),
        )),
    }
}

async fn call_tool(
    state: &McpServerState,
    session: &AuthorizedSession,
    id: &Value,
    params: Value,
) -> Result<Value, RpcErrorObject> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcErrorObject::new(jsonrpc::INVALID_PARAMS, "tools/call needs a name"))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let scope = session.scope.clone();
    let (server, _) = names::split_qualified(name).unwrap_or(("", name));
    let cancel = CancellationToken::new();
    let key = id.to_string();
    state
        .inflight
        .lock()
        .await
        .insert(key.clone(), cancel.clone());
    let mut context = ToolCallContext::new(scope.clone(), server, TOOL_TIMEOUT).with_cancel(cancel);
    if let Some(token) = params
        .get("_meta")
        .and_then(|meta| meta.get("progressToken"))
    {
        let _ = token;
        // Progress is accepted but the hub does not stream it back: the call
        // completes in one HTTP response, which is what Streamable HTTP allows.
    }
    let result = state.hub.call_tool(&scope, name, arguments, &context).await;
    state.inflight.lock().await.remove(&key);
    if let Some(progress) = context.progress.take() {
        drop(progress);
    }
    match result {
        Ok(result) => Ok(serde_json::to_value(result).unwrap_or_else(|_| json!({"content": []}))),
        Err(HubError::Tool(error)) => Err(RpcErrorObject::new(error.code, error.message)),
        Err(HubError::Upstream(crate::upstream::UpstreamError::Rpc(error))) => Err(error),
        Err(error) => Err(hub_error(error)),
    }
}

fn hub_error(error: HubError) -> RpcErrorObject {
    match error {
        HubError::Tool(error) => RpcErrorObject::new(error.code, error.message),
        HubError::Upstream(crate::upstream::UpstreamError::Rpc(error)) => error,
        other => RpcErrorObject::new(jsonrpc::INTERNAL_ERROR, other.to_string()),
    }
}
