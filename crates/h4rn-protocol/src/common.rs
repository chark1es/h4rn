//! Shared value types used by both requests and events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::ids::{DeviceId, HostId, SubagentId, ThreadId, TurnId};
use crate::rewind::RewindMethod;
use crate::subagents::SubagentState;

/// The coding harness driving a thread (ADR-005).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum HarnessKind {
    /// Anthropic Claude Code (`claude -p ... stream-json`).
    ClaudeCode,
    /// OpenAI Codex (`codex app-server`).
    Codex,
    /// OpenCode (`opencode serve` over HTTP + SSE).
    Opencode,
    /// Google Antigravity (`agy acp`, ACP-first).
    Antigravity,
    /// A user-configured local ACP command.
    ///
    /// The executable, arguments and environment are stored by the daemon;
    /// the thread names the configured instance in its `agent_id`.
    Acp,
}

impl HarnessKind {
    /// The stable lowercase identifier used in blobatar seeds and config keys.
    pub fn slug(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Antigravity => "antigravity",
            Self::Acp => "acp",
        }
    }
}

/// The three thread tabs (ADR-004).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ThreadTab {
    /// A turn is running, queued, awaiting a hand or scheduled to resume.
    Working,
    /// The last turn finished and the user has not acknowledged it.
    Done,
    /// Acknowledged, auto-settled or closed by a linked branch.
    Settled,
}

/// Agent state rendered by the orb/static-glyph mapping in `DESIGN.md`.
///
/// This is the exact set of states the design system maps; the UI owns the
/// table that turns each value into an orb mode or a static glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Alive but not working; an explicit "Keep in Working" pin or queued.
    Idle,
    /// A turn is queued behind another run (alive, not working).
    Queued,
    /// Auto-resume is armed and waiting for the provider's reset clock.
    Scheduled,
    /// The host running the thread is unreachable.
    Unreachable,
    /// Model reasoning.
    Thinking,
    /// Web search, grep or glob.
    Searching,
    /// Reading a file or URL.
    Reading,
    /// Writing a file or producing a diff.
    Editing,
    /// Executing a tool, build or command.
    RunningTool,
    /// A decision is required from the user.
    AwaitingApproval,
    /// A usage limit was hit; auto-resume may be armed.
    RateLimited,
    /// The last action failed.
    Error,
    /// The last turn completed.
    Done,
}

/// How much autonomy a harness session is granted (ADR-005).
///
/// The daemon maps this to the harness's own permission switch; the Claude Code
/// adapter uses `--permission-mode`. A change applies to the live session where
/// the harness can switch in place, or at the next session start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    /// Every command, edit and network action needs an approval.
    #[default]
    ApprovalRequired,
    /// File edits are accepted; commands and network access still ask.
    AutoAcceptEdits,
    /// Read, plan and report without changing the workspace.
    ///
    /// Only offered for harnesses with a real plan mode (Claude Code and ACP
    /// agents that advertise one); every other adapter refuses it.
    Plan,
    /// No approvals; the harness may run unattended.
    FullAccess,
}

impl RuntimeMode {
    /// How much autonomy the mode grants, for clamping.
    ///
    /// `Plan` writes nothing, so it sits at the same floor as
    /// `ApprovalRequired`; the two differ in what the agent may *attempt*, not
    /// in whether a write can happen unattended.
    fn autonomy(self) -> u8 {
        match self {
            Self::ApprovalRequired | Self::Plan => 0,
            Self::AutoAcceptEdits => 1,
            Self::FullAccess => 2,
        }
    }

    /// The stricter of `self` and `ceiling`.
    ///
    /// A message queued by an outside agent carries the most autonomy its grant
    /// allows. The turn runs with the thread's own mode clamped to that: raising
    /// the thread's mode afterwards can never lift the ceiling of a message that
    /// was already queued.
    pub fn clamped_to(self, ceiling: Self) -> Self {
        if self.autonomy() <= ceiling.autonomy() {
            self
        } else {
            ceiling
        }
    }
}

