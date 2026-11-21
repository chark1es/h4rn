//! ACP wire types and normalisation.
//!
//! Agent Client Protocol (ACP) is JSON-RPC 2.0 over newline-delimited JSON on
//! stdio. This module owns the shapes the client sends and the shapes it
//! accepts, and deliberately keeps every vendor name (`agent_message_chunk`,
//! `tool_call_update`, ...) so another ACP agent can reuse [`crate::acp`]
//! without a second parser.
//!
//! Capability fields are read from the raw JSON rather than a derived struct
//! because the installed Antigravity server answers both `loadSession` and
//! `load_session` in the same object; a serde alias would reject that as a
//! duplicate field. Unknown update kinds and unknown tool kinds are preserved
//! as `Unknown` instead of being dropped silently.

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// The ACP protocol revision this client offers.
pub const ACP_PROTOCOL_VERSION: u32 = 1;
/// The JSON-RPC version string used on every frame.
pub const JSONRPC_VERSION: &str = "2.0";

/// The result of the ACP `initialize` handshake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpServerInfo {
    /// The protocol revision the agent chose, read from its reply.
    pub protocol_version: u32,
    /// The agent's display name.
    pub agent_name: String,
    /// The agent's version string.
    pub agent_version: String,
    /// Negotiated agent capabilities.
    pub capabilities: AgentCapabilities,
    /// Sign-in methods the agent offers.
    pub auth_methods: Vec<AcpAuthMethod>,
    /// The raw reply, for diagnostics.
    pub raw: Value,
}

/// Capabilities the agent advertised in `initialize`.
///
/// Every field is a plain boolean derived from the camelCase or snake_case
/// spelling, whichever the agent uses. `raw` keeps the original object so an
/// adapter can inspect vendor `_meta`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentCapabilities {
    /// Whether `session/load` can resume a stored conversation.
    pub load_session: bool,
    /// Whether `session/resume` is advertised.
    pub resume_session: bool,
    /// Whether `session/close` is advertised.
    pub close_session: bool,
    /// Whether `session/list` is advertised.
    pub list_sessions: bool,
    /// Whether prompts accept image blocks.
    pub image: bool,
    /// Whether prompts accept audio blocks.
    pub audio: bool,
    /// Whether embedded resource blocks are accepted.
    pub embedded_context: bool,
    /// Whether `mcpServers` accepts HTTP servers.
    pub http_mcp: bool,
    /// Whether `mcpServers` accepts SSE servers.
    pub sse_mcp: bool,
    /// Whether the agent calls `terminal/*` client methods.
    pub terminal: bool,
    /// The raw capability object.
    pub raw: Value,
}

impl AgentCapabilities {
    /// Parses the `agentCapabilities` object of an initialize reply.
    pub fn from_value(capabilities: &Value) -> Self {
        let prompt = field(capabilities, &["promptCapabilities", "prompt_capabilities"]);
        let mcp = field(capabilities, &["mcpCapabilities", "mcp_capabilities"]);
        let session = field(
            capabilities,
            &["sessionCapabilities", "session_capabilities"],
        );
        let terminal = field(capabilities, &["terminal"]);
        Self {
            load_session: bool_field(capabilities, &["loadSession", "load_session"])
                || session.is_some_and(|value| {
                    field(value, &["load"]).is_some() || field(value, &["resume"]).is_some()
                }),
            resume_session: session.is_some_and(|value| field(value, &["resume"]).is_some()),
            close_session: session.is_some_and(|value| field(value, &["close"]).is_some()),
            list_sessions: session.is_some_and(|value| field(value, &["list"]).is_some()),
            image: prompt.is_some_and(|value| bool_field(value, &["image"])),
            audio: prompt.is_some_and(|value| bool_field(value, &["audio"])),
            embedded_context: prompt
                .is_some_and(|value| bool_field(value, &["embeddedContext", "embedded_context"])),
            http_mcp: mcp.is_some_and(|value| capability_present(value, &["http"])),
            sse_mcp: mcp.is_some_and(|value| capability_present(value, &["sse"])),
            terminal: terminal.is_some_and(capability_present_value),
            raw: capabilities.clone(),
        }
    }

    /// Whether a resume should use `session/load`.
    pub fn supports_load(&self) -> bool {
        self.load_session
    }

    /// Whether MCP servers can be delivered over HTTP.
    pub fn supports_http_mcp(&self) -> bool {
        self.http_mcp
    }
}

/// One sign-in method from the initialize reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpAuthMethod {
    /// Stable method id (`oauth-personal`, `gemini-api-key`, ...).
    pub id: String,
    /// Human label.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
}

impl AcpServerInfo {
    /// Parses an `initialize` result.
    pub fn from_value(result: &Value) -> Self {
        let agent_info = field(result, &["agentInfo", "agent_info"]).unwrap_or(&Value::Null);
        Self {
            protocol_version: u64_field(result, &["protocolVersion", "protocol_version"]) as u32,
            agent_name: string_field(agent_info, &["name"]).unwrap_or_default(),
            agent_version: string_field(agent_info, &["version"]).unwrap_or_default(),
            capabilities: AgentCapabilities::from_value(
                field(result, &["agentCapabilities", "agent_capabilities"]).unwrap_or(&Value::Null),
            ),
            auth_methods: field(result, &["authMethods", "auth_methods"])
                .and_then(Value::as_array)
                .map(|methods| methods.iter().map(parse_auth_method).collect())
                .unwrap_or_default(),
            raw: result.clone(),
        }
    }
}

