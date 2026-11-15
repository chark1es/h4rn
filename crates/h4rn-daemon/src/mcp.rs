//! The daemon-owned MCP hub (ADR-006).
//!
//! One hub per daemon. It exposes a loopback Streamable-HTTP endpoint at
//! `/mcp`, mints one bearer token per harness session, and proxies the user's
//! own MCP servers over stdio, streamable HTTP and legacy SSE. This module is
//! the integration layer: it opens the durable registry and secret store, wires
//! the hub into the protocol, injects the endpoint into new harness sessions,
//! and serves the owner half of cross-machine sharing on `/secure-mcp`.
//!
//! Secret handling is deliberately split: the registry stores only references
//! such as `mcp/<server id>/env/GITHUB_TOKEN`, and the value lives in the OS
//! keychain (or the encrypted fallback file). Nothing in this module ever
//! formats a secret into a protocol response or a log line.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::response::Response;
use h4rn_mcp::hub::{HubError, McpHub, McpReply};
use h4rn_mcp::jsonrpc::{self, Incoming, RpcErrorObject};
use h4rn_mcp::names;
use h4rn_mcp::provider::{EchoProvider, ToolCallContext};
use h4rn_mcp::registry::{RegistryStore, SqliteRegistry};
use h4rn_mcp::secrets::OpenSecretStore;
use h4rn_mcp::server::McpServerState;
use h4rn_mcp::session::McpScope;
use h4rn_mcp::sharing::{
    ConfirmationGate, DenyConfirmation, McpTunnel, ShareAudit, ShareAuthorizer, ShareCall,
    ShareError, TunnelSession,
};
use h4rn_mcp::upstream::{DEFAULT_TOOL_TIMEOUT, UpstreamError, UpstreamLimits};
use h4rn_protocol::{
    ClientRequest, HarnessKind, HostId, McpRequest, ProtoError, ResponseBody, ThreadId,
};
use h4rn_transport::AppFrame;
use h4rn_transport::client::{Client, ClientEvent};
use h4rn_transport::keys::{DeviceIdentity, KEY_LEN};
use h4rn_transport::server::{AcceptOutcome, Link, SecureService};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::mcp_peers::{GRANTS_HANDSHAKE, PeerDirectory, PeerLinks, serve_grants};
use crate::remote::{RemoteAudit, SessionOrigin};
use crate::threads::{McpSession, McpSessionProvider};

/// Version marker for the internal tunnel handshake.
const TUNNEL_HANDSHAKE: &str = "h4rn-mcp-tunnel-1";

/// How long `/secure-mcp` waits for the tunnel handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The daemon-wide MCP service.
pub struct McpService {
    hub: Arc<McpHub>,
    state: McpServerState,
    authorizer: Arc<ShareAuthorizer>,
    audit: Arc<DaemonShareAudit>,
    endpoint: Arc<RwLock<Option<String>>>,
    background: std::sync::Mutex<Option<JoinHandle<()>>>,
    /// Registry that decides which Noise keys are peer daemons. Without one,
    /// `/secure-mcp` admits nobody.
    devices: std::sync::RwLock<Option<Arc<crate::remote::DeviceRegistry>>>,
    peers: Arc<PeerDirectory>,
    peering: std::sync::Mutex<Option<(tokio::sync::watch::Sender<bool>, JoinHandle<()>)>>,
}

impl McpService {
    /// Points `/secure-mcp` at the registry that authenticates peer daemons.
    pub fn set_devices(&self, devices: Arc<crate::remote::DeviceRegistry>) {
        *self
            .devices
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(devices);
    }