/// Whether to run the thread in an isolated worktree.
///
/// `true` asks the daemon to generate a branch name, while a string names the
/// branch explicitly. Serialised untagged so the wire value is a bare JSON
/// boolean or string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(untagged)]
pub enum WorktreeRequest {
    /// Create a worktree on a daemon-generated branch.
    Generated(bool),
    /// Create a worktree checked out from this branch name.
    Named(String),
}

impl WorktreeRequest {
    /// The branch name to use, or `None` when the daemon must generate one.
    pub fn branch(&self) -> Option<&str> {
        match self {
            Self::Generated(_) => None,
            Self::Named(branch) => Some(branch),
        }
    }

    /// Whether the request actually asks for a worktree.
    pub fn is_requested(&self) -> bool {
        match self {
            Self::Generated(requested) => *requested,
            Self::Named(_) => true,
        }
    }
}

/// How a turn finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    /// The harness reported a normal completion.
    Completed,
    /// The harness reported an error.
    Failed,
    /// The user or daemon interrupted the turn.
    Interrupted,
    /// The agent stopped because its reply hit the output token limit.
    TokenLimit,
    /// The agent stopped because it hit its request limit for this turn.
    TurnLimit,
}

/// Lifecycle state of one tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// The tool is still running.
    Running,
    /// The tool returned successfully.
    Completed,
    /// The tool returned an error.
    Failed,
    /// The approval for the tool was denied.
    Denied,
}

/// The kind of hand a harness is raising.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// Run a shell command.
    Command,
    /// Apply a file change or diff.
    FileChange,
    /// Use a network, filesystem or provider permission.
    Permission,
    /// Approve a plan before execution.
    Plan,
    /// Answer a question rather than allow an action.
    Question,
}

/// A user's response to an approval request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Allow this one action.
    Approve,
    /// Refuse the action.
    Deny,
    /// Abandon the turn.
    Cancel,
}

/// One selectable answer attached to an approval request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ApprovalOption {
    /// Stable option id echoed back with [`ApprovalDecision`].
    pub id: String,
    /// Human label shown on the standby bar.
    pub label: String,
}

/// A subscribed event topic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    /// Thread list and thread stream events.
    Threads,
    /// Host presence and inventory changes.
    Hosts,
    /// Usage and limit events.
    Usage,
    /// MCP server health and inventory.
    Mcp,
    /// Skill inventory changes.
    Skills,
    /// Pull-request and forge events.
    Forge,
    /// CLIProxyAPI capability and configuration events.
    Proxy,
    /// Everything the connection is allowed to see.
    All,
}

/// Identity of the connected client, sent once in [`crate::ClientRequest::Hello`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DeviceInfo {
    /// The paired device id.
    pub device_id: DeviceId,
    /// Human display name ("chark's iPhone").
    pub name: String,
    /// `linux`, `macos`, `windows`, `android`, `ios` or `web`.
    pub platform: String,
    /// Client application version.
    pub app_version: String,
}

/// One daemon host as shown in the host list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct HostSummary {
    /// Host id.
    pub host_id: HostId,
    /// Human label.
    pub name: String,
    /// Operating system or environment ("linux", "wsl:Ubuntu").
    pub platform: String,
    /// Whether a live connection to the daemon exists.
    pub online: bool,
    /// Last time the host was seen.
    pub last_seen: DateTime<Utc>,
    /// Harnesses detected on this host.
    pub harnesses: Vec<HarnessKind>,
    /// Round-trip time of the active transport path, when the client measured
    /// one. The daemon does not know the client's path, so this is `None` on
    /// the wire and the client overlays its own live value.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub latency_ms: Option<u32>,
    /// Physical path carrying the session (`direct`, `tailscale`, `relay`).
    /// Filled by the client from `TransportStatus`; the daemon reports `None`.
    #[serde(default)]
    pub path: Option<crate::transport::TransportPathKind>,
}

