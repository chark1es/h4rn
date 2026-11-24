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
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use h4rn_protocol::{
    AgentState, ApprovalDecision, ApprovalKind, ApprovalOption, AttachmentId, HarnessKind,
    RewindMethod, RuntimeMode, SessionRewind, ToolCall, TurnStatus, Usage,
};
use serde::{Deserialize, Serialize};

pub mod acp;
mod acp_agent;
mod antigravity;
mod attachments;
mod claude_code;
mod codex;
mod controls;
mod fake;
mod homes;
mod opencode;
pub mod registry;
pub mod stats;
#[cfg(test)]
mod test_scripts;
pub mod versions;

pub use controls::{
    CLAUDE_EFFORTS, CODEX_EFFORTS, SessionControls, ThreadControls, thread_controls,
};

pub use acp_agent::{
    AcpAgentConfigSource, GenericAcpHarness, StaticAcpAgents, probe_acp_agent, resolve_acp_command,
};
pub use antigravity::{
    AgyStreamParser, AntigravityHarness, ENV_AGY_MODE, ENV_AGY_PATH, ENV_AGY_TRANSPORT,
    ENV_APP_DATA_DIR, ENV_CLOUD_CODE_ENDPOINT, ENV_GEMINI_API_KEY, ENV_GEMINI_DIR,
    ENV_GEMINI_MODEL, ENV_GOOGLE_GEMINI_BASE_URL, detect_agy,
};
pub use attachments::{AttachmentClass, AttachmentDelivery, media_type, path_line, strategy};
pub use claude_code::{
    ClaudeCodeHarness, ClaudeCodeSession, ClaudeStreamParser, ENV_ANTHROPIC_AUTH_TOKEN,
    ENV_ANTHROPIC_BASE_URL, ENV_ANTHROPIC_MODEL, ENV_ANTHROPIC_SMALL_FAST_MODEL,
    ENV_GATEWAY_MODEL_DISCOVERY, ENV_MCP_AUTH_TOKEN, ENV_MCP_BEARER_TOKEN, ENV_PERMISSION_MODE,
    ENV_PERMISSION_PROMPT_TOOL, LimitSignal, TurnReport, build_user_frame, detect_claude,
    detect_limit, parse_reset_time, state_for_tool, tool_paths, tool_summary,
};
pub use codex::{
    ApprovalRequest, CodexHarness, CodexParser, CodexSession, ENV_API_KEY_ENV, ENV_BASE_URL,
    ENV_MODEL_PROVIDER, ENV_WIRE_API, approval_from_request, approval_response, build_turn_input,
    detect_codex,
};
pub use fake::{
    EventInjector, FakeHarness, FakeSession, Gate, InterruptCount, McpInjection, RewindSpecs,
    SendCount, SessionCount, SessionMcp, SessionModels, SessionSettings, SettingChange, test_spec,
};
pub use homes::{HomeError, harness_home, home_variable, resolve_home};
pub use opencode::{
    ENV_CLIPROXY_API_KEY, ENV_CLIPROXY_BASE_URL, ENV_OPENCODE_API_KEY, ENV_OPENCODE_BASE_URL,
    ENV_OPENCODE_MODELS, ENV_OPENCODE_PACKAGE, ENV_OPENCODE_PROVIDER, MINIMUM_OPENCODE_VERSION,
    OpenCodeHarness, OpenCodeParser, OpenCodeSession, config_content_for, detect_opencode,
    provider_config,
};
pub use registry::{HarnessRegistry, builtin_adapters};

