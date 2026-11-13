//! One `Harness` trait, four adapters (ADR-005).
//!
//! Every harness — Claude Code, Codex, OpenCode and Antigravity — is one
//! adapter behind [`Harness`]. The trait normalises the vendor protocols into
//! [`HarnessEvent`] so the daemon's projection and lifecycle never branch on
//! which harness is running. Capabilities are a separate concern (ADR-020).
//!
//! Phase 1a ships the trait, the normalized event vocabulary and a
//! [`FakeHarness`] used by every downstream test. Phase 1b adds the Claude
//! Code adapter ([`ClaudeCodeHarness`]) and the [`HarnessRegistry`] the daemon
//! resolves adapters through. Codex, OpenCode and Antigravity register the
//! same way as their adapters land.

#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use h4rn_protocol::{
    AgentState, ApprovalDecision, ApprovalKind, ApprovalOption, AttachmentId, HarnessKind,
    RuntimeMode, ToolCall, TurnStatus, Usage,
};
use serde::{Deserialize, Serialize};

pub mod acp;
mod antigravity;
mod attachments;
mod claude_code;
mod codex;
mod fake;
mod opencode;
pub mod registry;

pub use antigravity::{
    AgyStreamParser, AntigravityHarness, ENV_AGY_MODE, ENV_AGY_PATH, ENV_AGY_TRANSPORT,
    ENV_APP_DATA_DIR, ENV_CLOUD_CODE_ENDPOINT, ENV_GEMINI_DIR, detect_agy,
};
pub use attachments::{AttachmentClass, AttachmentDelivery, media_type, path_line, strategy};
pub use claude_code::{
    ClaudeCodeHarness, ClaudeCodeSession, ClaudeStreamParser, ENV_MCP_AUTH_TOKEN,
    ENV_MCP_BEARER_TOKEN, ENV_PERMISSION_MODE, ENV_PERMISSION_PROMPT_TOOL, LimitSignal, TurnReport,
    build_user_frame, detect_claude, detect_limit, parse_reset_time, state_for_tool, tool_paths,
    tool_summary,
};
pub use codex::{
    ApprovalRequest, CodexHarness, CodexParser, CodexSession, ENV_BASE_URL, ENV_MODEL_PROVIDER,
    approval_from_request, approval_response, build_turn_input, detect_codex,
};
pub use fake::{FakeHarness, FakeSession, test_spec};
pub use opencode::{
    ENV_CLIPROXY_API_KEY, ENV_CLIPROXY_BASE_URL, ENV_OPENCODE_API_KEY, ENV_OPENCODE_BASE_URL,
    ENV_OPENCODE_MODELS, ENV_OPENCODE_PACKAGE, ENV_OPENCODE_PROVIDER, MINIMUM_OPENCODE_VERSION,
    OpenCodeHarness, OpenCodeParser, OpenCodeSession, detect_opencode,
};
pub use registry::{HarnessRegistry, builtin_adapters};

/// A detected harness installation on this host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessInstall {
    /// Absolute path to the executable.
    pub path: PathBuf,
    /// Version reported by the harness, when it reports one.
    pub version: String,
}

/// One attachment handed to a harness.
///
/// Delivery differs per harness (native image blocks, data URLs, `file://` or a
/// path in the prompt); the adapter owns that matrix (ADR-008).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentRef {
    /// Content-addressed blob id.
    pub id: AttachmentId,
    /// Absolute path of the blob on the daemon host.
    pub path: PathBuf,
    /// Media type, when known.
    pub media_type: String,
}

/// Everything needed to start one harness session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSpec {
    /// Absolute workspace path on the host.
    pub workspace: PathBuf,
    /// Model slug.
    pub model: String,
    /// Permission autonomy chosen when the thread was created.
    pub runtime_mode: RuntimeMode,
    /// Harness session to resume, when resuming.
    pub resume_id: Option<String>,
    /// The single h4rn MCP hub endpoint injected into the session.
    pub mcp_endpoint: Option<String>,
    /// Extra environment variables (never secrets in argv).
    pub env_overrides: BTreeMap<String, String>,
    /// Attachments that must be delivered with the first prompt.
    pub attachments: Vec<AttachmentRef>,
}

/// One message from the user or daemon to the harness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserInput {
    /// Message text.
    pub text: String,
    /// Attachments to deliver with this message.
    pub attachments: Vec<AttachmentRef>,
}

