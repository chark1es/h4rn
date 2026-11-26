//! The `h4rn__*` MCP tools a parent agent uses to delegate work (ADR-038).
//!
//! The hub mints one token per harness session and passes the session's scope
//! to every provider call. The thread id in that scope is the whole
//! authorization model for delegation: a child may only be spawned by, listed
//! for, or cancelled from the thread that owns the token. The tools therefore
//! never take a parent thread from the model.
//!
//! Waits are clamped below the hub's tool timeout ([`MAX_RESULT_WAIT`]) so a
//! healthy `get_subagent_result` always answers instead of the transport
//! timing out underneath it; the caller polls again when the child is slow.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use h4rn_mcp::provider::{ToolCallContext, ToolError, ToolProvider};
use h4rn_mcp::session::McpScope;
use h4rn_mcp::types::{CallToolResult, Content, ToolAnnotations, ToolDefinition};
use h4rn_protocol::{
    HarnessKind, SpawnSubagentRequest, SubagentBudget, SubagentContext, SubagentId,
    SubagentIsolation, SubagentView, ThreadId,
};
use serde_json::{Value, json};

use super::{SubagentError, SubagentManager};

/// Default bounded wait for [`SubagentProvider`]'s result tool.
const DEFAULT_RESULT_WAIT: Duration = Duration::from_secs(30);
/// Longest wait the result tool accepts, kept below the hub's 300s call cap.
const MAX_RESULT_WAIT: Duration = Duration::from_secs(180);

/// The `h4rn` subagent tool set.
pub struct SubagentProvider {
    manager: Arc<SubagentManager>,
}

impl SubagentProvider {
    /// Creates the provider around the daemon-wide supervisor.
    pub fn new(manager: Arc<SubagentManager>) -> Self {
        Self { manager }
    }

    fn manager(&self) -> &SubagentManager {
        &self.manager
    }
}

#[async_trait]
impl ToolProvider for SubagentProvider {
    fn namespace(&self) -> &str {
        h4rn_mcp::HUB_SERVER_NAME
    }

