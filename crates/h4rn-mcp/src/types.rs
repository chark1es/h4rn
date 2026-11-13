//! MCP model types used inside the hub.
//!
//! These mirror the MCP 2025-06-18 schema closely enough to proxy it without
//! loss, while staying small enough to reason about. Unknown vendor fields are
//! preserved where it matters (`annotations`, `_meta`) and dropped where they
//! cannot affect behaviour (logging levels, completion contexts).
//!
//! Every conversion is fallible and explicit: a malformed upstream response
//! becomes an [`UpstreamError`](crate::upstream::UpstreamError) rather than a
//! silently empty tool list, because "the server has no tools" and "the server
//! lied about its tools" must not look the same to a user.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Caller-visible annotations for a tool (MCP `ToolAnnotations`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolAnnotations {
    /// Human-readable title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The tool does not modify its environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// The tool may perform destructive updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// Repeated calls with the same arguments have no additional effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// The tool interacts with an open world (network, third parties).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

impl ToolAnnotations {
    /// Whether the server declared the tool read-only.
    pub fn read_only(&self) -> bool {
        self.read_only_hint.unwrap_or(false)
    }
}

/// One tool definition from a provider or upstream server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Name inside the server.
    pub name: String,
    /// Model-facing description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for arguments.
    #[serde(default = "empty_schema", rename = "inputSchema")]
    pub input_schema: Value,
    /// Optional output schema.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "outputSchema"
    )]
    pub output_schema: Option<Value>,
    /// Safety hints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
}

fn empty_schema() -> Value {
    json!({"type": "object", "additionalProperties": true})
}

impl ToolDefinition {
    /// Builds a tool with an object schema and no output schema.
    pub fn new(name: impl Into<String>, description: Option<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description,
            input_schema,
            output_schema: None,
            annotations: None,
        }
    }

    /// Whether this tool is annotated read-only.
    pub fn read_only(&self) -> bool {
        self.annotations
            .as_ref()
            .map(ToolAnnotations::read_only)
            .unwrap_or(false)
    }
}

/// One piece of tool-call content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Plain text.
    Text {
        /// Text body.
        text: String,
    },
    /// Base64 image data.
    Image {
        /// Base64 payload.
        data: String,
        /// Media type.
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Base64 audio data.
    Audio {
        /// Base64 payload.
        data: String,
        /// Media type.
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// An embedded resource.
    Resource {
        /// The resource and its contents.
        resource: ResourceContents,
    },
    /// A link to a resource the client can read separately.
    ResourceLink {
        /// Target URI.
        uri: String,
        /// Display name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Media type.
        #[serde(default, rename = "mimeType", skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
}

impl Content {
    /// Creates a text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Extracts text, if this is a text block.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            _ => None,
        }
    }

    /// Approximate byte size, used for output caps.
    pub fn size_bytes(&self) -> usize {
        match self {
            Self::Text { text } => text.len(),
            Self::Image { data, .. } | Self::Audio { data, .. } => data.len(),
            Self::Resource { resource } => {
                resource.text.as_ref().map(String::len).unwrap_or(0)
                    + resource.blob.as_ref().map(String::len).unwrap_or(0)
            }
            Self::ResourceLink { uri, name, .. } => {
                uri.len() + name.as_ref().map(String::len).unwrap_or(0)
            }
        }
    }
}

/// The result of a tool call.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CallToolResult {
    /// Content blocks.
    #[serde(default)]
    pub content: Vec<Content>,
    /// Structured result, when the server provides one.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "structuredContent"
    )]
    pub structured_content: Option<Value>,
    /// The tool ran but reported a failure.
    #[serde(default, rename = "isError")]
    pub is_error: bool,
}