/// How a harness kind drops rewound turns from its own history.
///
/// This is the static capability a thread row advertises before any session
/// exists. A live adapter still answers [`Harness::rewind_method`] itself, and
/// the daemon falls back to a handoff when the adapter cannot keep a native
/// promise (for example because the thread never had a provider session).
pub fn rewind_method(kind: HarnessKind) -> RewindMethod {
    match kind {
        HarnessKind::ClaudeCode | HarnessKind::Codex => RewindMethod::Native,
        HarnessKind::Antigravity | HarnessKind::Acp => RewindMethod::Handoff,
        HarnessKind::Opencode => RewindMethod::Handoff,
    }
}
pub use versions::{
    NpmRegistry, ProcessUpdateRunner, RegistryClient, UpdateLine, UpdateRunner,
    detect_update_method, is_newer, npm_package, parse_registry_latest, parse_version,
    self_update_args, update_command,
};

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
    /// Permission autonomy chosen when the thread was created (or last
    /// changed).
    pub runtime_mode: RuntimeMode,
    /// Reasoning effort in the harness's own vocabulary, when the thread asked
    /// for one.
    pub reasoning_effort: Option<String>,
    /// Whether the thread asked for the harness's fast tier.
    pub fast: bool,
    /// Harness session to resume, when resuming.
    pub resume_id: Option<String>,
    /// The single h4rn MCP hub endpoint injected into the session.
    pub mcp_endpoint: Option<String>,
    /// Extra environment variables (never secrets in argv).
    pub env_overrides: BTreeMap<String, String>,
    /// Attachments that must be delivered with the first prompt.
    pub attachments: Vec<AttachmentRef>,
    /// Configured ACP agent instance, for [`HarnessKind::Acp`].
    pub agent_id: Option<String>,
    /// A provider history boundary this start must apply before the first
    /// prompt (a rewind the previous session did not live to apply).
    ///
    /// An adapter whose harness cannot apply the named boundary refuses the
    /// start; the daemon only sets the one its adapter advertised.
    pub rewind: Option<SessionRewind>,
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
    /// The agent published a plan, replacing any earlier one.
    Plan {
        /// Every step, in the agent's order.
        entries: Vec<h4rn_protocol::PlanStep>,
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
    ///
    /// This is the ordinary diagnostic row: it is always published as its own
    /// error. A transport that died is reported as [`Self::TransportClosed`]
    /// instead, so the two are never confused.
    Error {
        /// What happened.
        message: String,
        /// Whether a retry is safe.
        retryable: bool,
    },
    /// The session's write transport died under the turn.
    ///
    /// This is the same death the daemon reports when a prompt write fails, so
    /// it is tagged where it is produced (the ACP bridge's writer-error path)
    /// rather than inferred from the session's health afterwards: only these
    /// two reports of one death are collapsed into one row, and any other
    /// diagnostic still gets its own.
    ///
    /// A report without an exit status or stderr yet (the bridge's generic
    /// stdout-close notice) is `retryable: true`; the report that carries the
    /// exit status and the drained stderr is `retryable: false`. A death
    /// reported between turns is held for the first report and published from
    /// the second, so the row is never the generic notice.
    ///
    /// `kind` says which part of the transport died. Only [`Exit`] is the
    /// process-exit report that can carry the provider's reset: a read error is
    /// also `retryable: false`, but it is not the last account of the death.
    ///
    /// [`Exit`]: TransportCloseKind::Exit
    TransportClosed {
        /// What happened.
        message: String,
        /// Whether a retry is safe.
        retryable: bool,
        /// Which end of the transport reported the death.
        kind: TransportCloseKind,
        /// The provider reset the exit report's stderr named, when it named
        /// one.
        ///
        /// Only an [`Exit`](TransportCloseKind::Exit) report can carry this:
        /// the daemon would otherwise have to infer the reset from a document
        /// beside the report, and a death reported without the usual typed
        /// limit notification would silently lose it.
        #[serde(default)]
        reset_at: Option<DateTime<Utc>>,
    },
}

/// Which part of a session's transport reported its death.
///
/// A transport can die in several ways, and one of them is final: only the
/// agent's exit report can still carry its stderr and a provider reset after
/// the stream is already unusable. The others may be followed by that report
/// within the same bounded window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TransportCloseKind {
    /// The agent's stdout ended (the bridge's generic close notice).
    Eof,
    /// Reading the agent's stdout failed.
    Read,
    /// Writing to the agent failed, timed out, or the writer exited.
    Write,
    /// The agent process exited and was reaped. Its report is the specific
    /// account of the death, with the status and stderr it drained.
    Exit,
}

/// The more specific of two messages that describe one failure of the same
/// kind.
///
/// The agent's stderr is the most specific detail either report can add and it
/// always spans an extra line, so a multi-line message wins; between equally
/// detailed notices the longer one is kept. The adapter and the daemon share
/// this rule so their merged failure rows pick the same text. Only reports of
/// one kind are compared this way: [`merge_transport_detail`] decides between
/// the generic close notice and the exit/status report.
pub fn most_specific_detail(first: String, second: String) -> String {
    let first_lines = first.lines().count();
    let second_lines = second.lines().count();
    match second_lines.cmp(&first_lines) {
        std::cmp::Ordering::Greater => second,
        std::cmp::Ordering::Less => first,
        std::cmp::Ordering::Equal => {
            if second.len() > first.len() {
                second
            } else {
                first
            }
        }
    }
}