fn parse_auth_method(value: &Value) -> AcpAuthMethod {
    AcpAuthMethod {
        id: string_field(value, &["id"]).unwrap_or_default(),
        name: string_field(value, &["name"]).unwrap_or_default(),
        description: string_field(value, &["description"]),
    }
}

/// A model the agent offers for a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpModel {
    /// Stable model id passed to `session/set_model`.
    pub model_id: String,
    /// Human label.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
}

/// An execution mode the agent offers for a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpMode {
    /// Stable mode id passed to `session/set_mode`.
    pub id: String,
    /// Human label.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
}

/// The state returned by `session/new`, `session/load` or `session/resume`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpSessionInfo {
    /// The harness-native session id, used for resume.
    pub session_id: String,
    /// Models the account can use.
    pub models: Vec<AcpModel>,
    /// The model selected when the session opened.
    pub current_model_id: Option<String>,
    /// Modes the agent can run in.
    pub modes: Vec<AcpMode>,
    /// The mode selected when the session opened.
    pub current_mode_id: Option<String>,
    /// Select options the agent exposes (model, thought level, mode, ...).
    pub config_options: Value,
    /// The raw reply, for diagnostics.
    pub raw: Value,
}

impl AcpSessionInfo {
    /// Parses a session result, using `fallback_id` when the reply omits it
    /// (`session/load` on some builds returns only models and modes).
    pub fn from_value(result: &Value, fallback_id: &str) -> Self {
        let models = field(result, &["models"]);
        let modes = field(result, &["modes"]);
        Self {
            session_id: string_field(result, &["sessionId", "session_id"])
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| fallback_id.to_string()),
            models: models
                .and_then(|value| field(value, &["availableModels", "available_models"]))
                .and_then(Value::as_array)
                .map(|models| models.iter().map(parse_model).collect())
                .unwrap_or_default(),
            current_model_id: models
                .and_then(|value| string_field(value, &["currentModelId", "current_model_id"])),
            modes: modes
                .and_then(|value| field(value, &["availableModes", "available_modes"]))
                .and_then(Value::as_array)
                .map(|modes| modes.iter().map(parse_mode).collect())
                .unwrap_or_default(),
            current_mode_id: modes
                .and_then(|value| string_field(value, &["currentModeId", "current_mode_id"])),
            config_options: field(result, &["configOptions", "config_options"])
                .cloned()
                .unwrap_or(Value::Null),
            raw: result.clone(),
        }
    }

    /// Finds the model id a requested slug refers to.
    ///
    /// Matches, in order: exact id, case-insensitive id, exact display name,
    /// case-insensitive display name, then a prefix match (`gemini-4-argon`
    /// selecting the same effort suffix as the current model). Returns `None`
    /// when the account does not offer the requested model.
    pub fn resolve_model(&self, requested: &str) -> Option<String> {
        let requested = requested.trim();
        if requested.is_empty() || requested.eq_ignore_ascii_case("default") {
            return self.current_model_id.clone();
        }
        if let Some(model) = self.models.iter().find(|model| model.model_id == requested) {
            return Some(model.model_id.clone());
        }
        if let Some(model) = self
            .models
            .iter()
            .find(|model| model.model_id.eq_ignore_ascii_case(requested))
        {
            return Some(model.model_id.clone());
        }
        if let Some(model) = self.models.iter().find(|model| model.name == requested) {
            return Some(model.model_id.clone());
        }
        if let Some(model) = self
            .models
            .iter()
            .find(|model| model.name.eq_ignore_ascii_case(requested))
        {
            return Some(model.model_id.clone());
        }

        let current_suffix = self
            .current_model_id
            .as_deref()
            .and_then(|current| current.strip_prefix(requested))
            .map(str::to_string);
        if let Some(suffix) = current_suffix
            && let Some(model) = self
                .models
                .iter()
                .find(|model| model.model_id.strip_prefix(requested) == Some(suffix.as_str()))
        {
            return Some(model.model_id.clone());
        }
        self.models
            .iter()
            .find(|model| {
                model
                    .model_id
                    .strip_prefix(requested)
                    .is_some_and(|rest| rest.starts_with('-'))
            })
            .map(|model| model.model_id.clone())
    }
}

fn parse_model(value: &Value) -> AcpModel {
    AcpModel {
        model_id: string_field(value, &["modelId", "model_id", "id"]).unwrap_or_default(),
        name: string_field(value, &["name"]).unwrap_or_default(),
        description: string_field(value, &["description"]),
    }
}

fn parse_mode(value: &Value) -> AcpMode {
    AcpMode {
        id: string_field(value, &["id", "modeId", "mode_id"]).unwrap_or_default(),
        name: string_field(value, &["name"]).unwrap_or_default(),
        description: string_field(value, &["description"]),
    }
}

/// A content block in a `session/prompt`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptContent {
    /// Plain text.
    Text(String),
    /// A base64 image.
    Image {
        /// Base64-encoded bytes.
        data: String,
        /// Media type (`image/png`).
        mime_type: String,
    },
    /// An embedded resource, usually a small text file.
    Resource {
        /// Resource URI (`file:///...`).
        uri: String,
        /// Optional media type.
        mime_type: Option<String>,
        /// Inline text content.
        text: Option<String>,
        /// Base64 blob content.
        blob: Option<String>,
    },
    /// A reference to a resource the agent reads itself.
    ResourceLink {
        /// Resource URI.
        uri: String,
        /// Human name.
        name: String,
        /// Optional media type.
        mime_type: Option<String>,
        /// Size in bytes, when known.
        size: Option<u64>,
    },
}

