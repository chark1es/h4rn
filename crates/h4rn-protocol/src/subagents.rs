//! Cross-harness subagents (ADR-038, ADR-039).
//!
//! A subagent is an ordinary h4rn thread whose `parent_thread_id` points at the
//! thread that delegated the work. That is the whole trick: the child gets the
//! same lifecycle, journal, approval and usage machinery as a top-level thread,
//! and the UI can open it like any other conversation. These types add the
//! delegation view on top: who spawned it, what context it received, its
//! budget, and the handoff result.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::common::{AgentState, HarnessKind, Usage};
use crate::ids::{HostId, SubagentId, ThreadId};

/// Lifecycle state of one subagent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    /// The child thread row exists but its harness session has not started.
    Starting,
    /// The child is working on its task.
    Running,
    /// The child is blocked on an approval the parent inbox is showing.
    AwaitingApproval,
    /// The child hit a usage limit and is waiting for its auto-resume.
    ///
    /// The child thread keeps the daemon's normal auto-resume schedule, so the
    /// parent card shows a countdown instead of a stalled child.
    Scheduled,
    /// The child finished and its result is available.
    Completed,
    /// The child failed before producing a result.
    Failed,
    /// The user or the parent cancelled the child.
    Cancelled,
    /// A turn, wall-time or token budget was exhausted.
    BudgetExceeded,
}

impl SubagentState {
    /// Whether no further work will happen for this subagent.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::BudgetExceeded
        )
    }
}

/// Whether a child shares the parent's workspace or gets its own worktree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SubagentIsolation {
    /// Run in the parent's workspace. Edits are visible to both immediately.
    #[default]
    Shared,
    /// Run in a detached git worktree under the daemon data directory.
    Worktree,
}

/// How the parent handed context to the child.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SubagentContextKind {
    /// Only the task text.
    None,
    /// The task plus a caller-authored summary of the parent conversation.
    Summary,
    /// The task plus the contents of selected workspace files.
    Files,
}

/// The context block delivered with a subagent's first prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubagentContext {
    /// Only the task text is delivered.
    #[default]
    None,
    /// A summary written by the caller is delivered with the task.
    Summary {
        /// Caller-authored summary of the parent conversation.
        summary: String,
    },
    /// Selected workspace-relative files are delivered with the task.
    Files {
        /// Workspace-relative paths whose contents are included.
        paths: Vec<String>,
        /// Optional summary appended after the files.
        #[serde(default)]
        summary: Option<String>,
    },
}

impl SubagentContext {
    /// The discriminant, for compact views and UI labels.
    pub fn kind(&self) -> SubagentContextKind {
        match self {
            Self::None => SubagentContextKind::None,
            Self::Summary { .. } => SubagentContextKind::Summary,
            Self::Files { .. } => SubagentContextKind::Files,
        }
    }

    /// Paths the child received, empty for `None` and `Summary`.
    pub fn paths(&self) -> &[String] {
        match self {
            Self::Files { paths, .. } => paths,
            _ => &[],
        }
    }
}

/// Limits the daemon enforces on one subagent.
///
/// `None` means "use the daemon default", not "unlimited": an unbounded child
/// is exactly how a delegation loop burns a month of tokens overnight.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SubagentBudget {
    /// Maximum harness turns the child may start.
    #[ts(type = "number | null")]
    pub max_turns: Option<u32>,
    /// Maximum wall-clock time in seconds.
    #[ts(type = "number | null")]
    pub max_wall_time_secs: Option<u64>,
    /// Maximum cumulative tokens (input plus output).
    #[ts(type = "number | null")]
    pub max_tokens: Option<u64>,
}

/// Live counters rendered beside the child's budget bar.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SubagentProgress {
    /// Harness turns started.
    #[ts(type = "number")]
    pub turns_started: u32,
    /// Harness turns completed.
    #[ts(type = "number")]
    pub turns_completed: u32,
    /// Cumulative tokens reported by the harness.
    #[ts(type = "number")]
    pub tokens: u64,
    /// Wall-clock milliseconds since the child row was created.
    #[ts(type = "number")]
    pub elapsed_ms: u64,
    /// One-line description of what the child is doing, when known.
    pub current: Option<String>,
}

/// One file the child changed, summarised from the worktree status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SubagentFileChange {
    /// Workspace-relative path.
    pub path: String,
    /// Porcelain status (`M`, `A`, `D`, `??`, ...).
    pub status: String,
}

/// What a finished child hands back to its parent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SubagentResult {
    /// Final assistant text from the child's last turn.
    pub text: String,
    /// Usage the child consumed.
    pub usage: Usage,
    /// Files the child changed, when its workspace was a worktree.
    pub files_changed: Vec<SubagentFileChange>,
    /// Redacted failure message, when the child failed or exceeded a budget.
    pub error: Option<String>,
}

/// Which credential a child used, for the detail panel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SubagentCredential {
    /// Human label ("Claude subscription", "CLIProxyAPI: work").
    pub label: String,
    /// Whether the child shares the parent session's credential.
    pub shared_with_parent: bool,
}