    /// Opens the registry, secret store and hub for a daemon.
    pub async fn open(
        config: &Config,
        local_host: HostId,
        remote_audit: Option<Arc<RemoteAudit>>,
        gate: Arc<dyn ConfirmationGate>,
    ) -> anyhow::Result<Self> {
        let registry: Arc<dyn RegistryStore> = Arc::new(
            SqliteRegistry::open(config.mcp_db_path())
                .await
                .map_err(|error| anyhow::anyhow!("open the MCP registry: {error}"))?,
        );
        let secrets: Arc<dyn h4rn_mcp::secrets::SecretStore> = Arc::from(
            OpenSecretStore::open(config.mcp_secrets_path())
                .map_err(|error| anyhow::anyhow!("open the MCP secret store: {error}"))?
                .into_store(),
        );
        let hub = Arc::new(
            McpHub::open(
                local_host,
                Arc::clone(&registry),
                secrets,
                UpstreamLimits::default(),
            )
            .await
            .map_err(|error| anyhow::anyhow!("open the MCP hub: {error}"))?,
        );
        // Phase 4 ships the built-in `h4rn` namespace with its diagnostic echo
        // tool; the orchestration provider registers against the same hub.
        hub.register_provider(Arc::new(EchoProvider::new(env!("CARGO_PKG_VERSION"))));
        hub.start();

        let audit = Arc::new(DaemonShareAudit {
            remote: remote_audit,
        });
        let authorizer = Arc::new(ShareAuthorizer::new(
            registry,
            gate,
            Arc::clone(&audit) as Arc<dyn ShareAudit>,
        ));
        let state = McpServerState::new(Arc::clone(&hub));
        let service = Self {
            hub,
            state,
            authorizer,
            audit,
            endpoint: Arc::new(RwLock::new(None)),
            background: std::sync::Mutex::new(None),
            devices: std::sync::RwLock::new(None),
            peers: Arc::new(PeerDirectory::new()),
            peering: std::sync::Mutex::new(None),
        };
        service.start_background();
        Ok(service)
    }

    /// The hub itself.
    pub fn hub(&self) -> Arc<McpHub> {
        Arc::clone(&self.hub)
    }

    /// The shared Streamable-HTTP route state.
    pub fn http_state(&self) -> McpServerState {
        self.state.clone()
    }

    /// The sibling daemons this machine may share MCP servers with.
    pub fn peers(&self) -> Arc<PeerDirectory> {
        Arc::clone(&self.peers)
    }

    /// Starts following peers' grants: for every online peer, mirror the
    /// servers it shared with this machine into the hub, authenticating the
    /// tunnels as this daemon's own `identity`. Idempotent.
    pub fn start_peering(&self, identity: DeviceIdentity) {
        let mut peering = self
            .peering
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if peering.is_some() {
            return;
        }
        let links = PeerLinks::new(
            Arc::clone(&self.hub),
            Arc::clone(&self.peers),
            identity,
            self.hub.local_host().clone(),
        );
        let (stop, stopped) = tokio::sync::watch::channel(false);
        *peering = Some((stop, tokio::spawn(links.run(stopped))));
    }

    /// Sets the loopback URL injected into harness sessions.
    pub async fn set_endpoint(&self, endpoint: impl Into<String>) {
        let endpoint = endpoint.into();
        *self
            .endpoint
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(endpoint.clone());
        self.hub.set_endpoint(endpoint).await;
    }