    async fn list_tools(&self, scope: &McpScope) -> Vec<ToolDefinition> {
        // Delegation only makes sense for a thread-scoped harness session;
        // a CLI client testing tools can still see the definitions.
        let _ = scope;
        vec![
            tool(
                "spawn_subagent",
                "Delegate a task to a child thread on any harness this daemon can run. \
                 Returns immediately with the subagent id; poll get_subagent_result for \
                 the handoff. The child inherits this thread's approval policy and its \
                 approvals appear on this thread.",
                json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "The complete task for the child. It does not see this conversation."
                        },
                        "harness": {
                            "type": "string",
                            "enum": ["claude-code", "codex", "opencode", "antigravity"],
                            "description": "Harness to run the child on; defaults to this thread's harness."
                        },
                        "model": {
                            "type": "string",
                            "description": "Model slug for the child; defaults to this thread's model."
                        },
                        "isolation": {
                            "type": "string",
                            "enum": ["shared", "worktree"],
                            "description": "shared (default) runs in this workspace; worktree gets a disposable git worktree."
                        },
                        "context": {
                            "type": "object",
                            "description": "Optional handoff: {kind:'summary',summary} or {kind:'files',paths,summary?}.",
                            "properties": {
                                "kind": {"type": "string", "enum": ["none", "summary", "files"]},
                                "summary": {"type": "string"},
                                "paths": {"type": "array", "items": {"type": "string"}}
                            },
                            "required": ["kind"]
                        },
                        "budget": {
                            "type": "object",
                            "description": "Optional caps; omitted fields use daemon defaults.",
                            "properties": {
                                "max_turns": {"type": "integer", "minimum": 1},
                                "max_wall_time_secs": {"type": "integer", "minimum": 1},
                                "max_tokens": {"type": "integer", "minimum": 1}
                            }
                        }
                    },
                    "required": ["task"],
                    "additionalProperties": false
                }),
            ),
            read_only_tool(
                "get_subagent_status",
                "Read one subagent's state, budget counters and last activity. Returns immediately.",
                json!({
                    "type": "object",
                    "properties": {
                        "subagent_id": {"type": "string", "description": "Id returned by spawn_subagent."}
                    },
                    "required": ["subagent_id"],
                    "additionalProperties": false
                }),
            ),
            read_only_tool(
                "get_subagent_result",
                "Wait up to wait_secs for a subagent to finish, then return its result text, \
                 usage and changed files. When it is still running the answer says so; call \
                 again to keep waiting.",
                json!({
                    "type": "object",
                    "properties": {
                        "subagent_id": {"type": "string", "description": "Id returned by spawn_subagent."},
                        "wait_secs": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 180,
                            "description": "Bounded wait in seconds (default 30, max 180)."
                        }
                    },
                    "required": ["subagent_id"],
                    "additionalProperties": false
                }),
            ),
            tool(
                "cancel_subagent",
                "Cancel a running subagent, interrupt its turn and delete its worktree branch.",
                json!({
                    "type": "object",
                    "properties": {
                        "subagent_id": {"type": "string", "description": "Id returned by spawn_subagent."}
                    },
                    "required": ["subagent_id"],
                    "additionalProperties": false
                }),
            ),
            read_only_tool(
                "list_subagents",
                "List this thread's subagents, newest first.",
                json!({
                    "type": "object",
                    "properties": {
                        "include_finished": {
                            "type": "boolean",
                            "description": "Include subagents that already finished (default false)."
                        }
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
        let parent = caller_thread(context)?;
        match tool {
            "spawn_subagent" => {
                let request = spawn_request(&parent, &arguments)?;
                let view = self
                    .manager()
                    .spawn_admitted(request, context.admission.clone())
                    .await
                    .map_err(to_tool_error)?;
                Ok(view_result(&view, "spawned"))
            }
            "get_subagent_status" => {
                let id = subagent_id(&arguments)?;
                let view = self
                    .manager()
                    .get(&id)
                    .await
                    .ok_or_else(|| not_found(&id))?;
                ensure_parent(&view, &parent)?;
                Ok(view_result(&view, "status"))
            }
            "get_subagent_result" => {
                let id = subagent_id(&arguments)?;
                let wait = result_wait(&arguments)?;
                context
                    .report_progress(
                        0.0,
                        Some(1.0),
                        Some(format!("waiting up to {}s for {id}", wait.as_secs())),
                    )
                    .await;
                let view = self
                    .manager()
                    .wait(&id, wait)
                    .await
                    .map_err(to_tool_error)?;
                ensure_parent(&view, &parent)?;
                Ok(result_view(&view))
            }
            "cancel_subagent" => {
                let id = subagent_id(&arguments)?;
                let existing = self
                    .manager()
                    .get(&id)
                    .await
                    .ok_or_else(|| not_found(&id))?;
                ensure_parent(&existing, &parent)?;
                let view = self.manager().cancel(&id).await.map_err(to_tool_error)?;
                Ok(view_result(&view, "cancelled"))
            }
            "list_subagents" => {
                let include_finished = arguments
                    .get("include_finished")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let views = self.manager().list(Some(&parent), include_finished).await;
                Ok(views_result(&views))
            }
            other => Err(ToolError::protocol(
                h4rn_mcp::jsonrpc::METHOD_NOT_FOUND,
                format!("unknown h4rn tool \"{other}\""),
            )),
        }
    }
}

fn tool(name: &str, description: &str, schema: Value) -> ToolDefinition {
    ToolDefinition::new(name, Some(description.into()), schema)
}

fn read_only_tool(name: &str, description: &str, schema: Value) -> ToolDefinition {
    let mut definition = tool(name, description, schema);
    definition.annotations = Some(ToolAnnotations {
        read_only_hint: Some(true),
        idempotent_hint: Some(true),
        ..ToolAnnotations::default()
    });
    definition
}

fn caller_thread(context: &ToolCallContext) -> Result<ThreadId, ToolError> {
    context.scope.thread_id.clone().ok_or_else(|| {
        ToolError::invalid(
            "delegation needs a session bound to a thread; open the thread that should delegate",
        )
    })
}

fn spawn_request(parent: &ThreadId, arguments: &Value) -> Result<SpawnSubagentRequest, ToolError> {
    let task = arguments
        .get("task")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|task| !task.is_empty())
        .ok_or_else(|| ToolError::invalid("spawn_subagent requires a non-empty \"task\""))?
        .to_owned();
    let harness = arguments
        .get("harness")
        .and_then(Value::as_str)
        .map(parse_harness)
        .transpose()?;
    let model = arguments
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let isolation = match arguments.get("isolation").and_then(Value::as_str) {
        None | Some("shared") => SubagentIsolation::Shared,
        Some("worktree") => SubagentIsolation::Worktree,
        Some(other) => {
            return Err(ToolError::invalid(format!(
                "isolation must be \"shared\" or \"worktree\", not \"{other}\""
            )));
        }
    };
    Ok(SpawnSubagentRequest {
        parent_thread: parent.clone(),
        task,
        harness,
        host: None,
        model,
        isolation,
        context: parse_context(arguments.get("context"))?,
        budget: parse_budget(arguments.get("budget"))?,
    })
}

fn parse_harness(value: &str) -> Result<HarnessKind, ToolError> {
    match value {
        "claude-code" | "claude_code" | "claude" => Ok(HarnessKind::ClaudeCode),
        "codex" => Ok(HarnessKind::Codex),
        "opencode" => Ok(HarnessKind::Opencode),
        "antigravity" => Ok(HarnessKind::Antigravity),
        other => Err(ToolError::invalid(format!(
            "unknown harness \"{other}\"; use claude-code, codex, opencode or antigravity"
        ))),
    }
}

fn parse_context(value: Option<&Value>) -> Result<SubagentContext, ToolError> {
    let Some(value) = value else {
        return Ok(SubagentContext::None);
    };
    if value.is_null() {
        return Ok(SubagentContext::None);
    }
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::invalid("context needs a \"kind\" of none, summary or files"))?;
    match kind {
        "none" => Ok(SubagentContext::None),
        "summary" => {
            let summary = value
                .get("summary")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ToolError::invalid("context.kind \"summary\" needs a \"summary\" string")
                })?;
            Ok(SubagentContext::Summary {
                summary: summary.to_owned(),
            })
        }
        "files" => {
            let paths = value
                .get("paths")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    ToolError::invalid("context.kind \"files\" needs a \"paths\" array")
                })?
                .iter()
                .map(|path| {
                    path.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| ToolError::invalid("context.paths entries must be strings"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let summary = value
                .get("summary")
                .and_then(Value::as_str)
                .map(str::to_owned);
            Ok(SubagentContext::Files { paths, summary })
        }
        other => Err(ToolError::invalid(format!(
            "unknown context kind \"{other}\"; use none, summary or files"
        ))),
    }
}

