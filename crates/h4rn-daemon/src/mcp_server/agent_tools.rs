//! h4rn's own tools for agents it runs.
//!
//! A harness session reaches these through the hub's per-session bearer token,
//! whose scope names the thread. Every tool therefore acts on the calling
//! thread and accepts no thread id: a token minted for thread A cannot name
//! thread B even if a model tries. The tools read the daemon's real state
//! (`PrLinkService`, the thread journal and the forge), never a cache of their
//! own.

use std::sync::Arc;

use async_trait::async_trait;
use h4rn_mcp::provider::{ToolCallContext, ToolError, ToolProvider};
use h4rn_mcp::session::McpScope;
use h4rn_mcp::types::{CallToolResult, Content, ToolAnnotations, ToolDefinition};
use h4rn_protocol::{
    ForgePrFilter, ForgeRepoRef, HarnessKind, HostId, ThreadId, ThreadPullRequest,
};
use serde_json::{Value, json};

use super::settings::AgentToolSettingsStore;
use crate::forge::{ForgeService, ForgeServiceError};
use crate::pr_links::{PrLinkError, PrLinkService};
use crate::threads::ThreadManager;

/// Tools every harness session can use against its own thread.
pub const TOOL_NAMES: &[&str] = &[
    "link_pull_request",
    "unlink_pull_request",
    "list_thread_pull_requests",
    "watch_pull_request",
    "unwatch_pull_request",
    "thread_status",
    "list_forge_pull_requests",
];

/// The built-in `h4rn` provider.
pub struct AgentToolsProvider {
    manager: Arc<ThreadManager>,
    pr_links: Arc<PrLinkService>,
    forge: Arc<ForgeService>,
    settings: Arc<AgentToolSettingsStore>,
    host: HostId,
}

impl AgentToolsProvider {
    /// Builds the provider over the daemon's live services.
    pub fn new(
        manager: Arc<ThreadManager>,
        pr_links: Arc<PrLinkService>,
        forge: Arc<ForgeService>,
        settings: Arc<AgentToolSettingsStore>,
        host: HostId,
    ) -> Self {
        Self {
            manager,
            pr_links,
            forge,
            settings,
            host,
        }
    }

    fn calling_thread(scope: &McpScope) -> Result<ThreadId, ToolError> {
        scope.thread_id.clone().ok_or_else(|| {
            ToolError::tool(
                "this tool acts on the thread that called it, and this session has no thread",
            )
        })
    }

    fn number(arguments: &Value) -> Result<u32, ToolError> {
        let number = arguments
            .get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| ToolError::invalid("`number` is required"))?;
        u32::try_from(number).map_err(|_| ToolError::invalid("`number` is out of range"))
    }

    /// Resolves the repository a tool call names, or the thread's only forge
    /// remote when it names none.
    async fn resolve_repo(
        &self,
        thread_id: &ThreadId,
        repository: Option<&str>,
    ) -> Result<ForgeRepoRef, ToolError> {
        let meta = self.manager.thread_meta(thread_id).await.ok_or_else(|| {
            ToolError::tool(format!("thread {thread_id} was not found on this daemon"))
        })?;
        let remotes = self
            .forge
            .detect_repo(&meta.workspace_path)
            .map_err(|error: ForgeServiceError| ToolError::tool(error.to_string()))?;
        if remotes.is_empty() {
            return Err(ToolError::tool(format!(
                "the thread's workspace {} has no GitHub, GitLab, Azure DevOps or Forgejo remote",
                meta.workspace_path
            )));
        }
        let available = remotes
            .iter()
            .map(|remote| format!("{} ({})", remote.repo.full_name(), remote.remote))
            .collect::<Vec<_>>()
            .join(", ");
        if let Some(wanted) = repository.map(str::trim).filter(|value| !value.is_empty()) {
            let wanted = wanted.to_ascii_lowercase();
            return remotes
                .into_iter()
                .find(|remote| {
                    remote.remote.eq_ignore_ascii_case(&wanted)
                        || remote.repo.full_name().eq_ignore_ascii_case(&wanted)
                })
                .map(|remote| remote.repo)
                .ok_or_else(|| {
                    ToolError::tool(format!(
                        "no forge remote matches `{wanted}`; the workspace has {available}"
                    ))
                });
        }
        if remotes.len() > 1 {
            let available = remotes
                .iter()
                .map(|remote| format!("{} ({})", remote.repo.full_name(), remote.remote))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ToolError::invalid(format!(
                "this workspace has more than one forge remote ({available}); pass `repository`"
            )));
        }
        Ok(remotes
            .into_iter()
            .next()
            .map(|remote| remote.repo)
            .expect("one remote"))
    }

    fn pr_error(error: PrLinkError) -> ToolError {
        let proto = error.into_proto();
        ToolError::tool(proto.message)
    }
}