/// Names one dead transport's failure row from two reports of it.
///
/// A report that carries the exit status or the drained stderr
/// (`retryable: false`) is always the specific account of the death, even when
/// its text is shorter than the generic stdout-close notice
/// (`retryable: true`); a generic notice that arrives after it adds nothing.
/// Only two reports of the same kind are compared by [`most_specific_detail`].
/// The exit report's own message already appends its stderr when it drained
/// any.
pub fn merge_transport_detail(
    held: String,
    held_retryable: bool,
    message: String,
    retryable: bool,
) -> String {
    match (held_retryable, retryable) {
        (true, false) => message,
        (false, true) => held,
        _ => most_specific_detail(held, message),
    }
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
    /// The harness refused a prompt because of a provider usage limit.
    ///
    /// Only an adapter that parsed the limit out of its own failure detail
    /// raises this. The daemon arms an auto-resume from this typed signal and
    /// never from an arbitrary error's text, so a transport failure that
    /// merely mentions a limit (an HTTP 413 payload limit, say) stays an
    /// ordinary failed turn.
    ///
    /// The display is the message alone: the adapter already names the harness
    /// and the failure, and the daemon prefixes the limit for the user.
    #[error("{message}")]
    RateLimited {
        /// Harness involved.
        harness: HarnessKind,
        /// The failure detail, exactly as the harness reported it.
        message: String,
        /// The limit the adapter parsed, with its reset when the detail named
        /// one.
        limit: LimitSignal,
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
    /// This live session cannot apply a value it accepts, but the next session
    /// start can.
    ///
    /// Unlike [`Self::Unsupported`] the value is valid. The daemon records it,
    /// shows it, and starts a session that really uses it on the next turn
    /// instead of refusing the change. The one case today is an ACP agent's
    /// default-model alias: ACP has no "reset to the agent's default" method
    /// and the catalogue does not name which entry is the default, so a live
    /// session cannot honour the alias while a fresh one can.
    #[error("{harness:?} will apply {feature} on the next session")]
    Deferred {
        /// Harness involved.
        harness: HarnessKind,
        /// The value that is deferred.
        feature: String,
    },
}

impl HarnessError {
    /// The provider limit this failure carries, when its adapter parsed one.
    ///
    /// This is the daemon's only limit signal from a failed write: an error
    /// that does not carry one is an ordinary failure, whatever its text says.
    pub fn limit(&self) -> Option<&LimitSignal> {
        match self {
            Self::RateLimited { limit, .. } => Some(limit),
            _ => None,
        }
    }
}

/// A lock-free view of a session's transport health.
///
/// The daemon's event pump must never wait on a session's own lock: `send` may
/// hold that lock for the length of a harness turn, and an adapter with a
/// bounded event queue would then be unable to deliver the reply `send` waits
/// for. A session that can lose its transport mid-turn publishes that fact here
/// when it happens, so the pump (and the daemon's other non-blocking readers)
/// can see it without locking.
///
/// The first close wins and keeps the reason it was closed for, so a request
/// rejected afterwards names what actually happened (a stdout EOF is not a
/// write failure).
#[derive(Clone, Debug, Default)]
pub struct TransportHealth(std::sync::Arc<std::sync::atomic::AtomicU8>);

/// [`TransportHealth`]'s sentinel for a transport that is still open.
const TRANSPORT_OPEN: u8 = 0;

impl TransportCloseKind {
    /// The byte [`TransportHealth`] stores for this kind.
    const fn code(self) -> u8 {
        match self {
            Self::Eof => 1,
            Self::Read => 2,
            Self::Write => 3,
            Self::Exit => 4,
        }
    }

    /// The kind a stored byte names, if the transport has closed.
    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Eof),
            2 => Some(Self::Read),
            3 => Some(Self::Write),
            4 => Some(Self::Exit),
            _ => None,
        }
    }
}

impl TransportHealth {
    /// A transport that is still healthy.
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks the transport closed. Every later read sees it.
    ///
    /// The reason is the general writer failure; a path that observed a
    /// specific close uses [`Self::close_with`] instead.
    pub fn close(&self) {
        self.close_with(TransportCloseKind::Write);
    }