/// Normalized harness output.
///
/// Adapters translate their vendor stream into these values; the daemon
/// projects them into [`h4rn_protocol::Event`] and lifecycle events. Unknown
/// vendor events are dropped, never guessed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    /// The harness accepted the session and reports its resume id.
    SessionStarted {
        /// Harness-native session id used for `--resume`/`--conversation`.
        session_id: String,
    },
    /// A chunk of assistant text.
    TextDelta {
        /// Text to append to the current item.
        text: String,
    },
    /// A tool call started or changed state.
    ToolCall {
        /// Normalized tool-call state.
        tool_call: ToolCall,
    },
    /// The harness is waiting for a decision.
    ApprovalRequested {
        /// Opaque approval id.
        approval_id: String,
        /// What kind of hand is raised.
        kind: ApprovalKind,
        /// What the harness wants to do.
        prompt: String,
        /// Selectable answers, when the hand is a question.
        options: Vec<ApprovalOption>,
    },
    /// Usage for the current turn or session.
    Usage {
        /// Token and cost accounting.
        usage: Usage,
    },
    /// The provider reported a limit and a reset time.
    LimitHit {
        /// When the limit resets, if known.
        reset_at: Option<DateTime<Utc>>,
        /// Whether the daemon should arm auto-resume.
        auto_resume: bool,
    },
    /// The turn finished.
    TurnCompleted {
        /// How it ended.
        status: TurnStatus,
        /// Final usage for the turn.
        usage: Usage,
    },
    /// A transport, protocol or harness failure.
    Error {
        /// What happened.
        message: String,
        /// Whether a retry is safe.
        retryable: bool,
    },
}

/// Harness adapter failures.
#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    /// The executable was not found on this host.
    #[error("{kind:?} is not installed on this host")]
    NotDetected {
        /// Harness that was missing.
        kind: HarnessKind,
    },
    /// The process or transport could not be started.
    #[error("failed to start {harness:?}: {message}")]
    Spawn {
        /// Harness involved.
        harness: HarnessKind,
        /// Underlying failure.
        message: String,
    },
    /// A transport failed after startup.
    #[error("{harness:?} transport failed: {message}")]
    Transport {
        /// Harness involved.
        harness: HarnessKind,
        /// Underlying failure.
        message: String,
    },
    /// The vendor protocol did not match what the adapter understands.
    #[error("{harness:?} protocol error: {message}")]
    Protocol {
        /// Harness involved.
        harness: HarnessKind,
        /// What was wrong.
        message: String,
    },
    /// The harness cannot perform the requested operation.
    #[error("{harness:?} does not support {feature}")]
    Unsupported {
        /// Harness involved.
        harness: HarnessKind,
        /// Requested feature.
        feature: String,
    },
}

/// A running harness session.
#[async_trait]
pub trait HarnessSession: Send {
    /// Sends a user message to the harness.
    async fn send(&mut self, input: UserInput) -> Result<(), HarnessError>;

    /// Interrupts the running turn without killing the session.
    async fn interrupt(&mut self) -> Result<(), HarnessError>;

    /// Answers an approval request raised by the harness.
    ///
    /// `option_id` is the selected [`ApprovalOption`] when the hand is a
    /// question; adapters that only know approve/deny may ignore it.
    async fn respond_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
        option_id: Option<&str>,
    ) -> Result<(), HarnessError>;

    /// Returns the normalized event stream.
    ///
    /// The stream ends when the session ends. Calling this twice returns an
    /// empty stream, so a session has exactly one consumer.
    fn events(&mut self) -> BoxStream<'static, HarnessEvent>;

    /// Stops the harness process and releases its resources.
    async fn shutdown(&mut self) -> Result<(), HarnessError>;

    /// The harness-native session id, once the harness reported it.
    ///
    /// Adapters that cannot report one keep the default (`None`); the daemon
    /// then simply cannot resume that session across a restart.
    fn session_id(&self) -> Option<String> {
        None
    }

    /// A hint for the orb/glyph state derived from the harness's own activity.
    ///
    /// The daemon's projection owns the authoritative state; this is the
    /// adapter's best local read (tool names, stream events) and defaults to
    /// [`AgentState::Idle`] for adapters that do not derive one.
    fn agent_state(&self) -> AgentState {
        AgentState::Idle
    }

    /// Fine-grained accounting for the most recent completed turn.
    fn last_turn(&self) -> Option<TurnReport> {
        None
    }

    /// Paths the session's mutating file tools touched, for the diff viewer.
    fn touched_paths(&self) -> Vec<String> {
        Vec::new()
    }

    /// The tail of the harness's stderr, for error surfaces.
    fn stderr_tail(&self) -> String {
        String::new()
    }
}

/// A harness adapter.
#[async_trait]
pub trait Harness: Send + Sync {
    /// Which harness this adapter implements.
    fn kind(&self) -> HarnessKind;

    /// Detects an installation on this host.
    fn detect(&self) -> Option<HarnessInstall>;

    /// Starts (or resumes) a session.
    async fn start_session(
        &self,
        spec: SessionSpec,
    ) -> Result<Box<dyn HarnessSession>, HarnessError>;
}