/// Token and cost accounting for a turn or a thread (ADR-024).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Usage {
    /// Prompt tokens, including cached input.
    #[ts(type = "number")]
    pub input_tokens: u64,
    /// Completion tokens.
    #[ts(type = "number")]
    pub output_tokens: u64,
    /// Input tokens served from cache.
    #[ts(type = "number")]
    pub cached_input_tokens: u64,
    /// Cost in millionths of a US dollar, to avoid floating-point drift.
    #[ts(type = "number")]
    pub cost_micros: u64,
    /// Wall-clock duration of the turn.
    #[ts(type = "number")]
    pub duration_ms: u64,
    /// Time spent generating, wall time minus tool time. Set by the meter
    /// when a turn closes; adapters leave it zero.
    #[serde(default)]
    #[ts(type = "number")]
    pub generation_ms: u64,
    /// Output tokens per second of generation time (tool time excluded),
    /// rounded; zero when unknown. Derived by [`Usage::refresh_tps`], never by
    /// a client.
    #[serde(default)]
    #[ts(type = "number")]
    pub tps: u64,
}

impl Usage {
    /// Recomputes [`Usage::tps`] from the output tokens and generation time.
    pub fn refresh_tps(&mut self) {
        self.tps = if self.generation_ms == 0 {
            0
        } else {
            let tokens = u128::from(self.output_tokens) * 1000;
            let millis = u128::from(self.generation_ms);
            u64::try_from((tokens + millis / 2) / millis).unwrap_or(u64::MAX)
        };
    }

    /// Percent (0-100) of `window` the billed tokens fill; `None` without a
    /// window.
    pub fn context_fill_percent(&self, window: Option<u64>) -> Option<u8> {
        let window = window.filter(|window| *window > 0)?;
        let filled = u128::from(self.total_tokens()) * 100 / u128::from(window);
        Some(u8::try_from(filled.min(100)).unwrap_or(100))
    }

    /// Total tokens billed for the turn.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Adds another usage record into this one.
    pub fn merge(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.cost_micros = self.cost_micros.saturating_add(other.cost_micros);
        self.duration_ms = self.duration_ms.saturating_add(other.duration_ms);
        self.generation_ms = self.generation_ms.saturating_add(other.generation_ms);
        self.refresh_tps();
    }
}

/// Where a thread was forked from.
///
/// Only the ids are recorded: the origin title is read from the thread list,
/// so a rename of the source shows in every fork without rewriting them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForkRef {
    /// Thread the fork was copied from.
    pub thread_id: ThreadId,
    /// Turn the fork's transcript ends at, inclusive.
    pub turn_id: TurnId,
}