/// One subagent as the UI and the parent agent see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SubagentView {
    /// Stable subagent id (`sa_...`).
    pub subagent_id: SubagentId,
    /// Thread that delegated the work.
    pub parent_thread_id: ThreadId,
    /// The child thread, once its row exists.
    pub child_thread_id: Option<ThreadId>,
    /// The task text the caller supplied.
    pub task: String,
    /// Harness driving the child.
    pub harness: HarnessKind,
    /// Host running the child.
    pub host: HostId,
    /// Model slug.
    pub model: String,
    /// Shared workspace or isolated worktree.
    pub isolation: SubagentIsolation,
    /// Absolute path the child runs in.
    pub workspace_path: String,
    /// Worktree branch, when isolated.
    pub branch: Option<String>,
    /// Nesting depth, 1 for a child of a top-level thread.
    #[ts(type = "number")]
    pub depth: u32,
    /// Current state.
    pub state: SubagentState,
    /// Orb/glyph state mirrored from the child thread.
    pub agent_state: AgentState,
    /// Live counters.
    pub progress: SubagentProgress,
    /// Limits in force.
    pub budget: SubagentBudget,
    /// Context the caller handed over.
    pub context: SubagentContext,
    /// Credential provenance.
    pub credential: SubagentCredential,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// When the child harness session started, if it did.
    pub started_at: Option<DateTime<Utc>>,
    /// When the child reached a terminal state.
    pub finished_at: Option<DateTime<Utc>>,
    /// Result, once the child finished.
    pub result: Option<SubagentResult>,
}

impl SubagentView {
    /// Whether no further work will happen for this subagent.
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }
}

/// A request to spawn a child thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SpawnSubagentRequest {
    /// Thread that delegates the work and receives the approvals.
    pub parent_thread: ThreadId,
    /// Task the child must perform.
    pub task: String,
    /// Harness to run; defaults to the parent's harness.
    #[serde(default)]
    pub harness: Option<HarnessKind>,
    /// Host to run on; only the local host is supported today.
    #[serde(default)]
    pub host: Option<HostId>,
    /// Model slug; defaults to the parent's model.
    #[serde(default)]
    pub model: Option<String>,
    /// Shared workspace or isolated worktree.
    #[serde(default)]
    pub isolation: SubagentIsolation,
    /// Context handed to the child.
    #[serde(default)]
    pub context: SubagentContext,
    /// Per-subagent limits; omitted fields use daemon defaults.
    #[serde(default)]
    pub budget: SubagentBudget,
}

/// One subagent operation (ADR-038).
///
/// Every operation rides this envelope so the delegation surface can grow
/// without adding a top-level [`crate::ClientRequest`] discriminant each time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SubagentRequest {
    /// List the children of one parent thread.
    ListSubagents {
        /// Parent to list; `None` lists the whole daemon.
        #[serde(default)]
        parent_thread: Option<ThreadId>,
        /// Include children that already reached a terminal state.
        #[serde(default)]
        include_finished: bool,
    },
    /// Fetch one subagent.
    GetSubagent {
        /// Subagent to fetch.
        subagent_id: SubagentId,
    },
    /// Cancel one subagent and clean up its worktree.
    CancelSubagent {
        /// Subagent to cancel.
        subagent_id: SubagentId,
    },
    /// Spawn a child thread (the UI Delegate button and the MCP tool).
    SpawnSubagent {
        /// Fields of the new child.
        request: SpawnSubagentRequest,
    },
}

/// Default turn cap when the caller does not set one.
pub const DEFAULT_MAX_TURNS: u32 = 20;
/// Default wall-clock cap (15 minutes).
pub const DEFAULT_MAX_WALL_TIME_SECS: u64 = 900;
/// Default token cap (50k tokens).
pub const DEFAULT_MAX_TOKENS: u64 = 50_000;
/// Default nesting depth (a top-level thread may spawn children and
/// grandchildren, but not great-grandchildren).
pub const DEFAULT_MAX_DEPTH: u32 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_kinds_round_trip_with_a_stable_tag() {
        let context = SubagentContext::Files {
            paths: vec!["src/lib.rs".into()],
            summary: Some("look here".into()),
        };
        let value = serde_json::to_value(&context).unwrap();
        assert_eq!(value["kind"], "files");
        assert_eq!(value["paths"][0], "src/lib.rs");
        assert_eq!(context.kind(), SubagentContextKind::Files);
        assert_eq!(context.paths(), ["src/lib.rs".to_owned()]);
    }

    #[test]
    fn terminal_states_are_explicit() {
        assert!(SubagentState::Completed.is_terminal());
        assert!(SubagentState::BudgetExceeded.is_terminal());
        assert!(!SubagentState::Running.is_terminal());
        assert!(!SubagentState::AwaitingApproval.is_terminal());
        assert!(!SubagentState::Scheduled.is_terminal());
    }
}
