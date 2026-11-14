//! The Codex `app-server` JSON-RPC normaliser.
//!
//! `codex app-server` speaks newline-delimited JSON-RPC 2.0 over stdio. Three
//! frame shapes arrive on stdout:
//!
//! - responses to our requests (`id` + `result` or `error`),
//! - server-initiated requests (`id` + `method`), which are approvals and
//!   user-input prompts,
//! - notifications (`method` + `params`), which carry the turn stream.
//!
//! [`CodexParser`] normalises notifications. The session's stdout task routes
//! responses to their waiting request and turns server requests into
//! [`HarnessEvent::ApprovalRequested`] via [`approval_from_request`].
//!
//! A line that is not JSON becomes a diagnostic [`HarnessEvent::Error`] rather
//! than a panic (ADR-023).

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use h4rn_protocol::{
    AgentState, ApprovalDecision, ApprovalKind, ApprovalOption, ToolCall, ToolCallStatus,
    TurnStatus, Usage,
};
use serde_json::{Value, json};

use super::limit;
use crate::{HarnessEvent, TurnReport};

const OUTPUT_PREVIEW_CHARS: usize = 2_000;
const SUMMARY_CHARS: usize = 140;
const DIAGNOSTIC_CHARS: usize = 320;

/// The normaliser for one `codex app-server` process.
///
/// Feed it each notification line with [`push_line`](Self::push_line). Frames
/// that are responses or server requests are ignored here; the session routes
/// those separately.
#[derive(Debug)]
pub struct CodexParser {
    session_id: Option<String>,
    model: Option<String>,
    started: bool,
    state: AgentState,
    turn_id: Option<String>,
    tool_calls: HashMap<String, ToolMeta>,
    agent_deltas: HashSet<String>,
    touched: Vec<String>,
    last_turn: Option<TurnReport>,
    usage: Usage,
    usage_emitted: bool,
    limit_seen: bool,
    error_seen: bool,
}

#[derive(Clone, Debug)]
struct ToolMeta {
    name: String,
    summary: String,
    input: Option<Value>,
    started_at: DateTime<Utc>,
}

impl Default for CodexParser {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexParser {
    /// Creates a parser with no session state.
    pub fn new() -> Self {
        Self {
            session_id: None,
            model: None,
            started: false,
            state: AgentState::Idle,
            turn_id: None,
            tool_calls: HashMap::new(),
            agent_deltas: HashSet::new(),
            touched: Vec::new(),
            last_turn: None,
            usage: Usage::default(),
            usage_emitted: false,
            limit_seen: false,
            error_seen: false,
        }
    }