/// A thread as projected for the list pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ThreadSummary {
    /// Thread id.
    pub thread_id: ThreadId,
    /// Thread that delegated this one, when this is a subagent.
    ///
    /// Child threads are hidden from the top-level list unless the filter asks
    /// for them; the subagent panel reads the tree through this field.
    #[serde(default)]
    pub parent_thread_id: Option<ThreadId>,
    /// Host running the thread.
    pub host: HostId,
    /// Harness driving the thread.
    pub harness: HarnessKind,
    /// Harness the conversation was imported from, when it was imported.
    ///
    /// The thread can later switch harness, so this is the one durable answer
    /// to "where did this conversation come from?"; the thread's current
    /// `harness` no longer names it.
    #[serde(default)]
    #[ts(optional)]
    pub imported_from: Option<HarnessKind>,
    /// Configured ACP agent instance, when `harness` is [`HarnessKind::Acp`].
    ///
    /// Older daemons did not have this field; threads written before it
    /// deserialize with `None` and keep running on their built-in harness.
    #[serde(default)]
    #[ts(optional)]
    pub agent_id: Option<String>,
    /// Human title.
    pub title: String,
    /// Workspace path on the host.
    pub workspace_path: String,
    /// Checkout this thread belongs to for grouping, when it differs from the
    /// workspace it runs in.
    ///
    /// A conversation imported from a linked worktree still runs in that
    /// worktree, but it belongs beside the main checkout in the sidebar.
    #[serde(default)]
    #[ts(optional)]
    pub project_path: Option<String>,
    /// Thread and turn this thread was forked from, when it was.
    ///
    /// A fork is otherwise an ordinary thread and holds no live state of its
    /// source, so the origin disappearing never affects it.
    #[serde(default)]
    #[ts(optional)]
    pub forked_from: Option<ForkRef>,
    /// Current git branch of the workspace, when it has one.
    #[serde(default)]
    #[ts(optional)]
    pub branch: Option<String>,
    /// Model slug reported by the harness.
    pub model: String,
    /// Permission autonomy for the thread.
    ///
    /// Older daemons did not report the live setting; their threads
    /// deserialize as [`RuntimeMode::ApprovalRequired`], the old fixed mode.
    #[serde(default)]
    pub runtime_mode: RuntimeMode,
    /// Reasoning effort for the thread, in the harness's own vocabulary.
    ///
    /// `None` leaves the harness on its own default. Harnesses without a real
    /// effort control never report a value here.
    #[serde(default)]
    #[ts(optional)]
    pub reasoning_effort: Option<String>,
    /// Effort values the live session advertised.
    ///
    /// Empty until a session names a catalogue. A configured ACP agent only
    /// reveals its effort option at session start, so the picker fills from
    /// here rather than guessing.
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
    /// Model ids the live session advertised.
    ///
    /// Empty until a session names a catalogue. Codex publishes its
    /// `model/list` at session start, and the picker offers exactly those ids
    /// (plus the current one) instead of the adapter's static guess. An older
    /// daemon's thread has none, so the picker falls back to the static list.
    #[serde(default)]
    pub models: Vec<String>,
    /// Whether the thread asks the harness for its fast tier.
    ///
    /// Only harnesses with a real fast tier (Codex service tiers) report this.
    #[serde(default)]
    pub fast: bool,
    /// Derived tab placement.
    pub tab: ThreadTab,
    /// Whether the current tab is where the person just placed the thread.
    ///
    /// A manual move is a placement, not a lifecycle transition: the client
    /// that moved it is not "finishing" a turn, and every other client sees
    /// the same summary. Notifiers stay quiet for a transition this names.
    #[serde(default)]
    pub placed_by_user: bool,
    /// A hand-set Working pin (cleared when the next turn starts).
    pub pinned: bool,
    /// Snooze expiry, when snoozed.
    pub snoozed_until: Option<DateTime<Utc>>,
    /// Last user-authored message or turn activity.
    pub last_activity: DateTime<Utc>,
    /// Current agent state.
    pub agent_state: AgentState,
    /// Cumulative usage for the thread.
    pub usage: Usage,
    /// Time to first token of the most recent metered turn (ADR-024).
    #[serde(default)]
    #[ts(type = "number | null")]
    pub ttft_ms: Option<u64>,
    /// Context window of the active model, when the proxy catalog reports it.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub context_window: Option<u64>,
    /// Share of the context window the thread has filled, 0-100, when the
    /// window is known. Derived here so clients only format it.
    #[serde(default)]
    #[ts(type = "number | null")]
    pub context_fill_percent: Option<u8>,
    /// Whether the thread is raising a hand.
    pub needs_attention: bool,
    /// Guest messages waiting for the owner to send or dismiss (ADR-044).
    #[serde(default)]
    pub pending_guest_requests: u32,
    /// Whether a finished turn has not been acknowledged.
    pub unread: bool,
    /// When auto-resume will try this thread again, when scheduled (ADR-024).
    #[serde(default)]
    pub resume_at: Option<DateTime<Utc>>,
    /// How this thread's harness drops rewound turns from its own history.
    ///
    /// A client offers **Edit from here** when this is not
    /// [`RewindMethod::Unsupported`], and says which method will run.
    #[serde(default)]
    pub rewind: RewindMethod,
    /// Whether the workspace is the thread's own linked git worktree.
    ///
    /// Only then can a rewind put files back to the pre-message checkpoint;
    /// a thread in the project's main checkout rewinds its conversation only.
    #[serde(default)]
    pub file_rewind: bool,
    /// The limit currently blocking the thread, when one was observed.
    #[serde(default)]
    pub limit: Option<crate::usage::LimitBrief>,
    /// How many messages wait for the thread's next turn.
    ///
    /// The contents are not in the summary: a full queue can be larger than the
    /// paired transport's frame. A client reads them with
    /// [`crate::ClientRequest::GetThreadQueue`] and refetches when
    /// `queue_revision` changes. A queued message is durable and not yet in the
    /// transcript; it is delivered as the next turn's first message, or steered
    /// into the running turn by an explicit Send now.
    #[serde(default)]
    pub queued_count: u32,
    /// A counter that changes whenever the queue's contents or order change.
    ///
    /// Clients compare it with the revision their cached queue came from: a
    /// different value means the cached rows are stale and must be refetched.
    /// It is a per-daemon-run counter, not a durable sequence.
    #[serde(default)]
    #[ts(type = "number")]
    pub queue_revision: u64,
    /// The turn that is running now, when one is.
    ///
    /// A client steers by naming it, so a turn that ended in the meantime is
    /// refused rather than catching a newer one. It stays set while a running
    /// turn is paused by a limit, which is why it is the honest answer to "is a
    /// turn running" and `agent_state` alone is not.
    #[serde(default)]
    #[ts(optional)]
    pub active_turn_id: Option<crate::ids::TurnId>,
    /// Whether the running harness can accept a message mid-turn.
    ///
    /// Capabilities are advertised per harness; a client offers Steer only when
    /// this is true and a turn is running.
    #[serde(default)]
    pub steer: bool,
}

