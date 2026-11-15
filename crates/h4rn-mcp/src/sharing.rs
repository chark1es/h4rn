//! Cross-machine MCP proxying and the owner-side share policy.
//!
//! A server configured on machine A can be used from machine B without copying
//! its command, endpoint or credentials. B holds only a `host B -> host A`
//! reference and a tunnel; A executes the server, so the secret never leaves
//! the machine that owns it, and A can audit and refuse every call.
//!
//! [`McpTunnel`] is the seam between the two sides. The shipping implementation
//! lives in the daemon (it needs the Noise transport), while this module owns
//! the protocol-facing half: [`RemoteUpstream`] makes a tunneled server look
//! like any other [`Upstream`](crate::upstream::Upstream), and
//! [`ShareAuthorizer`] is the policy check A performs before executing a tool.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use h4rn_protocol::{ApprovalDecision, HostId, McpShareApproval, McpSharePolicy};
use serde_json::Value;
use tokio::sync::{broadcast, oneshot};

use crate::hub::HubEvent;
use crate::provider::ToolError;
use crate::registry::{RegistryStore, ServerRecord, ShareRecord};
use crate::types::{CallToolResult, ServerCapabilities};
use crate::upstream::{
    ConnectedUpstream, Upstream, UpstreamError, UpstreamLimits, discover, initialize_params,
};

/// The client half of one cross-machine MCP connection.
#[async_trait]
pub trait McpTunnel: Send + Sync {
    /// Opens a tunnel bound to `server` on the remote machine.
    async fn open(&self, server: &str) -> Result<Arc<dyn TunnelSession>, UpstreamError>;
}

/// One open JSON-RPC channel to a remote hub.
#[async_trait]
pub trait TunnelSession: Send + Sync {
    /// Sends one request and waits for its response.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, UpstreamError>;

    /// Sends one notification.
    async fn notify(&self, method: &str, params: Value) -> Result<(), UpstreamError>;

    /// Closes the channel. Idempotent.
    async fn shutdown(&self);
}

/// An upstream reached through a tunnel.
pub struct RemoteUpstream {
    session: Arc<dyn TunnelSession>,
    server_info: std::sync::OnceLock<Option<String>>,
    capabilities: std::sync::OnceLock<ServerCapabilities>,
    instructions: std::sync::OnceLock<Option<String>>,
}

impl RemoteUpstream {
    /// Opens a tunnel and performs the MCP handshake through it.
    pub async fn connect(
        name: &str,
        _machine: HostId,
        tunnel: Arc<dyn McpTunnel>,
        limits: &UpstreamLimits,
    ) -> Result<ConnectedUpstream, UpstreamError> {
        let session = tunnel.open(name).await?;
        let upstream = Arc::new(Self {
            session,
            server_info: std::sync::OnceLock::new(),
            capabilities: std::sync::OnceLock::new(),
            instructions: std::sync::OnceLock::new(),
        });
        let initialize = upstream
            .request("initialize", initialize_params(), limits.request_timeout)
            .await?;
        let capabilities =
            ServerCapabilities::from_json(initialize.get("capabilities").unwrap_or(&Value::Null));
        let _ = upstream.capabilities.set(capabilities);
        let _ = upstream.server_info.set(
            initialize
                .get("serverInfo")
                .and_then(|info| info.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        );
        let _ = upstream.instructions.set(
            initialize
                .get("instructions")
                .and_then(Value::as_str)
                .map(str::to_owned),
        );
        upstream
            .notify("notifications/initialized", Value::Null)
            .await?;
        discover(upstream, limits).await
    }
}

fn empty_capabilities() -> &'static ServerCapabilities {
    static EMPTY: std::sync::OnceLock<ServerCapabilities> = std::sync::OnceLock::new();
    EMPTY.get_or_init(ServerCapabilities::default)
}

#[async_trait]
impl Upstream for RemoteUpstream {
    fn server_info(&self) -> Option<&str> {
        self.server_info.get().and_then(|info| info.as_deref())
    }