impl CallToolResult {
    /// Builds a text result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(text)],
            structured_content: None,
            is_error: false,
        }
    }

    /// Builds an error result that is still a successful JSON-RPC response,
    /// which is how MCP separates tool failures from protocol failures.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(message)],
            structured_content: None,
            is_error: true,
        }
    }

    /// Concatenates text content for the UI's test-call view.
    pub fn text_output(&self) -> String {
        self.content
            .iter()
            .filter_map(Content::as_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Truncates text and resource bodies to `limit` bytes across the result.
    ///
    /// Upstream servers can return unbounded output; a harness that pipes that
    /// into a model would blow the context window. The cap is applied at the
    /// hub so every harness gets the same bound. `is_error` is preserved and
    /// the truncation is visible in the content rather than silent.
    pub fn cap_output(&mut self, limit: usize) -> bool {
        let mut remaining = limit;
        let mut truncated = false;
        self.content.retain_mut(|content| {
            let size = content.size_bytes();
            if size <= remaining {
                remaining -= size;
                return true;
            }
            truncated = true;
            trim_content(content, remaining);
            remaining = 0;
            // Keep the first over-limit block (trimmed) and drop the rest.
            !matches!(content, Content::Text { text } if text.is_empty())
        });
        if truncated {
            self.content.push(Content::text(format!(
                "[h4rn: output truncated at {limit} bytes]"
            )));
        }
        truncated
    }
}

fn trim_content(content: &mut Content, remaining: usize) {
    match content {
        Content::Text { text } => truncate_utf8(text, remaining),
        Content::Image { data, .. } | Content::Audio { data, .. } => truncate_utf8(data, remaining),
        Content::Resource { resource } => {
            if let Some(text) = &mut resource.text {
                truncate_utf8(text, remaining);
            } else if let Some(blob) = &mut resource.blob {
                truncate_utf8(blob, remaining);
            }
        }
        Content::ResourceLink { .. } => {}
    }
}

fn truncate_utf8(value: &mut String, limit: usize) {
    if value.len() <= limit {
        return;
    }
    let mut boundary = limit;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

/// One resource definition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResourceDefinition {
    /// Canonical URI.
    pub uri: String,
    /// Display name.
    pub name: String,
    /// Description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Media type.
    #[serde(default, rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Size in bytes, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// Resource contents returned by `resources/read`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResourceContents {
    /// URI the contents belong to.
    pub uri: String,
    /// Media type.
    #[serde(default, rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Text contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Base64 binary contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// A prompt argument.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptArgument {
    /// Argument name.
    pub name: String,
    /// Description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the argument is required.
    #[serde(default)]
    pub required: bool,
}

/// A prompt definition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptDefinition {
    /// Name inside the server.
    pub name: String,
    /// Description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Arguments.
    #[serde(default)]
    pub arguments: Vec<PromptArgument>,
}

/// One message in a rendered prompt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptMessage {
    /// `user` or `assistant`.
    pub role: String,
    /// Message content.
    pub content: Content,
}

/// Capabilities an upstream server advertised during `initialize`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerCapabilities {
    /// Supports `tools/*`.
    pub tools: bool,
    /// Emits `notifications/tools/list_changed`.
    pub tools_list_changed: bool,
    /// Supports `resources/*`.
    pub resources: bool,
    /// Emits `notifications/resources/list_changed`.
    pub resources_list_changed: bool,
    /// Supports `resources/subscribe`.
    pub resources_subscribe: bool,
    /// Supports `prompts/*`.
    pub prompts: bool,
    /// Emits `notifications/prompts/list_changed`.
    pub prompts_list_changed: bool,
    /// Supports `logging/setLevel`.
    pub logging: bool,
    /// Supports `completion/complete`.
    pub completions: bool,
}

