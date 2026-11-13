//! One `Harness` trait, four adapters (ADR-005).
//!
//! Every harness — Claude Code, Codex, OpenCode and Antigravity — is one
//! adapter behind [`Harness`]. The trait normalises the vendor protocols into
//! [`HarnessEvent`] so the daemon's projection and lifecycle never branch on
//! which harness is running. Capabilities are a separate concern (ADR-020);
//! Phase 1a ships the trait, the normalized event vocabulary and a
//! [`FakeHarness`] used by every downstream test.
//!
//! No real adapter is implemented here yet; the Claude Code adapter is the
//! next crate-level deliverable.

#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use h4rn_protocol::{
    ApprovalDecision, ApprovalKind, ApprovalOption, AttachmentId, HarnessKind, ToolCall,
    TurnStatus, Usage,
};
use serde::{Deserialize, Serialize};

mod fake;

pub use fake::{FakeHarness, FakeSession, test_spec};

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
    async fn respond_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), HarnessError>;

    /// Returns the normalized event stream.
    ///
    /// The stream ends when the session ends. Calling this twice returns an
    /// empty stream, so a session has exactly one consumer.
    fn events(&mut self) -> BoxStream<'static, HarnessEvent>;

    /// Stops the harness process and releases its resources.
    async fn shutdown(&mut self) -> Result<(), HarnessError>;
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