impl PromptContent {
    /// A text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    /// An image block from already-encoded base64 data.
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }

    /// An embedded text resource.
    pub fn resource_text(
        uri: impl Into<String>,
        mime_type: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self::Resource {
            uri: uri.into(),
            mime_type: Some(mime_type.into()),
            text: Some(text.into()),
            blob: None,
        }
    }

    /// A link to a resource that stays on disk.
    pub fn resource_link(
        uri: impl Into<String>,
        name: impl Into<String>,
        mime_type: Option<String>,
        size: Option<u64>,
    ) -> Self {
        Self::ResourceLink {
            uri: uri.into(),
            name: name.into(),
            mime_type,
            size,
        }
    }

    /// Converts the block to its ACP JSON shape.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Text(text) => json!({"type": "text", "text": text}),
            Self::Image { data, mime_type } => {
                json!({"type": "image", "data": data, "mimeType": mime_type})
            }
            Self::Resource {
                uri,
                mime_type,
                text,
                blob,
            } => {
                let mut resource = json!({"uri": uri});
                if let Some(mime_type) = mime_type {
                    resource["mimeType"] = json!(mime_type);
                }
                if let Some(text) = text {
                    resource["text"] = json!(text);
                }
                if let Some(blob) = blob {
                    resource["blob"] = json!(blob);
                }
                json!({"type": "resource", "resource": resource})
            }
            Self::ResourceLink {
                uri,
                name,
                mime_type,
                size,
            } => {
                let mut link = json!({"type": "resource_link", "uri": uri, "name": name});
                if let Some(mime_type) = mime_type {
                    link["mimeType"] = json!(mime_type);
                }
                if let Some(size) = size {
                    link["size"] = json!(size);
                }
                link
            }
        }
    }
}

/// One content chunk in a `session/update` notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpContentChunk {
    /// The ACP content type (`text`, `image`, `audio`, `resource_link`, ...).
    pub kind: String,
    /// Text extracted from the chunk, when it has any.
    pub text: Option<String>,
    /// Optional media type.
    pub mime_type: Option<String>,
    /// The raw chunk.
    pub raw: Value,
}

/// A tool kind reported by ACP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcpToolKind {
    /// Reading a file or listing a directory.
    Read,
    /// Writing or editing a file.
    Edit,
    /// Running a command or build.
    Execute,
    /// Searching the workspace or the web.
    Search,
    /// Fetching a URL.
    Fetch,
    /// Thinking or planning.
    Think,
    /// A vendor kind this client does not know.
    Other(String),
}

impl AcpToolKind {
    /// Parses the ACP `kind` string.
    pub fn parse(value: &str) -> Self {
        match value {
            "read" => Self::Read,
            "edit" => Self::Edit,
            "execute" => Self::Execute,
            "search" => Self::Search,
            "fetch" => Self::Fetch,
            "think" => Self::Think,
            other => Self::Other(other.to_string()),
        }
    }

    /// A short human label for the tool row.
    pub fn label(&self) -> &str {
        match self {
            Self::Read => "Read",
            Self::Edit => "Edit",
            Self::Execute => "Run command",
            Self::Search => "Search",
            Self::Fetch => "Fetch",
            Self::Think => "Think",
            Self::Other(other) => other,
        }
    }
}

/// A tool call status reported by ACP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcpToolStatus {
    /// Queued but not started.
    Pending,
    /// Running.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Finished with an error.
    Failed,
    /// Cancelled by the user or agent.
    Cancelled,
    /// A vendor status this client does not know.
    Other(String),
}

impl AcpToolStatus {
    /// Parses the ACP `status` string.
    pub fn parse(value: &str) -> Self {
        match value {
            "pending" => Self::Pending,
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" | "canceled" => Self::Cancelled,
            other => Self::Other(other.to_string()),
        }
    }

    /// Whether the status is terminal.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Other(_)
        )
    }
}

/// A file location touched by a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpLocation {
    /// Absolute path.
    pub path: String,
    /// Optional 1-based line.
    pub line: Option<u64>,
}

/// A diff reported by a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpDiff {
    /// File path.
    pub path: Option<String>,
    /// Old text, when supplied.
    pub old_text: Option<String>,
    /// New text.
    pub new_text: String,
}

/// One content item attached to a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolCallContent {
    /// A text result or the command being run.
    Text(String),
    /// A file diff.
    Diff(AcpDiff),
    /// An image (only its media type is retained).
    Image {
        /// Optional media type.
        mime_type: Option<String>,
    },
    /// A resource link.
    ResourceLink {
        /// Resource URI.
        uri: String,
    },
    /// A content item this client does not understand.
    Other,
}

/// A `tool_call` or `tool_call_update` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpToolCall {
    /// Stable tool-call id.
    pub tool_call_id: String,
    /// Human title supplied by the agent.
    pub title: Option<String>,
    /// Tool kind.
    pub kind: AcpToolKind,
    /// Current status.
    pub status: AcpToolStatus,
    /// Content items produced so far.
    pub content: Vec<ToolCallContent>,
    /// Locations the tool touched.
    pub locations: Vec<AcpLocation>,
    /// Raw tool input.
    pub raw_input: Option<Value>,
    /// Raw tool output.
    pub raw_output: Option<Value>,
    /// The raw update.
    pub raw: Value,
}

