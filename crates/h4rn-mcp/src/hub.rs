//! The hub: one object that owns the registry, the providers, the upstream
//! manager and the per-session tokens.
//!
//! The HTTP endpoint in [`crate::server`] and the daemon's protocol handlers are
//! both thin adapters over this type. Keeping the policy here means the local
//! UI, a harness session and a remote machine all see the same enable matrix,
//! the same namespacing and the same share rules.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use chrono::Utc;
use h4rn_protocol::{
    HarnessKind, HostId, McpCaller, McpConflictResolution, McpEnabledMatrix, McpImportPreview,
    McpRequest, McpSecretInput, McpServerInput, McpServerStatusKind, McpServerView, McpSharePolicy,
    McpToolInfo, McpTransportInput, McpTransportView, ProtoError, ProtoErrorCode,
};
use serde_json::Value;
use tokio::sync::{Mutex, broadcast, watch};

use crate::import::{self, ImportError, ImportedServer, ImportedTransport};
use crate::names::{self, NameError};
use crate::provider::{ProviderRegistry, ToolCallContext, ToolError, ToolProvider};
use crate::registry::{
    RegistryError, RegistryStore, ServerRecord, ShareRecord, StoredSecret, StoredTransport,
};
use crate::secrets::{SecretError, SecretStore, secret_id};
use crate::session::{AuthorizedSession, IssuedToken, McpScope, McpSessionRegistry};
use crate::sharing::{McpTunnel, ShareCall};
use crate::types::{CallToolResult, PromptMessage, ResourceContents, ToolDefinition};
use crate::upstream::{
    UpstreamConfig, UpstreamError, UpstreamEvent, UpstreamLimits, UpstreamManager,
};

/// A hub failure.
#[derive(Debug, thiserror::Error)]
pub enum HubError {
    /// The registry failed.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// The secret store failed.
    #[error(transparent)]
    Secret(#[from] SecretError),
    /// A name was invalid.
    #[error(transparent)]
    Name(#[from] NameError),
    /// A native configuration could not be read.
    #[error(transparent)]
    Import(#[from] ImportError),
    /// An upstream failed.
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    /// A tool/provider failed at the protocol level.
    #[error(transparent)]
    Tool(#[from] ToolError),
    /// The request was malformed.
    #[error("{0}")]
    Invalid(String),
    /// Nothing is configured under that name.
    #[error("no MCP server named \"{0}\" is configured")]
    NotFound(String),
    /// A name collision.
    #[error("an MCP server named \"{0}\" already exists")]
    Conflict(String),
}

impl HubError {
    /// Converts to the wire error the UI displays.
    pub fn into_proto(self) -> ProtoError {
        match self {
            Self::Name(error) => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                error.to_string(),
                "Server names must match [a-z0-9][a-z0-9_-]*, must not contain __, and \"h4rn\" is reserved",
            ),
            Self::Invalid(message) => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                message,
                "Fix the request payload and retry",
            ),
            Self::NotFound(message) => ProtoError::new(
                ProtoErrorCode::NotFound,
                message,
                "Refresh the MCP server list",
            ),
            Self::Conflict(message) => ProtoError::new(
                ProtoErrorCode::Conflict,
                message,
                "Choose a different name or remove the existing server first",
            ),
            Self::Registry(error) => ProtoError::new(
                ProtoErrorCode::Internal,
                error.to_string(),
                "Retry the request",
            ),
            Self::Secret(error) => ProtoError::new(
                ProtoErrorCode::Internal,
                error.to_string(),
                "Check the daemon's keychain access",
            ),
            Self::Import(error) => ProtoError::new(
                ProtoErrorCode::InvalidRequest,
                error.to_string(),
                "Check that the harness configuration file is valid",
            ),
            Self::Upstream(error) => ProtoError::new(
                ProtoErrorCode::Internal,
                error.to_string(),
                "Check the server's status and retry",
            ),
            Self::Tool(error) => ProtoError::new(
                ProtoErrorCode::Internal,
                error.message,
                "Retry the call or inspect the server's logs",
            ),
        }
    }
}

/// A runtime change the daemon forwards to its clients.
#[derive(Clone, Debug)]
pub enum HubEvent {
    /// One server's status changed.
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
    /// One tool call passed through the hub.
    ToolCalled {
        /// Server namespace.
        server: String,
        /// Unqualified tool name.
        tool: String,
        /// Fully qualified name.
        qualified_name: String,
        /// Who called it.
        caller: McpCaller,
        /// Whether it succeeded.
        ok: bool,
        /// Round trip in milliseconds.
        duration_ms: u64,
        /// Redacted error.
        error: Option<String>,
        /// When it returned.
        at: chrono::DateTime<Utc>,
    },
    /// A server's tool list is stale.
    ToolsChanged {
        /// Server name.
        server: String,
    },
    /// A shared tool call is waiting for the owner's decision (ADR-037).
    ShareApprovalRequested {
        /// What the owner is asked to approve.
        approval: h4rn_protocol::McpShareApproval,
    },
    /// A pending shared tool call was answered, expired or cancelled.
    ShareApprovalResolved {
        /// Approval that is no longer pending.
        approval_id: String,
        /// What happened to it.
        decision: h4rn_protocol::ApprovalDecision,
    },
}

/// Where a qualified tool lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolSource {
    /// An in-process provider, by namespace and registration index.
    Provider {
        /// Namespace the provider exposes tools under.
        namespace: String,
        /// Position in that namespace's provider list.
        index: usize,
    },
    /// A configured upstream server, by name.
    Upstream(String),
}

/// One model-visible tool.
#[derive(Clone, Debug)]
pub struct ResolvedTool {
    /// Fully qualified name.
    pub qualified: String,
    /// Namespace (server or provider).
    pub server: String,
    /// Unqualified tool name.
    pub tool: String,
    /// Definition sent to the client.
    pub definition: ToolDefinition,
    /// Where to execute it.
    pub source: ToolSource,
}

struct RemoteBinding {
    machine: HostId,
    tunnel: Arc<dyn McpTunnel>,
}

/// The daemon-wide MCP hub.
pub struct McpHub {
    local_host: HostId,
    registry: Arc<dyn RegistryStore>,
    secrets: Arc<dyn SecretStore>,
    upstreams: Arc<UpstreamManager>,
    sessions: Arc<McpSessionRegistry>,
    providers: RwLock<ProviderRegistry>,
    events: broadcast::Sender<HubEvent>,
    endpoint: Mutex<Option<String>>,
    remotes: RwLock<BTreeMap<String, RemoteBinding>>,
    /// Bumped whenever a share grant is added, changed or removed, so the
    /// daemon can push the new grant set to the machines it affects.
    grants: watch::Sender<u64>,
    bridge: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl McpHub {
    /// Opens the hub over durable storage and restores the configured servers.
    pub async fn open(
        local_host: HostId,
        registry: Arc<dyn RegistryStore>,
        secrets: Arc<dyn SecretStore>,
        limits: UpstreamLimits,
    ) -> Result<Self, HubError> {
        let upstreams = Arc::new(UpstreamManager::new(limits));
        let (events, _) = broadcast::channel(512);
        let hub = Self {
            local_host,
            registry,
            secrets,
            upstreams,
            sessions: Arc::new(McpSessionRegistry::new()),
            providers: RwLock::new(ProviderRegistry::new()),
            events,
            endpoint: Mutex::new(None),
            remotes: RwLock::new(BTreeMap::new()),
            grants: watch::channel(0).0,
            bridge: std::sync::Mutex::new(None),
        };
        hub.reload().await;
        Ok(hub)
    }

    /// Starts forwarding upstream lifecycle events. Call once, after wrapping
    /// the hub in an `Arc`.
    pub fn start(self: &Arc<Self>) {
        let mut receiver = self.upstreams.subscribe();
        let hub = Arc::clone(self);
        let handle = tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(event) => hub.forward_upstream(event).await,
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "the MCP hub fell behind upstream events");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        *self
            .bridge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
    }