/// One message waiting for a thread's next turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct QueuedMessage {
    /// Stable id used to edit, remove, move or steer this message.
    pub message_id: crate::ids::QueuedMessageId,
    /// Message text.
    pub text: String,
    /// Attachments delivered with the message, in the order they were added.
    #[serde(default)]
    pub attachments: Vec<crate::blob::Attachment>,
    /// When the message was queued.
    pub queued_at: DateTime<Utc>,
    /// Whether Send now can steer this row into the running turn.
    ///
    /// A row with its own permission ceiling, or one waiting on a harness
    /// without mid-turn input, is moved to the front instead. The hint and the
    /// button use this so they never promise a steer the daemon will not make.
    #[serde(default)]
    pub steerable: bool,
    /// Whether this row may already have reached the agent once.
    ///
    /// Set when the daemon stopped after handing the message to the adapter
    /// but before it knew the outcome. The row is never sent automatically;
    /// the person can send it again or discard it.
    #[serde(default)]
    pub delivery_unknown: bool,
}

/// The most queue bytes one response carries.
///
/// A full queue is unbounded, but a paired transport refuses a frame over its
/// limit, so no response may carry the whole thing. Half the default 512 KiB
/// frame leaves room for the envelope and for the one message that overflows
/// the budget (a message that was queued had to arrive in a frame that fit).
pub const QUEUE_PAGE_BYTES: usize = 256 * 1024;

/// The most messages one queue page carries, whatever their size.
pub const QUEUE_PAGE_MESSAGES: usize = 100;

/// Selects one bounded page of `queued`, starting after `offset` messages.
///
/// Whole messages are only, in queue order: a page never splits a message, so a
/// client renders exactly what the person saved. The first message after
/// `offset` is always included, even when it alone is larger than
/// [`QUEUE_PAGE_BYTES`]; every message that could be queued had to fit a
/// transport frame to get there.
pub fn queue_page(queued: &[QueuedMessage], offset: usize) -> Vec<QueuedMessage> {
    let mut page = Vec::new();
    let mut bytes = 0usize;
    for message in queued.iter().skip(offset).take(QUEUE_PAGE_MESSAGES) {
        let size = serde_json::to_vec(message)
            .map(|encoded| encoded.len())
            .unwrap_or_default();
        if !page.is_empty() && bytes + size > QUEUE_PAGE_BYTES {
            break;
        }
        bytes += size;
        page.push(message.clone());
    }
    page
}

/// One thread setting a harness switch had to drop.
///
/// The new harness does not offer the setting the old one was using, so the
/// daemon records the value it dropped and the client can say which ones it
/// was. The value is display-ready: a mode is its protocol slug and an effort
/// is the harness's own word.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DroppedSetting {
    /// Which setting was dropped.
    pub setting: DroppedSettingKind,
    /// The value the thread had before the switch.
    pub value: String,
}

/// A thread setting a switch can drop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum DroppedSettingKind {
    /// The permission mode the old harness could run in.
    RuntimeMode,
    /// The reasoning effort the old harness accepted.
    ReasoningEffort,
    /// The old harness's fast tier.
    Fast,
}