    /// Marks the transport closed for the reason `kind` observed.
    ///
    /// The first close wins: a later teardown (a shutdown after the reader saw
    /// EOF) must not rewrite the cause that actually ended the transport.
    pub fn close_with(&self, kind: TransportCloseKind) {
        let _ = self.0.compare_exchange(
            TRANSPORT_OPEN,
            kind.code(),
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    /// Whether the transport has closed.
    pub fn is_closed(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst) != TRANSPORT_OPEN
    }

    /// The close the transport observed, when one was recorded.
    pub fn close_kind(&self) -> Option<TransportCloseKind> {
        TransportCloseKind::from_code(self.0.load(std::sync::atomic::Ordering::SeqCst))
    }
}

/// A running harness session.
#[async_trait]
pub trait HarnessSession: Send {
    /// Which harness this session belongs to.
    ///
    /// The session already knows; the daemon uses it for errors that name the
    /// adapter when a session-level switch is not supported.
    fn kind(&self) -> HarnessKind;

    /// Sends a user message to the harness.
    async fn send(&mut self, input: UserInput) -> Result<(), HarnessError>;

    /// Interrupts the running turn without killing the session.
    ///
    /// The daemon holds this session's lock across both the claim and the wait
    /// for `send` to return, so an adapter may keep issuing the cancel without
    /// racing a prompt that is still being written: by the time `interrupt`
    /// runs, a prompt whose `send` returned has already reached the harness.
    async fn interrupt(&mut self) -> Result<(), HarnessError>;

    /// Sends a user message tagged with the prompt id the harness records.
    ///
    /// The daemon tags a turn with its own UUID so a later native rewind can
    /// name the message it drops. Adapters that do not record prompt ids keep
    /// the default, which ignores the tag.
    async fn send_tagged(
        &mut self,
        input: UserInput,
        _prompt_id: Option<&str>,
    ) -> Result<(), HarnessError> {
        self.send(input).await
    }

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

    /// Whether the transport behind this session is already gone.
    ///
    /// A failed write can kill the transport without the event stream ending,
    /// so the daemon asks this before it detaches the session: the next
    /// message then starts a fresh one instead of writing to a dead pipe.
    /// Callers that must not wait for the session lock read
    /// [`Self::transport_health`] instead.
    fn is_closed(&self) -> bool {
        false
    }

    /// A lock-free view of this session's transport health.
    ///
    /// Adapters whose transport can die asynchronously (the ACP bridge, which
    /// reports a writer failure on its own stream) publish it here. The daemon's
    /// event pump samples this instead of [`Self::is_closed`], so draining an
    /// adapter's events never waits on a `send` that holds the session lock.
    fn transport_health(&self) -> Option<TransportHealth> {
        None
    }

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

    /// Thread settings this live session advertised, when the adapter's
    /// catalogue only exists at runtime.
    ///
    /// `None` means the harness's static [`thread_controls`] are authoritative.
    fn controls(&self) -> Option<SessionControls> {
        None
    }

    /// Switches the live session to `mode`.
    ///
    /// An adapter without an in-place permission switch answers
    /// [`HarnessError::Unsupported`]; the daemon then keeps the change for the
    /// next session start instead of pretending it applied now.
    async fn set_runtime_mode(&mut self, _mode: RuntimeMode) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported {
            harness: self.kind(),
            feature: "switching the permission mode on a live session".into(),
        })
    }

    /// Switches the live session to `model`.
    ///
    /// The next turn runs on it; a conversation already in progress keeps its
    /// history. A valid value this session cannot apply but a fresh one can
    /// (ACP's default-model alias) answers [`HarnessError::Deferred`], which the
    /// daemon records and applies from the next session instead of refusing.
    async fn set_model(&mut self, _model: &str) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported {
            harness: self.kind(),
            feature: "switching the model on a live session".into(),
        })
    }

    /// Switches the live session to `effort` (`None` is the harness default).
    async fn set_reasoning_effort(&mut self, _effort: Option<&str>) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported {
            harness: self.kind(),
            feature: "switching reasoning effort on a live session".into(),
        })
    }

    /// Switches the live session to (or off) the harness's fast tier.
    async fn set_fast(&mut self, _fast: bool) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported {
            harness: self.kind(),
            feature: "switching the fast tier on a live session".into(),
        })
    }
}

