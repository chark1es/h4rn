//! Issues from forge trackers and Linear, shared by the daemon and the UI.
//!
//! Forge issues use the same normalised shape for every provider (ADR-001,
//! ADR-002); Linear keeps its own row because its identifiers, workflow states
//! and comments come from a different model. Both can be linked to a thread;
//! the link stores only the tracker coordinates so its state is always read
//! live.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::forge::{ForgeLabel, ForgePrState, ForgeRepoRef};

/// Whether an issue is open or closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ForgeIssueState {
    /// Still open.
    Open,
    /// Closed, with or without a reason.
    Closed,
}

/// Which issues to list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ForgeIssueFilter {
    /// Open issues, newest first.
    Open,
    /// Closed issues, newest first.
    Closed,
    /// Every issue, used by the forge's own search.
    All,
    /// Issues assigned to the authenticated account.
    Assigned,
    /// Issues the authenticated account opened.
    Created,
    /// Issues that mention the authenticated account.
    Mentioning,
}

/// Why an issue was closed, where the tracker records one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ForgeIssueCloseReason {
    /// Work finished.
    Completed,
    /// Not planned.
    NotPlanned,
    /// Superseded by another issue.
    Duplicate,
}

/// A person named on an issue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeIssuePerson {
    /// Forge login.
    pub login: String,
    /// Human name, when the forge reports one.
    pub name: Option<String>,
}

/// A pull request the tracker says relates to an issue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeIssueLinkedPr {
    /// PR/MR number.
    pub number: u32,
    /// Title.
    pub title: String,
    /// State known to the tracker, when it reports one.
    #[serde(default)]
    pub state: Option<ForgePrState>,
    /// Browser URL.
    pub web_url: String,
}

/// A row in the issue list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeIssueSummary {
    /// Repository the issue belongs to.
    pub repo: ForgeRepoRef,
    /// Issue number.
    pub number: u32,
    /// Title.
    pub title: String,
    /// Open or closed.
    pub state: ForgeIssueState,
    /// Author login.
    pub author: String,
    /// Labels, quiet and flat.
    pub labels: Vec<ForgeLabel>,
    /// Assignees.
    pub assignees: Vec<ForgeIssuePerson>,
    /// Number of comments the tracker reports.
    pub comment_count: u32,
    /// Last update on the tracker, when it reports one.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    /// Browser URL.
    pub web_url: String,
}

/// One issue's detail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeIssue {
    /// Summary fields shared with the list.
    pub summary: ForgeIssueSummary,
    /// Markdown body.
    pub body: String,
    /// When it was opened, when the tracker reports it.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// When it was closed.
    pub closed_at: Option<DateTime<Utc>>,
    /// Close reason, where the tracker records one.
    pub close_reason: Option<ForgeIssueCloseReason>,
    /// Pull requests the tracker links to this issue.
    pub linked_pull_requests: Vec<ForgeIssueLinkedPr>,
}

/// One issue comment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeIssueComment {
    /// Provider comment id.
    pub comment_id: String,
    /// Author login.
    pub author: String,
    /// Markdown body.
    pub body: String,
    /// When it was written.
    pub created_at: DateTime<Utc>,
    /// Last edit, when edited.
    pub updated_at: Option<DateTime<Utc>>,
    /// Browser URL.
    pub web_url: Option<String>,
}

/// One page of issue rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeIssuePage {
    /// Repository.
    pub repo: ForgeRepoRef,
    /// Matching issues.
    pub issues: Vec<ForgeIssueSummary>,
    /// Page number, starting at 1.
    pub page: u32,
    /// Another page exists.
    pub has_more: bool,
}

/// One issue's comments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeIssueCommentPage {
    /// Repository.
    pub repo: ForgeRepoRef,
    /// Issue number.
    pub number: u32,
    /// Comments, oldest first.
    pub comments: Vec<ForgeIssueComment>,
    /// The tracker has more comments than this page returned.
    pub has_more: bool,
    /// The provider continuation for the next page, carried unchanged.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// The fields a write request may change.
///
/// `None` leaves a field alone. The daemon only sends the fields the caller
/// named, so an edit never rewrites labels or assignees by accident.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeIssueUpdate {
    /// New title.
    #[serde(default)]
    pub title: Option<String>,
    /// New markdown body.
    #[serde(default)]
    pub body: Option<String>,
    /// Close or reopen the issue.
    #[serde(default)]
    pub state: Option<ForgeIssueState>,
    /// Why it was closed, where the tracker records one.
    #[serde(default)]
    pub close_reason: Option<ForgeIssueCloseReason>,
    /// The issue this one duplicates, for a duplicate close reason.
    #[serde(default)]
    pub duplicate_issue_id: Option<u32>,
    /// Replacement label names.
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// Replacement assignee logins.
    #[serde(default)]
    pub assignees: Option<Vec<String>>,
}

impl ForgeIssueUpdate {
    /// Whether anything would change.
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.body.is_none()
            && self.state.is_none()
            && self.close_reason.is_none()
            && self.duplicate_issue_id.is_none()
            && self.labels.is_none()
            && self.assignees.is_none()
    }
}

