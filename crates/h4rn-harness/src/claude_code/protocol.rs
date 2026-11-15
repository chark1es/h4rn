//! The Claude Code stream-json normaliser.
//!
//! Claude Code's `--output-format stream-json` emits newline-delimited JSON.
//! The frames this adapter understands are:
//!
//! - `system` `init` — session id, model and tool inventory.
//! - `stream_event` — partial message events, used for text deltas.
//! - `assistant` — a completed assistant message with text and tool calls.
//! - `user` — tool results echoed by the harness.
//! - `result` — the turn's usage, cost, duration and error status.
//! - `rate_limit_event` — the provider's window utilisation.
//!
//! Everything else is ignored, except that a line which is not JSON at all
//! becomes a diagnostic [`HarnessEvent::Error`]; a malformed vendor frame must
//! never panic the session (ADR-023).

use std::collections::HashMap;

use chrono::{DateTime, TimeZone, Utc};
use h4rn_protocol::{AgentState, ToolCall, ToolCallStatus, TurnStatus, Usage};
use serde::Deserialize;
use serde_json::Value;

use super::limit;
use crate::HarnessEvent;

const OUTPUT_PREVIEW_CHARS: usize = 2_000;
const SUMMARY_CHARS: usize = 140;
const DIAGNOSTIC_CHARS: usize = 320;

/// Normalised summary of one completed Claude Code turn.
///
/// [`HarnessEvent::TurnCompleted`] carries the status and [`Usage`]; this
/// struct also keeps the fields that have no wire type yet (`num_turns`,
/// `stop_reason`). The daemon can read it from
/// [`super::ClaudeCodeSession::last_turn`] for metering (ADR-024).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnReport {
    /// Turns Claude Code counted in this response, including tool turns.
    pub num_turns: u64,
    /// Wall-clock duration reported by the CLI.
    pub duration_ms: u64,
    /// Time spent in API calls.
    pub duration_api_ms: u64,
    /// Cost in millionths of a US dollar.
    pub cost_micros: u64,
    /// Whether the CLI marked the result as an error.
    pub is_error: bool,
    /// The provider's stop reason, when reported.
    pub stop_reason: Option<String>,
    /// The result subtype (`success`, `error_during_execution`, ...).
    pub subtype: Option<String>,
}

/// Maps a Claude Code tool name to the agent state the UI renders.
///
/// MCP tools are named `mcp__server__tool`; the last segment identifies the
/// behaviour. Unknown tools are [`AgentState::RunningTool`]: they are still
/// doing work, and guessing a more specific state would be worse than the
/// generic one.
pub fn state_for_tool(name: &str) -> AgentState {
    let short = short_tool_name(name);
    match short.as_str() {
        "grep" | "glob" | "websearch" | "web_search" | "search" => AgentState::Searching,
        "read" | "notebookread" | "webfetch" | "web_fetch" => AgentState::Reading,
        "edit" | "write" | "multiedit" | "notebookedit" | "applypatch" | "str_replace_editor" => {
            AgentState::Editing
        }
        "askuserquestion" | "ask_user_question" | "exitplanmode" | "exit_plan_mode" => {
            AgentState::AwaitingApproval
        }
        _ => AgentState::RunningTool,
    }
}

/// File paths a tool call touches, for the diff viewer (ADR-013, ADR-022).
///
/// Only the mutating file tools are reported (`Edit`, `Write`, `MultiEdit`,
/// `NotebookEdit`); read-only tools return an empty list. The raw `input`
/// remains on [`ToolCall`] as well, so a projection can re-derive paths if a
/// future Claude Code version renames the field.
pub fn tool_paths(name: &str, input: Option<&Value>) -> Vec<String> {
    let short = short_tool_name(name);
    if !matches!(
        short.as_str(),
        "edit" | "write" | "multiedit" | "notebookedit" | "applypatch" | "str_replace_editor"
    ) {
        return Vec::new();
    }

    let Some(input) = input else {
        return Vec::new();
    };

    let mut paths = Vec::new();
    for key in ["file_path", "path", "notebook_path"] {
        if let Some(path) = input.get(key).and_then(Value::as_str)
            && !path.is_empty()
        {
            paths.push(path.to_string());
        }
    }
    paths
}

