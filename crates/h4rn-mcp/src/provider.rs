//! The built-in tool extension point.
//!
//! h4rn's own tools — `spawn_subagent`, `task_status`, `task_cancel`,
//! preview/render and forge operations — are not upstream MCP servers. They run
//! in-process and share the hub's policy, progress and cancellation machinery.
//! [`ToolProvider`] is their seam: another module or crate implements it and
//! registers it with [`ProviderRegistry`], and the hub treats it exactly like a
//! server namespace named by [`ToolProvider::namespace`].
//!
//! The trait is deliberately small. A provider lists tools for a scope and
//! calls one; resources and prompts have defaults so a tools-only provider does
//! not have to stub four methods. Progress and cancellation arrive through
//! [`ToolCallContext`], so a long-running provider (a subagent wait, say) gets
//! the same behaviour as a proxied upstream.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use h4rn_protocol::{TurnId, WorkGrant};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::session::McpScope;
use crate::types::{
    CallToolResult, PromptDefinition, PromptMessage, ResourceContents, ResourceDefinition,
    ToolDefinition,
};

/// The turn a harness tool call was admitted under.
///
/// A tool can outlive the turn that called it (a PR watch, a delegated child),
/// so the daemon captures the calling turn and its outside grant when the MCP
/// request arrives, not when the tool's async setup finishes. A later turn's
/// grant can then never be inherited by work an earlier turn asked for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolAdmission {
    /// The turn that was running when the request was admitted.
    pub turn_id: Option<TurnId>,
    /// The outside grant that turn ran under, when one did.
    pub grant: Option<WorkGrant>,
}

/// Supplies the admission snapshot for one MCP request.
///
/// The daemon implements this over its thread manager. The hub itself knows no
/// threads, so a scope with no thread simply gets the empty admission.
#[async_trait]
pub trait ToolAdmissionSource: Send + Sync {
    /// The turn and grant a request from this scope is admitted under.
    async fn admission(&self, scope: &McpScope) -> ToolAdmission;
}

/// One progress report from a long-running tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct ProgressUpdate {
    /// 0-based index of this update.
    pub progress: f64,
    /// Total when known.
    pub total: Option<f64>,
    /// Human-readable status.
    pub message: Option<String>,
}

/// Everything a provider needs while executing one call.
#[derive(Clone)]
pub struct ToolCallContext {
    /// Caller identity, for authorization and logging.
    pub scope: McpScope,
    /// The turn this call was admitted under, captured before any await.
    pub admission: ToolAdmission,
    /// Server namespace the call arrived through.
    pub server: String,
    /// Progress sink, present when the harness supplied a progress token.
    pub progress: Option<mpsc::Sender<ProgressUpdate>>,
    /// Cancellation token, cancelled by `notifications/cancelled`.
    pub cancel: CancellationToken,
    /// Absolute deadline for the call.
    pub deadline: Option<Instant>,
}

impl ToolCallContext {
    /// Builds a context with no progress sink and a deadline `timeout` ahead.
    pub fn new(scope: McpScope, server: impl Into<String>, timeout: Duration) -> Self {
        Self {
            scope,
            admission: ToolAdmission::default(),
            server: server.into(),
            progress: None,
            cancel: CancellationToken::new(),
            deadline: Some(Instant::now() + timeout),
        }
    }

    /// Attaches the turn and grant the request was admitted under.
    pub fn with_admission(mut self, admission: ToolAdmission) -> Self {
        self.admission = admission;
        self
    }

    /// Attaches a progress sink.
    pub fn with_progress(mut self, progress: mpsc::Sender<ProgressUpdate>) -> Self {
        self.progress = Some(progress);
        self
    }

    /// Attaches a cancellation token.
    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Reports progress when a sink is attached.
    pub async fn report_progress(
        &self,
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    ) {
        if let Some(sink) = &self.progress {
            let _ = sink
                .send(ProgressUpdate {
                    progress,
                    total,
                    message,
                })
                .await;
        }
    }

    /// Fails when the call is cancelled.
    pub fn check_cancelled(&self) -> Result<(), ToolError> {
        if self.cancel.is_cancelled() {
            return Err(ToolError::cancelled());
        }
        Ok(())
    }
}