    /// Feeds one stdout line and returns the events it produced.
    pub fn push_line(&mut self, line: &str) -> Vec<HarnessEvent> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(error) => {
                return vec![HarnessEvent::Error {
                    message: format!(
                        "unparseable codex output ({error}): {}",
                        truncate_chars(line, DIAGNOSTIC_CHARS)
                    ),
                    retryable: false,
                }];
            }
        };
        self.push_value(&value)
    }

    /// Feeds one already-parsed frame and returns the events it produced.
    pub fn push_value(&mut self, value: &Value) -> Vec<HarnessEvent> {
        if value.get("id").is_some() {
            // Responses and server requests are routed by the session.
            return Vec::new();
        }
        let Some(method) = value.get("method").and_then(Value::as_str) else {
            return vec![HarnessEvent::Error {
                message: format!(
                    "unparseable codex output: {}",
                    truncate_chars(&value.to_string(), DIAGNOSTIC_CHARS)
                ),
                retryable: false,
            }];
        };
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        self.on_notification(method, &params)
    }

    /// The Codex thread id, once `thread/started` or a handshake reported it.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// The model reported by the thread.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The current agent-state hint.
    pub fn agent_state(&self) -> AgentState {
        self.state
    }

    /// The current turn id, once `turn/started` reported it.
    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    /// Paths touched by file-change items so far.
    pub fn touched_paths(&self) -> &[String] {
        &self.touched
    }

    /// The most recent completed turn.
    pub fn last_turn(&self) -> Option<&TurnReport> {
        self.last_turn.as_ref()
    }

    fn on_notification(&mut self, method: &str, params: &Value) -> Vec<HarnessEvent> {
        match method {
            "thread/started" => self.on_thread_started(params),
            "thread/status/changed" => {
                match params["status"]["type"].as_str().unwrap_or_default() {
                    "active" => self.state = AgentState::Thinking,
                    "systemError" => self.state = AgentState::Error,
                    "idle" if self.state == AgentState::Thinking => self.state = AgentState::Idle,
                    _ => {}
                }
                Vec::new()
            }
            "turn/started" => {
                self.turn_id = params["turn"]["id"].as_str().map(str::to_string);
                self.usage = Usage::default();
                self.usage_emitted = false;
                self.limit_seen = false;
                self.error_seen = false;
                self.agent_deltas.clear();
                self.state = AgentState::Thinking;
                Vec::new()
            }
            "item/agentMessage/delta" => {
                let Some(delta) = params["delta"].as_str() else {
                    return Vec::new();
                };
                if let Some(item_id) = params["itemId"].as_str() {
                    self.agent_deltas.insert(item_id.to_string());
                }
                self.state = AgentState::Thinking;
                if delta.is_empty() {
                    Vec::new()
                } else {
                    vec![HarnessEvent::TextDelta {
                        text: delta.to_string(),
                    }]
                }
            }
            "item/reasoning/textDelta"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/summaryPartAdded"
            | "item/plan/delta" => {
                self.state = AgentState::Thinking;
                Vec::new()
            }
            "item/started" => self.on_item(params, false),
            "item/completed" => self.on_item(params, true),
            "thread/tokenUsage/updated" => self.on_usage(params),
            "account/rateLimits/updated" => self.on_rate_limits(params),
            "error" => self.on_error(params),
            "turn/completed" => self.on_turn_completed(params),
            "warning"
            | "configWarning"
            | "account/updated"
            | "mcpServer/startupStatus/updated"
            | "remoteControl/status/changed"
            | "thread/closed" => Vec::new(),
            _ => Vec::new(),
        }
    }

    fn on_thread_started(&mut self, params: &Value) -> Vec<HarnessEvent> {
        let thread = &params["thread"];
        if let Some(id) = thread["id"]
            .as_str()
            .or_else(|| thread["sessionId"].as_str())
        {
            self.session_id = Some(id.to_string());
        }
        if let Some(model) = thread["model"].as_str() {
            self.model = Some(model.to_string());
        }
        if self.started {
            return Vec::new();
        }
        let Some(session_id) = self.session_id.clone() else {
            return Vec::new();
        };
        self.started = true;
        self.state = AgentState::Idle;
        vec![HarnessEvent::SessionStarted { session_id }]
    }

    fn on_usage(&mut self, params: &Value) -> Vec<HarnessEvent> {
        let last = &params["tokenUsage"]["last"];
        let usage = Usage {
            input_tokens: u64_field(last, "inputTokens"),
            output_tokens: u64_field(last, "outputTokens"),
            cached_input_tokens: u64_field(last, "cachedInputTokens"),
            cost_micros: 0,
            duration_ms: self.usage.duration_ms,
        };
        self.usage = usage.clone();
        self.usage_emitted = true;
        self.state = AgentState::Thinking;
        if usage.total_tokens() == 0 {
            Vec::new()
        } else {
            vec![HarnessEvent::Usage { usage }]
        }
    }

    fn on_rate_limits(&mut self, params: &Value) -> Vec<HarnessEvent> {
        if self.limit_seen {
            return Vec::new();
        }
        let Some(reset_at) = limit::rate_limit_hit(params, Utc::now()) else {
            return Vec::new();
        };
        self.limit_seen = true;
        self.state = AgentState::RateLimited;
        vec![HarnessEvent::LimitHit {
            reset_at,
            auto_resume: reset_at.is_some(),
        }]
    }

    fn on_error(&mut self, params: &Value) -> Vec<HarnessEvent> {
        let message = params["error"]["message"]
            .as_str()
            .unwrap_or("codex reported an error")
            .to_string();
        let retryable = params["willRetry"].as_bool().unwrap_or(false);
        if let Some(signal) = limit::detect_limit(&message, Utc::now()) {
            if self.limit_seen {
                return Vec::new();
            }
            self.limit_seen = true;
            self.state = AgentState::RateLimited;
            return vec![HarnessEvent::LimitHit {
                reset_at: signal.reset_at,
                auto_resume: signal.reset_at.is_some(),
            }];
        }
        if self.error_seen {
            return Vec::new();
        }
        self.error_seen = true;
        self.state = if retryable {
            AgentState::Thinking
        } else {
            AgentState::Error
        };
        vec![HarnessEvent::Error {
            message: truncate_chars(&message, OUTPUT_PREVIEW_CHARS),
            retryable,
        }]
    }

    fn on_item(&mut self, params: &Value, completed: bool) -> Vec<HarnessEvent> {
        let item = &params["item"];
        let id = item["id"].as_str().unwrap_or_default();
        match item["type"].as_str().unwrap_or_default() {
            "agentMessage" => {
                self.state = AgentState::Thinking;
                if !completed {
                    return Vec::new();
                }
                if self.agent_deltas.remove(id) {
                    return Vec::new();
                }
                let text = item["text"].as_str().unwrap_or_default();
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![HarnessEvent::TextDelta {
                        text: text.to_string(),
                    }]
                }
            }
            "reasoning" | "plan" | "userMessage" | "hookPrompt" | "functionCallOutput"
            | "contextCompaction" => {
                if item["type"].as_str() == Some("reasoning") {
                    self.state = AgentState::Thinking;
                }
                Vec::new()
            }
            "commandExecution" => self.on_command(item, completed),
            "fileChange" => self.on_file_change(item, completed),
            "mcpToolCall" => self.on_mcp_tool(item, completed),
            "dynamicToolCall" | "collabAgentToolCall" => self.on_dynamic_tool(item, completed),
            "webSearch" => self.on_web_search(item, completed),
            "imageView" => self.on_image_view(item, completed),
            _ => Vec::new(),
        }
    }

    fn on_command(&mut self, item: &Value, completed: bool) -> Vec<HarnessEvent> {
        let command = item["command"].as_str().unwrap_or_default();
        let input = json!({
            "command": command,
            "cwd": item["cwd"].clone(),
        });
        self.state = AgentState::RunningTool;
        let status = if completed {
            tool_status(item["status"].as_str())
        } else {
            ToolCallStatus::Running
        };
        let output = if completed {
            item["aggregatedOutput"]
                .as_str()
                .filter(|output| !output.trim().is_empty())
                .map(|output| truncate_chars(output.trim(), OUTPUT_PREVIEW_CHARS))
        } else {
            None
        };
        self.tool_event(
            item,
            "command",
            &truncate_chars(command.trim(), SUMMARY_CHARS),
            Some(input),
            status,
            output,
        )
    }

    fn on_file_change(&mut self, item: &Value, completed: bool) -> Vec<HarnessEvent> {
        let paths = file_change_paths(item);
        for path in &paths {
            if !self.touched.contains(path) {
                self.touched.push(path.clone());
            }
        }
        self.state = AgentState::Editing;
        let summary = truncate_chars(&paths.join(", "), SUMMARY_CHARS);
        let input = json!({ "changes": item["changes"].clone() });
        let status = if completed {
            tool_status(item["status"].as_str())
        } else {
            ToolCallStatus::Running
        };
        self.tool_event(item, "apply_patch", &summary, Some(input), status, None)
    }

    fn on_mcp_tool(&mut self, item: &Value, completed: bool) -> Vec<HarnessEvent> {
        let server = item["server"].as_str().unwrap_or("mcp");
        let tool = item["tool"].as_str().unwrap_or("tool");
        let name = format!("mcp__{server}__{tool}");
        let summary = value_summary(&item["arguments"]);
        let input = Some(item["arguments"].clone()).filter(|value| !value.is_null());
        let status = if completed {
            tool_status(item["status"].as_str())
        } else {
            ToolCallStatus::Running
        };
        let output = if completed {
            item["result"]
                .as_str()
                .map(str::to_string)
                .or_else(|| {
                    item["error"]
                        .as_str()
                        .map(|error| truncate_chars(error, OUTPUT_PREVIEW_CHARS))
                })
                .or_else(|| (!item["result"].is_null()).then(|| value_preview(&item["result"])))
        } else {
            None
        };
        self.state = AgentState::RunningTool;
        self.tool_event(item, &name, &summary, input, status, output)
    }

    fn on_dynamic_tool(&mut self, item: &Value, completed: bool) -> Vec<HarnessEvent> {
        let name = item["tool"].as_str().unwrap_or("tool");
        let summary = value_summary(&item["arguments"]);
        let input = Some(item["arguments"].clone()).filter(|value| !value.is_null());
        let status = if completed {
            tool_status(item["status"].as_str())
        } else {
            ToolCallStatus::Running
        };
        let output = if completed && !item["contentItems"].is_null() {
            Some(value_preview(&item["contentItems"]))
        } else {
            None
        };
        self.state = AgentState::RunningTool;
        self.tool_event(item, name, &summary, input, status, output)
    }

    fn on_web_search(&mut self, item: &Value, completed: bool) -> Vec<HarnessEvent> {
        let query = item["query"].as_str().unwrap_or_default();
        let input = json!({ "query": query });
        let status = if completed {
            ToolCallStatus::Completed
        } else {
            ToolCallStatus::Running
        };
        let output = if completed && !item["results"].is_null() {
            Some(value_preview(&item["results"]))
        } else {
            None
        };
        self.state = AgentState::Searching;
        self.tool_event(
            item,
            "web_search",
            &truncate_chars(query.trim(), SUMMARY_CHARS),
            Some(input),
            status,
            output,
        )
    }

    fn on_image_view(&mut self, item: &Value, completed: bool) -> Vec<HarnessEvent> {
        let path = item["path"].as_str().unwrap_or_default();
        let input = json!({ "path": path });
        let status = if completed {
            ToolCallStatus::Completed
        } else {
            ToolCallStatus::Running
        };
        self.state = AgentState::Reading;
        self.tool_event(
            item,
            "image_view",
            &truncate_chars(path, SUMMARY_CHARS),
            Some(input),
            status,
            None,
        )
    }

    fn tool_event(
        &mut self,
        item: &Value,
        name: &str,
        summary: &str,
        input: Option<Value>,
        status: ToolCallStatus,
        output: Option<String>,
    ) -> Vec<HarnessEvent> {
        let Some(id) = item["id"].as_str() else {
            return Vec::new();
        };
        let (started_at, name, summary) = if status == ToolCallStatus::Running {
            let started_at = Utc::now();
            self.tool_calls.insert(
                id.to_string(),
                ToolMeta {
                    name: name.to_string(),
                    summary: summary.to_string(),
                    input: input.clone(),
                    started_at,
                },
            );
            (started_at, name.to_string(), summary.to_string())
        } else {
            match self.tool_calls.remove(id) {
                Some(meta) => (
                    meta.started_at,
                    meta.name,
                    if meta.summary.is_empty() {
                        summary.to_string()
                    } else {
                        meta.summary
                    },
                ),
                None => (Utc::now(), name.to_string(), summary.to_string()),
            }
        };
        let completed_at = (status != ToolCallStatus::Running).then(Utc::now);
        let input = input.or_else(|| self.tool_calls.get(id).and_then(|meta| meta.input.clone()));
        // Codex reports file edits as `fileChange` unified diffs (kept in the
        // input); it has no search tool, so only edits get stats.
        let stats = (status == ToolCallStatus::Completed)
            .then(|| crate::stats::derive(&name, input.as_ref(), output.as_deref(), None))
            .flatten();
        vec![HarnessEvent::ToolCall {
            tool_call: ToolCall {
                tool_call_id: id.to_string(),
                name,
                status,
                summary,
                input,
                output,
                started_at,
                completed_at,
                stats,
            },
        }]
    }

    fn on_turn_completed(&mut self, params: &Value) -> Vec<HarnessEvent> {
        let turn = &params["turn"];
        let status_text = turn["status"].as_str().unwrap_or("failed");
        let error_message = turn["error"]["message"].as_str().map(str::to_string);
        let duration_ms = turn["durationMs"]
            .as_u64()
            .or_else(|| duration_from_timestamps(turn))
            .unwrap_or(self.usage.duration_ms);
        self.usage.duration_ms = duration_ms;

        let mut events = Vec::new();
        match status_text {
            "failed" => {
                if let Some(message) = &error_message
                    && let Some(signal) = limit::detect_limit(message, Utc::now())
                {
                    if !self.limit_seen {
                        self.limit_seen = true;
                        self.state = AgentState::RateLimited;
                        events.push(HarnessEvent::LimitHit {
                            reset_at: signal.reset_at,
                            auto_resume: signal.reset_at.is_some(),
                        });
                    }
                } else if !self.error_seen {
                    self.error_seen = true;
                    self.state = AgentState::Error;
                    events.push(HarnessEvent::Error {
                        message: error_message.clone().unwrap_or_default(),
                        retryable: true,
                    });
                }
            }
            "interrupted" => self.state = AgentState::Done,
            _ => self.state = AgentState::Done,
        }

        let status = match status_text {
            "completed" => TurnStatus::Completed,
            "interrupted" => TurnStatus::Interrupted,
            _ => TurnStatus::Failed,
        };
        let is_error = status == TurnStatus::Failed;

        self.last_turn = Some(TurnReport {
            num_turns: 1,
            duration_ms,
            duration_api_ms: 0,
            cost_micros: 0,
            is_error,
            stop_reason: error_message.clone(),
            subtype: Some(status_text.to_string()),
        });

        let usage = self.usage.clone();
        if !self.usage_emitted && usage.total_tokens() > 0 {
            events.push(HarnessEvent::Usage {
                usage: usage.clone(),
            });
        }
        events.push(HarnessEvent::TurnCompleted { status, usage });

        self.limit_seen = false;
        self.error_seen = false;
        self.usage_emitted = false;
        self.agent_deltas.clear();
        self.turn_id = None;
        events
    }
}