/// What one forge's issue tracker can do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeIssueCapabilities {
    /// Issues can be listed.
    pub read: bool,
    /// Titles, bodies, state, labels and assignees can be changed.
    pub write: bool,
    /// New comments can be posted.
    pub comments: bool,
    /// Labels can be replaced.
    pub labels: bool,
    /// Assignees can be replaced.
    pub assignees: bool,
    /// Closing records a reason.
    pub close_reason: bool,
    /// The tracker has a search endpoint.
    pub search: bool,
    /// The assigned-to-me filter is available.
    pub assigned_filter: bool,
    /// The opened-by-me filter is available.
    pub created_filter: bool,
    /// The mentions-me filter is available.
    pub mentioning_filter: bool,
    /// Detail reports linked pull requests.
    pub linked_pull_requests: bool,
}

/// A connected Linear account. The API key never crosses this contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LinearAccount {
    /// Daemon-chosen connection id.
    pub account_id: String,
    /// Workspace display name, when Linear reports one.
    pub name: String,
    /// Account email, when Linear reports one.
    pub email: Option<String>,
    /// When the key was validated and stored.
    pub created_at: DateTime<Utc>,
}

/// One Linear team.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LinearTeam {
    /// Linear team id.
    pub id: String,
    /// Short key (`ENG`).
    pub key: String,
    /// Display name.
    pub name: String,
}

/// A project's chosen Linear team.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LinearProjectTeam {
    /// h4rn project id.
    pub project_id: String,
    /// Linear connection.
    pub account_id: String,
    /// Linear team id.
    pub team_id: String,
    /// Linear team key, for display without another round trip.
    pub team_key: String,
    /// Linear team name.
    pub team_name: String,
}

/// Linear's workflow state family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum LinearStateType {
    /// Not started, in the backlog.
    Backlog,
    /// Not started, planned.
    Unstarted,
    /// In progress.
    Started,
    /// Done.
    Completed,
    /// Cancelled.
    Canceled,
}

/// One Linear issue row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LinearIssueSummary {
    /// Linear issue id.
    pub id: String,
    /// Human identifier (`ENG-123`).
    pub identifier: String,
    /// Title.
    pub title: String,
    /// Workflow state name.
    pub state_name: String,
    /// Workflow state family.
    pub state_type: LinearStateType,
    /// Team id.
    pub team_id: String,
    /// Team key.
    pub team_key: String,
    /// Assignee display names.
    pub assignees: Vec<String>,
    /// Label names.
    pub labels: Vec<String>,
    /// Last update, when Linear reports one.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    /// Browser URL.
    pub url: String,
}

/// One Linear issue's detail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LinearIssue {
    /// Summary fields shared with the list.
    pub summary: LinearIssueSummary,
    /// Markdown description.
    pub description: String,
    /// When it was opened, when Linear reports it.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// Comments, oldest first.
    pub comments: Vec<LinearIssueComment>,
    /// The provider continuation for the next comment page, when Linear has
    /// more comments than this page returned.
    #[serde(default)]
    pub comments_next_cursor: Option<String>,
    /// Pull requests Linear links to the issue.
    pub linked_pull_requests: Vec<LinearIssueLinkedPr>,
}

/// One Linear comment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LinearIssueComment {
    /// Linear comment id.
    pub id: String,
    /// Author display name.
    pub author: String,
    /// Markdown body.
    pub body: String,
    /// When it was written.
    pub created_at: DateTime<Utc>,
    /// Last edit, when edited.
    pub updated_at: Option<DateTime<Utc>>,
}

/// A pull request Linear attaches to an issue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LinearIssueLinkedPr {
    /// PR title.
    pub title: String,
    /// Browser URL.
    pub url: String,
    /// State Linear reports.
    pub state: String,
}

/// One page of Linear issues.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LinearIssuePage {
    /// Matching issues.
    pub issues: Vec<LinearIssueSummary>,
    /// Cursor for the next page, when there is one.
    pub next_cursor: Option<String>,
}

/// Where a linked issue lives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "tracker", rename_all = "snake_case")]
pub enum IssueScope {
    /// A forge repository.
    Forge {
        /// Repository.
        repo: ForgeRepoRef,
    },
    /// A Linear team.
    Linear {
        /// Team id.
        team_id: String,
    },
}

/// The latest state of a linked issue, from whichever tracker owns it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "tracker", rename_all = "snake_case")]
pub enum LinkedIssueState {
    /// A forge issue.
    Forge {
        /// Latest detail.
        issue: ForgeIssueSummary,
    },
    /// A Linear issue.
    Linear {
        /// Latest detail.
        issue: LinearIssueSummary,
    },
}

/// An issue linked to a thread.
///
/// The link is durable metadata (scope and issue id). The state is read live;
/// a failed read is reported as such rather than kept from the last success.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ThreadIssue {
    /// Tracker coordinates.
    pub scope: IssueScope,
    /// Provider issue id. Forge issues use the decimal number.
    pub issue_id: String,
    /// Account the daemon reads it with.
    pub account_id: String,
    /// Latest state read from the tracker.
    pub state: Option<LinkedIssueState>,
    /// Why the latest read failed, when it did.
    pub error: Option<String>,
    /// When the link was created.
    pub linked_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_scope_is_internally_tagged() {
        let scope = IssueScope::Linear {
            team_id: "team_1".into(),
        };
        let value = serde_json::to_value(&scope).unwrap();
        assert_eq!(value["tracker"], "linear");
        assert_eq!(value["team_id"], "team_1");
    }

    #[test]
    fn an_empty_update_changes_nothing() {
        assert!(ForgeIssueUpdate::default().is_empty());
        assert!(
            !ForgeIssueUpdate {
                state: Some(ForgeIssueState::Closed),
                ..ForgeIssueUpdate::default()
            }
            .is_empty()
        );
    }
}