    async fn forward_upstream(&self, event: UpstreamEvent) {
        match event {
            UpstreamEvent::Status {
                server,
                status,
                tool_count,
                message,
                at,
            } => {
                let _ = self.events.send(HubEvent::Status {
                    server,
                    status,
                    tool_count,
                    message,
                    at,
                });
            }
            UpstreamEvent::ToolsChanged { server } => {
                self.upstreams.invalidate_tools(&server).await;
                let _ = self.events.send(HubEvent::ToolsChanged { server });
            }
            UpstreamEvent::ResourcesChanged { server } => {
                self.upstreams.invalidate_resources(&server).await;
            }
            UpstreamEvent::PromptsChanged { server } => {
                self.upstreams.invalidate_prompts(&server).await;
            }
        }
    }

    /// Registers an in-process provider (the built-in tools and future
    /// subagent tools plug in here).
    pub fn register_provider(&self, provider: Arc<dyn ToolProvider>) {
        self.providers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .register(provider);
    }

    /// A stable snapshot of the registered providers.
    fn provider_snapshot(&self) -> Vec<(String, usize, Arc<dyn ToolProvider>)> {
        let providers = self
            .providers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut snapshot: Vec<(String, usize, Arc<dyn ToolProvider>)> = providers
            .namespaces()
            .flat_map(|namespace| {
                providers
                    .for_namespace(namespace)
                    .into_iter()
                    .enumerate()
                    .map(move |(index, provider)| (namespace.to_owned(), index, provider))
            })
            .collect();
        snapshot.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
        snapshot
    }

    fn provider(&self, namespace: &str, index: usize) -> Option<Arc<dyn ToolProvider>> {
        self.providers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .for_namespace(namespace)
            .into_iter()
            .nth(index)
    }

    /// This machine's id.
    pub fn local_host(&self) -> &HostId {
        &self.local_host
    }

    /// The session registry.
    pub fn sessions(&self) -> Arc<McpSessionRegistry> {
        Arc::clone(&self.sessions)
    }

    /// The upstream manager (diagnostics and sharing).
    pub fn upstreams(&self) -> Arc<UpstreamManager> {
        Arc::clone(&self.upstreams)
    }

    /// A sender for the hub's transient event stream. The daemon's approval
    /// gate publishes through it so approval cards share the MCP forwarder.
    pub fn event_sender(&self) -> broadcast::Sender<HubEvent> {
        self.events.clone()
    }

    /// Subscribes to status and audit events.
    pub fn subscribe(&self) -> broadcast::Receiver<HubEvent> {
        self.events.subscribe()
    }

    /// Sets the loopback endpoint injected into harness sessions.
    pub async fn set_endpoint(&self, endpoint: impl Into<String>) {
        *self.endpoint.lock().await = Some(endpoint.into());
    }

    /// The endpoint injected into harness sessions, once bound.
    pub async fn endpoint(&self) -> Option<String> {
        self.endpoint.lock().await.clone()
    }

    /// Durable server and share storage, for the daemon's `/secure-mcp`
    /// authorizer.
    pub fn registry(&self) -> Arc<dyn RegistryStore> {
        Arc::clone(&self.registry)
    }

    /// Mints a token for one harness session.
    pub fn issue_session(&self, scope: McpScope) -> IssuedToken {
        self.sessions.issue(scope)
    }

    /// Validates a bearer token.
    pub fn authorize(&self, token: &str) -> Option<AuthorizedSession> {
        self.sessions.authorize(token)
    }

    /// Revokes every token belonging to a thread.
    pub fn revoke_thread(&self, thread_id: &h4rn_protocol::ThreadId) -> usize {
        self.sessions.revoke_thread(thread_id)
    }