/// One-line collapsed summary of a tool call.
pub fn tool_summary(name: &str, input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    let short = short_tool_name(name);

    let candidate = match short.as_str() {
        "bash" | "shell" | "powershell" => input
            .get("description")
            .or_else(|| input.get("command"))
            .and_then(Value::as_str),
        "read" | "write" | "edit" | "multiedit" | "notebookedit" => input
            .get("file_path")
            .or_else(|| input.get("notebook_path"))
            .and_then(Value::as_str),
        "grep" | "glob" => input.get("pattern").and_then(Value::as_str),
        "websearch" | "web_search" => input.get("query").and_then(Value::as_str),
        "webfetch" | "web_fetch" => input.get("url").and_then(Value::as_str),
        "task" | "agent" => input.get("description").and_then(Value::as_str),
        _ => None,
    };

    match candidate {
        Some(text) => truncate_chars(text.trim(), SUMMARY_CHARS),
        None => {
            let json = serde_json::to_string(input).unwrap_or_default();
            if json == "{}" || json == "null" {
                String::new()
            } else {
                truncate_chars(&json, SUMMARY_CHARS)
            }
        }
    }
}

/// The stream-json parser.
///
/// One parser belongs to one long-lived `claude` process. Feed it each stdout
/// line with [`push_line`](Self::push_line); it returns zero or more
/// normalised events in order. State that has no wire event yet (agent state,
/// touched paths, the last [`TurnReport`]) is available through the getters.
#[derive(Debug)]
pub struct ClaudeStreamParser {
    session_id: Option<String>,
    model: Option<String>,
    tools: Vec<String>,
    started: bool,
    partial_text: bool,
    state: AgentState,
    tool_calls: HashMap<String, ToolMeta>,
    denied_tools: HashMap<String, ()>,
    touched: Vec<String>,
    last_turn: Option<TurnReport>,
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

impl Default for ClaudeStreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeStreamParser {
    /// Creates a parser with no session state.
    pub fn new() -> Self {
        Self {
            session_id: None,
            model: None,
            tools: Vec::new(),
            started: false,
            partial_text: false,
            state: AgentState::Idle,
            tool_calls: HashMap::new(),
            denied_tools: HashMap::new(),
            touched: Vec::new(),
            last_turn: None,
            limit_seen: false,
            error_seen: false,
        }
    }

    /// Feeds one stdout line and returns the events it produced.
    ///
    /// Blank lines produce nothing. A line that is not JSON produces a
    /// diagnostic [`HarnessEvent::Error`] with `retryable: false`; the parser
    /// stays usable so the rest of the stream is still normalised.
    pub fn push_line(&mut self, line: &str) -> Vec<HarnessEvent> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }

        let frame: Frame = match serde_json::from_str(line) {
            Ok(frame) => frame,
            Err(error) => {
                return vec![HarnessEvent::Error {
                    message: format!(
                        "unparseable claude output ({error}): {}",
                        truncate_chars(line, DIAGNOSTIC_CHARS)
                    ),
                    retryable: false,
                }];
            }
        };

