//! Tracked git worktrees (subagent isolation, thread workspaces, harness-made).
//!
//! h4rn creates worktrees for isolated subagents, and threads (or the harness
//! CLIs under them) work in or create others. These types describe what the
//! daemon knows about each one so Settings can show what is in use, what is
//! settled, and let the owner clear the leftovers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ids::{SubagentId, ThreadId};

/// Who created a tracked worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeOrigin {
    /// h4rn created it for a worktree-isolated subagent and owns its directory
    /// and its `h4rn/sa-*` branch.
    Subagent,
    /// A thread's (or subagent child's) workspace is a linked worktree h4rn did
    /// not create.
    Thread,
    /// A linked worktree of a thread's repository that the agent or its
    /// harness-native subagents created or touched during a turn.
    Harness,
}

/// Whether anything is using a tracked worktree right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeStatus {
    /// In use: its subagent has not finished, or a thread that accessed it is
    /// working.
    Active,
    /// Work finished and nothing running uses it.
    Settled,
    /// An owned subagent worktree whose subagent is finished but whose
    /// directory is still on disk (cleanup failed or the daemon restarted
    /// mid-run).
    Orphaned,
}

/// One thread (or subagent child) that used a worktree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct WorktreeAccess {
    /// The thread that used the worktree.
    pub thread_id: ThreadId,
    /// The thread's title; empty when the thread no longer exists.
    pub title: String,
    /// Set when the thread is a subagent child.
    pub subagent_id: Option<SubagentId>,
    /// Whether the thread is busy (running, queued, scheduled, awaiting
    /// approval, or pinned in Working).
    pub working: bool,
    /// When the daemon last saw this thread use the worktree.
    pub last_seen: DateTime<Utc>,
}

/// One worktree h4rn tracks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TrackedWorktree {
    /// Absolute worktree path; the stable key.
    pub path: String,
    /// Path of the repository's main worktree.
    pub repo: String,
    /// Checked-out branch, when known.
    pub branch: Option<String>,
    /// Who created it.
    pub origin: WorktreeOrigin,
    /// Whether anything is using it.
    pub status: WorktreeStatus,
    /// Whether the directory is on disk.
    pub exists: bool,
    /// Uncommitted changes; `None` when the directory is missing or git failed.
    pub dirty: Option<bool>,
    /// h4rn created it, so clearing may delete its `h4rn/sa-*` branch.
    pub owned: bool,
    /// The subagent that owns it, for `subagent` origin.
    pub subagent_id: Option<SubagentId>,
    /// The subagent's task, for owned entries.
    pub task: Option<String>,
    /// Threads that used it.
    pub accessed_by: Vec<WorktreeAccess>,
    /// When the daemon first saw it.
    pub first_seen: DateTime<Utc>,
    /// When the daemon last saw it in use.
    pub last_seen: DateTime<Utc>,
}

/// A worktree a clear request left in place, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SkippedWorktree {
    /// Absolute worktree path.
    pub path: String,
    /// Human-readable reason.
    pub reason: String,
}