    /// Rebuilds the upstream manager from the registry and remote bindings.
    pub async fn reload(&self) {
        let records = match self.registry.list().await {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, "could not load MCP servers");
                return;
            }
        };
        let mut configs = BTreeMap::new();
        for record in &records {
            match self.resolve_config(record).await {
                Ok(Some(config)) => {
                    configs.insert(record.name.clone(), config);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(server = %record.name, %error, "skipping an unresolvable MCP server");
                }
            }
        }
        let remotes: Vec<(String, HostId, Arc<dyn McpTunnel>)> = {
            let remotes = self
                .remotes
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            remotes
                .iter()
                .map(|(name, binding)| {
                    (
                        name.clone(),
                        binding.machine.clone(),
                        Arc::clone(&binding.tunnel),
                    )
                })
                .collect()
        };
        for (name, machine, tunnel) in remotes {
            configs.insert(name.clone(), UpstreamConfig::Remote { machine, tunnel });
        }
        self.upstreams.replace(configs).await;
    }

    async fn resolve_config(
        &self,
        record: &ServerRecord,
    ) -> Result<Option<UpstreamConfig>, HubError> {
        let secrets = Arc::clone(&self.secrets);
        let resolve = |stored: &[StoredSecret]| -> Result<BTreeMap<String, String>, HubError> {
            let mut values = BTreeMap::new();
            for secret in stored {
                match secrets.get(&secret.secret_id)? {
                    Some(value) => {
                        values.insert(secret.name.clone(), value);
                    }
                    None => {
                        return Err(HubError::Invalid(format!(
                            "the secret for \"{}\" is missing",
                            secret.name
                        )));
                    }
                }
            }
            Ok(values)
        };
        Ok(Some(match &record.transport {
            StoredTransport::Stdio {
                command,
                args,
                cwd,
                env,
            } => UpstreamConfig::Stdio {
                command: command.clone(),
                args: args.clone(),
                cwd: cwd.clone().map(PathBuf::from),
                env: resolve(env)?,
            },
            StoredTransport::Http { url, headers } => UpstreamConfig::Http {
                url: url.clone(),
                headers: resolve(headers)?,
            },
            StoredTransport::Sse { url, headers } => UpstreamConfig::Sse {
                url: url.clone(),
                headers: resolve(headers)?,
            },
        }))
    }

    /// Registers a server that lives on another machine.
    pub async fn register_remote(
        &self,
        name: impl Into<String>,
        machine: HostId,
        tunnel: Arc<dyn McpTunnel>,
    ) {
        let name = name.into();
        {
            let mut remotes = self
                .remotes
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            remotes.insert(
                name.clone(),
                RemoteBinding {
                    machine: machine.clone(),
                    tunnel: Arc::clone(&tunnel),
                },
            );
        }
        self.upstreams
            .upsert(name, UpstreamConfig::Remote { machine, tunnel })
            .await;
    }

    /// Drops a remote binding and stops its tunnel. Returns whether it existed.
    pub async fn unregister_remote(&self, name: &str) -> bool {
        let existed = self
            .remotes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(name)
            .is_some();
        if existed {
            self.upstreams.remove(name).await;
        }
        existed
    }

    /// Whether a locally configured server already uses `name`.
    pub async fn has_local_server(&self, name: &str) -> bool {
        let name = names::normalise_server_name(name);
        matches!(self.registry.get(&name).await, Ok(Some(_)))
    }

    /// Whether `name` is a remote binding (not a local server).
    pub fn is_remote(&self, name: &str) -> bool {
        self.remotes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(name)
    }

    /// Every configured server plus remote bindings, with runtime status.
    pub async fn list_servers(&self) -> Result<Vec<McpServerView>, HubError> {
        let records = self.registry.list().await?;
        let shares = self.registry.list_shares().await?;
        let mut views = Vec::with_capacity(records.len());
        for record in &records {
            views.push(self.view_for(record, &shares).await);
        }
        let remote_names: Vec<(String, HostId)> = {
            let remotes = self
                .remotes
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            remotes
                .iter()
                .filter(|(name, _)| !records.iter().any(|record| &record.name == *name))
                .map(|(name, binding)| (name.clone(), binding.machine.clone()))
                .collect()
        };
        for (name, machine) in remote_names {
            let status = self.upstreams.status(&name).await;
            views.push(McpServerView {
                id: format!("remote:{name}"),
                name: name.clone(),
                transport: McpTransportView::Http {
                    url: format!("h4rn-remote://{machine}/{name}"),
                    header_keys: Vec::new(),
                },
                tags: Vec::new(),
                enabled: McpEnabledMatrix::default(),
                status: status.status,
                tool_count: status.tool_count,
                last_error: status.last_error,
                runs_on: Some(machine),
                shared_to: Vec::new(),
                imported_from: None,
                updated_at: Utc::now(),
            });
        }
        views.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(views)
    }

    async fn view_for(&self, record: &ServerRecord, shares: &[ShareRecord]) -> McpServerView {
        let status = self.upstreams.status(&record.name).await;
        McpServerView {
            id: record.id.clone(),
            name: record.name.clone(),
            transport: import::stored_view(&record.transport),
            tags: record.tags.clone(),
            enabled: record.enabled.clone(),
            status: status.status,
            tool_count: status.tool_count,
            last_error: status.last_error,
            runs_on: status.machine,
            shared_to: shares
                .iter()
                .filter(|share| share.server_id == record.id)
                .map(|share| share.machine.clone())
                .collect(),
            imported_from: record.imported_from.clone(),
            updated_at: record.updated_at,
        }
    }

    /// One server by name.
    pub async fn server(&self, name: &str) -> Result<McpServerView, HubError> {
        let name = names::normalise_server_name(name);
        let record = self
            .registry
            .get(&name)
            .await?
            .ok_or_else(|| HubError::NotFound(name.clone()))?;
        let shares = self.registry.list_shares().await?;
        Ok(self.view_for(&record, &shares).await)
    }

    /// Adds or updates a server.
    pub async fn upsert(&self, input: McpServerInput) -> Result<McpServerView, HubError> {
        let name = names::normalise_server_name(&input.name);
        names::validate_server_name(&name)?;
        let existing = self.registry.get(&name).await?;
        let now = Utc::now();
        let record = if let Some(existing) = &existing {
            ServerRecord {
                id: existing.id.clone(),
                name: name.clone(),
                transport: self.build_transport(&existing.id, &input.transport)?,
                tags: input.tags.clone(),
                enabled: input.enabled.clone(),
                imported_from: existing.imported_from.clone(),
                created_at: existing.created_at,
                updated_at: now,
            }
        } else {
            let id = format!("srv_{}", uuid::Uuid::now_v7());
            let transport = self.build_transport(&id, &input.transport)?;
            ServerRecord {
                id,
                name: name.clone(),
                transport,
                tags: input.tags.clone(),
                enabled: input.enabled.clone(),
                imported_from: None,
                created_at: now,
                updated_at: now,
            }
        };
        if let Some(previous) = &existing {
            self.delete_unreferenced_secrets(previous, &record.transport)?;
        }
        self.registry.put(&record).await?;
        if let Some(config) = self.resolve_config(&record).await? {
            self.upstreams.upsert(record.name.clone(), config).await;
        }
        let shares = self.registry.list_shares().await?;
        Ok(self.view_for(&record, &shares).await)
    }

    fn build_transport(
        &self,
        server_id: &str,
        input: &McpTransportInput,
    ) -> Result<StoredTransport, HubError> {
        Ok(match input {
            McpTransportInput::Stdio {
                command,
                args,
                cwd,
                env,
            } => {
                if command.trim().is_empty() {
                    return Err(HubError::Invalid("the command must not be empty".into()));
                }
                StoredTransport::Stdio {
                    command: command.clone(),
                    args: args.clone(),
                    cwd: cwd.clone(),
                    env: self.store_secrets(server_id, "env", env),
                }
            }
            McpTransportInput::Http { url, headers } => StoredTransport::Http {
                url: required_url(url)?,
                headers: self.store_secrets(server_id, "header", headers),
            },
            McpTransportInput::Sse { url, headers } => StoredTransport::Sse {
                url: required_url(url)?,
                headers: self.store_secrets(server_id, "header", headers),
            },
        })
    }

    fn store_secrets(
        &self,
        server_id: &str,
        kind: &str,
        inputs: &[McpSecretInput],
    ) -> Vec<StoredSecret> {
        let mut stored = BTreeMap::new();
        for input in inputs {
            let name = input.name.trim();
            if name.is_empty() {
                continue;
            }
            let reference = secret_id(server_id, kind, name);
            match &input.value {
                Some(value) if value.is_empty() => {
                    let _ = self.secrets.delete(&reference);
                }
                Some(value) => {
                    if let Err(error) = self.secrets.put(&reference, value) {
                        tracing::warn!(%error, "could not store an MCP secret");
                        continue;
                    }
                }
                None => {
                    // Keep the stored value when one exists. A missing value on
                    // a new server is dropped rather than stored empty.
                    match self.secrets.get(&reference) {
                        Ok(Some(_)) => {}
                        Ok(None) => continue,
                        Err(error) => {
                            tracing::warn!(%error, "could not read an MCP secret");
                            continue;
                        }
                    }
                }
            }
            stored.insert(
                name.to_owned(),
                StoredSecret {
                    name: name.to_owned(),
                    secret_id: reference,
                },
            );
        }
        stored.into_values().collect()
    }

    fn delete_unreferenced_secrets(
        &self,
        previous: &ServerRecord,
        next: &StoredTransport,
    ) -> Result<(), HubError> {
        let next_ids: BTreeSet<&str> = next
            .secrets()
            .into_iter()
            .map(|secret| secret.secret_id.as_str())
            .collect();
        for secret in previous.transport.secrets() {
            if !next_ids.contains(secret.secret_id.as_str()) {
                self.secrets.delete(&secret.secret_id)?;
            }
        }
        Ok(())
    }

    /// Removes one server and its secret values.
    pub async fn remove(&self, name: &str) -> Result<(), HubError> {
        let name = names::normalise_server_name(name);
        let record = self
            .registry
            .get(&name)
            .await?
            .ok_or_else(|| HubError::NotFound(name.clone()))?;
        for secret in record.transport.secrets() {
            self.secrets.delete(&secret.secret_id)?;
        }
        self.registry.remove_shares_for(&record.id).await?;
        self.registry.remove(&record.id).await?;
        self.grants.send_modify(|version| *version += 1);
        self.upstreams.remove(&name).await;
        Ok(())
    }

    /// Toggles one cell of the enable matrix.
    pub async fn set_enabled(
        &self,
        name: &str,
        harness: Option<HarnessKind>,
        machine: Option<HostId>,
        enabled: bool,
    ) -> Result<McpServerView, HubError> {
        if harness.is_none() && machine.is_none() {
            return Err(HubError::Invalid(
                "set_enabled needs a harness or a machine".into(),
            ));
        }
        let name = names::normalise_server_name(name);
        let mut record = self
            .registry
            .get(&name)
            .await?
            .ok_or_else(|| HubError::NotFound(name.clone()))?;
        if let Some(harness) = harness {
            record.enabled.set_harness(harness, enabled);
        }
        if let Some(machine) = machine {
            record.enabled.set_machine(machine, enabled);
        }
        record.updated_at = Utc::now();
        self.registry.put(&record).await?;
        let shares = self.registry.list_shares().await?;
        Ok(self.view_for(&record, &shares).await)
    }

    /// Reads a harness's native configuration and previews an import.
    pub async fn import(
        &self,
        harness: HarnessKind,
        workspace: Option<String>,
    ) -> Result<McpImportPreview, HubError> {
        let scan = self.read_import(harness, workspace.as_deref())?;
        let existing = self.registry.list().await?;
        Ok(import::preview(harness, scan, &existing))
    }

    /// Applies every non-conflicting candidate.
    pub async fn apply_import(
        &self,
        harness: HarnessKind,
        workspace: Option<String>,
    ) -> Result<Vec<McpServerView>, HubError> {
        let scan = self.read_import(harness, workspace.as_deref())?;
        let existing = self.registry.list().await?;
        let preview = import::preview(harness, scan, &existing);
        let mut applied = Vec::new();
        for candidate in preview.candidates {
            if candidate.existing {
                continue;
            }
            if let Some(server) =
                self.find_import(harness, workspace.as_deref(), &candidate.name)?
            {
                applied.push(
                    self.upsert(import_input(
                        &candidate.name,
                        &server,
                        Some(candidate.origin.clone()),
                    ))
                    .await?,
                );
            }
        }
        Ok(applied)
    }

    /// Applies one conflict resolution.
    pub async fn resolve_import(
        &self,
        harness: HarnessKind,
        name: &str,
        resolution: McpConflictResolution,
        workspace: Option<String>,
    ) -> Result<Option<McpServerView>, HubError> {
        let target = match resolution {
            McpConflictResolution::KeepMine | McpConflictResolution::Skip => return Ok(None),
            McpConflictResolution::UseTheirs => names::normalise_server_name(name),
            McpConflictResolution::Rename { name } => names::normalise_server_name(&name),
        };
        let Some(server) = self.find_import(harness, workspace.as_deref(), name)? else {
            return Err(HubError::NotFound(name.to_owned()));
        };
        Ok(Some(
            self.upsert(import_input(&target, &server, Some(server.origin.clone())))
                .await?,
        ))
    }

    fn read_import(
        &self,
        harness: HarnessKind,
        workspace: Option<&str>,
    ) -> Result<import::ImportScan, HubError> {
        let home = dirs::home_dir().unwrap_or_default();
        let workspace = workspace.map(PathBuf::from);
        Ok(import::read_native(harness, workspace.as_deref(), &home)?)
    }

    fn find_import(
        &self,
        harness: HarnessKind,
        workspace: Option<&str>,
        name: &str,
    ) -> Result<Option<ImportedServer>, HubError> {
        let name = names::normalise_server_name(name);
        let scan = self.read_import(harness, workspace)?;
        Ok(scan.servers.into_iter().find(|server| server.name == name))
    }

    /// Shares a local server with another machine.
    pub async fn share(
        &self,
        name: &str,
        to_machine: HostId,
        policy: McpSharePolicy,
    ) -> Result<McpServerView, HubError> {
        let name = names::normalise_server_name(name);
        let record = self
            .registry
            .get(&name)
            .await?
            .ok_or_else(|| HubError::NotFound(name.clone()))?;
        if self.upstreams.machine(&name).await.is_some() {
            return Err(HubError::Invalid(
                "a proxied server cannot be re-shared".into(),
            ));
        }
        self.registry
            .put_share(&ShareRecord {
                server_id: record.id.clone(),
                machine: to_machine,
                policy,
                created_at: Utc::now(),
            })
            .await?;
        self.grants.send_modify(|version| *version += 1);
        let shares = self.registry.list_shares().await?;
        Ok(self.view_for(&record, &shares).await)
    }

    /// Revokes one share grant.
    pub async fn unshare(&self, name: &str, machine: HostId) -> Result<(), HubError> {
        let name = names::normalise_server_name(name);
        let record = self
            .registry
            .get(&name)
            .await?
            .ok_or_else(|| HubError::NotFound(name.clone()))?;
        self.registry.remove_share(&record.id, &machine).await?;
        self.grants.send_modify(|version| *version += 1);
        Ok(())
    }

    /// A receiver that changes every time a share grant does.
    pub fn subscribe_grants(&self) -> watch::Receiver<u64> {
        self.grants.subscribe()
    }

    /// The servers shared with `machine` and the policy for each, by name.
    pub async fn grants_for(
        &self,
        machine: &HostId,
    ) -> Result<Vec<(String, McpSharePolicy)>, HubError> {
        let shares = self.registry.list_shares().await?;
        let records = self.registry.list().await?;
        let mut grants: Vec<(String, McpSharePolicy)> = shares
            .into_iter()
            .filter(|share| &share.machine == machine)
            .filter_map(|share| {
                records
                    .iter()
                    .find(|record| record.id == share.server_id)
                    .map(|record| (record.name.clone(), share.policy))
            })
            .collect();
        grants.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(grants)
    }

    /// Every server the scope may see, honouring the enable matrix.
    pub async fn visible_records(&self, scope: &McpScope) -> Result<Vec<ServerRecord>, HubError> {
        let mut records: Vec<ServerRecord> = self
            .registry
            .list()
            .await?
            .into_iter()
            .filter(|record| {
                record.enabled.is_enabled(scope.harness, &scope.machine)
                    && scope.permits_server(&record.name)
            })
            .collect();
        // A remote binding is configured on this machine but its record lives
        // on the owning machine. Present it as a synthetic record so tool,
        // resource and prompt discovery treat it like any other server.
        let remotes: Vec<(String, HostId)> = {
            let remotes = self
                .remotes
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            remotes
                .iter()
                .map(|(name, binding)| (name.clone(), binding.machine.clone()))
                .collect()
        };
        for (name, machine) in remotes {
            if records.iter().any(|record| record.name == name) || !scope.permits_server(&name) {
                continue;
            }
            records.push(ServerRecord::new(
                name.clone(),
                StoredTransport::Http {
                    url: format!("h4rn-remote://{machine}/{name}"),
                    headers: Vec::new(),
                },
                Vec::new(),
                McpEnabledMatrix::default(),
                None,
            ));
        }
        Ok(records)
    }

    /// Lists every tool the scope can see, with names resolved.
    pub async fn resolved_tools(&self, scope: &McpScope) -> Result<Vec<ResolvedTool>, HubError> {
        let mut used = BTreeSet::new();
        let mut tools = Vec::new();
        for (namespace, index, provider) in self.provider_snapshot() {
            for definition in provider.list_tools(scope).await {
                if !names::tool_name_is_addressable(&definition.name) {
                    continue;
                }
                let base = names::qualified(&namespace, &definition.name);
                if !scope.permits_tool(&namespace, &base) {
                    continue;
                }
                let qualified = names::allocate(&base, &mut used);
                tools.push(ResolvedTool {
                    qualified,
                    server: namespace.clone(),
                    tool: definition.name.clone(),
                    definition,
                    source: ToolSource::Provider {
                        namespace: namespace.clone(),
                        index,
                    },
                });
            }
        }
        for record in self.visible_records(scope).await? {
            let definitions = match self.upstreams.list_tools(&record.name).await {
                Ok(definitions) => definitions,
                Err(error) => {
                    tracing::debug!(server = %record.name, %error, "could not list tools");
                    continue;
                }
            };
            for definition in definitions {
                if !names::tool_name_is_addressable(&definition.name) {
                    continue;
                }
                let base = names::qualified(&record.name, &definition.name);
                if !scope.permits_tool(&record.name, &base) {
                    continue;
                }
                let qualified = names::allocate(&base, &mut used);
                tools.push(ResolvedTool {
                    qualified,
                    server: record.name.clone(),
                    tool: definition.name.clone(),
                    definition,
                    source: ToolSource::Upstream(record.name.clone()),
                });
            }
        }
        Ok(tools)
    }

    /// Lists every tool in the UI contract shape.
    pub async fn list_tools(&self, scope: &McpScope) -> Result<Vec<McpToolInfo>, HubError> {
        Ok(self
            .resolved_tools(scope)
            .await?
            .into_iter()
            .map(|tool| {
                let read_only = tool.definition.read_only();
                McpToolInfo {
                    name: tool.qualified,
                    server: tool.server,
                    tool: tool.tool,
                    description: tool.definition.description,
                    input_schema: tool.definition.input_schema,
                    read_only,
                }
            })
            .collect())
    }

    /// Calls one qualified tool.
    pub async fn call_tool(
        &self,
        scope: &McpScope,
        qualified: &str,
        arguments: Value,
        context: &ToolCallContext,
    ) -> Result<CallToolResult, HubError> {
        let started = Instant::now();
        let tools = self.resolved_tools(scope).await?;
        let Some(tool) = tools.into_iter().find(|tool| tool.qualified == qualified) else {
            return Err(HubError::Invalid(format!(
                "no tool named \"{qualified}\" is available to this session"
            )));
        };
        if !scope.permits_tool(&tool.server, &tool.qualified) {
            return Err(HubError::Invalid(format!(
                "\"{qualified}\" is not permitted for this session"
            )));
        }
        let result = match &tool.source {
            ToolSource::Provider { namespace, index } => {
                let provider = self
                    .provider(namespace, *index)
                    .ok_or_else(|| HubError::NotFound(namespace.clone()))?;
                match provider.call_tool(&tool.tool, arguments, context).await {
                    Ok(result) => Ok(result),
                    Err(error) if error.is_tool_error => Ok(CallToolResult::error(error.message)),
                    Err(error) => Err(HubError::Tool(error)),
                }
            }
            ToolSource::Upstream(server) => self
                .upstreams
                .call_tool(server, &tool.tool, arguments, context)
                .await
                .map_err(HubError::Upstream),
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        let _ = self.events.send(HubEvent::ToolCalled {
            server: tool.server.clone(),
            tool: tool.tool.clone(),
            qualified_name: tool.qualified.clone(),
            caller: scope.caller.clone(),
            ok: result
                .as_ref()
                .map(|result| !result.is_error)
                .unwrap_or(false),
            duration_ms,
            error: result.as_ref().err().map(ToString::to_string),
            at: Utc::now(),
        });
        result
    }

    /// Lists resources with upstream URIs namespaced so two servers cannot
    /// collide on the same URI.
    pub async fn list_resources(
        &self,
        scope: &McpScope,
    ) -> Result<Vec<crate::types::ResourceDefinition>, HubError> {
        let mut resources = Vec::new();
        for (_namespace, _index, provider) in self.provider_snapshot() {
            resources.extend(provider.list_resources(scope).await);
        }
        for record in self.visible_records(scope).await? {
            if let Ok(mut listed) = self.upstreams.list_resources(&record.name).await {
                for resource in &mut listed {
                    resource.uri = encode_resource_uri(&record.name, &resource.uri);
                }
                resources.extend(listed);
            }
        }
        Ok(resources)
    }

    /// Reads one resource.
    pub async fn read_resource(
        &self,
        scope: &McpScope,
        uri: &str,
    ) -> Result<Vec<ResourceContents>, HubError> {
        if let Some((server, original)) = decode_resource_uri(uri) {
            if !scope.permits_server(&server) {
                return Err(HubError::Invalid(format!(
                    "\"{server}\" is not permitted for this session"
                )));
            }
            return self
                .upstreams
                .read_resource(&server, &original)
                .await
                .map_err(HubError::Upstream);
        }
        for (_namespace, _index, provider) in self.provider_snapshot() {
            if let Ok(contents) = provider.read_resource(uri, scope).await {
                return Ok(contents);
            }
        }
        Err(HubError::NotFound(uri.to_owned()))
    }

    /// Lists prompts, qualifying upstream prompt names with their server.
    pub async fn list_prompts(
        &self,
        scope: &McpScope,
    ) -> Result<Vec<crate::types::PromptDefinition>, HubError> {
        let mut prompts = Vec::new();
        for (_namespace, _index, provider) in self.provider_snapshot() {
            prompts.extend(provider.list_prompts(scope).await);
        }
        for record in self.visible_records(scope).await? {
            if let Ok(mut listed) = self.upstreams.list_prompts(&record.name).await {
                for prompt in &mut listed {
                    prompt.name = names::qualified(&record.name, &prompt.name);
                }
                prompts.extend(listed);
            }
        }
        Ok(prompts)
    }

    /// Renders one prompt.
    pub async fn get_prompt(
        &self,
        scope: &McpScope,
        name: &str,
        arguments: &BTreeMap<String, String>,
    ) -> Result<Vec<PromptMessage>, HubError> {
        if let Some((server, prompt)) = names::split_qualified(name) {
            if !scope.permits_server(server) {
                return Err(HubError::Invalid(format!(
                    "\"{server}\" is not permitted for this session"
                )));
            }
            return self
                .upstreams
                .get_prompt(server, prompt, arguments)
                .await
                .map_err(HubError::Upstream);
        }
        for (_namespace, _index, provider) in self.provider_snapshot() {
            if let Ok(messages) = provider.get_prompt(name, arguments, scope).await {
                return Ok(messages);
            }
        }
        Err(HubError::NotFound(name.to_owned()))
    }

    /// The owner-side check for one remote call.
    pub async fn share_record(
        &self,
        name: &str,
        machine: &HostId,
    ) -> Result<Option<(ServerRecord, ShareRecord)>, HubError> {
        let name = names::normalise_server_name(name);
        let Some(record) = self.registry.get(&name).await? else {
            return Ok(None);
        };
        let shares = self.registry.list_shares().await?;
        Ok(shares
            .into_iter()
            .find(|share| share.server_id == record.id && &share.machine == machine)
            .map(|share| (record, share)))
    }

    /// Records a remote tool call in the hub's transient event stream.
    pub fn audit_remote(
        &self,
        call: &ShareCall,
        result: &Result<CallToolResult, crate::sharing::ShareError>,
    ) {
        let _ = self.events.send(HubEvent::ToolCalled {
            server: call.server.clone(),
            tool: call.tool.clone(),
            qualified_name: names::qualified(&call.server, &call.tool),
            caller: McpCaller::Remote {
                device_id: call.device_id.clone(),
                machine: Some(call.machine.clone()),
            },
            ok: result.as_ref().is_ok_and(|result| !result.is_error),
            duration_ms: 0,
            error: result.as_ref().err().map(ToString::to_string),
            at: Utc::now(),
        });
    }

    /// Dispatches one UI request; used by the daemon protocol handler.
    pub async fn handle_request(&self, request: McpRequest) -> Result<McpReply, HubError> {
        match request {
            McpRequest::ListServers => Ok(McpReply::Servers(self.list_servers().await?)),
            McpRequest::Upsert { input } => Ok(McpReply::Server(self.upsert(input).await?)),
            McpRequest::Remove { name } => {
                self.remove(&name).await?;
                Ok(McpReply::Ack)
            }
            McpRequest::SetEnabled {
                name,
                harness,
                machine,
                enabled,
            } => Ok(McpReply::Server(
                self.set_enabled(&name, harness, machine, enabled).await?,
            )),
            McpRequest::Import { harness, workspace } => {
                Ok(McpReply::Preview(self.import(harness, workspace).await?))
            }
            McpRequest::ApplyImport { harness, workspace } => {
                let servers = self.apply_import(harness, workspace).await?;
                Ok(McpReply::Imported(servers))
            }
            McpRequest::ResolveConflict {
                harness,
                name,
                resolution,
                workspace,
            } => {
                let server = self
                    .resolve_import(harness, &name, resolution, workspace)
                    .await?;
                Ok(McpReply::Resolved(server))
            }
            McpRequest::Share {
                name,
                to_machine,
                policy,
            } => Ok(McpReply::Server(
                self.share(&name, to_machine, policy).await?,
            )),
            McpRequest::Unshare { name, machine } => {
                self.unshare(&name, machine).await?;
                Ok(McpReply::Ack)
            }
            McpRequest::ListTools { name } => {
                let scope = McpScope::client(self.local_host.clone());
                let name = names::normalise_server_name(&name);
                let tools = self.list_tools(&scope).await?;
                Ok(McpReply::Tools {
                    server: name.clone(),
                    tools: tools
                        .into_iter()
                        .filter(|tool| tool.server == name)
                        .collect(),
                })
            }
            // Share approvals live on the daemon's confirmation gate, not in
            // the hub; `McpService::handle` answers these before dispatch.
            McpRequest::PendingApprovals | McpRequest::RespondApproval { .. } => {
                Err(HubError::Invalid(
                    "share approvals are handled by the daemon".into(),
                ))
            }
            McpRequest::TestCall {
                name,
                tool,
                arguments,
            } => {
                let scope = McpScope::client(self.local_host.clone());
                let qualified = names::qualified(&names::normalise_server_name(&name), &tool);
                let context = ToolCallContext::new(
                    scope.clone(),
                    name,
                    crate::upstream::DEFAULT_TOOL_TIMEOUT,
                );
                let started = Instant::now();
                let result = self
                    .call_tool(&scope, &qualified, arguments, &context)
                    .await?;
                Ok(McpReply::Call(h4rn_protocol::McpTestCallResult {
                    content: result.text_output(),
                    is_error: result.is_error,
                    truncated: false,
                    duration_ms: started.elapsed().as_millis() as u64,
                }))
            }
        }
    }

    /// One background pass: upstream health and token expiry.
    pub async fn health_tick(&self) {
        self.upstreams.health_tick().await;
        self.sessions.prune_expired();
    }

    /// Stops every upstream and the event bridge.
    pub async fn shutdown(&self) {
        let bridge = self
            .bridge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(bridge) = bridge {
            bridge.abort();
        }
        self.upstreams.shutdown().await;
    }
}