        match frame {
            Frame::System(frame) => self.on_system(frame),
            Frame::StreamEvent(frame) => self.on_stream_event(frame),
            Frame::Assistant(frame) => self.on_assistant(frame),
            Frame::User(frame) => self.on_user(frame),
            Frame::Result(frame) => self.on_result(frame),
            Frame::RateLimit(frame) => self.on_rate_limit(frame),
            Frame::ControlResponse | Frame::Other => Vec::new(),
        }
    }

    /// The harness-native session id, once the init frame arrived.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// The model reported by the init frame.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The tool inventory reported by the init frame.
    pub fn tools(&self) -> &[String] {
        &self.tools
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

    fn on_system(&mut self, frame: SystemFrame) -> Vec<HarnessEvent> {
        if let Some(session_id) = frame.session_id {
            self.session_id = Some(session_id);
        }
        if let Some(model) = frame.model {
            self.model = Some(model);
        }
        if !frame.tools.is_empty() {
            self.tools = frame.tools;
        }

        let mut events = Vec::new();
        match frame.subtype.as_str() {
            "init" => {
                if !self.started
                    && let Some(session_id) = self.session_id.clone()
                {
                    self.started = true;
                    self.state = AgentState::Idle;
                    events.push(HarnessEvent::SessionStarted { session_id });
                }
            }
            "status" => {
                if frame.status.as_deref() == Some("requesting") {
                    self.state = AgentState::Thinking;
                }
            }
            "thinking_tokens" => {
                self.state = AgentState::Thinking;
            }
            "permission_denied" => {
                let tool_name = frame.tool_name.unwrap_or_else(|| "tool".into());
                if let Some(tool_use_id) = frame.tool_use_id {
                    self.denied_tools.insert(tool_use_id, ());
                }
                let detail = frame.message.unwrap_or_default();
                events.push(HarnessEvent::Error {
                    message: format!("claude did not have permission to use {tool_name}: {detail}"),
                    retryable: false,
                });
            }
            _ => {}
        }
        events
    }

    fn on_stream_event(&mut self, frame: StreamEventFrame) -> Vec<HarnessEvent> {
        // Subagent output is forwarded only with `--forward-subagent-text`; if
        // it ever appears, keep it out of the parent's text item.
        if frame.parent_tool_use_id.is_some() {
            return Vec::new();
        }

        let mut events = Vec::new();
        match frame.event.kind.as_str() {
            "message_start" => {
                self.partial_text = false;
                self.state = AgentState::Thinking;
            }
            "content_block_start" => {
                if let Some(block) = frame.event.content_block
                    && block.get("type").and_then(Value::as_str) == Some("tool_use")
                    && let Some(name) = block.get("name").and_then(Value::as_str)
                {
                    self.state = state_for_tool(name);
                }
            }
            "content_block_delta" => {
                if let Some(delta) = frame.event.delta {
                    match delta.kind.as_deref() {
                        Some("text_delta") => {
                            if let Some(text) = delta.text
                                && !text.is_empty()
                            {
                                self.partial_text = true;
                                self.state = AgentState::Thinking;
                                events.push(HarnessEvent::TextDelta { text });
                            }
                        }
                        Some("thinking_delta") => self.state = AgentState::Thinking,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        events
    }

    fn on_assistant(&mut self, frame: AssistantFrame) -> Vec<HarnessEvent> {
        if frame.parent_tool_use_id.is_some() {
            return Vec::new();
        }

        let synthetic = frame.message.model.as_deref() == Some("<synthetic>");
        let blocks = frame.message.content;
        let mut events = Vec::new();

        if synthetic {
            // A synthetic message is the CLI reporting a transport or quota
            // error, not model output. Never surface it as assistant text.
            let text = blocks
                .iter()
                .filter_map(block_text)
                .collect::<Vec<_>>()
                .join("\n");
            let text = text.trim();
            if text.is_empty() {
                return events;
            }
            if let Some(signal) = limit::detect_limit(text, Utc::now()) {
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
                    message: text.to_string(),
                    retryable: true,
                });
            }
            return events;
        }

        for block in &blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    // With `--include-partial-messages` the text was already
                    // delivered as deltas; re-emitting it would duplicate the
                    // item in the thread.
                    if self.partial_text {
                        continue;
                    }
                    if let Some(text) = block.get("text").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        events.push(HarnessEvent::TextDelta {
                            text: text.to_string(),
                        });
                    }
                }
                Some("tool_use") => {
                    let Some(tool_use_id) = block.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string();
                    let input = block.get("input").cloned();
                    let summary = tool_summary(&name, input.as_ref());
                    let started_at = Utc::now();

                    for path in tool_paths(&name, input.as_ref()) {
                        if !self.touched.contains(&path) {
                            self.touched.push(path);
                        }
                    }
                    self.state = state_for_tool(&name);
                    self.tool_calls.insert(
                        tool_use_id.to_string(),
                        ToolMeta {
                            name: name.clone(),
                            summary: summary.clone(),
                            input: input.clone(),
                            started_at,
                        },
                    );
                    events.push(HarnessEvent::ToolCall {
                        tool_call: ToolCall {
                            tool_call_id: tool_use_id.to_string(),
                            name,
                            status: ToolCallStatus::Running,
                            summary,
                            input,
                            output: None,
                            started_at,
                            completed_at: None,
                            stats: None,
                        },
                    });
                }
                _ => {}
            }
        }

        self.partial_text = false;
        events
    }

    fn on_user(&mut self, frame: UserFrame) -> Vec<HarnessEvent> {
        if frame.parent_tool_use_id.is_some() {
            return Vec::new();
        }

        let blocks = match frame.message.content {
            Value::Array(blocks) => blocks,
            Value::Null => return Vec::new(),
            other => vec![other],
        };

        let mut events = Vec::new();
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let output = output_preview(block.get("content"));
            let full_output = full_output_text(block.get("content"));
            let auto_denied = output
                .as_deref()
                .is_some_and(|text| text.to_ascii_lowercase().contains("requested permissions"));
            let denied = self.denied_tools.remove(tool_use_id).is_some() || auto_denied;
            let status = if denied {
                ToolCallStatus::Denied
            } else if is_error {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            };
            let completed_at = Utc::now();
            let meta = self.tool_calls.remove(tool_use_id).unwrap_or(ToolMeta {
                name: "tool".into(),
                summary: String::new(),
                input: None,
                started_at: completed_at,
            });

            self.state = AgentState::Thinking;
            let structured = (!frame.tool_use_result.is_null()).then_some(&frame.tool_use_result);
            let stats = (status == ToolCallStatus::Completed)
                .then(|| {
                    crate::stats::derive(
                        &meta.name,
                        meta.input.as_ref(),
                        full_output.as_deref(),
                        structured,
                    )
                })
                .flatten();
            events.push(HarnessEvent::ToolCall {
                tool_call: ToolCall {
                    tool_call_id: tool_use_id.to_string(),
                    name: meta.name,
                    status,
                    summary: meta.summary,
                    input: meta.input,
                    output,
                    started_at: meta.started_at,
                    completed_at: Some(completed_at),
                    stats,
                },
            });
        }
        events
    }

    fn on_result(&mut self, frame: ResultFrame) -> Vec<HarnessEvent> {
        let is_error = frame.is_error;
        let usage = frame
            .usage
            .as_ref()
            .map(|raw| map_usage(raw, frame.total_cost_usd, frame.duration_ms))
            .unwrap_or_else(|| Usage {
                duration_ms: frame.duration_ms,
                cost_micros: cost_micros(frame.total_cost_usd),
                ..Usage::default()
            });

        let mut events = Vec::new();
        if is_error {
            let text = frame.result.clone().unwrap_or_default();
            if !self.limit_seen
                && let Some(signal) = limit::detect_limit(&text, Utc::now())
            {
                self.limit_seen = true;
                events.push(HarnessEvent::LimitHit {
                    reset_at: signal.reset_at,
                    auto_resume: signal.reset_at.is_some(),
                });
            } else if !self.error_seen && !self.limit_seen && !text.trim().is_empty() {
                events.push(HarnessEvent::Error {
                    message: truncate_chars(text.trim(), OUTPUT_PREVIEW_CHARS),
                    retryable: true,
                });
            }
        }

        self.last_turn = Some(TurnReport {
            num_turns: frame.num_turns,
            duration_ms: frame.duration_ms,
            duration_api_ms: frame.duration_api_ms,
            cost_micros: cost_micros(frame.total_cost_usd),
            is_error,
            stop_reason: frame.stop_reason,
            subtype: frame.subtype,
        });
        self.state = if is_error {
            if self.limit_seen {
                AgentState::RateLimited
            } else {
                AgentState::Error
            }
        } else {
            AgentState::Done
        };

        if usage.total_tokens() > 0 || usage.cost_micros > 0 {
            events.push(HarnessEvent::Usage {
                usage: usage.clone(),
            });
        }
        events.push(HarnessEvent::TurnCompleted {
            status: if is_error {
                TurnStatus::Failed
            } else {
                TurnStatus::Completed
            },
            usage,
        });

        // The next user message starts a new turn.
        self.limit_seen = false;
        self.error_seen = false;
        self.partial_text = false;
        events
    }

    fn on_rate_limit(&mut self, frame: RateLimitFrame) -> Vec<HarnessEvent> {
        let status = frame.rate_limit_info.status.to_ascii_lowercase();
        if matches!(
            status.as_str(),
            "" | "allowed" | "allowed_warning" | "ok" | "normal"
        ) {
            return Vec::new();
        }
        if self.limit_seen {
            return Vec::new();
        }

        self.limit_seen = true;
        self.state = AgentState::RateLimited;
        let reset_at = frame
            .rate_limit_info
            .resets_at
            .as_ref()
            .and_then(epoch_value_to_utc);
        vec![HarnessEvent::LimitHit {
            reset_at,
            auto_resume: reset_at.is_some(),
        }]
    }
}