impl AcpToolCall {
    /// Parses a `tool_call` or `tool_call_update` object.
    pub fn from_value(value: &Value) -> Self {
        Self {
            tool_call_id: string_field(value, &["toolCallId", "tool_call_id"]).unwrap_or_default(),
            title: string_field(value, &["title"]),
            kind: AcpToolKind::parse(
                &string_field(value, &["kind"]).unwrap_or_else(|| "other".into()),
            ),
            status: AcpToolStatus::parse(
                &string_field(value, &["status"]).unwrap_or_else(|| "pending".into()),
            ),
            content: field(value, &["content"])
                .and_then(Value::as_array)
                .map(|items| items.iter().map(parse_tool_content).collect())
                .unwrap_or_default(),
            locations: field(value, &["locations"])
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(parse_location).collect())
                .unwrap_or_default(),
            raw_input: field(value, &["rawInput", "raw_input"]).cloned(),
            raw_output: field(value, &["rawOutput", "raw_output"]).cloned(),
            raw: value.clone(),
        }
    }

    /// A one-line summary suitable for the collapsed tool row.
    pub fn summary(&self) -> String {
        Self::summary_of(
            self.title.as_deref(),
            self.raw_input.as_ref(),
            &self.locations,
        )
    }

    /// The collapsed-row summary for fields that updates have been merged into.
    pub fn summary_of(
        title: Option<&str>,
        raw_input: Option<&Value>,
        locations: &[AcpLocation],
    ) -> String {
        if let Some(title) = title.filter(|title| !title.is_empty()) {
            return truncate(title, 140);
        }
        if let Some(input) = raw_input {
            for key in ["command", "path", "file_path", "query", "url", "pattern"] {
                if let Some(text) = input.get(key).and_then(Value::as_str) {
                    return truncate(text, 140);
                }
            }
        }
        if let Some(path) = locations.first() {
            return truncate(&path.path, 140);
        }
        String::new()
    }

    /// A text preview of the tool output.
    pub fn output_preview(&self) -> Option<String> {
        let mut parts = Vec::new();
        for item in &self.content {
            match item {
                ToolCallContent::Text(text) => parts.push(text.clone()),
                ToolCallContent::Diff(diff) => {
                    let path = diff.path.as_deref().unwrap_or("diff");
                    parts.push(format!("--- {path}\n+++ {path}\n{}", diff.new_text));
                }
                ToolCallContent::Image { .. } => parts.push("[image]".into()),
                ToolCallContent::ResourceLink { uri } => parts.push(uri.clone()),
                ToolCallContent::Other => {}
            }
        }
        if let Some(raw) = self.raw_output.as_ref() {
            match raw {
                Value::String(text) => parts.push(text.clone()),
                Value::Null => {}
                other => parts.push(other.to_string()),
            }
        }
        let text = parts.join("\n");
        let text = text.trim();
        (!text.is_empty()).then(|| truncate(text, 2_000))
    }

    /// The diffstat of the call's `diff` content, or the match count of a
    /// finished search.
    ///
    /// ACP hands over old and new text for an edit, so the diffstat is exact;
    /// a file that has no old text is a new file and counts as all added.
    pub fn stats(&self) -> Option<h4rn_protocol::ToolStats> {
        let (mut added, mut removed) = (0u32, 0u32);
        let mut diffs = 0usize;
        for item in &self.content {
            if let ToolCallContent::Diff(diff) = item {
                let (a, r) =
                    crate::stats::line_diff(diff.old_text.as_deref().unwrap_or(""), &diff.new_text);
                added = added.saturating_add(a);
                removed = removed.saturating_add(r);
                diffs += 1;
            }
        }
        if diffs > 0 {
            return Some(h4rn_protocol::ToolStats::Diff { added, removed });
        }
        if self.kind == AcpToolKind::Search && self.status == AcpToolStatus::Completed {
            let mut text = self
                .content
                .iter()
                .filter_map(|item| match item {
                    ToolCallContent::Text(text) => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(Value::String(raw)) = self.raw_output.as_ref() {
                text.push('\n');
                text.push_str(raw);
            }
            return crate::stats::derive("search", None, Some(&text), None);
        }
        None
    }

    /// Paths the call touched, including diff targets.
    pub fn paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .locations
            .iter()
            .map(|location| location.path.clone())
            .collect();
        for item in &self.content {
            if let ToolCallContent::Diff(diff) = item
                && let Some(path) = &diff.path
                && !paths.contains(path)
            {
                paths.push(path.clone());
            }
        }
        paths
    }
}

fn parse_tool_content(value: &Value) -> ToolCallContent {
    let kind = string_field(value, &["type"]).unwrap_or_default();
    match kind.as_str() {
        "diff" => ToolCallContent::Diff(AcpDiff {
            path: string_field(value, &["path", "filePath", "file_path"]),
            old_text: string_field(value, &["oldText", "old_text"]),
            new_text: string_field(value, &["newText", "new_text"]).unwrap_or_default(),
        }),
        "content" => field(value, &["content"])
            .map(parse_tool_content)
            .unwrap_or(ToolCallContent::Other),
        "text" => ToolCallContent::Text(string_field(value, &["text"]).unwrap_or_default()),
        "image" => ToolCallContent::Image {
            mime_type: string_field(value, &["mimeType", "mime_type"]),
        },
        "resource_link" => ToolCallContent::ResourceLink {
            uri: string_field(value, &["uri"]).unwrap_or_default(),
        },
        _ => ToolCallContent::Other,
    }
}