fn read_tool(name: &str, description: &str, schema: Value) -> ToolDefinition {
    let mut tool = ToolDefinition::new(name, Some(description.into()), schema);
    tool.annotations = Some(ToolAnnotations {
        read_only_hint: Some(true),
        idempotent_hint: Some(true),
        ..ToolAnnotations::default()
    });
    tool
}

fn write_tool(name: &str, description: &str, schema: Value) -> ToolDefinition {
    ToolDefinition::new(name, Some(description.into()), schema)
}

fn structured(text: impl Into<String>, value: Value) -> CallToolResult {
    CallToolResult {
        content: vec![Content::text(text.into())],
        structured_content: Some(value),
        is_error: false,
    }
}

#[async_trait]
impl ToolProvider for AgentToolsProvider {
    fn namespace(&self) -> &str {
        h4rn_mcp::HUB_SERVER_NAME
    }

    async fn list_tools(&self, scope: &McpScope) -> Vec<ToolDefinition> {
        // The switch is per harness on the machine that runs the session. A
        // session without a thread cannot use any of these tools, so it sees
        // none rather than a list that fails on every call.
        if scope.thread_id.is_none()
            || scope.machine != self.host
            || !self.settings.is_enabled(scope.harness)
        {
            return Vec::new();
        }
        vec![
            write_tool(
                "link_pull_request",
                "Link a pull request to this thread. The daemon reads it once from the forge so the result names its current state.",
                json!({
                    "type": "object",
                    "properties": {
                        "number": {"type": "integer", "minimum": 1, "description": "Pull request number."},
                        "repository": {"type": "string", "description": "owner/name or a git remote name. Omit when the workspace has one forge remote."}
                    },
                    "required": ["number"],
                    "additionalProperties": false
                }),
            ),
            write_tool(
                "unlink_pull_request",
                "Remove a linked pull request from this thread and stop watching it.",
                json!({
                    "type": "object",
                    "properties": {
                        "number": {"type": "integer", "minimum": 1},
                        "repository": {"type": "string"}
                    },
                    "required": ["number"],
                    "additionalProperties": false
                }),
            ),
            read_tool(
                "list_thread_pull_requests",
                "List this thread's linked pull requests with their latest forge state.",
                json!({"type": "object", "properties": {}, "additionalProperties": false}),
            ),
            write_tool(
                "watch_pull_request",
                "Watch a linked pull request: when its checks, reviews or comments change, the daemon appends a message that starts a turn.",
                json!({
                    "type": "object",
                    "properties": {
                        "number": {"type": "integer", "minimum": 1},
                        "repository": {"type": "string"}
                    },
                    "required": ["number"],
                    "additionalProperties": false
                }),
            ),
            write_tool(
                "unwatch_pull_request",
                "Stop watching a linked pull request. The link stays.",
                json!({
                    "type": "object",
                    "properties": {
                        "number": {"type": "integer", "minimum": 1},
                        "repository": {"type": "string"}
                    },
                    "required": ["number"],
                    "additionalProperties": false
                }),
            ),
            read_tool(
                "thread_status",
                "This thread's state and queue: what the agent is doing and how many messages wait behind it.",
                json!({"type": "object", "properties": {}, "additionalProperties": false}),
            ),
            read_tool(
                "list_forge_pull_requests",
                "List pull requests in this thread's repository, read-only.",
                json!({
                    "type": "object",
                    "properties": {
                        "filter": {
                            "type": "string",
                            "enum": ["open", "mine", "review_requested"],
                            "description": "Which pull requests to list. Defaults to open."
                        },
                        "page": {"type": "integer", "minimum": 1, "description": "One-based page. Defaults to 1."},
                        "repository": {"type": "string", "description": "owner/name or a git remote name."}
                    },
                    "additionalProperties": false
                }),
            ),
        ]
    }