impl ServerCapabilities {
    /// Parses the `capabilities` object of an `initialize` result.
    pub fn from_json(value: &Value) -> Self {
        let flag = |capability: &str, nested: &str| {
            value
                .get(capability)
                .and_then(|value| value.get(nested))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        let present = |capability: &str| value.get(capability).is_some_and(|v| !v.is_null());
        Self {
            tools: present("tools"),
            tools_list_changed: flag("tools", "listChanged"),
            resources: present("resources"),
            resources_list_changed: flag("resources", "listChanged"),
            resources_subscribe: flag("resources", "subscribe"),
            prompts: present("prompts"),
            prompts_list_changed: flag("prompts", "listChanged"),
            logging: present("logging"),
            completions: present("completions"),
        }
    }

    /// Serialises in the shape `initialize` expects.
    pub fn to_json(&self) -> Value {
        let mut capabilities = serde_json::Map::new();
        if self.tools {
            capabilities.insert(
                "tools".into(),
                json!({"listChanged": self.tools_list_changed}),
            );
        }
        if self.resources {
            capabilities.insert(
                "resources".into(),
                json!({
                    "listChanged": self.resources_list_changed,
                    "subscribe": self.resources_subscribe,
                }),
            );
        }
        if self.prompts {
            capabilities.insert(
                "prompts".into(),
                json!({"listChanged": self.prompts_list_changed}),
            );
        }
        if self.logging {
            capabilities.insert("logging".into(), json!({}));
        }
        if self.completions {
            capabilities.insert("completions".into(), json!({}));
        }
        Value::Object(capabilities)
    }
}

/// Parses a `tools/list` result.
pub fn parse_tools(result: &Value) -> Result<(Vec<ToolDefinition>, Option<String>), String> {
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| "tools/list result has no tools array".to_owned())?;
    let mut parsed = Vec::with_capacity(tools.len());
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "a tool has no name".to_owned())?;
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let input_schema = tool
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(empty_schema);
        let output_schema = tool.get("outputSchema").cloned();
        let annotations = tool
            .get("annotations")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok());
        parsed.push(ToolDefinition {
            name: name.to_owned(),
            description,
            input_schema,
            output_schema,
            annotations,
        });
    }
    let cursor = result
        .get("nextCursor")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((parsed, cursor))
}

/// Parses a `tools/call` result.
pub fn parse_call_result(result: &Value) -> Result<CallToolResult, String> {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let content = match result.get("content") {
        Some(Value::Array(items)) => items.iter().filter_map(parse_content).collect::<Vec<_>>(),
        Some(_) => return Err("tools/call content is not an array".into()),
        None => Vec::new(),
    };
    Ok(CallToolResult {
        content,
        structured_content: result.get("structuredContent").cloned(),
        is_error,
    })
}

