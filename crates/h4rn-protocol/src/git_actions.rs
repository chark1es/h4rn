//! Git actions for a thread: status, commit, push and the text written for
//! commit messages and pull requests.
//!
//! Everything reported here comes from `git` on the host that owns the thread
//! workspace. Paths are workspace-relative, as git prints them, and each list
//! is bounded: a repository with thousands of changes still answers quickly,
//! and the client shows the count beside the first [`MAX_GIT_PATHS`] paths.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Most paths one change set carries.
///
/// `count` stays exact; only the paths are capped, so a large repository never
/// makes a status response unbounded.
pub const MAX_GIT_PATHS: usize = 100;

/// One kind of in-progress git operation that blocks a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum GitOperation {
    /// A rebase (`rebase-merge` or `rebase-apply`).
    Rebase,
    /// A merge waiting for its commit.
    Merge,
    /// A cherry-pick waiting for its commit.
    CherryPick,
}

impl GitOperation {
    /// A lowercase label for messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::Rebase => "rebase",
            Self::Merge => "merge",
            Self::CherryPick => "cherry-pick",
        }
    }
}

/// A bounded list of workspace-relative paths.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GitChangeSet {
    /// Exact number of changes.
    #[ts(type = "number")]
    pub count: u32,
    /// The first [`MAX_GIT_PATHS`] paths, sorted as git prints them.
    pub paths: Vec<String>,
    /// Whether `paths` stops before `count`.
    pub truncated: bool,
}

impl GitChangeSet {
    /// Builds a set from every path, keeping the count and capping the list.
    pub fn new<I: IntoIterator<Item = String>>(paths: I) -> Self {
        let all: Vec<String> = paths.into_iter().collect();
        let count = u32::try_from(all.len()).unwrap_or(u32::MAX);
        let truncated = all.len() > MAX_GIT_PATHS;
        Self {
            count,
            paths: all.into_iter().take(MAX_GIT_PATHS).collect(),
            truncated,
        }
    }
}

/// The state of a thread workspace as `git` reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GitThreadStatus {
    /// Checked-out branch; `None` for a detached `HEAD`.
    pub branch: Option<String>,
    /// Upstream branch (`origin/main`), when the branch has one.
    pub upstream: Option<String>,
    /// Commits on the branch that the upstream lacks.
    #[ts(type = "number")]
    pub ahead: u32,
    /// Commits on the upstream that the branch lacks.
    #[ts(type = "number")]
    pub behind: u32,
    /// Changes already in the index.
    pub staged: GitChangeSet,
    /// Tracked changes not in the index.
    pub unstaged: GitChangeSet,
    /// Files git does not track and is not told to ignore.
    pub untracked: GitChangeSet,
    /// A rebase, merge or cherry-pick in progress, which blocks a commit.
    pub operation: Option<GitOperation>,
    /// The branch a pull request should target by default.
    pub default_branch: Option<String>,
}

impl GitThreadStatus {
    /// Whether any staged, unstaged or untracked path exists.
    pub fn is_dirty(&self) -> bool {
        self.staged.count > 0 || self.unstaged.count > 0 || self.untracked.count > 0
    }
}

/// What a commit produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GitCommitResult {
    /// Full commit id.
    pub commit: String,
    /// Short id, as `git rev-parse --short` prints it.
    pub short_commit: String,
    /// First line of the commit message git recorded.
    pub subject: String,
    /// Fresh status after the commit.
    pub status: GitThreadStatus,
}

/// What a push produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GitPushResult {
    /// Branch that was pushed.
    pub branch: String,
    /// The remote branch this push updated (`origin/main`).
    ///
    /// This is the push target, not the branch's tracking upstream: the
    /// upstream changes only when the request asked to set it.
    pub remote_branch: String,
    /// Fresh status after the push.
    pub status: GitThreadStatus,
}