fn parse_location(value: &Value) -> Option<AcpLocation> {
    let path = string_field(value, &["path"])?;
    Some(AcpLocation {
        path,
        line: u64_field(value, &["line"])
            .checked_sub(0)
            .filter(|_| false)
            .or_else(|| {
                field(value, &["line"])
                    .and_then(|line| line.as_u64().or_else(|| line.as_i64().map(|v| v as u64)))
            }),
    })
}

/// One step in an ACP plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanEntry {
    /// Step text.
    pub content: String,
    /// Priority (`high`, `medium`, `low`).
    pub priority: String,
    /// Status (`pending`, `in_progress`, `completed`).
    pub status: String,
}

/// One slash command the agent advertises.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpCommand {
    /// Command name.
    pub name: String,
    /// Human description.
    pub description: Option<String>,
}

/// A `session/update` notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcpUpdate {
    /// The user's message echoed by the agent.
    UserMessageChunk {
        /// Content chunks.
        content: Vec<AcpContentChunk>,
    },
    /// Streamed assistant text.
    AgentMessageChunk {
        /// Content chunks.
        content: Vec<AcpContentChunk>,
    },
    /// Streamed reasoning text.
    AgentThoughtChunk {
        /// Content chunks.
        content: Vec<AcpContentChunk>,
    },
    /// A tool call started.
    ToolCall {
        /// The tool call.
        tool_call: AcpToolCall,
    },
    /// A tool call changed state.
    ToolCallUpdate {
        /// The tool call delta.
        tool_call: AcpToolCall,
    },
    /// The agent published a plan.
    Plan {
        /// Plan steps.
        entries: Vec<PlanEntry>,
    },
    /// The agent published its slash-command surface.
    AvailableCommands {
        /// Commands.
        commands: Vec<AcpCommand>,
    },
    /// The session title or timestamp changed.
    SessionInfo {
        /// Auto-generated conversation title.
        title: Option<String>,
    },
    /// The execution mode changed.
    CurrentMode {
        /// New mode id.
        mode_id: String,
    },
    /// The agent's select options changed.
    ConfigOptions {
        /// Raw `configOptions` array.
        config: Value,
    },
    /// The agent reported usage for the session.
    Usage {
        /// Usage counters.
        usage: AcpUsage,
    },
    /// An update kind this client does not understand.
    Unknown {
        /// The `sessionUpdate` discriminator.
        session_update: String,
        /// The raw update object.
        raw: Value,
    },
}

impl AcpUpdate {
    /// Parses the `update` object from a `session/update` notification.
    pub fn from_value(update: &Value) -> Self {
        let kind = string_field(update, &["sessionUpdate", "session_update"])
            .unwrap_or_else(|| "unknown".into());
        match kind.as_str() {
            "user_message_chunk" => Self::UserMessageChunk {
                content: parse_content_chunks(update),
            },
            "agent_message_chunk" => Self::AgentMessageChunk {
                content: parse_content_chunks(update),
            },
            "agent_thought_chunk" => Self::AgentThoughtChunk {
                content: parse_content_chunks(update),
            },
            "tool_call" => Self::ToolCall {
                tool_call: AcpToolCall::from_value(update),
            },
            "tool_call_update" => Self::ToolCallUpdate {
                tool_call: AcpToolCall::from_value(update),
            },
            "plan" => Self::Plan {
                entries: field(update, &["entries"])
                    .and_then(Value::as_array)
                    .map(|entries| entries.iter().map(parse_plan_entry).collect())
                    .unwrap_or_default(),
            },
            "available_commands_update" => Self::AvailableCommands {
                commands: field(update, &["availableCommands", "available_commands"])
                    .and_then(Value::as_array)
                    .map(|commands| commands.iter().map(parse_command).collect())
                    .unwrap_or_default(),
            },
            "session_info_update" => Self::SessionInfo {
                title: string_field(update, &["title"]),
            },
            "current_mode_update" => Self::CurrentMode {
                mode_id: string_field(update, &["currentModeId", "current_mode_id"])
                    .unwrap_or_default(),
            },
            "config_option_update" => Self::ConfigOptions {
                config: field(update, &["configOptions", "config_options"])
                    .cloned()
                    .unwrap_or(Value::Null),
            },
            "usage_update" => Self::Usage {
                usage: AcpUsage::from_value(update),
            },
            other => Self::Unknown {
                session_update: other.to_string(),
                raw: update.clone(),
            },
        }
    }

    /// Concatenated text of the chunk variants, if any.
    pub fn text(&self) -> Option<String> {
        let chunks = match self {
            Self::UserMessageChunk { content }
            | Self::AgentMessageChunk { content }
            | Self::AgentThoughtChunk { content } => content,
            _ => return None,
        };
        let text = chunks
            .iter()
            .filter_map(|chunk| chunk.text.as_deref())
            .collect::<Vec<_>>()
            .join("");
        (!text.is_empty()).then_some(text)
    }
}