fn short_tool_name(name: &str) -> String {
    name.rsplit("__")
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase()
}

fn block_text(block: &Value) -> Option<String> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string),
        Some("image") => Some("[image]".into()),
        _ => block
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// The whole result text, untruncated: counting matches from the 4 KB preview
/// would undercount a long result list.
fn full_output_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(block_text)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        other => Some(other.to_string()),
    }
}

fn output_preview(content: Option<&Value>) -> Option<String> {
    let content = content?;
    let text = match content {
        Value::Null => return None,
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(block_text)
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    };
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(truncate_chars(text, OUTPUT_PREVIEW_CHARS))
    }
}

fn map_usage(raw: &UsageRaw, total_cost_usd: Option<f64>, duration_ms: u64) -> Usage {
    let cache_read = raw.cache_read_input_tokens;
    let cache_creation = raw.cache_creation_input_tokens;
    Usage {
        // `Usage::input_tokens` is documented as including cached input, while
        // the Anthropic usage object reports those separately.
        input_tokens: raw
            .input_tokens
            .saturating_add(cache_read)
            .saturating_add(cache_creation),
        output_tokens: raw.output_tokens,
        cached_input_tokens: cache_read,
        cost_micros: cost_micros(total_cost_usd),
        duration_ms,
        ..Usage::default()
    }
}

