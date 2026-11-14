//! The OpenCode v2 event-stream normaliser.
//!
//! `opencode serve` publishes server-sent events on `GET /api/event`. Each
//! event is one JSON value in a `data:` line:
//!
//! ```text
//! data: {"id":"evt_...","type":"session.text.delta","data":{...}}
//! : heartbeat
//! ```
//!
//! [`OpenCodeParser`] accepts raw SSE lines (and bare JSON values, for tests)
//! and normalises the session, text, tool, permission, usage and execution
//! events into [`HarnessEvent`]. Unknown event types and fields are ignored:
//! the v2 surface is marked experimental, and the adapter must survive an
//! added field rather than fail the turn (ADR-005, ADR-023).

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use h4rn_protocol::{
    AgentState, ApprovalKind, ApprovalOption, ToolCall, ToolCallStatus, TurnStatus, Usage,
};
use serde_json::Value;

use super::limit;
use crate::{HarnessEvent, TurnReport};

const OUTPUT_PREVIEW_CHARS: usize = 2_000;
const SUMMARY_CHARS: usize = 140;
const DIAGNOSTIC_CHARS: usize = 320;

/// The normaliser for one OpenCode server event stream.
#[derive(Debug)]
pub struct OpenCodeParser {
    session_id: Option<String>,
    model: Option<String>,
    started: bool,
    state: AgentState,
    tool_calls: HashMap<String, ToolMeta>,
    text_deltas: HashSet<String>,
    touched: Vec<String>,
    last_turn: Option<TurnReport>,
    turn_usage: Usage,
    turn_steps: u64,
    turn_started_ms: Option<f64>,
    turn_finish: Option<String>,
    turn_done: bool,
    limit_seen: bool,
}

#[derive(Clone, Debug)]
struct ToolMeta {
    name: String,
    summary: String,
    input: Option<Value>,
    started_at: DateTime<Utc>,
}

impl Default for OpenCodeParser {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenCodeParser {
    /// Creates a parser with no session state.
    pub fn new() -> Self {
        Self {
            session_id: None,
            model: None,
            started: false,
            state: AgentState::Idle,
            tool_calls: HashMap::new(),
            text_deltas: HashSet::new(),
            touched: Vec::new(),
            last_turn: None,
            turn_usage: Usage::default(),
            turn_steps: 0,
            turn_started_ms: None,
            turn_finish: None,
            turn_done: false,
            limit_seen: false,
        }
    }

    /// Feeds one SSE line or bare JSON value and returns the events it produced.
    pub fn push_line(&mut self, line: &str) -> Vec<HarnessEvent> {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with(':') {
            return Vec::new();
        }
        let payload = line
            .strip_prefix("data:")
            .map(str::trim_start)
            .unwrap_or(line);
        if payload.is_empty() || payload == "[DONE]" {
            return Vec::new();
        }
        let value: Value = match serde_json::from_str(payload) {
            Ok(value) => value,
            Err(error) => {
                return vec![HarnessEvent::Error {
                    message: format!(
                        "unparseable opencode event ({error}): {}",
                        truncate_chars(payload, DIAGNOSTIC_CHARS)
                    ),
                    retryable: false,
                }];
            }
        };
        self.push_value(&value)
    }

    /// Feeds one already-parsed event and returns the events it produced.
    pub fn push_value(&mut self, value: &Value) -> Vec<HarnessEvent> {
        let Some(event_type) = value["type"].as_str().filter(|value| !value.is_empty()) else {
            return vec![HarnessEvent::Error {
                message: format!(
                    "unparseable opencode event: {}",
                    truncate_chars(&value.to_string(), DIAGNOSTIC_CHARS)
                ),
                retryable: false,
            }];
        };
        let data = &value["data"];
        let created = created_ms(value);
        let mut events = Vec::new();

        if let Some(session_id) = data["sessionID"].as_str() {
            if let Some(event) = self.ensure_session(session_id) {
                events.push(event);
            }
        }
        events.extend(self.on_event(event_type, data, created));
        events
    }

    /// The OpenCode session id, once an event reported it.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// The model reported by the event stream (`provider/model`).
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The current agent-state hint.
    pub fn agent_state(&self) -> AgentState {
        self.state
    }