/// Parses `params` from a `session/update` notification.
pub fn parse_session_update(params: &Value) -> Option<(String, AcpUpdate)> {
    let session_id = string_field(params, &["sessionId", "session_id"])?;
    let update = field(params, &["update"])?;
    Some((session_id, AcpUpdate::from_value(update)))
}

fn parse_content_chunks(update: &Value) -> Vec<AcpContentChunk> {
    let mut chunks = Vec::new();
    if let Some(content) = field(update, &["content"]) {
        chunks.push(parse_content_chunk(content));
    }
    if let Some(array) = field(update, &["content"]).and_then(Value::as_array) {
        chunks = array.iter().map(parse_content_chunk).collect();
    }
    chunks
}

fn parse_content_chunk(content: &Value) -> AcpContentChunk {
    let kind = string_field(content, &["type"]).unwrap_or_default();
    AcpContentChunk {
        kind,
        text: string_field(content, &["text"]),
        mime_type: string_field(content, &["mimeType", "mime_type"]),
        raw: content.clone(),
    }
}

fn parse_plan_entry(value: &Value) -> PlanEntry {
    PlanEntry {
        content: string_field(value, &["content"]).unwrap_or_default(),
        priority: string_field(value, &["priority"]).unwrap_or_default(),
        status: string_field(value, &["status"]).unwrap_or_default(),
    }
}

fn parse_command(value: &Value) -> AcpCommand {
    AcpCommand {
        name: string_field(value, &["name"]).unwrap_or_default(),
        description: string_field(value, &["description"]),
    }
}

/// Token counters reported by ACP.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpUsage {
    /// Prompt tokens, excluding cached input.
    pub input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Input tokens served from cache.
    pub cached_read_tokens: u64,
    /// Input tokens written to cache.
    pub cached_write_tokens: u64,
    /// Reasoning tokens included in the output.
    pub thought_tokens: u64,
    /// Total tokens, when the agent reports one.
    pub total_tokens: u64,
}

impl AcpUsage {
    /// Parses a usage object from a prompt result or usage update.
    pub fn from_value(value: &Value) -> Self {
        let mut usage = Self {
            input_tokens: u64_field(value, &["inputTokens", "input_tokens"]),
            output_tokens: u64_field(value, &["outputTokens", "output_tokens"]),
            cached_read_tokens: u64_field(value, &["cachedReadTokens", "cached_read_tokens"]),
            cached_write_tokens: u64_field(value, &["cachedWriteTokens", "cached_write_tokens"]),
            thought_tokens: u64_field(value, &["thoughtTokens", "thought_tokens"]),
            total_tokens: u64_field(value, &["totalTokens", "total_tokens"]),
        };
        if usage.total_tokens == 0 {
            usage.total_tokens =
                usage.input_tokens + usage.output_tokens + usage.cached_read_tokens;
        }
        usage
    }

    /// Whether the agent reported no counters at all.
    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cached_read_tokens == 0
            && self.cached_write_tokens == 0
            && self.total_tokens == 0
    }
}

/// The result of one `session/prompt`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpPromptResult {
    /// The agent's stop reason (`end_turn`, `max_tokens`, `cancelled`, ...).
    pub stop_reason: String,
    /// Usage for the turn.
    pub usage: AcpUsage,
    /// The raw reply.
    pub raw: Value,
}

impl AcpPromptResult {
    /// Parses a `session/prompt` result.
    ///
    /// A missing or non-string `stopReason` becomes an empty string, which is
    /// not a normal completion: the adapter reports it as a failure instead of
    /// guessing that the turn ended.
    pub fn from_value(value: &Value) -> Self {
        Self {
            stop_reason: field(value, &["stopReason", "stop_reason"])
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            usage: field(value, &["usage"])
                .map(AcpUsage::from_value)
                .unwrap_or_default(),
            raw: value.clone(),
        }
    }

    /// Whether the turn ended normally.
    ///
    /// Only `end_turn` is a normal completion. `max_tokens` and
    /// `max_turn_requests` ended the turn early and are reported separately so
    /// the UI never shows a truncated reply as a completed one.
    pub fn completed(&self) -> bool {
        self.stop_reason == "end_turn"
    }

    /// Whether the user cancelled the turn.
    pub fn cancelled(&self) -> bool {
        self.stop_reason == "cancelled"
    }

    /// Whether the reply hit the output token limit.
    pub fn token_limited(&self) -> bool {
        self.stop_reason == "max_tokens"
    }

    /// Whether the turn hit its request limit.
    pub fn turn_limited(&self) -> bool {
        self.stop_reason == "max_turn_requests"
    }
}

/// One selectable answer in a permission request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpPermissionOption {
    /// Stable option id echoed back in the response.
    pub option_id: String,
    /// Human label.
    pub name: String,
    /// `allow_once`, `allow_always`, `reject_once`, `reject_always`, ...
    pub kind: String,
}

/// A `session/request_permission` request from the agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpPermissionRequest {
    /// Session the request belongs to.
    pub session_id: String,
    /// The tool call being approved.
    pub tool_call: Option<AcpToolCall>,
    /// Selectable answers.
    pub options: Vec<AcpPermissionOption>,
    /// The raw params.
    pub raw: Value,
}

impl AcpPermissionRequest {
    /// Parses `session/request_permission` params.
    pub fn from_value(params: &Value) -> Self {
        Self {
            session_id: string_field(params, &["sessionId", "session_id"]).unwrap_or_default(),
            tool_call: field(params, &["toolCall", "tool_call"]).map(AcpToolCall::from_value),
            options: field(params, &["options"])
                .and_then(Value::as_array)
                .map(|options| options.iter().map(parse_permission_option).collect())
                .unwrap_or_default(),
            raw: params.clone(),
        }
    }
}