/// A successful reply to a UI request.
#[derive(Debug)]
pub enum McpReply {
    /// Every server.
    Servers(Vec<McpServerView>),
    /// One server.
    Server(McpServerView),
    /// An import preview.
    Preview(McpImportPreview),
    /// Servers created by an import.
    Imported(Vec<McpServerView>),
    /// One conflict resolution's result.
    Resolved(Option<McpServerView>),
    /// Tools.
    Tools {
        /// Server name.
        server: String,
        /// Qualified tools.
        tools: Vec<McpToolInfo>,
    },
    /// One diagnostic call.
    Call(h4rn_protocol::McpTestCallResult),
    /// The request changed state but returns nothing.
    Ack,
}

fn required_url(url: &str) -> Result<String, HubError> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(HubError::Invalid("the URL must not be empty".into()));
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(HubError::Invalid(
            "the URL must start with http:// or https://".into(),
        ));
    }
    Ok(trimmed.to_owned())
}

/// Encodes an upstream resource URI so it is unambiguous in the flat list.
pub fn encode_resource_uri(server: &str, uri: &str) -> String {
    format!("h4rn-resource://{server}/{}", hex::encode(uri))
}

/// Reverses [`encode_resource_uri`].
pub fn decode_resource_uri(qualified: &str) -> Option<(String, String)> {
    let rest = qualified.strip_prefix("h4rn-resource://")?;
    let (server, encoded) = rest.split_once('/')?;
    let uri = String::from_utf8(hex::decode(encoded).ok()?).ok()?;
    Some((server.to_owned(), uri))
}