/// One completed stream item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    /// A user, assistant or system message.
    Message {
        /// Stable item id.
        item_id: String,
        /// Who authored the message.
        role: MessageRole,
        /// Message text.
        text: String,
        /// Attachments delivered with the message, in the order they were sent.
        #[serde(default)]
        attachments: Vec<crate::blob::Attachment>,
    },
    /// A tool call and its result.
    ToolCall {
        /// Stable item id.
        item_id: String,
        /// The completed tool call.
        tool_call: ToolCall,
    },
    /// A parent delegated work to a child thread.
    ///
    /// Rendered inline on the parent transcript as the subagent orb plus its
    /// task and live state. The child thread itself is a normal thread with
    /// `parent_thread_id` set.
    Delegated {
        /// Stable item id.
        item_id: String,
        /// Subagent record this row follows.
        subagent_id: SubagentId,
        /// Child thread, once its row exists.
        thread_id: Option<ThreadId>,
        /// Task text.
        task: String,
        /// Harness driving the child.
        harness: HarnessKind,
        /// Host running the child.
        host: HostId,
        /// Model slug.
        model: String,
        /// Delegation lifecycle state.
        state: SubagentState,
        /// Orb/glyph state of the child.
        agent_state: AgentState,
    },
}

/// Author of a stream message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// The human.
    User,
    /// The agent.
    Assistant,
    /// The daemon or harness.
    System,
}

/// One step in an agent-authored plan.
///
/// ACP agents publish plans as a whole list on every `plan` update. The
/// status and priority stay strings so a vendor value this build does not know
/// is passed through instead of dropped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct PlanStep {
    /// Step text as the agent wrote it.
    pub content: String,
    /// `pending`, `in_progress` or `completed`; other values pass through.
    pub status: String,
    /// `high`, `medium` or `low`; other values pass through.
    #[serde(default)]
    pub priority: String,
}

/// A tool call rendered as one collapsible row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ToolCall {
    /// Stable tool-call id.
    pub tool_call_id: String,
    /// Tool name ("Bash", "Edit", "WebSearch").
    pub name: String,
    /// Current status.
    pub status: ToolCallStatus,
    /// One-line collapsed summary.
    pub summary: String,
    /// Raw arguments, if safe to show.
    pub input: Option<Value>,
    /// Raw result preview, if safe to show.
    pub output: Option<String>,
    /// When the call started.
    pub started_at: DateTime<Utc>,
    /// When the call finished, if it did.
    pub completed_at: Option<DateTime<Utc>>,
    /// What the call changed or found, derived in Rust from the tool's own
    /// result: the diffstat of an edit, the match count of a search. `None`
    /// for tools that report neither, or while the call is still running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub stats: Option<ToolStats>,
}

/// What a search counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum MatchUnit {
    /// Matching lines or occurrences.
    Matches,
    /// Matching files or paths (a glob, or a search that lists files).
    Files,
}

/// The one-number summary a tool row shows next to its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolStats {
    /// An edit or write: lines added and removed.
    Diff {
        /// Lines added.
        added: u32,
        /// Lines removed.
        removed: u32,
    },
    /// A search: how many results it returned.
    Matches {
        /// The count.
        count: u32,
        /// What was counted.
        unit: MatchUnit,
    },
    /// A test run: how many tests passed and failed.
    Tests {
        /// Tests that passed.
        passed: u32,
        /// Tests that failed.
        failed: u32,
    },
}