fn parse_permission_option(value: &Value) -> AcpPermissionOption {
    AcpPermissionOption {
        option_id: string_field(value, &["optionId", "option_id"]).unwrap_or_default(),
        name: string_field(value, &["name"]).unwrap_or_default(),
        kind: string_field(value, &["kind"]).unwrap_or_default(),
    }
}

/// One MCP server passed to `session/new` or `session/load`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcpMcpServer {
    /// Streamable HTTP server (the h4rn hub's normal shape).
    Http {
        /// Entry name.
        name: String,
        /// Endpoint URL.
        url: String,
        /// Extra request headers, usually `Authorization`.
        headers: BTreeMap<String, String>,
    },
    /// A stdio server, used when the agent does not advertise HTTP MCP.
    Stdio {
        /// Entry name.
        name: String,
        /// Executable.
        command: String,
        /// Arguments.
        args: Vec<String>,
        /// Environment variables.
        env: BTreeMap<String, String>,
    },
}

impl AcpMcpServer {
    /// An HTTP entry for the h4rn hub.
    pub fn hub_http(url: impl Into<String>, token: Option<&str>) -> Self {
        let mut headers = BTreeMap::new();
        if let Some(token) = token.filter(|token| !token.is_empty()) {
            headers.insert("Authorization".to_string(), format!("Bearer {token}"));
        }
        Self::Http {
            name: "h4rn".into(),
            url: url.into(),
            headers,
        }
    }

    /// A stdio bridge entry for an HTTP hub endpoint.
    ///
    /// ACP's stdio entry cannot point at an HTTP URL, so the documented
    /// fallback is the `mcp-remote` shim:
    /// `npx -y mcp-remote <endpoint>`. Deployments that ship their own bridge
    /// override the command through [`ENV_MCP_STDIO_BRIDGE`].
    pub fn hub_stdio(url: impl Into<String>, command: Option<&str>) -> Self {
        let url = url.into();
        let (command, mut args) = match command.filter(|command| !command.trim().is_empty()) {
            Some(command) => {
                let mut parts = command.split_whitespace();
                let command = parts.next().unwrap_or("npx").to_string();
                (command, parts.map(str::to_string).collect::<Vec<_>>())
            }
            None => ("npx".to_string(), vec!["-y".to_string()]),
        };
        if args.last().map(String::as_str) != Some("mcp-remote") && command == "npx" {
            args.push("mcp-remote".into());
        }
        args.push(url);
        Self::Stdio {
            name: "h4rn".into(),
            command,
            args,
            env: BTreeMap::new(),
        }
    }

    /// Converts the entry to the ACP `mcpServers` shape.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Http { name, url, headers } => {
                let headers: Vec<Value> = headers
                    .iter()
                    .map(|(name, value)| json!({"name": name, "value": value}))
                    .collect();
                json!({"type": "http", "name": name, "url": url, "headers": headers})
            }
            Self::Stdio {
                name,
                command,
                args,
                env,
            } => {
                let env: Vec<Value> = env
                    .iter()
                    .map(|(name, value)| json!({"name": name, "value": value}))
                    .collect();
                json!({"type": "stdio", "name": name, "command": command, "args": args, "env": env})
            }
        }
    }
}

/// Environment override naming an executable used to bridge an HTTP hub over
/// stdio when an ACP agent does not advertise HTTP MCP support.
pub const ENV_MCP_STDIO_BRIDGE: &str = "H4RN_ACP_MCP_STDIO_BRIDGE";

/// Returns the first present field from `value`.
pub(crate) fn field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a Value> {
    names
        .iter()
        .find_map(|name| value.get(*name).filter(|field| !field.is_null()))
}

/// Reads a boolean, treating an object/array marker as `true`.
pub(crate) fn bool_field(value: &Value, names: &[&str]) -> bool {
    field(value, names).is_some_and(capability_present_value)
}

fn capability_present(value: &Value, names: &[&str]) -> bool {
    field(value, names).is_some_and(capability_present_value)
}

fn capability_present_value(value: &Value) -> bool {
    match value {
        Value::Bool(present) => *present,
        Value::Null => false,
        Value::Object(map) => !map.is_empty() || true,
        _ => true,
    }
}

/// Reads a string field.
pub(crate) fn string_field(value: &Value, names: &[&str]) -> Option<String> {
    field(value, names).and_then(|field| match field {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    })
}

/// Reads an unsigned integer field.
pub(crate) fn u64_field(value: &Value, names: &[&str]) -> u64 {
    field(value, names)
        .and_then(|field| {
            field
                .as_u64()
                .or_else(|| field.as_i64().map(|number| number.max(0) as u64))
                .or_else(|| {
                    field
                        .as_f64()
                        .filter(|number| *number >= 0.0)
                        .map(|number| number as u64)
                })
                .or_else(|| field.as_str().and_then(|text| text.trim().parse().ok()))
        })
        .unwrap_or(0)
}