    fn capabilities(&self) -> &ServerCapabilities {
        self.capabilities
            .get()
            .unwrap_or_else(|| empty_capabilities())
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.get().and_then(|text| text.as_deref())
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, UpstreamError> {
        self.session.request(method, params, timeout).await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), UpstreamError> {
        self.session.notify(method, params).await
    }

    async fn shutdown(&self) {
        self.session.shutdown().await;
    }
}

/// Why the owner refused a remote tool call.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ShareError {
    /// No grant exists for that machine.
    #[error("\"{server}\" is not shared with {machine}")]
    NotShared {
        /// Server name.
        server: String,
        /// Machine that asked.
        machine: HostId,
    },
    /// The device id is not admitted by the grant.
    #[error("this device is not allowed to use \"{server}\"")]
    DeviceNotAllowed {
        /// Server name.
        server: String,
    },
    /// The tool is outside the grant's allow-list.
    #[error("tool \"{tool}\" is not shared")]
    ToolNotAllowed {
        /// Tool name.
        tool: String,
    },
    /// The call needs an approval and the owner did not give one.
    #[error("tool \"{tool}\" needs approval on the owning machine")]
    ConfirmationDenied {
        /// Tool name.
        tool: String,
    },
    /// The server failed.
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
}

impl ShareError {
    /// Converts to a JSON-RPC tool error without revealing policy internals.
    pub fn into_tool_error(self) -> ToolError {
        match self {
            Self::Upstream(error) => error.into_tool_error(),
            other => ToolError::protocol(crate::jsonrpc::INVALID_REQUEST, other.to_string()),
        }
    }
}

/// An explicit "yes" from the owner of a machine.
///
/// Confirmation is asynchronous because the shipping gate raises a request on
/// the owner's UI, which may take seconds or never answer. Every implementation
/// fails closed: a timeout, a dropped waiter or a cancelled wait denies the
/// call.
#[async_trait]
pub trait ConfirmationGate: Send + Sync {
    /// Returns true when the call may proceed.
    async fn confirm(&self, request: &ShareCall) -> bool;

    /// Requests waiting for the owner's decision, oldest first.
    fn pending(&self) -> Vec<McpShareApproval> {
        Vec::new()
    }

    /// Answers one pending request. Returns whether the id was still pending.
    fn respond(&self, _approval_id: &str, _decision: ApprovalDecision) -> bool {
        false
    }

    /// Denies every waiting request, for shutdown or a revoked peer.
    fn cancel_all(&self) {}
}

/// How long an approval waits before it fails closed.
pub const DEFAULT_APPROVAL_TTL: Duration = Duration::from_secs(120);

/// The longest argument preview an approval card carries.
const ARGUMENTS_PREVIEW_LIMIT: usize = 1024;

struct PendingApproval {
    approval: McpShareApproval,
    decision: oneshot::Sender<ApprovalDecision>,
}

/// The production gate: raises a pending approval and waits for the owner.
///
/// Requests are published as [`HubEvent`]s so every connected UI sees them,
/// and the pending set can be re-read after a reconnect. Nothing is durable:
/// a daemon restart drops pending calls, which is the fail-closed outcome.
pub struct ApprovalGate {
    pending: std::sync::Mutex<HashMap<String, PendingApproval>>,
    events: broadcast::Sender<HubEvent>,
    ttl: Duration,
    next: AtomicU64,
}

impl ApprovalGate {
    /// Creates a gate with the default expiry.
    pub fn new(events: broadcast::Sender<HubEvent>) -> Self {
        Self::with_ttl(events, DEFAULT_APPROVAL_TTL)
    }