    async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        context: &ToolCallContext,
    ) -> Result<CallToolResult, ToolError> {
        context.check_cancelled()?;
        let thread_id = Self::calling_thread(&context.scope)?;
        if !self.settings.is_enabled(context.scope.harness) {
            return Err(ToolError::tool(format!(
                "h4rn tools are turned off for {} sessions on this machine",
                harness_label(context.scope.harness)
            )));
        }
        match tool {
            "link_pull_request" => {
                let number = Self::number(&arguments)?;
                let repository = arguments.get("repository").and_then(Value::as_str);
                let repo = self.resolve_repo(&thread_id, repository).await?;
                let linked = self
                    .pr_links
                    .link(&thread_id, None, &repo, number)
                    .await
                    .map_err(Self::pr_error)?;
                let text = pull_text("Linked", &linked);
                Ok(structured(text, json!({"pull_request": linked})))
            }
            "unlink_pull_request" => {
                let number = Self::number(&arguments)?;
                let repository = arguments.get("repository").and_then(Value::as_str);
                let repo = self.linked_repo(&thread_id, number, repository).await?;
                self.pr_links
                    .unlink(&thread_id, &repo, number)
                    .await
                    .map_err(Self::pr_error)?;
                Ok(structured(
                    format!("Unlinked {}#{number} from this thread.", repo.full_name()),
                    json!({"repository": repo, "number": number, "linked": false}),
                ))
            }
            "list_thread_pull_requests" => {
                let pulls = self
                    .pr_links
                    .list(&thread_id)
                    .await
                    .map_err(Self::pr_error)?;
                if pulls.is_empty() {
                    return Ok(structured(
                        "No pull requests are linked to this thread.",
                        json!({"pull_requests": []}),
                    ));
                }
                let text = pulls
                    .iter()
                    .map(|pull| pull_text("", pull))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(structured(text, json!({"pull_requests": pulls})))
            }
            "watch_pull_request" => {
                let number = Self::number(&arguments)?;
                let repository = arguments.get("repository").and_then(Value::as_str);
                let repo = self.linked_repo(&thread_id, number, repository).await?;
                self.pr_links
                    .watch(&thread_id, &repo, number)
                    .await
                    .map_err(Self::pr_error)?;
                Ok(structured(
                    format!(
                        "Watching {}#{number}; a change starts a turn in this thread.",
                        repo.full_name()
                    ),
                    json!({"repository": repo, "number": number, "watching": true}),
                ))
            }
            "unwatch_pull_request" => {
                let number = Self::number(&arguments)?;
                let repository = arguments.get("repository").and_then(Value::as_str);
                let repo = self.linked_repo(&thread_id, number, repository).await?;
                self.pr_links.unwatch(&thread_id, &repo, number);
                Ok(structured(
                    format!(
                        "Stopped watching {}#{number}. The link stays.",
                        repo.full_name()
                    ),
                    json!({"repository": repo, "number": number, "watching": false}),
                ))
            }
            "thread_status" => {
                let summary = self
                    .manager
                    .thread_summary(&thread_id)
                    .await
                    .ok_or_else(|| {
                        ToolError::tool(format!("thread {thread_id} was not found on this daemon"))
                    })?;
                let running = self.manager.turn_in_flight(&thread_id).await;
                let queued = self.manager.thread_queue_len(&thread_id).await.unwrap_or(0);
                let text = format!(
                    "\"{}\" is {:?}{}{}.",
                    summary.title,
                    summary.agent_state,
                    if running { "; a turn is running" } else { "" },
                    match queued {
                        0 => String::new(),
                        1 => "; 1 message queued".into(),
                        count => format!("; {count} messages queued"),
                    }
                );
                Ok(structured(
                    text,
                    json!({
                        "thread_id": summary.thread_id,
                        "title": summary.title,
                        "state": summary.agent_state,
                        "tab": summary.tab,
                        "harness": summary.harness,
                        "model": summary.model,
                        "runtime_mode": summary.runtime_mode,
                        "workspace_path": summary.workspace_path,
                        "turn_in_flight": running,
                        "queued_messages": queued,
                        "needs_attention": summary.needs_attention,
                        "unread": summary.unread,
                        "last_activity": summary.last_activity,
                    }),
                ))
            }
            "list_forge_pull_requests" => {
                let repository = arguments.get("repository").and_then(Value::as_str);
                let repo = self.resolve_repo(&thread_id, repository).await?;
                let filter = match arguments
                    .get("filter")
                    .and_then(Value::as_str)
                    .unwrap_or("open")
                {
                    "open" => ForgePrFilter::Open,
                    "mine" => ForgePrFilter::Mine,
                    "review_requested" => ForgePrFilter::ReviewRequested,
                    other => {
                        return Err(ToolError::invalid(format!(
                            "`filter` must be open, mine or review_requested, not `{other}`"
                        )));
                    }
                };
                let page = arguments
                    .get("page")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or(1);
                let result = self
                    .forge
                    .list_prs(None, &repo, &filter, page)
                    .await
                    .map_err(|error| ToolError::tool(error.to_string()))?;
                let text = if result.prs.is_empty() {
                    format!("No pull requests match in {}.", repo.full_name())
                } else {
                    result
                        .prs
                        .iter()
                        .map(|pull| {
                            format!(
                                "{}#{} {} [{}] by {}",
                                pull.repo.full_name(),
                                pull.number,
                                pull.title,
                                pr_state(pull),
                                pull.author
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                Ok(structured(
                    text,
                    json!({
                        "repository": repo,
                        "page": page,
                        "pull_requests": result.prs,
                        "has_more": result.has_more,
                    }),
                ))
            }
            other => Err(ToolError::protocol(
                h4rn_mcp::jsonrpc::INVALID_PARAMS,
                format!("h4rn has no tool named `{other}`"),
            )),
        }
    }
}

impl AgentToolsProvider {
    /// Finds the repository of a link the thread really holds.
    async fn linked_repo(
        &self,
        thread_id: &ThreadId,
        number: u32,
        repository: Option<&str>,
    ) -> Result<ForgeRepoRef, ToolError> {
        let records = self
            .pr_links
            .records(thread_id)
            .await
            .map_err(Self::pr_error)?;
        let wanted = repository.map(|value| value.trim().to_ascii_lowercase());
        records
            .into_iter()
            .find(|record| {
                record.number == number
                    && wanted.as_ref().is_none_or(|wanted| {
                        record.repo.full_name().eq_ignore_ascii_case(wanted)
                            || wanted == &record.repo.owner.to_ascii_lowercase()
                    })
            })
            .map(|record| record.repo)
            .ok_or_else(|| {
                ToolError::tool(format!(
                    "{number} is not linked to this thread; link it before watching or unlinking it"
                ))
            })
    }
}

fn pull_text(prefix: &str, pull: &ThreadPullRequest) -> String {
    let lead = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix} ")
    };
    match &pull.summary {
        Some(summary) => format!(
            "{lead}{}#{} {} [{}]{}{}",
            pull.repo.full_name(),
            pull.number,
            summary.title,
            pr_state(summary),
            if pull.watching { "; watching" } else { "" },
            pull.watch_notice
                .as_ref()
                .map(|notice| format!("; {notice}"))
                .unwrap_or_default()
        ),
        None => format!(
            "{lead}{}#{} (the forge read failed: {}){}",
            pull.repo.full_name(),
            pull.number,
            pull.error.as_deref().unwrap_or("no detail"),
            if pull.watching { "; watching" } else { "" }
        ),
    }
}

fn pr_state(summary: &h4rn_protocol::ForgePrSummary) -> &'static str {
    use h4rn_protocol::ForgePrState;
    match summary.state {
        ForgePrState::Open if summary.draft => "draft",
        ForgePrState::Open => "open",
        ForgePrState::Merged => "merged",
        ForgePrState::Closed => "closed",
        ForgePrState::Draft => "draft",
    }
}

fn harness_label(harness: HarnessKind) -> &'static str {
    match harness {
        HarnessKind::ClaudeCode => "Claude Code",
        HarnessKind::Codex => "Codex",
        HarnessKind::Opencode => "OpenCode",
        HarnessKind::Antigravity => "Antigravity",
        HarnessKind::Acp => "ACP",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_schema_and_description() {
        assert_eq!(TOOL_NAMES.len(), 7);
    }
}