fn import_input(name: &str, server: &ImportedServer, origin: Option<String>) -> McpServerInput {
    let transport = match &server.transport {
        ImportedTransport::Stdio {
            command,
            args,
            cwd,
            env,
        } => McpTransportInput::Stdio {
            command: command.clone(),
            args: args.clone(),
            cwd: cwd.clone(),
            env: env
                .iter()
                .map(|secret| McpSecretInput {
                    name: secret.name.clone(),
                    value: Some(secret.value.clone()),
                })
                .collect(),
        },
        ImportedTransport::Http { url, headers } => McpTransportInput::Http {
            url: url.clone(),
            headers: headers
                .iter()
                .map(|secret| McpSecretInput {
                    name: secret.name.clone(),
                    value: Some(secret.value.clone()),
                })
                .collect(),
        },
        ImportedTransport::Sse { url, headers } => McpTransportInput::Sse {
            url: url.clone(),
            headers: headers
                .iter()
                .map(|secret| McpSecretInput {
                    name: secret.name.clone(),
                    value: Some(secret.value.clone()),
                })
                .collect(),
        },
    };
    let _ = origin;
    McpServerInput {
        name: name.to_owned(),
        transport,
        tags: Vec::new(),
        enabled: McpEnabledMatrix::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::EchoProvider;
    use crate::registry::{MemoryRegistry, SqliteRegistry};
    use crate::secrets::MemorySecretStore;
    use h4rn_protocol::ThreadId;
    use serde_json::json;
    use std::time::Duration;

    async fn hub() -> Arc<McpHub> {
        let hub = Arc::new(
            McpHub::open(
                HostId::new("host_a"),
                Arc::new(MemoryRegistry::new()),
                Arc::new(MemorySecretStore::new()),
                UpstreamLimits::default(),
            )
            .await
            .unwrap(),
        );
        hub.register_provider(Arc::new(EchoProvider::new("test")));
        hub
    }

    fn scope() -> McpScope {
        McpScope::harness(
            HarnessKind::ClaudeCode,
            HostId::new("host_a"),
            Some(ThreadId::new("th_1")),
        )
    }

    #[tokio::test]
    async fn providers_and_upstreams_share_one_namespace() {
        let hub = hub().await;
        let tools = hub.list_tools(&scope()).await.unwrap();
        assert!(tools.iter().any(|tool| tool.name == "h4rn__echo"));
    }

    #[tokio::test]
    async fn upsert_persists_only_secret_references() {
        let registry = Arc::new(SqliteRegistry::open_memory().await.unwrap());
        let secrets = Arc::new(MemorySecretStore::new());
        let hub = Arc::new(
            McpHub::open(
                HostId::new("host_a"),
                registry.clone(),
                secrets.clone(),
                UpstreamLimits::default(),
            )
            .await
            .unwrap(),
        );
        let view = hub
            .upsert(McpServerInput {
                name: "GitHub".into(),
                transport: McpTransportInput::Stdio {
                    command: "npx".into(),
                    args: vec!["-y".into(), "server".into()],
                    cwd: None,
                    env: vec![McpSecretInput {
                        name: "GITHUB_TOKEN".into(),
                        value: Some("ghp_secret".into()),
                    }],
                },
                tags: Vec::new(),
                enabled: McpEnabledMatrix::default(),
            })
            .await
            .unwrap();
        assert_eq!(view.name, "github");
        let record = registry.get("github").await.unwrap().unwrap();
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("ghp_secret"));
        assert!(json.contains("mcp/srv_"));
        assert!(
            secrets
                .get(&record.transport.secrets()[0].secret_id)
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn secret_values_never_reach_the_view() {
        let hub = hub().await;
        let view = hub
            .upsert(McpServerInput {
                name: "remote".into(),
                transport: McpTransportInput::Http {
                    url: "https://example.test/mcp".into(),
                    headers: vec![McpSecretInput {
                        name: "Authorization".into(),
                        value: Some("Bearer super-secret".into()),
                    }],
                },
                tags: Vec::new(),
                enabled: McpEnabledMatrix::default(),
            })
            .await
            .unwrap();
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("Authorization"));
        assert!(!json.contains("super-secret"));
    }

    #[tokio::test]
    async fn a_disabled_harness_does_not_see_the_server() {
        let hub = hub().await;
        hub.upsert(McpServerInput {
            name: "local".into(),
            transport: McpTransportInput::Stdio {
                command: "true".into(),
                args: Vec::new(),
                cwd: None,
                env: Vec::new(),
            },
            tags: Vec::new(),
            enabled: McpEnabledMatrix::default(),
        })
        .await
        .unwrap();
        hub.set_enabled("local", Some(HarnessKind::ClaudeCode), None, false)
            .await
            .unwrap();
        let scope = scope();
        assert!(
            !hub.visible_records(&scope)
                .await
                .unwrap()
                .iter()
                .any(|record| record.name == "local")
        );
    }

    #[tokio::test]
    async fn tool_calls_go_through_the_provider() {
        let hub = hub().await;
        let context = ToolCallContext::new(scope(), "h4rn", Duration::from_secs(1));
        let result = hub
            .call_tool(&scope(), "h4rn__echo", json!({"message": "hi"}), &context)
            .await
            .unwrap();
        assert_eq!(result.text_output(), "hi");
    }

    #[test]
    fn resource_uris_round_trip() {
        let uri = "file:///tmp/a b.txt";
        let encoded = encode_resource_uri("files", uri);
        assert_eq!(
            decode_resource_uri(&encoded),
            Some(("files".into(), uri.into()))
        );
        assert_eq!(decode_resource_uri("file:///tmp/x"), None);
    }

    #[test]
    fn import_input_keeps_secret_values_for_the_store_only() {
        let server = ImportedServer {
            name: "x".into(),
            origin: "test".into(),
            transport: ImportedTransport::Http {
                url: "https://x".into(),
                headers: vec![import::ImportedSecret {
                    name: "Authorization".into(),
                    value: "secret".into(),
                }],
            },
        };
        let input = import_input("x", &server, None);
        let json = serde_json::to_string(&input).unwrap();
        assert!(json.contains("secret"));
    }
}