fn parse_content(value: &Value) -> Option<Content> {
    let kind = value.get("type").and_then(Value::as_str)?;
    Some(match kind {
        "text" => Content::Text {
            text: value.get("text").and_then(Value::as_str)?.to_owned(),
        },
        "image" => Content::Image {
            data: value.get("data").and_then(Value::as_str)?.to_owned(),
            mime_type: value
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream")
                .to_owned(),
        },
        "audio" => Content::Audio {
            data: value.get("data").and_then(Value::as_str)?.to_owned(),
            mime_type: value
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("audio/wav")
                .to_owned(),
        },
        "resource" => {
            let resource = value.get("resource")?;
            Content::Resource {
                resource: ResourceContents {
                    uri: resource.get("uri").and_then(Value::as_str)?.to_owned(),
                    mime_type: resource
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    text: resource
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    blob: resource
                        .get("blob")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
            }
        }
        "resource_link" => Content::ResourceLink {
            uri: value.get("uri").and_then(Value::as_str)?.to_owned(),
            name: value.get("name").and_then(Value::as_str).map(str::to_owned),
            mime_type: value
                .get("mimeType")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        _ => return None,
    })
}

/// Parses a `resources/list` result.
pub fn parse_resources(
    result: &Value,
) -> Result<(Vec<ResourceDefinition>, Option<String>), String> {
    let resources = result
        .get("resources")
        .and_then(Value::as_array)
        .ok_or_else(|| "resources/list result has no resources array".to_owned())?;
    let mut parsed = Vec::with_capacity(resources.len());
    for resource in resources {
        parsed.push(ResourceDefinition {
            uri: resource
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(|| "a resource has no uri".to_owned())?
                .to_owned(),
            name: resource
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            description: resource
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned),
            mime_type: resource
                .get("mimeType")
                .and_then(Value::as_str)
                .map(str::to_owned),
            size: resource.get("size").and_then(Value::as_u64),
        });
    }
    let cursor = result
        .get("nextCursor")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((parsed, cursor))
}

/// Parses a `resources/read` result.
pub fn parse_resource_contents(result: &Value) -> Result<Vec<ResourceContents>, String> {
    let contents = result
        .get("contents")
        .and_then(Value::as_array)
        .ok_or_else(|| "resources/read result has no contents array".to_owned())?;
    Ok(contents
        .iter()
        .filter_map(|content| {
            Some(ResourceContents {
                uri: content.get("uri").and_then(Value::as_str)?.to_owned(),
                mime_type: content
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                text: content
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                blob: content
                    .get("blob")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect())
}

/// Parses a `prompts/list` result.
pub fn parse_prompts(result: &Value) -> Result<(Vec<PromptDefinition>, Option<String>), String> {
    let prompts = result
        .get("prompts")
        .and_then(Value::as_array)
        .ok_or_else(|| "prompts/list result has no prompts array".to_owned())?;
    let mut parsed = Vec::with_capacity(prompts.len());
    for prompt in prompts {
        let arguments = prompt
            .get("arguments")
            .and_then(Value::as_array)
            .map(|arguments| {
                arguments
                    .iter()
                    .filter_map(|argument| {
                        Some(PromptArgument {
                            name: argument.get("name")?.as_str()?.to_owned(),
                            description: argument
                                .get("description")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            required: argument
                                .get("required")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        parsed.push(PromptDefinition {
            name: prompt
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "a prompt has no name".to_owned())?
                .to_owned(),
            description: prompt
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned),
            arguments,
        });
    }
    let cursor = result
        .get("nextCursor")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((parsed, cursor))
}

/// Parses a `prompts/get` result into messages.
pub fn parse_prompt_messages(result: &Value) -> Result<Vec<PromptMessage>, String> {
    let messages = result
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "prompts/get result has no messages array".to_owned())?;
    Ok(messages
        .iter()
        .filter_map(|message| {
            Some(PromptMessage {
                role: message.get("role").and_then(Value::as_str)?.to_owned(),
                content: parse_content(message.get("content")?)?,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_caps_are_utf8_safe_and_visible() {
        let mut result = CallToolResult {
            content: vec![Content::text("é".repeat(10))],
            structured_content: None,
            is_error: false,
        };
        assert!(result.cap_output(5));
        let text = result.text_output();
        assert!(text.starts_with("éé"));
        assert!(text.contains("truncated"));
    }

    #[test]
    fn tools_parse_with_default_schema() {
        let (tools, cursor) = parse_tools(&json!({
            "tools": [{"name": "echo", "description": "say it"}]
        }))
        .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].input_schema["type"], "object");
        assert_eq!(cursor, None);
    }

    #[test]
    fn capabilities_round_trip() {
        let capabilities = ServerCapabilities {
            tools: true,
            tools_list_changed: true,
            resources: true,
            resources_list_changed: true,
            prompts: true,
            ..ServerCapabilities::default()
        };
        let json = capabilities.to_json();
        let parsed = ServerCapabilities::from_json(&json);
        assert!(parsed.tools_list_changed);
        assert!(parsed.resources_list_changed);
        assert!(parsed.prompts);
        assert!(!parsed.completions);
    }

    #[test]
    fn unknown_content_is_dropped_not_guessed() {
        let result = parse_call_result(&json!({
            "content": [{"type": "text", "text": "ok"}, {"type": "vendor_nonsense"}]
        }))
        .unwrap();
        assert_eq!(result.content.len(), 1);
        assert_eq!(result.text_output(), "ok");
    }
}