fn truncate(text: &str, max_chars: usize) -> String {
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

    #[test]
    fn capabilities_read_both_spellings_without_duplicate_fields() {
        let capabilities = json!({
            "loadSession": true,
            "load_session": true,
            "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
            "sessionCapabilities": {"resume": {}, "close": {}, "load": {}, "list": {}},
            "session_capabilities": {"resume": {}, "close": {}, "load": {}, "list": {}}
        });
        let parsed = AgentCapabilities::from_value(&capabilities);
        assert!(parsed.load_session);
        assert!(parsed.image);
        assert!(!parsed.audio);
        assert!(parsed.embedded_context);
        assert!(parsed.close_session);
        assert!(!parsed.http_mcp);
    }

    #[test]
    fn http_mcp_is_detected_from_the_capability_marker() {
        let capabilities = json!({"mcpCapabilities": {"http": true, "sse": {}}});
        let parsed = AgentCapabilities::from_value(&capabilities);
        assert!(parsed.supports_http_mcp());
        assert!(parsed.sse_mcp);
    }

    #[test]
    fn model_resolution_handles_base_names_and_effort_suffixes() {
        let info = AcpSessionInfo {
            models: vec![
                AcpModel {
                    model_id: "gemini-4-argon-medium".into(),
                    name: "Gemini 4 Argon (Medium)".into(),
                    description: None,
                },
                AcpModel {
                    model_id: "gemini-4-argon-high".into(),
                    name: "Gemini 4 Argon (High)".into(),
                    description: None,
                },
            ],
            current_model_id: Some("gemini-4-argon-medium".into()),
            ..AcpSessionInfo::default()
        };
        assert_eq!(
            info.resolve_model("gemini-4-argon-high").as_deref(),
            Some("gemini-4-argon-high")
        );
        assert_eq!(
            info.resolve_model("Gemini 4 Argon (High)").as_deref(),
            Some("gemini-4-argon-high")
        );
        assert_eq!(
            info.resolve_model("gemini-4-argon").as_deref(),
            Some("gemini-4-argon-medium")
        );
        assert_eq!(info.resolve_model("nope"), None);
    }

    #[test]
    fn prompt_blocks_serialize_to_acp_shapes() {
        let text = PromptContent::text("hello").to_json();
        assert_eq!(text["type"], "text");
        assert_eq!(text["text"], "hello");
        let image = PromptContent::image("aGVsbG8=", "image/png").to_json();
        assert_eq!(image["type"], "image");
        assert_eq!(image["mimeType"], "image/png");
        let resource = PromptContent::resource_text("file:///a.txt", "text/plain", "hi").to_json();
        assert_eq!(resource["type"], "resource");
        assert_eq!(resource["resource"]["text"], "hi");
        let link = PromptContent::resource_link("file:///a.bin", "a.bin", None, Some(3)).to_json();
        assert_eq!(link["type"], "resource_link");
        assert_eq!(link["size"], 3);
    }

    #[test]
    fn only_end_turn_is_a_normal_completion() {
        let end = AcpPromptResult::from_value(&json!({"stopReason": "end_turn"}));
        assert!(end.completed());
        assert!(!end.token_limited());
        assert!(!end.turn_limited());

        let tokens = AcpPromptResult::from_value(&json!({"stopReason": "max_tokens"}));
        assert!(!tokens.completed());
        assert!(tokens.token_limited());

        let requests = AcpPromptResult::from_value(&json!({"stopReason": "max_turn_requests"}));
        assert!(!requests.completed());
        assert!(requests.turn_limited());

        let refused = AcpPromptResult::from_value(&json!({"stopReason": "refusal"}));
        assert!(!refused.completed());
        assert!(!refused.cancelled());
        assert!(!refused.token_limited());
    }

    #[test]
    fn a_missing_or_non_string_stop_reason_is_not_a_completion() {
        for result in [
            json!({}),
            json!({"stopReason": 3}),
            json!({"stopReason": null}),
        ] {
            let parsed = AcpPromptResult::from_value(&result);
            assert_eq!(parsed.stop_reason, "", "{result}");
            assert!(
                !parsed.completed(),
                "a turn without a stop reason must not count as completed: {result}"
            );
            assert!(!parsed.cancelled());
            assert!(!parsed.token_limited());
            assert!(!parsed.turn_limited());
        }
    }

    #[test]
    fn tool_updates_keep_locations_and_diffs() {
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tc-1",
            "title": "Edit src/lib.rs",
            "kind": "edit",
            "status": "completed",
            "locations": [{"path": "/w/src/lib.rs", "line": 4}],
            "content": [
                {"type": "content", "content": {"type": "text", "text": "done"}},
                {"type": "diff", "path": "/w/src/lib.rs", "oldText": "a", "newText": "b"}
            ]
        });
        let parsed = AcpToolCall::from_value(&update);
        assert_eq!(parsed.tool_call_id, "tc-1");
        assert_eq!(parsed.kind, AcpToolKind::Edit);
        assert_eq!(parsed.status, AcpToolStatus::Completed);
        assert_eq!(parsed.paths(), vec!["/w/src/lib.rs".to_string()]);
        let preview = parsed.output_preview().expect("output");
        assert!(preview.contains("done"));
        assert!(preview.contains("+++ /w/src/lib.rs"));
    }

    #[test]
    fn unknown_updates_are_preserved() {
        let update = json!({"sessionUpdate": "future_kind", "x": 1});
        match AcpUpdate::from_value(&update) {
            AcpUpdate::Unknown {
                session_update,
                raw,
            } => {
                assert_eq!(session_update, "future_kind");
                assert_eq!(raw["x"], 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