    /// Creates a gate whose requests expire after `ttl`.
    pub fn with_ttl(events: broadcast::Sender<HubEvent>, ttl: Duration) -> Self {
        Self {
            pending: std::sync::Mutex::new(HashMap::new()),
            events,
            ttl,
            next: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingApproval>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Test seam: inserts a request whose expiry is already in the past, so
    /// the late-answer guard can be exercised without a timing race.
    #[cfg(test)]
    fn insert_expired_for_test(&self, approval_id: &str) {
        let now = Utc::now();
        let (decision, _wait) = oneshot::channel();
        self.lock().insert(
            approval_id.to_owned(),
            PendingApproval {
                approval: McpShareApproval {
                    approval_id: approval_id.to_owned(),
                    machine: HostId::new("host_b"),
                    server: "github".into(),
                    tool: "write".into(),
                    arguments_preview: String::new(),
                    requested_at: now - chrono::Duration::minutes(5),
                    expires_at: now - chrono::Duration::minutes(1),
                },
                decision,
            },
        );
    }

    /// Removes one request and publishes its outcome. Returns false when it
    /// was already answered or had expired.
    fn finish(&self, approval_id: &str, decision: ApprovalDecision) -> bool {
        let pending = self.lock().remove(approval_id);
        let Some(pending) = pending else {
            return false;
        };
        let _ = self.events.send(HubEvent::ShareApprovalResolved {
            approval_id: approval_id.to_owned(),
            decision,
        });
        let _ = pending.decision.send(decision);
        true
    }
}

impl Default for ApprovalGate {
    fn default() -> Self {
        Self::new(broadcast::channel(64).0)
    }
}

#[async_trait]
impl ConfirmationGate for ApprovalGate {
    async fn confirm(&self, request: &ShareCall) -> bool {
        let requested_at = Utc::now();
        let expires_at = requested_at
            + chrono::Duration::from_std(self.ttl).unwrap_or_else(|_| chrono::Duration::zero());
        let approval_id = format!(
            "mcp-{}-{}",
            requested_at.timestamp_millis(),
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let approval = McpShareApproval {
            approval_id: approval_id.clone(),
            machine: request.machine.clone(),
            server: request.server.clone(),
            tool: request.tool.clone(),
            arguments_preview: preview_arguments(request.arguments.as_ref()),
            requested_at,
            expires_at,
        };
        let (decision, wait) = oneshot::channel();
        self.lock().insert(
            approval_id.clone(),
            PendingApproval {
                approval: approval.clone(),
                decision,
            },
        );
        let _ = self
            .events
            .send(HubEvent::ShareApprovalRequested { approval });
        match tokio::time::timeout(self.ttl, wait).await {
            Ok(Ok(ApprovalDecision::Approve)) => true,
            // `respond` already published the deny/cancel outcome.
            Ok(Ok(_)) => false,
            // The waiter was dropped; nobody can answer it now.
            Ok(Err(_)) => {
                self.finish(&approval_id, ApprovalDecision::Cancel);
                false
            }
            // Fail closed when the owner never answers.
            Err(_) => {
                self.finish(&approval_id, ApprovalDecision::Deny);
                false
            }
        }
    }

    fn pending(&self) -> Vec<McpShareApproval> {
        let mut approvals: Vec<McpShareApproval> = self
            .lock()
            .values()
            .map(|pending| pending.approval.clone())
            .collect();
        approvals.sort_by(|a, b| {
            a.requested_at
                .cmp(&b.requested_at)
                .then_with(|| a.approval_id.cmp(&b.approval_id))
        });
        approvals
    }

    fn respond(&self, approval_id: &str, decision: ApprovalDecision) -> bool {
        // An answer that arrives after the card's expiry is too late: the call
        // must not run even if the timeout task has not woken yet. Deny it and
        // report that the id is no longer pending.
        let expired = self
            .lock()
            .get(approval_id)
            .is_some_and(|pending| pending.approval.expires_at <= Utc::now());
        if expired {
            self.finish(approval_id, ApprovalDecision::Deny);
            return false;
        }
        self.finish(approval_id, decision)
    }

    fn cancel_all(&self) {
        let drained: Vec<(String, PendingApproval)> = self.lock().drain().collect();
        for (approval_id, pending) in drained {
            let _ = self.events.send(HubEvent::ShareApprovalResolved {
                approval_id,
                decision: ApprovalDecision::Deny,
            });
            let _ = pending.decision.send(ApprovalDecision::Deny);
        }
    }
}

/// A bounded, single-line rendering of tool arguments for the approval card.
fn preview_arguments(arguments: Option<&Value>) -> String {
    let Some(arguments) = arguments else {
        return String::new();
    };
    let rendered = arguments.to_string();
    if rendered.chars().count() <= ARGUMENTS_PREVIEW_LIMIT {
        return rendered;
    }
    let mut preview: String = rendered.chars().take(ARGUMENTS_PREVIEW_LIMIT).collect();
    preview.push('…');
    preview
}

/// What the owner is asked to approve.
#[derive(Clone, Debug, PartialEq)]
pub struct ShareCall {
    /// Verified device id of the calling daemon, derived by the owner from the
    /// authenticated Noise key, never from a caller-supplied claim.
    pub device_id: Option<String>,
    /// Whether the transport proved the caller holds the registered daemon
    /// key of `machine`. A call without that proof is never admitted, so
    /// `allowed_devices: None` means "the target machine's daemon only".
    pub peer_verified: bool,
    /// Machine that asked.
    pub machine: HostId,
    /// Server name.
    pub server: String,
    /// Unqualified tool name.
    pub tool: String,
    /// Tool arguments, for the approval UI. Never logged wholesale.
    pub arguments: Option<Value>,
}

/// A gate that denies every confirmation (the safe default).
#[derive(Default)]
pub struct DenyConfirmation;

#[async_trait]
impl ConfirmationGate for DenyConfirmation {
    async fn confirm(&self, _request: &ShareCall) -> bool {
        false
    }
}

/// A gate that allows every confirmation (only for tests and explicit
/// "always allow" grants).
#[derive(Default)]
pub struct AllowConfirmation;

#[async_trait]
impl ConfirmationGate for AllowConfirmation {
    async fn confirm(&self, _request: &ShareCall) -> bool {
        true
    }
}

/// Owner-side audit of remote calls.
pub trait ShareAudit: Send + Sync {
    /// Records one decision or call.
    fn remote_call(&self, call: &ShareCall, allowed: bool, error: Option<&str>);
}

/// An audit sink that discards records (tests and signed-out daemons).
#[derive(Default)]
pub struct NoAudit;

impl ShareAudit for NoAudit {
    fn remote_call(&self, _call: &ShareCall, _allowed: bool, _error: Option<&str>) {}
}

/// Enforces a `server -> machine` grant.
pub struct ShareAuthorizer {
    registry: Arc<dyn RegistryStore>,
    gate: Arc<dyn ConfirmationGate>,
    audit: Arc<dyn ShareAudit>,
}

impl ShareAuthorizer {
    /// Creates the authorizer.
    pub fn new(
        registry: Arc<dyn RegistryStore>,
        gate: Arc<dyn ConfirmationGate>,
        audit: Arc<dyn ShareAudit>,
    ) -> Self {
        Self {
            registry,
            gate,
            audit,
        }
    }

    /// Returns the grant for `record` -> `machine`, if any.
    pub async fn grant(
        &self,
        record: &ServerRecord,
        machine: &HostId,
    ) -> Result<Option<ShareRecord>, ShareError> {
        let shares =
            self.registry.list_shares().await.map_err(|error| {
                ShareError::Upstream(UpstreamError::Unavailable(error.to_string()))
            })?;
        Ok(shares
            .into_iter()
            .find(|share| share.server_id == record.id && &share.machine == machine))
    }

    /// Checks one call without executing it.
    pub async fn authorize(
        &self,
        record: &ServerRecord,
        call: &ShareCall,
    ) -> Result<McpSharePolicy, ShareError> {
        let grant =
            self.grant(record, &call.machine)
                .await?
                .ok_or_else(|| ShareError::NotShared {
                    server: record.name.clone(),
                    machine: call.machine.clone(),
                })?;
        let policy = grant.policy;
        if !call.peer_verified {
            self.audit
                .remote_call(call, false, Some("peer is not the target machine"));
            return Err(ShareError::DeviceNotAllowed {
                server: record.name.clone(),
            });
        }
        if let Some(allowed) = &policy.allowed_devices {
            let admitted = call
                .device_id
                .as_ref()
                .is_some_and(|device| allowed.iter().any(|candidate| candidate == device));
            if !admitted {
                self.audit
                    .remote_call(call, false, Some("device not allowed"));
                return Err(ShareError::DeviceNotAllowed {
                    server: record.name.clone(),
                });
            }
        }
        if !policy.permits(&call.tool) {
            self.audit
                .remote_call(call, false, Some("tool not allowed"));
            return Err(ShareError::ToolNotAllowed {
                tool: call.tool.clone(),
            });
        }
        if policy.requires_confirmation(&call.tool) && !self.gate.confirm(call).await {
            self.audit
                .remote_call(call, false, Some("confirmation denied"));
            return Err(ShareError::ConfirmationDenied {
                tool: call.tool.clone(),
            });
        }
        Ok(policy)
    }

    /// Records a call the owner actually executed.
    pub fn record(&self, call: &ShareCall, result: &Result<CallToolResult, ShareError>) {
        match result {
            Ok(result) => self.audit.remote_call(
                call,
                true,
                result.is_error.then_some("tool reported an error"),
            ),
            Err(error) => self
                .audit
                .remote_call(call, false, Some(&error.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{MemoryRegistry, StoredTransport};
    use chrono::Utc;
    use h4rn_protocol::McpEnabledMatrix;

    fn record() -> ServerRecord {
        ServerRecord::new(
            "github",
            StoredTransport::Http {
                url: "https://example.test/mcp".into(),
                headers: Vec::new(),
            },
            Vec::new(),
            McpEnabledMatrix::default(),
            None,
        )
    }

    fn call(tool: &str) -> ShareCall {
        ShareCall {
            device_id: Some("dev_1".into()),
            peer_verified: true,
            machine: HostId::new("host_b"),
            server: "github".into(),
            tool: tool.into(),
            arguments: None,
        }
    }

    async fn authorizer(
        policy: McpSharePolicy,
        gate: Arc<dyn ConfirmationGate>,
    ) -> (ShareAuthorizer, ServerRecord) {
        let registry = Arc::new(MemoryRegistry::new());
        let record = record();
        registry.put(&record).await.unwrap();
        registry
            .put_share(&ShareRecord {
                server_id: record.id.clone(),
                machine: HostId::new("host_b"),
                policy,
                created_at: Utc::now(),
            })
            .await
            .unwrap();
        (
            ShareAuthorizer::new(registry, gate, Arc::new(NoAudit)),
            record,
        )
    }

    #[tokio::test]
    async fn confirmation_can_be_answered_from_the_pending_set() {
        let (events, mut received) = broadcast::channel(16);
        let gate = Arc::new(ApprovalGate::new(events));
        let request = call("write");
        let waiter = {
            let gate = Arc::clone(&gate);
            let request = request.clone();
            tokio::spawn(async move { gate.confirm(&request).await })
        };
        // The card the owner sees names the tool and previews its arguments.
        let approval = loop {
            match received.recv().await.expect("approval event") {
                HubEvent::ShareApprovalRequested { approval } => break approval,
                _ => continue,
            }
        };
        assert_eq!(approval.tool, "write");
        assert_eq!(approval.server, "github");
        assert_eq!(approval.machine, HostId::new("host_b"));
        assert_eq!(gate.pending(), vec![approval.clone()]);
        assert!(gate.respond(&approval.approval_id, ApprovalDecision::Approve));
        assert!(waiter.await.expect("waiter"));
        assert!(gate.pending().is_empty());
        assert!(matches!(
            received.recv().await.expect("resolved event"),
            HubEvent::ShareApprovalResolved {
                decision: ApprovalDecision::Approve,
                ..
            }
        ));
        // Answering twice is not an error the owner can trigger a second run
        // with: the second response finds nothing pending.
        assert!(!gate.respond(&approval.approval_id, ApprovalDecision::Approve));
    }

    #[tokio::test]
    async fn an_unanswered_approval_expires_closed() {
        let (events, _received) = broadcast::channel(16);
        let gate = Arc::new(ApprovalGate::with_ttl(events, Duration::from_millis(20)));
        let request = call("write");
        assert!(!gate.confirm(&request).await, "expiry must deny");
        assert!(
            gate.pending().is_empty(),
            "an expired approval is not pending"
        );
    }

    #[tokio::test]
    async fn a_late_approval_after_expiry_is_refused() {
        let (events, mut received) = broadcast::channel(16);
        let gate = Arc::new(ApprovalGate::new(events));
        gate.insert_expired_for_test("mcp-late");
        assert!(
            !gate.respond("mcp-late", ApprovalDecision::Approve),
            "an approval past its expiry must never run the call"
        );
        assert!(gate.pending().is_empty());
        assert!(matches!(
            received.recv().await.expect("resolved event"),
            HubEvent::ShareApprovalResolved {
                decision: ApprovalDecision::Deny,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_denied_or_cancelled_approval_never_runs_the_tool() {
        for decision in [ApprovalDecision::Deny, ApprovalDecision::Cancel] {
            let (events, mut received) = broadcast::channel(16);
            let gate = Arc::new(ApprovalGate::new(events));
            let request = call("write");
            let waiter = {
                let gate = Arc::clone(&gate);
                let request = request.clone();
                tokio::spawn(async move { gate.confirm(&request).await })
            };
            let approval = loop {
                match received.recv().await.expect("approval event") {
                    HubEvent::ShareApprovalRequested { approval } => break approval,
                    _ => continue,
                }
            };
            assert!(gate.respond(&approval.approval_id, decision));
            assert!(!waiter.await.expect("waiter"));
        }
    }

    #[tokio::test]
    async fn cancellation_denies_every_waiter_on_shutdown() {
        let (events, _received) = broadcast::channel(16);
        let gate = Arc::new(ApprovalGate::new(events));
        let request = call("write");
        let waiter = {
            let gate = Arc::clone(&gate);
            let request = request.clone();
            tokio::spawn(async move { gate.confirm(&request).await })
        };
        while gate.pending().is_empty() {
            tokio::task::yield_now().await;
        }
        gate.cancel_all();
        assert!(!waiter.await.expect("waiter"));
    }

    #[tokio::test]
    async fn unshared_machines_are_refused() {
        let (authorizer, record) =
            authorizer(McpSharePolicy::default(), Arc::new(DenyConfirmation)).await;
        let mut request = call("read");
        request.machine = HostId::new("host_c");
        assert!(matches!(
            authorizer.authorize(&record, &request).await,
            Err(ShareError::NotShared { .. })
        ));
    }

    #[tokio::test]
    async fn allow_lists_and_confirmation_are_enforced() {
        let policy = McpSharePolicy {
            allowed_tools: Some(vec!["read".into(), "write".into()]),
            confirm_tools: vec!["write".into()],
            ..McpSharePolicy::default()
        };
        let (deny, record) = authorizer(policy.clone(), Arc::new(DenyConfirmation)).await;
        assert!(matches!(
            deny.authorize(&record, &call("delete")).await,
            Err(ShareError::ToolNotAllowed { .. })
        ));
        assert!(matches!(
            deny.authorize(&record, &call("write")).await,
            Err(ShareError::ConfirmationDenied { .. })
        ));
        let (allow, record) = authorizer(policy, Arc::new(AllowConfirmation)).await;
        assert!(allow.authorize(&record, &call("write")).await.is_ok());
    }

    #[tokio::test]
    async fn a_policy_without_a_device_list_admits_only_the_verified_machine_daemon() {
        // `allowed_devices: None` used to admit any caller naming the machine.
        let (authorizer, record) =
            authorizer(McpSharePolicy::default(), Arc::new(DenyConfirmation)).await;
        assert!(authorizer.authorize(&record, &call("read")).await.is_ok());
        let mut spoofed = call("read");
        spoofed.peer_verified = false;
        assert!(matches!(
            authorizer.authorize(&record, &spoofed).await,
            Err(ShareError::DeviceNotAllowed { .. })
        ));
    }

    #[tokio::test]
    async fn an_explicit_device_list_compares_only_the_verified_id() {
        let policy = McpSharePolicy {
            allowed_devices: Some(vec!["dev_1".into()]),
            ..McpSharePolicy::default()
        };
        let (authorizer, record) = authorizer(policy, Arc::new(DenyConfirmation)).await;
        assert!(authorizer.authorize(&record, &call("read")).await.is_ok());
        let mut other = call("read");
        other.device_id = Some("dev_2".into());
        assert!(matches!(
            authorizer.authorize(&record, &other).await,
            Err(ShareError::DeviceNotAllowed { .. })
        ));
        let mut anonymous = call("read");
        anonymous.device_id = None;
        assert!(matches!(
            authorizer.authorize(&record, &anonymous).await,
            Err(ShareError::DeviceNotAllowed { .. })
        ));
    }
}