fn parse_budget(value: Option<&Value>) -> Result<SubagentBudget, ToolError> {
    let Some(value) = value else {
        return Ok(SubagentBudget::default());
    };
    if value.is_null() {
        return Ok(SubagentBudget::default());
    }
    let number = |field: &str| -> Result<Option<u64>, ToolError> {
        match value.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(number) => number.as_u64().map(Some).ok_or_else(|| {
                ToolError::invalid(format!("budget.{field} must be a positive integer"))
            }),
        }
    };
    let max_turns = number("max_turns")?
        .map(|turns| {
            u32::try_from(turns).map_err(|_| ToolError::invalid("budget.max_turns is too large"))
        })
        .transpose()?;
    Ok(SubagentBudget {
        max_turns,
        max_wall_time_secs: number("max_wall_time_secs")?,
        max_tokens: number("max_tokens")?,
    })
}

fn subagent_id(arguments: &Value) -> Result<SubagentId, ToolError> {
    let value = arguments
        .get("subagent_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ToolError::invalid("this tool requires \"subagent_id\", the id spawn_subagent returned")
        })?;
    Ok(SubagentId::new(value))
}

fn result_wait(arguments: &Value) -> Result<Duration, ToolError> {
    let seconds = arguments
        .get("wait_secs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_RESULT_WAIT.as_secs());
    let wait = Duration::from_secs(seconds.min(MAX_RESULT_WAIT.as_secs()));
    Ok(wait)
}

fn ensure_parent(view: &SubagentView, parent: &ThreadId) -> Result<(), ToolError> {
    if &view.parent_thread_id == parent {
        return Ok(());
    }
    Err(ToolError::invalid(format!(
        "subagent {} belongs to another thread",
        view.subagent_id
    )))
}

fn not_found(id: &SubagentId) -> ToolError {
    ToolError::invalid(format!(
        "subagent {id} does not exist; call list_subagents to see this thread's children"
    ))
}

fn to_tool_error(error: SubagentError) -> ToolError {
    ToolError::invalid(format!("{error}; {}", error.hint()))
}

fn view_result(view: &SubagentView, action: &str) -> CallToolResult {
    let structured = json!({
        "action": action,
        "subagent": view,
    });
    let text = format!(
        "{} {} — {} on {:?} (state: {:?}, {} turns, {} tokens, {}ms)",
        action,
        view.subagent_id,
        view.task.replace('\n', " "),
        view.harness,
        view.state,
        view.progress.turns_started,
        view.progress.tokens,
        view.progress.elapsed_ms,
    );
    result_with(structured, text, false)
}

fn views_result(views: &[SubagentView]) -> CallToolResult {
    let structured = json!({ "subagents": views });
    if views.is_empty() {
        return result_with(structured, "this thread has no subagents".to_owned(), false);
    }
    let mut lines = Vec::with_capacity(views.len());
    for view in views {
        lines.push(format!(
            "{} [{:?}] {:?} on {:?} — {}",
            view.subagent_id,
            view.state,
            view.harness,
            view.host,
            view.task.replace('\n', " "),
        ));
    }
    result_with(structured, lines.join("\n"), false)
}

fn result_view(view: &SubagentView) -> CallToolResult {
    let structured = json!({ "subagent": view });
    let text = match view.result.as_ref() {
        Some(result) if view.state == h4rn_protocol::SubagentState::Completed => {
            if result.text.trim().is_empty() {
                format!("{} completed without a text result", view.subagent_id)
            } else {
                result.text.clone()
            }
        }
        Some(result) if result.error.is_some() => format!(
            "{} finished as {:?}: {}",
            view.subagent_id,
            view.state,
            result.error.as_deref().unwrap_or("no detail")
        ),
        Some(_) => format!(
            "{} finished as {:?} with no text result",
            view.subagent_id, view.state
        ),
        None => format!(
            "{} is still {:?}; call get_subagent_result again to keep waiting",
            view.subagent_id, view.state
        ),
    };
    result_with(structured, text, false)
}

fn result_with(structured: Value, text: String, is_error: bool) -> CallToolResult {
    CallToolResult {
        content: vec![Content::text(text)],
        structured_content: Some(structured),
        is_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_arguments_become_a_request() {
        let request = spawn_request(
            &ThreadId::new("th_1"),
            &json!({
                "task": "  reply with pong  ",
                "harness": "codex",
                "isolation": "worktree",
                "context": {"kind": "files", "paths": ["src/lib.rs"]},
                "budget": {"max_turns": 2}
            }),
        )
        .unwrap();
        assert_eq!(request.task, "reply with pong");
        assert_eq!(request.harness, Some(HarnessKind::Codex));
        assert_eq!(request.isolation, SubagentIsolation::Worktree);
        assert_eq!(request.context.paths(), ["src/lib.rs".to_owned()]);
        assert_eq!(request.budget.max_turns, Some(2));
    }

    #[test]
    fn bad_harness_and_budget_are_rejected_with_a_hint() {
        let error = spawn_request(
            &ThreadId::new("th_1"),
            &json!({"task": "x", "harness": "gpt"}),
        )
        .unwrap_err();
        assert!(error.message.contains("unknown harness"));

        let error = spawn_request(
            &ThreadId::new("th_1"),
            &json!({"task": "x", "budget": {"max_turns": -1}}),
        )
        .unwrap_err();
        assert!(error.message.contains("max_turns"));
    }

    #[test]
    fn result_waits_are_bounded() {
        let wait = result_wait(&json!({"wait_secs": 10_000})).unwrap();
        assert_eq!(wait, MAX_RESULT_WAIT);
        assert_eq!(result_wait(&json!({})).unwrap(), DEFAULT_RESULT_WAIT);
    }
}