    /// Paths touched by mutating file tools so far.
    pub fn touched_paths(&self) -> &[String] {
        &self.touched
    }

    /// The most recent completed turn.
    pub fn last_turn(&self) -> Option<&TurnReport> {
        self.last_turn.as_ref()
    }

    /// Adopts a session id learned from the create/resume HTTP response.
    ///
    /// Returns `true` when the caller must publish a
    /// [`HarnessEvent::SessionStarted`]; it returns `false` when the event
    /// stream already announced the session, so the event is never doubled.
    pub fn adopt_session(&mut self, session_id: &str) -> bool {
        self.session_id = Some(session_id.to_string());
        if self.started {
            return false;
        }
        self.started = true;
        self.state = AgentState::Idle;
        true
    }

    /// Marks the session as working before the first stream event arrives.
    pub fn note_thinking(&mut self) {
        if !matches!(
            self.state,
            AgentState::AwaitingApproval | AgentState::RateLimited
        ) {
            self.state = AgentState::Thinking;
        }
    }

    fn ensure_session(&mut self, session_id: &str) -> Option<HarnessEvent> {
        if self.adopt_session(session_id) {
            Some(HarnessEvent::SessionStarted {
                session_id: session_id.to_string(),
            })
        } else {
            None
        }
    }

    fn on_event(
        &mut self,
        event_type: &str,
        data: &Value,
        created: Option<f64>,
    ) -> Vec<HarnessEvent> {
        match event_type {
            "server.connected" | "session.created" => Vec::new(),
            "session.model.selected" | "session.agent.selected" => {
                if let Some(model) = data.get("model") {
                    self.set_model(model);
                }
                Vec::new()
            }
            "session.inbox.enqueued" => {
                self.reset_turn();
                self.state = AgentState::Queued;
                Vec::new()
            }
            "session.inbox.delivered" => {
                self.state = AgentState::Thinking;
                Vec::new()
            }
            "session.execution.started" => {
                self.reset_turn();
                self.turn_started_ms = created;
                self.state = AgentState::Thinking;
                Vec::new()
            }
            "session.step.started" => {
                self.turn_steps = self.turn_steps.saturating_add(1);
                self.turn_started_ms.get_or_insert(created.unwrap_or(0.0));
                if let Some(model) = data.get("model") {
                    self.set_model(model);
                }
                self.state = AgentState::Thinking;
                Vec::new()
            }
            "session.step.streamed" => {
                self.state = AgentState::Thinking;
                Vec::new()
            }
            "session.step.ended" => self.on_step_ended(data, created),
            "session.step.failed" => self.on_execution_failed(data, created),
            "session.text.started" => {
                self.state = AgentState::Thinking;
                Vec::new()
            }
            "session.text.delta" => self.on_text_delta(data),
            "session.text.ended" => self.on_text_ended(data),
            "session.reasoning.started"
            | "session.reasoning.delta"
            | "session.reasoning.ended"
            | "session.compaction.started"
            | "session.compaction.delta"
            | "session.compaction.ended" => {
                self.state = AgentState::Thinking;
                Vec::new()
            }
            "session.compaction.failed" => self.on_execution_failed(data, created),
            "session.tool.input.started" => self.on_tool_input_started(data),
            "session.tool.input.ended" => self.on_tool_input_ended(data),
            "session.tool.called" => self.on_tool_called(data),
            "session.tool.progress" => {
                self.state = AgentState::RunningTool;
                Vec::new()
            }
            "session.tool.success" => self.on_tool_finished(data, false),
            "session.tool.failed" => self.on_tool_finished(data, true),
            "permission.asked" => self.on_permission_asked(data),
            "permission.replied" => {
                self.state = AgentState::Thinking;
                Vec::new()
            }
            "session.usage.updated" => {
                self.note_usage(data);
                Vec::new()
            }
            "session.execution.succeeded" => self.on_execution_succeeded(data, created),
            "session.execution.failed" => self.on_execution_failed(data, created),
            "session.execution.interrupted" => self.on_execution_interrupted(data, created),
            "session.retry.scheduled" => self.on_retry_scheduled(data),
            "session.status" => self.on_status(data),
            "session.idle" => {
                self.state = AgentState::Idle;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn on_text_delta(&mut self, data: &Value) -> Vec<HarnessEvent> {
        let Some(delta) = data["delta"].as_str() else {
            return Vec::new();
        };
        if let Some(message_id) = data["assistantMessageID"].as_str() {
            self.text_deltas.insert(message_id.to_string());
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

    fn on_text_ended(&mut self, data: &Value) -> Vec<HarnessEvent> {
        self.state = AgentState::Thinking;
        let message_id = data["assistantMessageID"].as_str().unwrap_or_default();
        if self.text_deltas.remove(message_id) {
            return Vec::new();
        }
        let text = data["text"].as_str().unwrap_or_default();
        if text.is_empty() {
            Vec::new()
        } else {
            vec![HarnessEvent::TextDelta {
                text: text.to_string(),
            }]
        }
    }

    fn on_tool_input_started(&mut self, data: &Value) -> Vec<HarnessEvent> {
        let Some(id) = data["id"].as_str() else {
            return Vec::new();
        };
        let name = data["name"].as_str().unwrap_or("tool").to_string();
        self.state = state_for_tool(&name);
        self.tool_calls.insert(
            id.to_string(),
            ToolMeta {
                summary: String::new(),
                name,
                input: None,
                started_at: Utc::now(),
            },
        );
        Vec::new()
    }

    fn on_tool_input_ended(&mut self, data: &Value) -> Vec<HarnessEvent> {
        let Some(id) = data["id"].as_str() else {
            return Vec::new();
        };
        let input = data["text"]
            .as_str()
            .and_then(|text| serde_json::from_str::<Value>(text).ok());
        if let Some(meta) = self.tool_calls.get_mut(id) {
            if let Some(input) = input {
                meta.summary = tool_summary(&meta.name, &input);
                meta.input = Some(input);
            }
        }
        Vec::new()
    }

    fn on_tool_called(&mut self, data: &Value) -> Vec<HarnessEvent> {
        let Some(id) = data["id"].as_str() else {
            return Vec::new();
        };
        let meta = self
            .tool_calls
            .entry(id.to_string())
            .or_insert_with(|| ToolMeta {
                name: "tool".into(),
                summary: String::new(),
                input: None,
                started_at: Utc::now(),
            });
        let input = Some(data["input"].clone()).filter(|value| !value.is_null());
        if let Some(input) = &input {
            meta.summary = tool_summary(&meta.name, input);
            meta.input = Some(input.clone());
        }
        let name = meta.name.clone();
        let summary = meta.summary.clone();
        let input = meta.input.clone();
        let started_at = meta.started_at;
        self.state = state_for_tool(&name);
        self.record_touched(&name, input.as_ref());

        vec![HarnessEvent::ToolCall {
            tool_call: complete_tool_call(
                id.to_string(),
                name,
                ToolCallStatus::Running,
                summary,
                input,
                None,
                None,
                started_at,
            ),
        }]
    }

    fn on_tool_finished(&mut self, data: &Value, failed: bool) -> Vec<HarnessEvent> {
        let Some(id) = data["id"].as_str() else {
            return Vec::new();
        };
        let meta = self.tool_calls.remove(id).unwrap_or_else(|| ToolMeta {
            name: "tool".into(),
            summary: String::new(),
            input: None,
            started_at: Utc::now(),
        });
        let output = if failed {
            data["error"]["message"]
                .as_str()
                .map(str::to_string)
                .or_else(|| {
                    data["content"]
                        .as_array()
                        .map(|content| content_text(content))
                        .filter(|text| !text.is_empty())
                })
        } else {
            data["content"]
                .as_array()
                .map(|content| content_text(content))
                .filter(|text| !text.is_empty())
                .or_else(|| data["metadata"]["output"].as_str().map(str::to_string))
        };
        let status = if failed {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        };
        self.state = if self.state == AgentState::AwaitingApproval {
            AgentState::Thinking
        } else {
            state_for_tool(&meta.name)
        };

        // Count from the whole result, not the truncated preview; OpenCode's
        // `metadata` carries `filediff` and `diff` for edits.
        let metadata = data.get("metadata").filter(|metadata| !metadata.is_null());
        let mut call = complete_tool_call(
            id.to_string(),
            meta.name,
            status,
            meta.summary,
            meta.input,
            output,
            metadata,
            meta.started_at,
        );
        call.output = call
            .output
            .map(|output| truncate_chars(output.trim(), OUTPUT_PREVIEW_CHARS));
        vec![HarnessEvent::ToolCall { tool_call: call }]
    }

    fn on_permission_asked(&mut self, data: &Value) -> Vec<HarnessEvent> {
        let Some(id) = data["id"].as_str() else {
            return Vec::new();
        };
        let action = data["action"].as_str().unwrap_or("permission");
        let resources = data["resources"]
            .as_array()
            .map(|resources| {
                resources
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let prompt = data["message"]
            .as_str()
            .filter(|message| !message.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| permission_prompt(action, &resources));
        self.state = AgentState::AwaitingApproval;

        vec![HarnessEvent::ApprovalRequested {
            approval_id: id.to_string(),
            kind: permission_kind(action),
            prompt,
            options: vec![
                ApprovalOption {
                    id: "once".into(),
                    label: "Allow once".into(),
                },
                ApprovalOption {
                    id: "always".into(),
                    label: "Always allow".into(),
                },
                ApprovalOption {
                    id: "reject".into(),
                    label: "Deny".into(),
                },
            ],
        }]
    }

    fn on_step_ended(&mut self, data: &Value, created: Option<f64>) -> Vec<HarnessEvent> {
        let usage = step_usage(data, self.duration_ms(created));
        self.merge_turn_usage(&usage);
        if let Some(finish) = data["finish"].as_str() {
            self.turn_finish = Some(finish.to_string());
        }
        self.state = AgentState::Thinking;
        if usage.total_tokens() == 0 && usage.cost_micros == 0 {
            Vec::new()
        } else {
            vec![HarnessEvent::Usage { usage }]
        }
    }

    fn note_usage(&mut self, data: &Value) {
        // `session.usage.updated` is a cumulative session snapshot. The turn
        // total comes from `session.step.ended`; this snapshot is a fallback
        // for servers that omit step accounting.
        if self.turn_usage.total_tokens() == 0 {
            self.turn_usage = session_usage(data, self.turn_usage.duration_ms);
        }
    }

    fn on_execution_succeeded(&mut self, _data: &Value, created: Option<f64>) -> Vec<HarnessEvent> {
        if self.turn_done {
            return Vec::new();
        }
        self.turn_done = true;
        self.state = AgentState::Done;
        let usage = self.finish_turn(TurnStatus::Completed, created);
        vec![HarnessEvent::TurnCompleted {
            status: TurnStatus::Completed,
            usage,
        }]
    }

    fn on_execution_failed(&mut self, data: &Value, created: Option<f64>) -> Vec<HarnessEvent> {
        if self.turn_done {
            return Vec::new();
        }
        self.turn_done = true;
        let error = &data["error"];
        let now = Utc::now();

        if let Some(signal) = limit::detect_limit(error, now) {
            self.state = AgentState::RateLimited;
            let usage = self.finish_turn(TurnStatus::Failed, created);
            // A limit is usually announced twice: once by
            // `session.retry.scheduled` and again when the execution finally
            // fails. Pause the thread once, and let the failure close the turn.
            if self.limit_seen {
                return vec![HarnessEvent::TurnCompleted {
                    status: TurnStatus::Failed,
                    usage,
                }];
            }
            self.limit_seen = true;
            return vec![
                HarnessEvent::LimitHit {
                    reset_at: signal.reset_at,
                    auto_resume: signal.reset_at.is_some(),
                },
                HarnessEvent::TurnCompleted {
                    status: TurnStatus::Failed,
                    usage,
                },
            ];
        }

        self.state = AgentState::Error;
        let message = error["message"]
            .as_str()
            .or_else(|| error.as_str())
            .unwrap_or("opencode reported an error");
        let status = error["status"].as_u64();
        let retryable = status
            .map(|status| status >= 500 || status == 408 || status == 429)
            .unwrap_or(true);
        let usage = self.finish_turn(TurnStatus::Failed, created);
        vec![
            HarnessEvent::Error {
                message: truncate_chars(message, OUTPUT_PREVIEW_CHARS),
                retryable,
            },
            HarnessEvent::TurnCompleted {
                status: TurnStatus::Failed,
                usage,
            },
        ]
    }

    fn on_execution_interrupted(
        &mut self,
        _data: &Value,
        created: Option<f64>,
    ) -> Vec<HarnessEvent> {
        if self.turn_done {
            return Vec::new();
        }
        self.turn_done = true;
        self.state = AgentState::Idle;
        let usage = self.finish_turn(TurnStatus::Interrupted, created);
        vec![HarnessEvent::TurnCompleted {
            status: TurnStatus::Interrupted,
            usage,
        }]
    }

    fn on_retry_scheduled(&mut self, data: &Value) -> Vec<HarnessEvent> {
        let error = &data["error"];
        let now = Utc::now();
        let reset_at = limit::event_clock(&data["at"])
            .or_else(|| limit::retry_after_reset(error, now))
            .or_else(|| limit::detect_limit(error, now).and_then(|signal| signal.reset_at));

        if let Some(signal) = limit::detect_limit(error, now) {
            if self.limit_seen {
                return Vec::new();
            }
            self.limit_seen = true;
            self.state = AgentState::RateLimited;
            return vec![HarnessEvent::LimitHit {
                reset_at: reset_at.or(signal.reset_at),
                auto_resume: true,
            }];
        }

        self.state = AgentState::Scheduled;
        Vec::new()
    }

    fn on_status(&mut self, data: &Value) -> Vec<HarnessEvent> {
        let status = &data["status"];
        match status["type"].as_str().unwrap_or_default() {
            "busy" => {
                self.state = AgentState::Thinking;
                Vec::new()
            }
            "idle" => {
                self.state = AgentState::Idle;
                Vec::new()
            }
            "retry" => {
                let message = status["message"].as_str().unwrap_or_default();
                let now = Utc::now();
                let text = format!(
                    "{message} {}",
                    status["action"]["message"].as_str().unwrap_or_default()
                );
                if let Some(signal) = limit::detect_limit(&Value::String(text), now) {
                    if self.limit_seen {
                        return Vec::new();
                    }
                    self.limit_seen = true;
                    self.state = AgentState::RateLimited;
                    let reset_at = limit::event_clock(&status["next"]).or(signal.reset_at);
                    return vec![HarnessEvent::LimitHit {
                        reset_at,
                        auto_resume: true,
                    }];
                }
                self.state = AgentState::Scheduled;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn set_model(&mut self, model: &Value) {
        let Some(id) = model["id"].as_str().filter(|id| !id.is_empty()) else {
            return;
        };
        self.model = Some(match model["providerID"].as_str() {
            Some(provider) if !provider.is_empty() && !id.starts_with(&format!("{provider}/")) => {
                format!("{provider}/{id}")
            }
            _ => id.to_string(),
        });
    }

    fn record_touched(&mut self, name: &str, input: Option<&Value>) {
        if !is_mutating_tool(name) {
            return;
        }
        let Some(input) = input else {
            return;
        };
        for key in ["filePath", "file_path", "path"] {
            if let Some(path) = input[key].as_str().filter(|path| !path.is_empty()) {
                if !self.touched.iter().any(|known| known == path) {
                    self.touched.push(path.to_string());
                }
            }
        }
    }

    fn reset_turn(&mut self) {
        self.turn_usage = Usage::default();
        self.turn_steps = 0;
        self.turn_started_ms = None;
        self.turn_finish = None;
        self.turn_done = false;
        self.limit_seen = false;
        self.text_deltas.clear();
        self.state = AgentState::Thinking;
    }

    fn duration_ms(&self, created: Option<f64>) -> u64 {
        let (Some(start), Some(end)) = (self.turn_started_ms, created) else {
            return 0;
        };
        (end - start).max(0.0) as u64
    }

    fn merge_turn_usage(&mut self, usage: &Usage) {
        self.turn_usage.input_tokens = self
            .turn_usage
            .input_tokens
            .saturating_add(usage.input_tokens);
        self.turn_usage.output_tokens = self
            .turn_usage
            .output_tokens
            .saturating_add(usage.output_tokens);
        self.turn_usage.cached_input_tokens = self
            .turn_usage
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        self.turn_usage.cost_micros = self
            .turn_usage
            .cost_micros
            .saturating_add(usage.cost_micros);
        self.turn_usage.duration_ms = usage.duration_ms;
    }

    fn finish_turn(&mut self, status: TurnStatus, created: Option<f64>) -> Usage {
        let duration_ms = self.duration_ms(created).max(self.turn_usage.duration_ms);
        self.turn_usage.duration_ms = duration_ms;
        let usage = self.turn_usage.clone();
        let subtype = match status {
            TurnStatus::Completed => "completed",
            TurnStatus::Failed => "failed",
            TurnStatus::Interrupted => "interrupted",
        };
        self.last_turn = Some(TurnReport {
            num_turns: self.turn_steps.max(1),
            duration_ms,
            duration_api_ms: duration_ms,
            cost_micros: usage.cost_micros,
            is_error: matches!(status, TurnStatus::Failed),
            stop_reason: self.turn_finish.clone(),
            subtype: Some(subtype.to_string()),
        });
        usage
    }
}

/// Maps an OpenCode tool name to the agent state the UI renders.
///
/// OpenCode's built-in names are lowercase (`shell`, `read`, `edit`, `grep`,
/// `glob`, `webfetch`); MCP tools are named `mcp__server__tool`. The shared
/// Claude mapping already handles those shapes.
pub fn state_for_tool(name: &str) -> AgentState {
    let lower = name.to_ascii_lowercase();
    if lower == "shell" || lower == "bash" || lower == "task" {
        return AgentState::RunningTool;
    }
    crate::state_for_tool(name)
}

fn permission_kind(action: &str) -> ApprovalKind {
    match action.to_ascii_lowercase().as_str() {
        "shell" | "bash" | "command" => ApprovalKind::Command,
        "edit" | "write" | "patch" | "apply_patch" | "multiedit" => ApprovalKind::FileChange,
        _ => ApprovalKind::Permission,
    }
}

fn permission_prompt(action: &str, resources: &str) -> String {
    let resources = if resources.is_empty() {
        "unknown resource"
    } else {
        resources
    };
    match action {
        "shell" => format!("Run command: {resources}"),
        "edit" | "write" | "patch" | "apply_patch" => format!("Edit {resources}"),
        other => format!("{other}: {resources}"),
    }
}

fn is_mutating_tool(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("edit")
        || lower.contains("write")
        || lower.contains("patch")
        || lower == "applypatch"
}

fn tool_summary(name: &str, input: &Value) -> String {
    let summary = match name.to_ascii_lowercase().as_str() {
        "shell" | "bash" => input["command"].as_str().unwrap_or_default().to_string(),
        "edit" | "write" | "patch" | "apply_patch" | "multiedit" | "read" => input["filePath"]
            .as_str()
            .or_else(|| input["file_path"].as_str())
            .or_else(|| input["path"].as_str())
            .unwrap_or_default()
            .to_string(),
        "grep" | "glob" => input["pattern"]
            .as_str()
            .or_else(|| input["path"].as_str())
            .unwrap_or_default()
            .to_string(),
        _ => value_preview(input),
    };
    let summary = if summary.trim().is_empty() {
        name.to_string()
    } else {
        summary
    };
    truncate_chars(summary.trim(), SUMMARY_CHARS)
}

fn value_preview(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn content_text(content: &[Value]) -> String {
    content
        .iter()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[allow(clippy::too_many_arguments)]
fn complete_tool_call(
    tool_call_id: String,
    name: String,
    status: ToolCallStatus,
    summary: String,
    input: Option<Value>,
    output: Option<String>,
    structured: Option<&Value>,
    started_at: DateTime<Utc>,
) -> ToolCall {
    let stats = (status == ToolCallStatus::Completed)
        .then(|| crate::stats::derive(&name, input.as_ref(), output.as_deref(), structured))
        .flatten();
    let completed_at = matches!(
        status,
        ToolCallStatus::Completed | ToolCallStatus::Failed | ToolCallStatus::Denied
    )
    .then(Utc::now);
    ToolCall {
        tool_call_id,
        name,
        status,
        summary,
        input,
        output,
        started_at,
        completed_at,
        stats,
    }
}

fn step_usage(data: &Value, duration_ms: u64) -> Usage {
    let tokens = &data["tokens"];
    Usage {
        input_tokens: u64_field(tokens, "input"),
        output_tokens: u64_field(tokens, "output"),
        cached_input_tokens: u64_field(&tokens["cache"], "read"),
        cost_micros: cost_micros(&data["cost"]),
        duration_ms,
    }
}

fn session_usage(data: &Value, duration_ms: u64) -> Usage {
    let tokens = &data["tokens"];
    Usage {
        input_tokens: u64_field(tokens, "input"),
        output_tokens: u64_field(tokens, "output"),
        cached_input_tokens: u64_field(&tokens["cache"], "read"),
        cost_micros: cost_micros(&data["cost"]),
        duration_ms,
    }
}

fn u64_field(value: &Value, key: &str) -> u64 {
    value[key]
        .as_u64()
        .or_else(|| value[key].as_f64().map(|value| value.max(0.0) as u64))
        .unwrap_or(0)
}

fn cost_micros(value: &Value) -> u64 {
    let usd = value
        .as_f64()
        .filter(|value| value.is_finite())
        .unwrap_or(0.0);
    if usd <= 0.0 {
        return 0;
    }
    (usd * 1_000_000.0).round() as u64
}

fn created_ms(value: &Value) -> Option<f64> {
    value["created"]
        .as_f64()
        .or_else(|| value["created"].as_i64().map(|value| value as f64))
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

    fn event(event_type: &str, data: Value) -> Vec<HarnessEvent> {
        let mut parser = OpenCodeParser::new();
        parser.push_value(&serde_json::json!({
            "id": "evt_test",
            "created": 1_000,
            "type": event_type,
            "data": data,
        }))
    }

    #[test]
    fn tool_names_map_to_states() {
        assert_eq!(state_for_tool("shell"), AgentState::RunningTool);
        assert_eq!(state_for_tool("read"), AgentState::Reading);
        assert_eq!(state_for_tool("grep"), AgentState::Searching);
        assert_eq!(state_for_tool("edit"), AgentState::Editing);
    }

    #[test]
    fn permission_actions_map_to_approval_kinds() {
        assert_eq!(permission_kind("shell"), ApprovalKind::Command);
        assert_eq!(permission_kind("edit"), ApprovalKind::FileChange);
        assert_eq!(permission_kind("webfetch"), ApprovalKind::Permission);
    }

    #[test]
    fn cost_is_converted_to_micros() {
        assert_eq!(cost_micros(&serde_json::json!(0.0123)), 12_300);
        assert_eq!(cost_micros(&serde_json::json!(0)), 0);
        assert_eq!(cost_micros(&serde_json::json!(null)), 0);
    }

    #[test]
    fn a_mutating_tool_reports_its_path() {
        let events = {
            let mut parser = OpenCodeParser::new();
            let mut events = parser.push_value(&serde_json::json!({
                "type": "session.tool.input.started",
                "data": { "sessionID": "ses_1", "assistantMessageID": "msg_1",
                          "id": "call_1", "name": "edit" }
            }));
            events.extend(parser.push_value(&serde_json::json!({
                "type": "session.tool.called",
                "data": { "sessionID": "ses_1", "assistantMessageID": "msg_1",
                          "id": "call_1", "input": { "filePath": "/workspace/a.rs" },
                          "executed": false }
            })));
            assert_eq!(parser.touched_paths(), ["/workspace/a.rs"]);
            events
        };
        assert!(matches!(
            events.last(),
            Some(HarnessEvent::ToolCall { tool_call })
                if tool_call.name == "edit"
                    && tool_call.summary == "/workspace/a.rs"
                    && tool_call.status == ToolCallStatus::Running
        ));
    }

    #[test]
    fn unknown_events_are_ignored() {
        assert!(
            event("something.new", serde_json::json!({ "sessionID": "ses_1" }))
                .iter()
                .all(|event| matches!(event, HarnessEvent::SessionStarted { .. }))
        );
    }
}