/// One configured adapter instance, resolved at a single instant.
///
/// A thread stores the fingerprint beside its resume id, and the same snapshot
/// is what a start launches. Reading the configuration twice would let an edit
/// between those two steps fingerprint one command line and run another, so the
/// daemon resolves it once and carries this value.
#[derive(Clone)]
pub struct InstanceSnapshot {
    fingerprint: String,
    payload: Arc<dyn std::any::Any + Send + Sync>,
}

impl std::fmt::Debug for InstanceSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InstanceSnapshot")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl InstanceSnapshot {
    /// Wraps the adapter-owned configuration a snapshot was resolved from.
    pub fn new<T: Send + Sync + 'static>(fingerprint: impl Into<String>, payload: T) -> Self {
        Self {
            fingerprint: fingerprint.into(),
            payload: Arc::new(payload),
        }
    }

    /// The stable fingerprint the daemon stores with the thread's resume id.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The adapter's own configuration, when this snapshot is its kind.
    pub fn payload<T: 'static>(&self) -> Option<&T> {
        self.payload.downcast_ref::<T>()
    }
}

/// A harness adapter.
#[async_trait]
pub trait Harness: Send + Sync {
    /// Which harness this adapter implements.
    fn kind(&self) -> HarnessKind;

    /// Detects an installation on this host.
    fn detect(&self) -> Option<HarnessInstall>;

    /// How this harness drops rewound turns from its own history.
    ///
    /// A native harness truncates the provider conversation; every other
    /// harness starts a fresh session with a bounded handoff of the kept
    /// history (ADR-047). Adapters must not claim a method they cannot honour.
    fn rewind_method(&self) -> RewindMethod {
        RewindMethod::Handoff
    }

    /// Whether this adapter can run the named configured instance.
    ///
    /// Only a multi-instance adapter (the generic ACP harness) has instances;
    /// built-in adapters answer `false` and are selected by kind alone.
    fn has_instance(&self, _instance: &str) -> bool {
        false
    }

    /// The directory one configured instance actually runs in.
    ///
    /// A thread workspace is the default; an adapter whose configuration fixes
    /// the directory overrides it, so the thread row and Files/Changes name the
    /// directory the agent really uses. Built-in adapters run in the thread
    /// workspace and answer `None`.
    fn instance_workspace(&self, _instance: &str, _workspace: &std::path::Path) -> Option<PathBuf> {
        None
    }

    /// Whether one configured instance ignores the thread's workspace because
    /// its directory is fixed (an ACP agent configured with a fixed folder).
    ///
    /// A thread asking for a new worktree cannot give such an agent the new
    /// checkout, so the daemon refuses the request instead of creating a
    /// checkout nothing runs in. Built-in adapters answer `false`.
    fn instance_fixes_workspace(&self, _instance: &str) -> bool {
        false
    }

    /// Resolves one configured instance, for a thread whose workspace is
    /// `workspace`, into the fingerprint a thread stores beside its resume id
    /// and the exact configuration a start must launch.
    ///
    /// Built-in adapters have one command per kind and answer `None`.
    fn resolve_instance(
        &self,
        _instance: &str,
        _workspace: &std::path::Path,
    ) -> Option<InstanceSnapshot> {
        None
    }

    /// A stable fingerprint of the command line behind one configured
    /// instance, for a thread whose workspace is `workspace`.
    ///
    /// The daemon stores it beside a thread's resume id: when the command line
    /// or the effective working directory changes, the old session id names a
    /// conversation the new command never saw, so the next start opens a fresh
    /// session.
    fn instance_fingerprint(&self, instance: &str, workspace: &std::path::Path) -> Option<String> {
        self.resolve_instance(instance, workspace)
            .map(|snapshot| snapshot.fingerprint().to_owned())
    }

    /// Starts (or resumes) a session.
    async fn start_session(
        &self,
        spec: SessionSpec,
    ) -> Result<Box<dyn HarnessSession>, HarnessError>;

    /// Starts a session from configuration resolved earlier.
    ///
    /// The default ignores the snapshot: an adapter with nothing to snapshot
    /// behaves exactly like [`Self::start_session`]. An adapter that offers
    /// [`Self::resolve_instance`] launches the snapshot's configuration so the
    /// fingerprint and the process can never disagree.
    async fn start_session_with(
        &self,
        spec: SessionSpec,
        _snapshot: Option<InstanceSnapshot>,
    ) -> Result<Box<dyn HarnessSession>, HarnessError> {
        self.start_session(spec).await
    }
}