    /// Starts the periodic upstream health and token-expiry pass.
    fn start_background(&self) {
        let hub = Arc::clone(&self.hub);
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(15));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                hub.health_tick().await;
            }
        });
        *self
            .background
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
    }

    /// Stops the background pass, every upstream and the hub event bridge.
    pub async fn shutdown(&self) {
        let peering = self
            .peering
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some((stop, handle)) = peering {
            let _ = stop.send(true);
            let _ = handle.await;
        }
        if let Some(handle) = self
            .background
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            handle.abort();
        }
        self.hub.shutdown().await;
    }

    /// Maps one UI MCP request onto the hub and the protocol response shape.
    pub async fn handle(&self, request: McpRequest) -> Result<ResponseBody, ProtoError> {
        match request {
            McpRequest::Upsert { input } => self
                .hub
                .upsert(input)
                .await
                .map(|server| ResponseBody::McpServer { server })
                .map_err(HubError::into_proto),
            McpRequest::Remove { name } => self
                .hub
                .remove(&name)
                .await
                .map(|()| ResponseBody::Ack)
                .map_err(HubError::into_proto),
            McpRequest::SetEnabled {
                name,
                harness,
                machine,
                enabled,
            } => self
                .hub
                .set_enabled(&name, harness, machine, enabled)
                .await
                .map(|server| ResponseBody::McpServer { server })
                .map_err(HubError::into_proto),
            other => self
                .hub
                .handle_request(other)
                .await
                .map(reply_to_protocol)
                .map_err(HubError::into_proto),
        }
    }

    /// The registry that authenticates peer daemons, if one is wired.
    fn devices(&self) -> Option<Arc<crate::remote::DeviceRegistry>> {
        self.devices
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The machine and device id of `key` when the registry lists it as a
    /// peer daemon; `None` for any other key (clients, unknown keys).
    fn verified_peer(&self, key: &[u8; KEY_LEN]) -> Option<(HostId, String)> {
        verified_peer(self.devices().as_deref(), key)
    }

    /// Serves the owner half of one `/secure-mcp` session.
    pub async fn serve_secure_mcp(
        self: Arc<Self>,
        service: Arc<SecureService>,
        link: Box<dyn Link>,
    ) {
        let handle = match service.accept(link).await {
            Ok(AcceptOutcome::New(handle)) => handle,
            Ok(AcceptOutcome::Resumed { session_id }) => {
                tracing::debug!(
                    session = %hex::encode(session_id),
                    "secure MCP session resumed on a new path"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "secure MCP handshake failed");
                return;
            }
        };
        let device = handle.device;
        let (sender, mut frames) = handle.into_parts();

        let handshake = match tokio::time::timeout(HANDSHAKE_TIMEOUT, frames.recv()).await {
            Ok(Some(frame)) => frame,
            _ => {
                tracing::debug!("secure MCP client sent no handshake");
                return;
            }
        };
        let Some(text) = handshake.as_text() else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return;
        };
        // Both the grants handshake and the tool tunnel are peer-daemon only:
        // identity comes from the authenticated Noise key alone, resolved
        // through the device registry. Anything the frame says about the
        // machine or device is a claim, never an override.
        let Some((machine, device_id)) = self.verified_peer(&device) else {
            tracing::warn!(
                key = %hex::encode(device),
                "refused a secure MCP peer that is not a registered daemon"
            );
            let error = jsonrpc::RpcErrorObject::new(
                jsonrpc::INVALID_REQUEST,
                "this key is not a registered peer daemon",
            );
            let _ = sender.send(AppFrame::text(error.response(Value::Null).to_string()));
            return;
        };
        if value.get("h4rn_mcp_grants").and_then(Value::as_str) == Some(GRANTS_HANDSHAKE) {
            let registry = self.devices();
            let verified = machine.clone();
            serve_grants(
                Arc::clone(&self.hub),
                Arc::clone(&self.peers),
                machine,
                move || {
                    verified_peer(registry.as_deref(), &device)
                        .is_some_and(|(current, _)| current == verified)
                },
                sender,
                frames,
            )
            .await;
            return;
        }
        if value.get("h4rn_mcp_tunnel").and_then(Value::as_str) != Some(TUNNEL_HANDSHAKE) {
            return;
        }
        let Some(server) = value.get("server").and_then(Value::as_str) else {
            return;
        };
        let server = names::normalise_server_name(server);
        let claimed_machine = value.get("machine").and_then(Value::as_str);
        let claimed_device = value.get("device_id").and_then(Value::as_str);
        if claimed_machine.is_some_and(|claimed| claimed != machine.as_str())
            || claimed_device.is_some_and(|claimed| claimed != device_id)
        {
            tracing::warn!(
                %machine,
                "refused a secure MCP peer whose claimed identity disagrees with its key"
            );
            let error = jsonrpc::RpcErrorObject::new(
                jsonrpc::INVALID_REQUEST,
                "the claimed machine or device does not match the authenticated key",
            );
            let _ = sender.send(AppFrame::text(error.response(Value::Null).to_string()));
            return;
        }
        let device_id = Some(device_id);

        if self
            .hub
            .share_record(&server, &machine)
            .await
            .ok()
            .flatten()
            .is_none()
        {
            let error = jsonrpc::RpcErrorObject::new(
                jsonrpc::INVALID_REQUEST,
                format!("\"{server}\" is not shared with {machine}"),
            );
            let _ = sender.send(AppFrame::text(error.response(Value::Null).to_string()));
            return;
        }

        tracing::info!(%server, %machine, "opened a shared MCP tunnel");
        while let Some(frame) = frames.recv().await {
            let Some(text) = frame.as_text() else {
                continue;
            };
            let Ok(message) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            let Ok(incoming) = jsonrpc::parse_message(message) else {
                continue;
            };
            match incoming {
                Incoming::Notification { .. } | Incoming::Response { .. } => {}
                Incoming::Request { id, method, params } => {
                    let outcome = self
                        .tunnel_request(
                            &server,
                            &machine,
                            device_id.clone(),
                            device,
                            &method,
                            params,
                        )
                        .await;
                    let response = match outcome {
                        Ok(result) => jsonrpc::success(id, result),
                        Err(error) => error.response(id),
                    };
                    if sender.send(AppFrame::text(response.to_string())).is_err() {
                        break;
                    }
                }
            }
        }
    }

    async fn tunnel_request(
        &self,
        server: &str,
        machine: &HostId,
        device_id: Option<String>,
        device: [u8; KEY_LEN],
        method: &str,
        params: Value,
    ) -> Result<Value, RpcErrorObject> {
        let scope = McpScope::remote(machine.clone(), device_id.clone())
            .with_servers(vec![server.to_owned()]);
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": h4rn_mcp::negotiate_protocol_version(
                    params.get("protocolVersion").and_then(Value::as_str)
                ),
                "capabilities": {
                    "tools": {"listChanged": true},
                    "resources": {"listChanged": true},
                    "prompts": {"listChanged": true}
                },
                "serverInfo": {"name": server, "version": env!("CARGO_PKG_VERSION")},
            })),
            "ping" => Ok(json!({})),
            "tools/list" => {
                let tools = self
                    .hub
                    .resolved_tools(&scope)
                    .await
                    .map_err(hub_error)?
                    .into_iter()
                    .filter(|tool| tool.server == server)
                    .map(|tool| serde_json::to_value(&tool.definition).unwrap_or(Value::Null))
                    .collect::<Vec<_>>();
                Ok(json!({ "tools": tools }))
            }
            "tools/call" => {
                let tool = params.get("name").and_then(Value::as_str).ok_or_else(|| {
                    RpcErrorObject::new(jsonrpc::INVALID_PARAMS, "tools/call needs a name")
                })?;
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                self.call_shared(server, machine, device_id, device, tool, arguments, &scope)
                    .await
            }
            "resources/list" => {
                let resources = self
                    .hub
                    .list_resources(&scope)
                    .await
                    .map_err(hub_error)?
                    .into_iter()
                    .filter_map(|mut resource| {
                        let (owner, original) = h4rn_mcp::hub::decode_resource_uri(&resource.uri)?;
                        if owner != server {
                            return None;
                        }
                        resource.uri = original;
                        Some(resource)
                    })
                    .collect::<Vec<_>>();
                Ok(json!({ "resources": resources }))
            }
            "resources/read" => {
                let uri = params.get("uri").and_then(Value::as_str).ok_or_else(|| {
                    RpcErrorObject::new(jsonrpc::INVALID_PARAMS, "resources/read needs a uri")
                })?;
                let qualified = h4rn_mcp::hub::encode_resource_uri(server, uri);
                let contents = self
                    .hub
                    .read_resource(&scope, &qualified)
                    .await
                    .map_err(hub_error)?;
                Ok(json!({ "contents": contents }))
            }
            "prompts/list" => {
                let prompts = self
                    .hub
                    .list_prompts(&scope)
                    .await
                    .map_err(hub_error)?
                    .into_iter()
                    .filter_map(|mut prompt| {
                        let (owner, original) = names::split_qualified(&prompt.name)?;
                        if owner != server {
                            return None;
                        }
                        prompt.name = original.to_owned();
                        Some(prompt)
                    })
                    .collect::<Vec<_>>();
                Ok(json!({ "prompts": prompts }))
            }
            "prompts/get" => {
                let name = params.get("name").and_then(Value::as_str).ok_or_else(|| {
                    RpcErrorObject::new(jsonrpc::INVALID_PARAMS, "prompts/get needs a name")
                })?;
                let qualified = names::qualified(server, name);
                let arguments = match params.get("arguments") {
                    Some(Value::Object(object)) => object
                        .iter()
                        .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                        .collect(),
                    _ => BTreeMap::new(),
                };
                let messages = self
                    .hub
                    .get_prompt(&scope, &qualified, &arguments)
                    .await
                    .map_err(hub_error)?;
                Ok(json!({ "description": null, "messages": messages }))
            }
            other => Err(RpcErrorObject::new(
                jsonrpc::METHOD_NOT_FOUND,
                format!("method \"{other}\" is not shared"),
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn call_shared(
        &self,
        server: &str,
        machine: &HostId,
        device_id: Option<String>,
        device: [u8; KEY_LEN],
        tool: &str,
        arguments: Value,
        scope: &McpScope,
    ) -> Result<Value, RpcErrorObject> {
        let Some((record, _share)) = self
            .hub
            .share_record(server, machine)
            .await
            .map_err(hub_error)?
        else {
            return Err(RpcErrorObject::new(
                jsonrpc::INVALID_REQUEST,
                format!("\"{server}\" is not shared with {machine}"),
            ));
        };
        let call = ShareCall {
            device_id: device_id.clone(),
            // Reaching this point means the caller's key is the registered
            // daemon key of `machine` (see `serve_secure_mcp`).
            peer_verified: true,
            machine: machine.clone(),
            server: server.to_owned(),
            tool: tool.to_owned(),
            arguments: Some(arguments.clone()),
        };
        if let Err(error) = self.authorizer.authorize(&record, &call).await {
            return Err(RpcErrorObject::new(
                jsonrpc::INVALID_REQUEST,
                error.to_string(),
            ));
        }
        let qualified = names::qualified(server, tool);
        let context = ToolCallContext::new(scope.clone(), server, DEFAULT_TOOL_TIMEOUT);
        let result = self
            .hub
            .call_tool(scope, &qualified, arguments, &context)
            .await;
        match result {
            Ok(result) => {
                self.hub
                    .audit_remote(&call, &Ok::<_, ShareError>(result.clone()));
                self.audit.record(device, &call, true, None);
                serde_json::to_value(result).map_err(|error| {
                    RpcErrorObject::new(jsonrpc::INTERNAL_ERROR, error.to_string())
                })
            }
            Err(error) => {
                let shared = ShareError::Upstream(UpstreamError::Unavailable(error.to_string()));
                self.hub.audit_remote(&call, &Err(shared));
                self.audit
                    .record(device, &call, false, Some(&error.to_string()));
                Err(hub_error(error))
            }
        }
    }
}

impl McpSessionProvider for McpService {
    fn prepare(&self, harness: HarnessKind, thread_id: &ThreadId) -> Option<McpSession> {
        let endpoint = self
            .endpoint
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()?;
        let issued = self.hub.issue_session(McpScope::harness(
            harness,
            self.hub.local_host().clone(),
            Some(thread_id.clone()),
        ));
        Some(McpSession {
            endpoint,
            token: issued.token,
        })
    }
}

/// Adapter that writes owner-side remote calls to the daemon audit trail.
struct DaemonShareAudit {
    remote: Option<Arc<RemoteAudit>>,
}

impl DaemonShareAudit {
    fn record(&self, device: [u8; KEY_LEN], call: &ShareCall, allowed: bool, error: Option<&str>) {
        let Some(audit) = &self.remote else {
            return;
        };
        let mut origin = SessionOrigin::secure(device);
        origin.device_id = call.device_id.clone();
        audit.record(
            &origin,
            "mcp.remote_call",
            Some("mcp_tool"),
            Some(&names::qualified(&call.server, &call.tool)),
            json!({
                "machine": call.machine.as_str(),
                "server": call.server,
                "tool": call.tool,
                "allowed": allowed,
                "error": error,
            }),
        );
    }
}

impl ShareAudit for DaemonShareAudit {
    fn remote_call(&self, call: &ShareCall, allowed: bool, error: Option<&str>) {
        // The authenticated device key is recorded by the tunnel loop; policy
        // denials still reach the trail through this best-effort entry.
        if let Some(audit) = &self.remote {
            let mut origin = SessionOrigin::relay(call.device_id.clone(), None);
            origin.device_id = call.device_id.clone();
            audit.record(
                &origin,
                "mcp.remote_call",
                Some("mcp_tool"),
                Some(&names::qualified(&call.server, &call.tool)),
                json!({
                    "machine": call.machine.as_str(),
                    "server": call.server,
                    "tool": call.tool,
                    "allowed": allowed,
                    "error": error,
                }),
            );
        }
    }
}

/// Resolves a Noise key to `(machine, device id)` when `registry` lists it as a
/// peer daemon. Client keys, unknown keys and a missing registry give `None`.
pub(crate) fn verified_peer(
    registry: Option<&crate::remote::DeviceRegistry>,
    key: &[u8; KEY_LEN],
) -> Option<(HostId, String)> {
    let auth = registry?
        .authorization(key)
        .filter(|auth| auth.role == crate::remote::Role::Host)?;
    let machine = auth.machine?;
    let device_id = auth.device_id.unwrap_or_else(|| hex::encode(key));
    Some((machine, device_id))
}

fn hub_error(error: HubError) -> RpcErrorObject {
    match error {
        HubError::Upstream(UpstreamError::Rpc(error)) => error,
        HubError::Tool(error) => RpcErrorObject::new(error.code, error.message),
        other => RpcErrorObject::new(jsonrpc::INTERNAL_ERROR, other.to_string()),
    }
}

fn reply_to_protocol(reply: McpReply) -> ResponseBody {
    match reply {
        McpReply::Servers(servers) => ResponseBody::McpServers { servers },
        McpReply::Server(server) => ResponseBody::McpServer { server },
        McpReply::Preview(preview) => ResponseBody::McpImportPreview { preview },
        McpReply::Imported(servers) => ResponseBody::McpServers { servers },
        McpReply::Resolved(Some(server)) => ResponseBody::McpServer { server },
        McpReply::Resolved(None) => ResponseBody::Ack,
        McpReply::Tools { server, tools } => ResponseBody::McpTools { server, tools },
        McpReply::Call(result) => ResponseBody::McpCall { result },
        McpReply::Ack => ResponseBody::Ack,
    }
}

/// Copies the legacy `McpList`/`McpUpsert`/`McpRemove` requests onto the
/// envelope that Phase 4 introduced.
pub fn legacy_request(request: ClientRequest) -> Option<McpRequest> {
    match request {
        ClientRequest::McpList => Some(McpRequest::ListServers),
        ClientRequest::McpUpsert { input } => Some(McpRequest::Upsert { input }),
        ClientRequest::McpRemove { name } => Some(McpRequest::Remove { name }),
        ClientRequest::Mcp { request } => Some(request),
        _ => None,
    }
}

/// Handles the daemon's `/mcp` POST route.
pub async fn http_post(state: McpServerState, headers: HeaderMap, body: Bytes) -> Response {
    state.handle_post(headers, body).await
}

/// Handles the daemon's `/mcp` GET route.
pub async fn http_get(state: McpServerState) -> Response {
    state.handle_get().await
}

/// Handles the daemon's `/mcp` DELETE route.
pub async fn http_delete(state: McpServerState, headers: HeaderMap) -> Response {
    state.handle_delete(headers).await
}

/// Confirmation policy for shared tool calls.
///
/// The daemon defaults to denying every confirmation: a remote call that the
/// policy marks as needing approval fails until the user has given one. Tests
/// and explicit "always allow" grants inject an allowing gate.
pub fn default_confirmation_gate() -> Arc<dyn ConfirmationGate> {
    Arc::new(DenyConfirmation)
}

/// Re-exported for callers that build a tunnel to another daemon.
pub use tunnel::{TransportMcpTunnel, TunnelConnection};

/// The B-side tunnel, kept in a submodule so the owner code reads top to bottom.
mod tunnel {
    use super::*;

    /// Everything needed to dial another daemon's `/secure-mcp` route.
    #[derive(Clone)]
    pub struct TunnelConnection {
        /// `ws://host:port/secure-mcp` candidates on the owning daemon (LAN,
        /// tailnet), probed in parallel by the multipath engine.
        pub urls: Vec<String>,
        /// This daemon's transport identity. The owner authenticates the
        /// tunnel by this key.
        pub identity: DeviceIdentity,
        /// The owning daemon's pinned X25519 key.
        pub daemon_public: [u8; 32],
        /// This daemon's host id, used by the owner's share policy.
        pub machine: HostId,
        /// Connect device id, when known.
        pub device_id: Option<String>,
    }

    /// A [`McpTunnel`] that carries JSON-RPC over the encrypted transport.
    pub struct TransportMcpTunnel {
        connection: TunnelConnection,
    }

    impl TransportMcpTunnel {
        /// Creates a tunnel factory.
        pub fn new(connection: TunnelConnection) -> Self {
            Self { connection }
        }
    }

    #[async_trait::async_trait]
    impl McpTunnel for TransportMcpTunnel {
        async fn open(&self, server: &str) -> Result<Arc<dyn TunnelSession>, UpstreamError> {
            let client = crate::mcp_peers::establish(
                &self.connection.urls,
                &self.connection.identity,
                self.connection.daemon_public,
            )
            .await
            .map_err(UpstreamError::Unavailable)?;

            let handshake = json!({
                "h4rn_mcp_tunnel": TUNNEL_HANDSHAKE,
                "server": server,
                "machine": self.connection.machine.as_str(),
                "device_id": self.connection.device_id,
            });
            client
                .send(AppFrame::text(handshake.to_string()))
                .await
                .map_err(|error| UpstreamError::Unavailable(error.to_string()))?;

            let session = Arc::new(TransportTunnelSession::start(client));
            Ok(session)
        }
    }

    struct TransportTunnelSession {
        outbound: mpsc::Sender<TunnelOutbound>,
        pending: TunnelPending,
        next_id: AtomicI64,
        reader: JoinHandle<()>,
    }

    type TunnelPending =
        Arc<tokio::sync::Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcErrorObject>>>>>;

    enum TunnelOutbound {
        Frame(AppFrame),
        Close,
    }

    impl TransportTunnelSession {
        fn start(mut client: Client) -> Self {
            let (outbound, mut rx) = mpsc::channel::<TunnelOutbound>(64);
            let pending: TunnelPending = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
            let reader_pending = Arc::clone(&pending);
            let reader = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        command = rx.recv() => {
                            match command {
                                Some(TunnelOutbound::Frame(frame)) => {
                                    if client.send(frame).await.is_err() {
                                        break;
                                    }
                                }
                                Some(TunnelOutbound::Close) | None => {
                                    client.close().await;
                                    break;
                                }
                            }
                        }
                        event = client.recv() => {
                            match event {
                                Some(ClientEvent::Frame(frame)) => {
                                    let Some(text) = frame.as_text() else { continue };
                                    let Ok(value) = serde_json::from_str::<Value>(text) else { continue };
                                    let (id, outcome) = match jsonrpc::parse_message(value) {
                                        Ok(Incoming::Response { id, result, error }) => {
                                            let outcome = match (result, error) {
                                                (Some(result), _) => Ok(result),
                                                (None, Some(error)) => Err(error),
                                                (None, None) => Err(RpcErrorObject::new(
                                                    jsonrpc::INTERNAL_ERROR,
                                                    "the remote hub sent an empty response",
                                                )),
                                            };
                                            (jsonrpc::numeric_id(&id), outcome)
                                        }
                                        _ => continue,
                                    };
                                    let Some(id) = id else { continue };
                                    if let Some(sender) = reader_pending.lock().await.remove(&id) {
                                        let _ = sender.send(outcome);
                                    }
                                }
                                Some(ClientEvent::Closed { reason }) => {
                                    tracing::debug!(%reason, "secure MCP tunnel closed");
                                    break;
                                }
                                Some(ClientEvent::Error { message, .. }) => {
                                    tracing::debug!(%message, "secure MCP tunnel error");
                                }
                                Some(ClientEvent::Status(_) | ClientEvent::Established(_)) => {}
                                None => break,
                            }
                        }
                    }
                }
                let mut pending = reader_pending.lock().await;
                for (_, sender) in pending.drain() {
                    let _ = sender.send(Err(RpcErrorObject::new(
                        jsonrpc::INTERNAL_ERROR,
                        "the secure MCP tunnel closed",
                    )));
                }
            });
            Self {
                outbound,
                pending,
                next_id: AtomicI64::new(1),
                reader,
            }
        }
    }

    #[async_trait::async_trait]
    impl TunnelSession for TransportTunnelSession {
        async fn request(
            &self,
            method: &str,
            params: Value,
            timeout: Duration,
        ) -> Result<Value, UpstreamError> {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let (sender, receiver) = oneshot::channel();
            self.pending.lock().await.insert(id, sender);
            let message = jsonrpc::request(id, method, params);
            if self
                .outbound
                .send(TunnelOutbound::Frame(AppFrame::text(message.to_string())))
                .await
                .is_err()
            {
                self.pending.lock().await.remove(&id);
                return Err(UpstreamError::Unavailable(
                    "the secure MCP tunnel is closed".into(),
                ));
            }
            match tokio::time::timeout(timeout, receiver).await {
                Ok(Ok(Ok(value))) => Ok(value),
                Ok(Ok(Err(error))) => Err(UpstreamError::Rpc(error)),
                Ok(Err(_)) => Err(UpstreamError::Unavailable(
                    "the secure MCP tunnel dropped the request".into(),
                )),
                Err(_) => {
                    self.pending.lock().await.remove(&id);
                    Err(UpstreamError::Timeout)
                }
            }
        }

        async fn notify(&self, method: &str, params: Value) -> Result<(), UpstreamError> {
            let message = jsonrpc::notification(method, params);
            self.outbound
                .send(TunnelOutbound::Frame(AppFrame::text(message.to_string())))
                .await
                .map_err(|_| UpstreamError::Unavailable("the secure MCP tunnel is closed".into()))
        }

        async fn shutdown(&self) {
            let _ = self.outbound.send(TunnelOutbound::Close).await;
            self.reader.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_requests_map_onto_the_envelope() {
        assert!(matches!(
            legacy_request(ClientRequest::McpList),
            Some(McpRequest::ListServers)
        ));
    }

    #[test]
    fn responses_map_to_protocol_variants() {
        assert!(matches!(
            reply_to_protocol(McpReply::Ack),
            ResponseBody::Ack
        ));
    }

    #[test]
    fn conflict_resolution_is_reachable() {
        // A compile-time check that the protocol enum stays wired here.
        let _ = h4rn_protocol::McpConflictResolution::KeepMine;
    }
}