/// A tool failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ToolError {
    /// JSON-RPC code when this is a protocol failure.
    pub code: i64,
    /// Redacted, user-facing message.
    pub message: String,
    /// Whether the tool ran and reported a failure (MCP `isError`) rather than
    /// the protocol failing. Tool errors are returned as a successful
    /// JSON-RPC response with an error content block.
    pub is_tool_error: bool,
}

impl ToolError {
    /// A protocol-level failure.
    pub fn protocol(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            is_tool_error: false,
        }
    }

    /// An invalid-argument failure.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::protocol(crate::jsonrpc::INVALID_PARAMS, message)
    }

    /// A tool-level failure.
    pub fn tool(message: impl Into<String>) -> Self {
        Self {
            code: 0,
            message: message.into(),
            is_tool_error: true,
        }
    }

    /// The standard cancellation failure.
    pub fn cancelled() -> Self {
        Self::protocol(
            crate::jsonrpc::REQUEST_CANCELLED,
            "the request was cancelled",
        )
    }
}

/// One in-process tool set.
#[async_trait]
pub trait ToolProvider: Send + Sync {
    /// Namespace the provider's tools are exposed under (`h4rn` for built-ins).
    fn namespace(&self) -> &str;

    /// Tool definitions visible to `scope`.
    async fn list_tools(&self, scope: &McpScope) -> Vec<ToolDefinition>;

    /// Executes one tool. `tool` is the unqualified name.
    async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        context: &ToolCallContext,
    ) -> Result<CallToolResult, ToolError>;

    /// Resources visible to `scope`.
    async fn list_resources(&self, _scope: &McpScope) -> Vec<ResourceDefinition> {
        Vec::new()
    }

    /// Reads one resource.
    async fn read_resource(
        &self,
        _uri: &str,
        _scope: &McpScope,
    ) -> Result<Vec<ResourceContents>, ToolError> {
        Err(ToolError::protocol(
            crate::jsonrpc::INVALID_PARAMS,
            "this provider exposes no resources",
        ))
    }

    /// Prompts visible to `scope`.
    async fn list_prompts(&self, _scope: &McpScope) -> Vec<PromptDefinition> {
        Vec::new()
    }

    /// Renders one prompt.
    async fn get_prompt(
        &self,
        _name: &str,
        _arguments: &BTreeMap<String, String>,
        _scope: &McpScope,
    ) -> Result<Vec<PromptMessage>, ToolError> {
        Err(ToolError::protocol(
            crate::jsonrpc::INVALID_PARAMS,
            "this provider exposes no prompts",
        ))
    }
}

/// The set of registered providers, keyed by namespace.
///
/// Several providers may share one namespace: Phase 4 registers both the
/// diagnostic `h4rn` tool set and the subagent tool set under `h4rn`, so the
/// registry holds a list per namespace instead of replacing on collision.
#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Vec<Arc<dyn ToolProvider>>>,
}

impl ProviderRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a provider at the end of its namespace's list.
    pub fn register(&mut self, provider: Arc<dyn ToolProvider>) {
        self.providers
            .entry(provider.namespace().to_owned())
            .or_default()
            .push(provider);
    }

    /// Looks up the first provider registered under a namespace.
    pub fn get(&self, namespace: &str) -> Option<Arc<dyn ToolProvider>> {
        self.providers
            .get(namespace)
            .and_then(|providers| providers.first())
            .cloned()
    }

    /// Every provider registered under one namespace, in registration order.
    pub fn for_namespace(&self, namespace: &str) -> Vec<Arc<dyn ToolProvider>> {
        self.providers.get(namespace).cloned().unwrap_or_default()
    }

    /// Every namespace, in deterministic order.
    pub fn namespaces(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }

    /// Every provider, for every namespace, in deterministic order.
    pub fn providers(&self) -> impl Iterator<Item = &Arc<dyn ToolProvider>> {
        self.providers.values().flatten()
    }

    /// Whether a namespace has at least one provider.
    pub fn contains(&self, namespace: &str) -> bool {
        self.providers.contains_key(namespace)
    }

    /// Number of distinct namespaces.
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    /// Whether the registry has no providers.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