fn cost_micros(total_cost_usd: Option<f64>) -> u64 {
    total_cost_usd
        .filter(|cost| cost.is_finite() && *cost > 0.0)
        .map(|cost| (cost * 1_000_000.0).round() as u64)
        .unwrap_or(0)
}

fn epoch_value_to_utc(value: &Value) -> Option<DateTime<Utc>> {
    let epoch = match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|f| f as i64)),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }?;
    if epoch.abs() > 100_000_000_000 {
        Utc.timestamp_millis_opt(epoch).single()
    } else {
        Utc.timestamp_opt(epoch, 0).single()
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

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Frame {
    #[serde(rename = "system")]
    System(SystemFrame),
    #[serde(rename = "stream_event")]
    StreamEvent(StreamEventFrame),
    #[serde(rename = "assistant")]
    Assistant(AssistantFrame),
    #[serde(rename = "user")]
    User(UserFrame),
    #[serde(rename = "result")]
    Result(ResultFrame),
    #[serde(rename = "rate_limit_event")]
    RateLimit(RateLimitFrame),
    #[serde(rename = "control_response")]
    ControlResponse,
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize)]
struct SystemFrame {
    #[serde(default)]
    subtype: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamEventFrame {
    event: StreamEvent,
    #[serde(default)]
    parent_tool_use_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    content_block: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AssistantFrame {
    message: AssistantMessage,
    #[serde(default)]
    parent_tool_use_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    content: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct UserFrame {
    message: UserMessage,
    /// Claude Code's structured result for the tool call (`structuredPatch`
    /// for edits, `mode`/`numFiles` for searches); absent on older versions.
    #[serde(default)]
    tool_use_result: Value,
    #[serde(default)]
    parent_tool_use_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserMessage {
    #[serde(default)]
    content: Value,
}

#[derive(Debug, Default, Deserialize)]
struct ResultFrame {
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    duration_api_ms: u64,
    #[serde(default)]
    num_turns: u64,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    usage: Option<UsageRaw>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct UsageRaw {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct RateLimitFrame {
    rate_limit_info: RateLimitInfo,
}

#[derive(Debug, Default, Deserialize)]
struct RateLimitInfo {
    #[serde(default)]
    status: String,
    #[serde(default, rename = "resetsAt")]
    resets_at: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_tool_names_to_agent_states() {
        assert_eq!(state_for_tool("Grep"), AgentState::Searching);
        assert_eq!(state_for_tool("Glob"), AgentState::Searching);
        assert_eq!(state_for_tool("WebSearch"), AgentState::Searching);
        assert_eq!(state_for_tool("Read"), AgentState::Reading);
        assert_eq!(state_for_tool("Edit"), AgentState::Editing);
        assert_eq!(state_for_tool("Write"), AgentState::Editing);
        assert_eq!(state_for_tool("MultiEdit"), AgentState::Editing);
        assert_eq!(state_for_tool("Bash"), AgentState::RunningTool);
        assert_eq!(
            state_for_tool("mcp__h4rn__spawn_subagent"),
            AgentState::RunningTool
        );
    }

    #[test]
    fn reports_paths_only_for_mutating_tools() {
        let input = serde_json::json!({"file_path": "/tmp/a.rs"});
        assert_eq!(
            tool_paths("Edit", Some(&input)),
            vec!["/tmp/a.rs".to_string()]
        );
        assert_eq!(
            tool_paths("MultiEdit", Some(&input)),
            vec!["/tmp/a.rs".to_string()]
        );
        assert!(tool_paths("Read", Some(&input)).is_empty());
        assert!(tool_paths("Bash", Some(&input)).is_empty());
    }

    #[test]
    fn summaries_prefer_human_fields() {
        let bash = serde_json::json!({"command": "ls", "description": "List files"});
        assert_eq!(tool_summary("Bash", Some(&bash)), "List files");
        let read = serde_json::json!({"file_path": "/tmp/a.rs"});
        assert_eq!(tool_summary("Read", Some(&read)), "/tmp/a.rs");
        assert_eq!(tool_summary("Bash", None), "");
    }

    #[test]
    fn malformed_lines_become_diagnostics() {
        let mut parser = ClaudeStreamParser::new();
        let events = parser.push_line("this is not json");
        assert!(matches!(
            events.as_slice(),
            [HarnessEvent::Error {
                retryable: false,
                ..
            }]
        ));
        assert!(parser.push_line("").is_empty());
    }
}
