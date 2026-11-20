//! Shared value types used by both requests and events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::ids::{DeviceId, HostId, SubagentId, ThreadId};
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
}

impl HarnessKind {
    /// The stable lowercase identifier used in blobatar seeds and config keys.
    pub fn slug(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Antigravity => "antigravity",
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
/// adapter uses `--permission-mode`. It is fixed for the life of a thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    /// Every command, edit and network action needs an approval.
    #[default]
    ApprovalRequired,
    /// File edits are accepted; commands and network access still ask.
    AutoAcceptEdits,
    /// No approvals; the harness may run unattended.
    FullAccess,
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
    /// Human title.
    pub title: String,
    /// Workspace path on the host.
    pub workspace_path: String,
    /// Current git branch of the workspace, when it has one.
    #[serde(default)]
    #[ts(optional)]
    pub branch: Option<String>,
    /// Model slug reported by the harness.
    pub model: String,
    /// Derived tab placement.
    pub tab: ThreadTab,
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
    /// Whether a finished turn has not been acknowledged.
    pub unread: bool,
    /// When auto-resume will try this thread again, when scheduled (ADR-024).
    #[serde(default)]
    pub resume_at: Option<DateTime<Utc>>,
    /// The limit currently blocking the thread, when one was observed.
    #[serde(default)]
    pub limit: Option<crate::usage::LimitBrief>,
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

    fn summary_with(
        resume_at: Option<DateTime<Utc>>,
        limit: Option<crate::usage::LimitBrief>,
    ) -> ThreadSummary {
        ThreadSummary {
            thread_id: ThreadId::new("th_summary"),
            parent_thread_id: None,
            host: HostId::new("host_test"),
            harness: HarnessKind::ClaudeCode,
            title: "usage limit".into(),
            workspace_path: "/tmp/work".into(),
            branch: Some("h4rn/usage-limit".into()),
            model: "claude-sonnet-5".into(),
            tab: ThreadTab::Working,
            pinned: false,
            snoozed_until: None,
            last_activity: Utc::now(),
            agent_state: AgentState::Scheduled,
            usage: Usage::default(),
            ttft_ms: None,
            context_window: None,
            context_fill_percent: None,
            needs_attention: false,
            unread: false,
            resume_at,
            limit,
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
}