/// The hub's own test tool set.
///
/// `echo` exists so the harness-injection tests can ask a real agent to call
/// `h4rn__echo` and assert the hub observed it; `server_info` is useful in a
/// transcript to show which machine and harness a session is on. It is not a
/// placeholder for the orchestration tools: those register their own provider.
pub struct EchoProvider {
    version: String,
}

impl EchoProvider {
    /// Creates the provider.
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
        }
    }
}

impl Default for EchoProvider {
    fn default() -> Self {
        Self::new(env!("CARGO_PKG_VERSION"))
    }
}

#[async_trait]
impl ToolProvider for EchoProvider {
    fn namespace(&self) -> &str {
        super::HUB_SERVER_NAME
    }

    async fn list_tools(&self, _scope: &McpScope) -> Vec<ToolDefinition> {
        let mut echo = ToolDefinition::new(
            "echo",
            Some("Echo a message back through the h4rn MCP hub.".into()),
            json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "Text to echo."
                    },
                    "uppercase": {
                        "type": "boolean",
                        "description": "Uppercase the echoed text."
                    }
                },
                "required": ["message"]
            }),
        );
        echo.annotations = Some(crate::types::ToolAnnotations {
            read_only_hint: Some(true),
            idempotent_hint: Some(true),
            ..crate::types::ToolAnnotations::default()
        });
        let info = ToolDefinition::new(
            "server_info",
            Some("Report the h4rn hub version and this session's scope.".into()),
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        );
        vec![echo, info]
    }

    async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        context: &ToolCallContext,
    ) -> Result<CallToolResult, ToolError> {
        context.check_cancelled()?;
        match tool {
            "echo" => {
                let message = arguments
                    .get("message")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ToolError::invalid("echo requires a string \"message\""))?;
                let uppercase = arguments
                    .get("uppercase")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let output = if uppercase {
                    message.to_uppercase()
                } else {
                    message.to_owned()
                };
                Ok(CallToolResult::text(output))
            }
            "server_info" => Ok(CallToolResult {
                content: vec![crate::types::Content::text(
                    json!({
                        "hub": super::HUB_SERVER_NAME,
                        "version": self.version,
                        "harness": context.scope.harness,
                        "machine": context.scope.machine,
                        "thread_id": context.scope.thread_id,
                        "caller": context.scope.caller,
                    })
                    .to_string(),
                )],
                structured_content: Some(json!({
                    "hub": super::HUB_SERVER_NAME,
                    "version": self.version,
                })),
                is_error: false,
            }),
            other => Err(ToolError::protocol(
                crate::jsonrpc::METHOD_NOT_FOUND,
                format!("unknown h4rn tool \"{other}\""),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use h4rn_protocol::{HarnessKind, HostId};

    fn scope() -> McpScope {
        McpScope::harness(HarnessKind::ClaudeCode, HostId::new("host_1"), None)
    }

    #[tokio::test]
    async fn echo_round_trips_and_can_uppercase() {
        let provider = EchoProvider::new("test");
        let context = ToolCallContext::new(scope(), "h4rn", Duration::from_secs(1));
        let result = provider
            .call_tool("echo", json!({"message": "hi"}), &context)
            .await
            .unwrap();
        assert_eq!(result.text_output(), "hi");
        let result = provider
            .call_tool(
                "echo",
                json!({"message": "hi", "uppercase": true}),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(result.text_output(), "HI");
    }

    #[tokio::test]
    async fn missing_arguments_and_cancellation_are_protocol_errors() {
        let provider = EchoProvider::new("test");
        let context = ToolCallContext::new(scope(), "h4rn", Duration::from_secs(1));
        let error = provider
            .call_tool("echo", json!({}), &context)
            .await
            .unwrap_err();
        assert_eq!(error.code, crate::jsonrpc::INVALID_PARAMS);
        context.cancel.cancel();
        let error = provider
            .call_tool("echo", json!({"message": "hi"}), &context)
            .await
            .unwrap_err();
        assert_eq!(error.code, crate::jsonrpc::REQUEST_CANCELLED);
    }

    #[test]
    fn registry_keeps_providers_that_share_a_namespace() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(EchoProvider::new("1")));
        registry.register(Arc::new(EchoProvider::new("2")));
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.for_namespace("h4rn").len(), 2);
        assert!(registry.contains("h4rn"));
    }
}