/// One server-initiated approval request, normalised for the session.
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    /// Opaque id echoed back by [`crate::HarnessSession::respond_approval`].
    pub approval_id: String,
    /// The JSON-RPC method that raised the request.
    pub method: String,
    /// What kind of hand is raised.
    pub kind: ApprovalKind,
    /// What the harness wants to do.
    pub prompt: String,
    /// Selectable answers.
    pub options: Vec<ApprovalOption>,
}

impl ApprovalRequest {
    /// Builds the event the daemon projects from this request.
    pub fn event(&self) -> HarnessEvent {
        HarnessEvent::ApprovalRequested {
            approval_id: self.approval_id.clone(),
            kind: self.kind,
            prompt: self.prompt.clone(),
            options: self.options.clone(),
        }
    }
}

/// Normalises a server-initiated Codex request into an approval.
///
/// Returns `None` for server requests the adapter does not implement (dynamic
/// tools, attestation, ChatGPT token refresh); the session answers those with a
/// JSON-RPC `method not found` error so Codex does not hang.
pub fn approval_from_request(method: &str, params: &Value) -> Option<ApprovalRequest> {
    match method {
        "item/commandExecution/requestApproval" => {
            let item_id = params["itemId"].as_str()?;
            let approval_id = params["approvalId"].as_str().unwrap_or(item_id).to_string();
            let command = params["command"].as_str().unwrap_or("a command");
            let prompt = match params["reason"]
                .as_str()
                .filter(|reason| !reason.is_empty())
            {
                Some(reason) => format!("{command}\n{reason}"),
                None => command.to_string(),
            };
            Some(ApprovalRequest {
                approval_id,
                method: method.to_string(),
                kind: ApprovalKind::Command,
                prompt,
                options: command_options(params),
            })
        }
        "item/fileChange/requestApproval" => {
            let item_id = params["itemId"].as_str()?;
            let prompt = params["reason"]
                .as_str()
                .filter(|reason| !reason.is_empty())
                .unwrap_or("Apply the requested file changes")
                .to_string();
            Some(ApprovalRequest {
                approval_id: item_id.to_string(),
                method: method.to_string(),
                kind: ApprovalKind::FileChange,
                prompt,
                options: file_change_options(params),
            })
        }
        "item/permissions/requestApproval" => {
            let item_id = params["itemId"].as_str()?;
            let prompt = params["reason"]
                .as_str()
                .filter(|reason| !reason.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "Grant additional permissions: {}",
                        value_preview(&params["permissions"])
                    )
                });
            Some(ApprovalRequest {
                approval_id: item_id.to_string(),
                method: method.to_string(),
                kind: ApprovalKind::Permission,
                prompt,
                options: standard_options(),
            })
        }
        "item/tool/requestUserInput" => {
            let item_id = params["itemId"].as_str()?;
            let questions = params["questions"].as_array();
            let prompt = questions
                .and_then(|questions| {
                    questions
                        .iter()
                        .map(|question| {
                            let header = question["header"].as_str().unwrap_or_default();
                            let question = question["question"].as_str().unwrap_or_default();
                            if header.is_empty() {
                                question.to_string()
                            } else {
                                format!("{header}: {question}")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                        .into()
                })
                .filter(|prompt: &String| !prompt.trim().is_empty())
                .unwrap_or_else(|| "Answer the agent's question".to_string());
            let options = questions
                .and_then(|questions| questions.first())
                .and_then(|question| question["options"].as_array())
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|option| {
                            let label = option["label"].as_str()?;
                            Some(ApprovalOption {
                                id: label.to_string(),
                                label: label.to_string(),
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some(ApprovalRequest {
                approval_id: item_id.to_string(),
                method: method.to_string(),
                kind: ApprovalKind::Question,
                prompt,
                options,
            })
        }
        "applyPatchApproval" => {
            let call_id = params["callId"].as_str()?;
            let paths = params["fileChanges"]
                .as_object()
                .map(|changes| changes.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            let prompt = params["reason"]
                .as_str()
                .filter(|reason| !reason.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if paths.is_empty() {
                        "Apply the requested patch".to_string()
                    } else {
                        format!("Apply the requested patch to {paths}")
                    }
                });
            Some(ApprovalRequest {
                approval_id: call_id.to_string(),
                method: method.to_string(),
                kind: ApprovalKind::FileChange,
                prompt,
                options: legacy_options(),
            })
        }
        "execCommandApproval" => {
            let call_id = params["callId"].as_str()?;
            let approval_id = params["approvalId"].as_str().unwrap_or(call_id).to_string();
            let command = params["command"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|command| !command.is_empty())
                .unwrap_or_else(|| "a command".to_string());
            Some(ApprovalRequest {
                approval_id,
                method: method.to_string(),
                kind: ApprovalKind::Command,
                prompt: command,
                options: legacy_options(),
            })
        }
        _ => None,
    }
}

/// Builds the full JSON-RPC response frame for an approval decision.
///
/// Modern command and file-change prompts accept `accept`, `acceptForSession`,
/// `decline` and `cancel`. Legacy `execCommandApproval`/`applyPatchApproval`
/// prompts accept the older `ReviewDecision` shape. Permission questions that
/// the schema only models as a grant are answered with a JSON-RPC error when
/// the user declines.
pub fn approval_response(
    rpc_id: &Value,
    method: &str,
    params: &Value,
    decision: ApprovalDecision,
    option_id: Option<&str>,
) -> Value {
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            let selected = match decision {
                ApprovalDecision::Approve => match option_id {
                    Some("acceptForSession") => "acceptForSession",
                    _ => "accept",
                },
                ApprovalDecision::Deny => "decline",
                ApprovalDecision::Cancel => "cancel",
            };
            json!({ "id": rpc_id, "result": { "decision": selected } })
        }
        "item/permissions/requestApproval" => match decision {
            ApprovalDecision::Approve => {
                let permissions = params["permissions"].clone();
                json!({
                    "id": rpc_id,
                    "result": { "permissions": permissions, "scope": "turn" }
                })
            }
            _ => json_rpc_error(rpc_id, "permission denied by user"),
        },
        "item/tool/requestUserInput" => match decision {
            ApprovalDecision::Approve => {
                let answer = option_id.unwrap_or_default();
                let mut answers = serde_json::Map::new();
                if let Some(questions) = params["questions"].as_array() {
                    for question in questions {
                        if let Some(id) = question["id"].as_str() {
                            answers.insert(id.to_string(), json!({ "answers": [answer] }));
                        }
                    }
                }
                json!({ "id": rpc_id, "result": { "answers": answers } })
            }
            _ => json_rpc_error(rpc_id, "user declined to answer"),
        },
        "applyPatchApproval" | "execCommandApproval" => {
            let decision = match decision {
                ApprovalDecision::Approve => match option_id {
                    Some("approved_for_session") => json!("approved_for_session"),
                    _ => json!("approved"),
                },
                ApprovalDecision::Deny => json!({ "denied": { "rejection": "denied by user" } }),
                ApprovalDecision::Cancel => json!("abort"),
            };
            json!({ "id": rpc_id, "result": { "decision": decision } })
        }
        _ => json_rpc_error(rpc_id, "unsupported approval request"),
    }
}

fn command_options(params: &Value) -> Vec<ApprovalOption> {
    let decisions = params["availableDecisions"]
        .as_array()
        .map(|decisions| {
            decisions
                .iter()
                .filter_map(|decision| match decision {
                    Value::String(id) => {
                        let (id, label) = match id.as_str() {
                            "accept" => ("accept", "Approve"),
                            "acceptForSession" => ("acceptForSession", "Approve for session"),
                            "decline" => ("decline", "Deny"),
                            "cancel" => ("cancel", "Cancel"),
                            _ => return None,
                        };
                        Some(ApprovalOption {
                            id: id.to_string(),
                            label: label.to_string(),
                        })
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if decisions.is_empty() {
        standard_options()
    } else {
        decisions
    }
}

fn file_change_options(params: &Value) -> Vec<ApprovalOption> {
    // The file-change schema has no availableDecisions; the full decision set
    // is always valid.
    let _ = params;
    standard_options()
}

fn standard_options() -> Vec<ApprovalOption> {
    vec![
        ApprovalOption {
            id: "accept".into(),
            label: "Approve".into(),
        },
        ApprovalOption {
            id: "acceptForSession".into(),
            label: "Approve for session".into(),
        },
        ApprovalOption {
            id: "decline".into(),
            label: "Deny".into(),
        },
        ApprovalOption {
            id: "cancel".into(),
            label: "Cancel".into(),
        },
    ]
}

fn legacy_options() -> Vec<ApprovalOption> {
    vec![
        ApprovalOption {
            id: "approved".into(),
            label: "Approve".into(),
        },
        ApprovalOption {
            id: "approved_for_session".into(),
            label: "Approve for session".into(),
        },
        ApprovalOption {
            id: "denied".into(),
            label: "Deny".into(),
        },
        ApprovalOption {
            id: "abort".into(),
            label: "Abort turn".into(),
        },
    ]
}

fn json_rpc_error(id: &Value, message: &str) -> Value {
    json!({
        "id": id,
        "error": { "code": -32000, "message": message }
    })
}

fn tool_status(status: Option<&str>) -> ToolCallStatus {
    match status {
        Some("completed") => ToolCallStatus::Completed,
        Some("failed") => ToolCallStatus::Failed,
        Some("declined") => ToolCallStatus::Denied,
        _ => ToolCallStatus::Running,
    }
}

fn file_change_paths(item: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(changes) = item["changes"].as_array() {
        for change in changes {
            if let Some(path) = change["path"].as_str()
                && !path.is_empty()
                && !paths.contains(&path.to_string())
            {
                paths.push(path.to_string());
            }
        }
    }
    paths
}

fn duration_from_timestamps(turn: &Value) -> Option<u64> {
    let started = turn["startedAt"].as_i64()?;
    let completed = turn["completedAt"].as_i64()?;
    u64::try_from((completed - started).max(0))
        .ok()
        .map(|seconds| seconds.saturating_mul(1_000))
}

fn u64_field(value: &Value, key: &str) -> u64 {
    value[key]
        .as_u64()
        .or_else(|| value[key].as_f64().map(|value| value as u64))
        .unwrap_or(0)
}

fn value_summary(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => truncate_chars(text.trim(), SUMMARY_CHARS),
        Value::Object(object) => {
            for key in [
                "query",
                "path",
                "file_path",
                "command",
                "description",
                "name",
            ] {
                if let Some(text) = object.get(key).and_then(Value::as_str)
                    && !text.is_empty()
                {
                    return truncate_chars(text.trim(), SUMMARY_CHARS);
                }
            }
            truncate_chars(&value.to_string(), SUMMARY_CHARS)
        }
        other => truncate_chars(&other.to_string(), SUMMARY_CHARS),
    }
}

fn value_preview(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => truncate_chars(text.trim(), OUTPUT_PREVIEW_CHARS),
        other => truncate_chars(&other.to_string(), OUTPUT_PREVIEW_CHARS),
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_started(item: Value) -> Value {
        json!({
            "item": item,
            "threadId": "thread-1",
            "turnId": "turn-1",
            "startedAtMs": 1_794_548_672_613i64
        })
    }

    #[test]
    fn agent_message_deltas_are_not_duplicated_by_the_completed_item() {
        let mut parser = CodexParser::new();
        parser.push_value(&json!({
            "method": "thread/started",
            "params": { "thread": { "id": "thread-1", "model": "gpt-6.1-sol" } }
        }));
        let delta = parser.push_value(&json!({
            "method": "item/agentMessage/delta",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "agent-1",
                "delta": "pong"
            }
        }));
        assert_eq!(
            delta,
            vec![HarnessEvent::TextDelta {
                text: "pong".into()
            }]
        );
        let completed = parser.push_value(&json!({
            "method": "item/completed",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "completedAtMs": 1_794_548_674_063i64,
                "item": { "type": "agentMessage", "id": "agent-1", "text": "pong" }
            }
        }));
        assert!(completed.is_empty());
    }

    #[test]
    fn command_and_file_change_items_become_tool_calls() {
        let mut parser = CodexParser::new();
        let started = parser.push_value(&json!({
            "method": "item/started",
            "params": item_started(json!({
                "type": "commandExecution",
                "id": "cmd-1",
                "command": "ls",
                "cwd": "/workspace",
                "status": "inProgress"
            }))
        }));
        assert!(matches!(
            started.as_slice(),
            [HarnessEvent::ToolCall { tool_call }]
                if tool_call.status == ToolCallStatus::Running
                    && tool_call.summary == "ls"
        ));

        let completed = parser.push_value(&json!({
            "method": "item/completed",
            "params": {
                "item": {
                    "type": "commandExecution",
                    "id": "cmd-1",
                    "command": "ls",
                    "cwd": "/workspace",
                    "status": "completed",
                    "exitCode": 0,
                    "aggregatedOutput": "a.rs\n"
                },
                "threadId": "thread-1",
                "turnId": "turn-1",
                "completedAtMs": 1_794_548_672_700i64
            }
        }));
        assert!(matches!(
            completed.as_slice(),
            [HarnessEvent::ToolCall { tool_call }]
                if tool_call.status == ToolCallStatus::Completed
                    && tool_call.output.as_deref() == Some("a.rs")
        ));

        let file = parser.push_value(&json!({
            "method": "item/completed",
            "params": {
                "item": {
                    "type": "fileChange",
                    "id": "patch-1",
                    "status": "completed",
                    "changes": [{"path": "/workspace/a.rs", "diff": "@@", "kind": "update"}]
                },
                "threadId": "thread-1",
                "turnId": "turn-1",
                "completedAtMs": 1_794_548_672_800i64
            }
        }));
        assert!(matches!(
            file.as_slice(),
            [HarnessEvent::ToolCall { tool_call }]
                if tool_call.status == ToolCallStatus::Completed
                    && tool_call.summary == "/workspace/a.rs"
        ));
        assert_eq!(parser.touched_paths(), ["/workspace/a.rs".to_string()]);
    }

    #[test]
    fn approval_requests_expose_the_decision_options() {
        let request = approval_from_request(
            "item/commandExecution/requestApproval",
            &json!({
                "itemId": "cmd-1",
                "command": "cargo test",
                "reason": "needs network",
                "availableDecisions": ["accept", "acceptForSession", "decline", "cancel"]
            }),
        )
        .expect("approval");
        assert_eq!(request.kind, ApprovalKind::Command);
        assert_eq!(request.prompt, "cargo test\nneeds network");
        assert_eq!(request.options.len(), 4);
        assert_eq!(request.options[1].id, "acceptForSession");

        let response = approval_response(
            &json!(7),
            "item/commandExecution/requestApproval",
            &json!({}),
            ApprovalDecision::Approve,
            Some("acceptForSession"),
        );
        assert_eq!(response["result"]["decision"], "acceptForSession");
    }

    #[test]
    fn legacy_patch_approvals_use_review_decision() {
        let request = approval_from_request(
            "applyPatchApproval",
            &json!({
                "callId": "call-1",
                "conversationId": "thread-1",
                "fileChanges": {"/workspace/a.rs": {"type": "update", "unified_diff": "@@"}}
            }),
        )
        .expect("approval");
        assert_eq!(request.kind, ApprovalKind::FileChange);
        assert!(request.prompt.contains("/workspace/a.rs"));

        let response = approval_response(
            &json!(9),
            "applyPatchApproval",
            &json!({}),
            ApprovalDecision::Deny,
            None,
        );
        assert_eq!(
            response["result"]["decision"]["denied"]["rejection"],
            "denied by user"
        );
    }

    #[test]
    fn malformed_lines_become_diagnostics() {
        let mut parser = CodexParser::new();
        assert!(matches!(
            parser.push_line("this is not json").as_slice(),
            [HarnessEvent::Error {
                retryable: false,
                ..
            }]
        ));
        assert!(parser.push_line("").is_empty());
    }
}
