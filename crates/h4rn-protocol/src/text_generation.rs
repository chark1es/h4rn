//! Text generation: the model that writes thread titles for you.
//!
//! A text generator is a model API that only writes text. Its key lives in the
//! daemon's secret store (ADR-016) and is never returned to a client: every
//! type that leaves the daemon reports only [`TextGenerator::has_api_key`].
//! The daemon can also use an installed coding-agent CLI for the same job; that
//! choice is the [`TitleModelSetting`].

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::common::HarnessKind;

/// Identifies one stored text generator.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
pub struct TextGeneratorId(String);

impl TextGeneratorId {
    /// Wraps an existing identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the identifier as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TextGeneratorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for TextGeneratorId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for TextGeneratorId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for TextGeneratorId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// The API one text generator speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum TextGeneratorKind {
    /// OpenAI's chat-completions shape: OpenAI, OpenRouter, Ollama, LM Studio
    /// and anything else that copies it.
    OpenAiCompatible,
    /// Anthropic's messages shape.
    Anthropic,
}

impl TextGeneratorKind {
    /// A stable label for logs and diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "OpenAI-compatible",
            Self::Anthropic => "Anthropic",
        }
    }
}

/// One stored text generator. Never contains a key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TextGenerator {
    /// Stable id.
    pub id: TextGeneratorId,
    /// Human label.
    pub name: String,
    /// API shape.
    pub kind: TextGeneratorKind,
    /// API root, for example `https://api.openai.com/v1`.
    pub base_url: String,
    /// Model the generator writes with by default.
    pub model: String,
    /// Whether a key is stored on this host.
    pub has_api_key: bool,
}

/// Fields accepted when adding a text generator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TextGeneratorDraft {
    /// Human label.
    pub name: String,
    /// API shape.
    pub kind: TextGeneratorKind,
    /// API root, for example `https://api.openai.com/v1`.
    pub base_url: String,
    /// Model that answers the tiny prompt and writes titles.
    pub model: String,
    /// API key. Stored in the daemon secret store, never returned.
    #[serde(default)]
    pub api_key: Option<String>,
}

/// A field-wise generator update. `None` leaves a field unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TextGeneratorPatch {
    /// New label.
    #[serde(default)]
    pub name: Option<String>,
    /// New API root.
    #[serde(default)]
    pub base_url: Option<String>,
    /// New model.
    #[serde(default)]
    pub model: Option<String>,
    /// Replacement API key.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Remove the stored key.
    #[serde(default)]
    pub clear_api_key: bool,
}

/// What the Test button observed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TextGeneratorTestReport {
    /// Whether the model answered.
    pub ok: bool,
    /// Round trip in milliseconds.
    #[ts(type = "number")]
    pub latency_ms: u64,
    /// The answer, trimmed; present only on success.
    pub reply: Option<String>,
    /// What went wrong, without the key.
    pub error: Option<String>,
    /// What to try next.
    pub hint: Option<String>,
}

/// Which model writes thread titles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum TitleModelMode {
    /// Keep the first prompt line as the title.
    Off,
    /// Run an installed coding-agent CLI once.
    #[default]
    CodingAgent,
    /// Call a configured text generator.
    TextGenerator,
}

/// The title model and the settings each mode needs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TitleModelSetting {
    /// Which backend writes titles.
    pub mode: TitleModelMode,
    /// Coding agent to run. `None` follows the thread's own harness, which is
    /// the default; picking one hands every thread's title to that agent.
    #[serde(default)]
    pub harness: Option<HarnessKind>,
    /// Text generator to call in [`TitleModelMode::TextGenerator`].
    #[serde(default)]
    pub generator_id: Option<TextGeneratorId>,
    /// Model override for the generator; `None` uses its stored model.
    #[serde(default)]
    pub model: Option<String>,
}

impl TitleModelSetting {
    /// Titles keep the first line of the first prompt.
    pub fn off() -> Self {
        Self {
            mode: TitleModelMode::Off,
            harness: None,
            generator_id: None,
            model: None,
        }
    }
}

impl Default for TitleModelSetting {
    fn default() -> Self {
        Self {
            mode: TitleModelMode::CodingAgent,
            harness: None,
            generator_id: None,
            model: None,
        }
    }
}

/// Daemon-wide text generation settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TextGenerationSettings {
    /// Which model writes thread titles.
    pub title: TitleModelSetting,
    /// Which model writes commit messages and pull request text.
    ///
    /// Older daemons wrote only `title`; a file without this field reads as
    /// [`SourceControlModelSetting::off`], so nothing starts generating until
    /// the person picks a mode.
    #[serde(default)]
    pub source_control: SourceControlModelSetting,
}

/// Longest custom instruction string the daemon accepts.
pub const MAX_SOURCE_CONTROL_INSTRUCTIONS: usize = 2_000;

/// Which model writes commit messages and pull request text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SourceControlModelMode {
    /// Leave the fields empty; Generate does nothing and no model runs.
    #[default]
    Off,
    /// Run an installed coding-agent CLI once.
    CodingAgent,
    /// Call a configured text generator.
    TextGenerator,
}

/// The source-control model and the settings each mode needs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SourceControlModelSetting {
    /// Which backend writes the text.
    pub mode: SourceControlModelMode,
    /// Coding agent to run. `None` follows the thread's own harness; picking
    /// one hands every thread's commit and PR text to that agent.
    #[serde(default)]
    pub harness: Option<HarnessKind>,
    /// Text generator to call in [`SourceControlModelMode::TextGenerator`].
    #[serde(default)]
    pub generator_id: Option<TextGeneratorId>,
    /// Model override for the generator; `None` uses its stored model.
    #[serde(default)]
    pub model: Option<String>,
    /// Extra instruction appended to the trusted prompt (style, tone, rules).
    #[serde(default)]
    pub instructions: Option<String>,
}

impl SourceControlModelSetting {
    /// Commit and PR text stay empty until the person asks for a model.
    pub fn off() -> Self {
        Self::default()
    }
}