/// Filter applied by [`crate::ClientRequest::ListThreads`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ThreadFilter {
    /// Restrict to these tabs; empty means every tab.
    pub tabs: Vec<ThreadTab>,
    /// Restrict to one host.
    pub host: Option<HostId>,
    /// Restrict to one harness.
    pub harness: Option<HarnessKind>,
    /// Case-insensitive title search.
    pub search: Option<String>,
    /// Include snoozed threads in the result set.
    pub include_snoozed: bool,
    /// Include subagent child threads in the result set.
    ///
    /// Child rows are hidden by default so the top-level list and inbox stay
    /// about the user's own conversations; opening a parent asks for them
    /// explicitly.
    #[serde(default)]
    pub include_children: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_slugs_match_the_seed_scheme() {
        assert_eq!(HarnessKind::ClaudeCode.slug(), "claude-code");
        assert_eq!(HarnessKind::Antigravity.slug(), "antigravity");
        assert_eq!(HarnessKind::Acp.slug(), "acp");
    }

    #[test]
    fn tps_excludes_tool_time_and_context_fill_is_capped() {
        let mut usage = Usage {
            output_tokens: 500,
            input_tokens: 1_500,
            duration_ms: 20_000,
            generation_ms: 5_000,
            ..Usage::default()
        };
        usage.refresh_tps();
        // 500 tokens over 5 s of generation, not 20 s of wall time.
        assert_eq!(usage.tps, 100);
        assert_eq!(usage.context_fill_percent(Some(4_000)), Some(50));
        assert_eq!(usage.context_fill_percent(Some(100)), Some(100));
        assert_eq!(usage.context_fill_percent(None), None);
        assert_eq!(Usage::default().tps, 0);
    }

    #[test]
    fn usage_merge_saturates_instead_of_overflowing() {
        let mut usage = Usage {
            input_tokens: u64::MAX,
            ..Usage::default()
        };
        usage.merge(&Usage {
            input_tokens: 10,
            output_tokens: 2,
            ..Usage::default()
        });
        assert_eq!(usage.input_tokens, u64::MAX);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.total_tokens(), u64::MAX);
    }

    #[test]
    fn an_old_thread_summary_without_an_agent_id_still_deserializes() {
        let json = serde_json::json!({
            "thread_id": "th_old",
            "host": "host_local",
            "harness": "claude_code",
            "title": "old thread",
            "workspace_path": "/tmp/work",
            "model": "claude-sonnet-5",
            "tab": "settled",
            "pinned": false,
            "snoozed_until": null,
            "last_activity": "2026-01-01T00:00:00Z",
            "agent_state": "idle",
            "usage": Usage::default(),
            "needs_attention": false,
            "unread": false
        });
        let summary: ThreadSummary = serde_json::from_value(json).expect("old JSON");
        assert_eq!(summary.agent_id, None);
        assert_eq!(summary.harness, HarnessKind::ClaudeCode);
        // A thread written before thread controls had no live settings: it
        // deserializes on the fixed mode it was created with.
        assert_eq!(summary.runtime_mode, RuntimeMode::ApprovalRequired);
        assert_eq!(summary.reasoning_effort, None);
        assert!(!summary.fast);
    }

    #[test]
    fn an_old_thread_summary_without_pending_guest_requests_still_deserializes() {
        // Written by a daemon that predates guest links.
        let json = serde_json::json!({
            "thread_id": "th_pre_guest",
            "host": "host_local",
            "harness": "codex",
            "title": "before guest links",
            "workspace_path": "/tmp/work",
            "model": "gpt-5",
            "tab": "working",
            "pinned": false,
            "last_activity": "2026-01-01T00:00:00Z",
            "agent_state": "idle",
            "usage": Usage::default(),
            "needs_attention": false,
            "unread": false
        });
        let summary: ThreadSummary = serde_json::from_value(json).expect("old JSON");
        assert_eq!(summary.pending_guest_requests, 0);
    }

    #[test]
    fn a_thread_summary_round_trips_its_thread_controls() {
        let mut summary = summary_with(None, None);
        summary.runtime_mode = RuntimeMode::Plan;
        summary.reasoning_effort = Some("xhigh".into());
        summary.fast = true;

        let json = serde_json::to_string(&summary).expect("serialise the summary");
        let decoded: ThreadSummary = serde_json::from_str(&json).expect("parse the summary");
        assert_eq!(decoded, summary);
        assert_eq!(decoded.runtime_mode, RuntimeMode::Plan);
        assert_eq!(decoded.reasoning_effort.as_deref(), Some("xhigh"));
        assert!(decoded.fast);
    }

    fn summary_with(
        resume_at: Option<DateTime<Utc>>,
        limit: Option<crate::usage::LimitBrief>,
    ) -> ThreadSummary {
        ThreadSummary {
            thread_id: ThreadId::new("th_summary"),
            parent_thread_id: None,
            host: HostId::new("host_test"),
            harness: HarnessKind::ClaudeCode,
            imported_from: None,
            agent_id: None,
            title: "usage limit".into(),
            workspace_path: "/tmp/work".into(),
            project_path: None,
            forked_from: None,
            branch: Some("h4rn/usage-limit".into()),
            model: "claude-sonnet-5".into(),
            runtime_mode: RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            reasoning_efforts: Vec::new(),
            models: Vec::new(),
            fast: false,
            tab: ThreadTab::Working,
            placed_by_user: false,
            pinned: false,
            snoozed_until: None,
            last_activity: Utc::now(),
            agent_state: AgentState::Scheduled,
            usage: Usage::default(),
            ttft_ms: None,
            context_window: None,
            context_fill_percent: None,
            needs_attention: false,
            pending_guest_requests: 0,
            unread: false,
            resume_at,
            limit,
            rewind: Default::default(),
            file_rewind: false,
            queued_count: 0,
            queue_revision: 0,
            active_turn_id: None,
            steer: false,
        }
    }

    #[test]
    fn thread_summary_round_trips_resume_and_limit_state() {
        let at = Utc::now();
        let summary = summary_with(
            Some(at),
            Some(crate::usage::LimitBrief {
                harness: Some(HarnessKind::ClaudeCode),
                reset_at: Some(at),
                message: Some("usage limit reached".into()),
            }),
        );

        let json = serde_json::to_string(&summary).expect("serialise the summary");
        let decoded: ThreadSummary = serde_json::from_str(&json).expect("parse the summary");
        assert_eq!(decoded, summary);
        assert_eq!(decoded.resume_at, Some(at));
        assert_eq!(
            decoded.limit.and_then(|limit| limit.reset_at),
            Some(at),
            "the inbox can show the reset without opening the thread"
        );
    }

    #[test]
    fn thread_summary_without_metering_state_omits_it() {
        let summary = summary_with(None, None);
        let json = serde_json::to_value(&summary).expect("serialise the summary");
        assert!(json["resume_at"].is_null());
        assert!(json["limit"].is_null());
    }

    fn queued(text: &str) -> QueuedMessage {
        QueuedMessage {
            message_id: crate::ids::QueuedMessageId::new_uuid_v7(),
            text: text.into(),
            attachments: Vec::new(),
            queued_at: Utc::now(),
            steerable: false,
            delivery_unknown: false,
        }
    }

    #[test]
    fn a_queue_page_keeps_whole_messages_under_the_budget() {
        let queue: Vec<QueuedMessage> = (0..QUEUE_PAGE_MESSAGES + 5)
            .map(|index| queued(&format!("message {index}")))
            .collect();
        let page = queue_page(&queue, 0);
        assert_eq!(
            page.len(),
            QUEUE_PAGE_MESSAGES,
            "a page is bounded by count"
        );
        assert_eq!(page[0].message_id, queue[0].message_id);

        let next = queue_page(&queue, page.len());
        assert_eq!(next.len(), 5, "the rest is the next page");
        assert_eq!(next[0].message_id, queue[QUEUE_PAGE_MESSAGES].message_id);
    }

    #[test]
    fn a_queue_page_stops_before_the_byte_budget_and_never_splits_a_message() {
        let big = "x".repeat(QUEUE_PAGE_BYTES / 2);
        let queue = vec![queued(&big), queued(&big), queued("small")];
        let page = queue_page(&queue, 0);
        assert_eq!(
            page.len(),
            1,
            "a second message would put the page over the budget"
        );
        assert_eq!(page[0].text.len(), big.len(), "the text is whole");
        let rest = queue_page(&queue, 1);
        assert_eq!(
            rest.len(),
            2,
            "the next page starts with the second big message, and the small one still fits"
        );
        assert_eq!(rest[0].text.len(), big.len());
        assert_eq!(rest[1].text, "small");
    }

    #[test]
    fn an_oversized_first_message_is_still_returned_whole() {
        let huge = queued(&"y".repeat(QUEUE_PAGE_BYTES + 1024));
        let page = queue_page(std::slice::from_ref(&huge), 0);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].text.len(), huge.text.len());
    }
}
