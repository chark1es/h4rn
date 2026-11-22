//! Forge (pull request) types shared by the daemon, the UI and the agent.
//!
//! The daemon speaks one normalised model for every supported forge so the PR
//! view never branches on the provider (ADR-001, ADR-002). Provider-specific
//! shapes — GitHub check runs, GitLab pipelines, Gitea commit statuses,
//! Bitbucket build statuses, Azure DevOps policy evaluations — are flattened
//! into [`ForgeCheck`] and [`ForgeChecks`] here. Review comments keep their
//! inline anchor (path/line/side) and their thread so the UI can render a real
//! conversation and hand it to an agent.
//!
//! Credentials never cross this contract. [`ForgeAccount`] carries only public
//! metadata; the token lives in the daemon's secret store (ADR-016).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A supported forge implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
pub enum ForgeKind {
    /// github.com or a GitHub Enterprise Server (GHES) instance.
    #[serde(rename = "github")]
    GitHub,
    /// gitlab.com or a self-hosted GitLab instance.
    #[serde(rename = "gitlab")]
    GitLab,
    /// gitea.com or a self-hosted Gitea instance.
    #[serde(rename = "gitea")]
    Gitea,
    /// codeberg.org or a self-hosted Forgejo instance.
    #[serde(rename = "forgejo")]
    Forgejo,
    /// bitbucket.org (Bitbucket Cloud). Bitbucket Server and Data Center are
    /// not supported.
    #[serde(rename = "bitbucket")]
    Bitbucket,
    /// dev.azure.com or a legacy `*.visualstudio.com` organization (Azure
    /// DevOps Services). Azure DevOps Server is not supported.
    #[serde(rename = "azure_devops")]
    AzureDevOps,
}

impl ForgeKind {
    /// Stable lowercase identifier used in routes, config keys and blobatar
    /// seeds.
    pub fn slug(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitLab => "gitlab",
            Self::Gitea => "gitea",
            Self::Forgejo => "forgejo",
            Self::Bitbucket => "bitbucket",
            Self::AzureDevOps => "azure_devops",
        }
    }

    /// Human-readable name for the UI.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::GitLab => "GitLab",
            Self::Gitea => "Gitea",
            Self::Forgejo => "Forgejo",
            Self::Bitbucket => "Bitbucket Cloud",
            Self::AzureDevOps => "Azure DevOps",
        }
    }

    /// Parses a slug or display name.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "github" | "gh" => Some(Self::GitHub),
            "gitlab" | "gl" => Some(Self::GitLab),
            "gitea" => Some(Self::Gitea),
            "forgejo" | "codeberg" => Some(Self::Forgejo),
            "bitbucket" | "bitbucket_cloud" | "bitbucket-cloud" | "bitbucket cloud" => {
                Some(Self::Bitbucket)
            }
            "azure_devops" | "azure-devops" | "azuredevops" | "azure" | "azure devops" => {
                Some(Self::AzureDevOps)
            }
            _ => None,
        }
    }

    /// Whether the forge supports RFC 8628 device authorization.
    ///
    /// GitHub does. GitLab, Gitea, Forgejo, Bitbucket Cloud and Azure DevOps
    /// do not expose a device grant for this use case, so they use a personal
    /// access token or OAuth access token the user creates.
    pub fn supports_device_flow(self) -> bool {
        matches!(self, Self::GitHub)
    }
}

/// Where a stored credential came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ForgeCredentialSource {
    /// A personal access token the user pasted into the daemon.
    Pat,
    /// A token obtained through GitHub's OAuth device flow.
    OAuthDevice,
    /// Imported from `gh auth token` after the user explicitly asked.
    GhCli,
    /// Imported from `glab auth status --show-token` after the user asked.
    GlabCli,
    /// A Bitbucket Cloud API token, sent as HTTP Basic with the Atlassian
    /// account email as the username. (App passwords were retired in July
    /// 2026 and are no longer offered.)
    #[serde(rename = "bitbucket_api_token")]
    BitbucketApiToken,
    /// A Bitbucket Cloud OAuth or workspace access token, sent as Bearer.
    #[serde(rename = "bitbucket_access_token")]
    BitbucketAccessToken,
}

impl ForgeCredentialSource {
    /// Short label for the account row.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pat => "personal access token",
            Self::OAuthDevice => "OAuth device flow",
            Self::GhCli => "gh CLI",
            Self::GlabCli => "glab CLI",
            Self::BitbucketApiToken => "Bitbucket API token (email + token)",
            Self::BitbucketAccessToken => "Bitbucket access token",
        }
    }
}

/// What a forge implementation can do.
///
/// The UI uses this to hide actions that would silently do nothing and the
/// agent context renderer uses it to explain a degraded forge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeCapabilities {
    /// OAuth device authorization grant.
    pub device_flow: bool,
    /// Inline review comments grouped into resolvable threads.
    pub review_threads: bool,
    /// Threads can be resolved through the API.
    pub resolve_threads: bool,
    /// A reply stays nested under the original comment.
    pub nested_replies: bool,
    /// Review comments can be anchored to a path and line.
    pub inline_review_comments: bool,
    /// Check runs/statuses are available.
    pub checks: bool,
    /// CI is exposed as pipelines and jobs (GitLab).
    pub pipelines: bool,
    /// The API paginates diffs server-side.
    pub diff_pagination: bool,
    /// GraphQL is used for review threads.
    pub graphql: bool,
    /// Failed checks can be re-run through the API (GitHub check suites, GitLab
    /// pipelines). Gitea, Forgejo, Bitbucket Cloud and Azure DevOps expose no
    /// equivalent for the PR surface.
    pub rerun_checks: bool,
}

/// One configured forge account.
///
/// `account_id` is daemon-chosen and stable across restarts. There may be
/// several accounts for the same forge (work and personal, two GHES hosts).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeAccount {
    /// Daemon-chosen account id.
    pub account_id: String,
    /// Forge implementation.
    pub forge: ForgeKind,
    /// Public web base URL, no trailing slash (`https://github.com`).
    pub base_url: String,
    /// Authenticated login, when the forge reports one.
    pub username: Option<String>,
    /// Human name shown beside the login.
    pub display_name: Option<String>,
    /// Scopes the token reports, for the least-privilege warning.
    pub scopes: Vec<String>,
    /// Where the credential came from.
    pub source: ForgeCredentialSource,
    /// When the account was added.
    pub created_at: DateTime<Utc>,
}

/// A repository address independent of any one remote.
///
/// `owner` may contain `/` on GitLab (nested groups); on every other forge it
/// is a single path segment. `base_url` is the public web URL; the client
/// derives the API URL from the forge kind. Azure DevOps keeps the
/// organization in `base_url` (`https://dev.azure.com/acme`) and uses `owner`
/// for the project, so [`ForgeRepoRef::web_url`] inserts the `/_git/` segment.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
pub struct ForgeRepoRef {
    /// Forge implementation.
    pub forge: ForgeKind,
    /// Public web base URL, no trailing slash.
    pub base_url: String,
    /// Owner, GitLab namespace, or Azure DevOps project.
    pub owner: String,
    /// Repository name without `.git`.
    pub repo: String,
}

impl ForgeRepoRef {
    /// `owner/repo`, with nested GitLab namespaces preserved.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// Browser URL for the repository.
    pub fn web_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        if self.forge == ForgeKind::AzureDevOps {
            format!("{base}/{}/_git/{}", self.owner, self.repo)
        } else {
            format!("{base}/{}/{}", self.owner, self.repo)
        }
    }
}

/// One repository found in a workspace's git remotes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeRemote {
    /// Git remote name (`origin`, `upstream`, ...).
    pub remote: String,
    /// Parsed repository.
    pub repo: ForgeRepoRef,
    /// Original remote URL, with any embedded credentials removed.
    pub url: String,
}

/// Which pull requests to list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ForgePrFilter {
    /// Every open pull request in the repository.
    Open,
    /// Pull requests authored by the authenticated account.
    Mine,
    /// Open pull requests where the authenticated account is a requested
    /// reviewer.
    ReviewRequested,
    /// Open pull requests whose source branch is `branch`.
    ByBranch {
        /// Source branch name.
        branch: String,
    },
}

/// Lifecycle state of a pull request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ForgePrState {
    /// Open and ready for review.
    Open,
    /// Open but marked as a draft.
    Draft,
    /// Merged.
    Merged,
    /// Closed without merging.
    Closed,
}

/// Whether the forge believes the branch can merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ForgeMergeable {
    /// No conflicts reported.
    Mergeable,
    /// The forge reports conflicts.
    Conflicting,
    /// The forge has not computed it yet, or the API cannot say.
    Unknown,
}

/// Aggregate review decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ForgeReviewState {
    /// At least one approving review and no outstanding change request.
    Approved,
    /// A reviewer requested changes.
    ChangesRequested,
    /// Review comments without a decision.
    Commented,
    /// A review is requested but not submitted.
    Pending,
    /// A review was dismissed.
    Dismissed,
}

/// One reviewer and their current decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeReview {
    /// Reviewer login.
    pub author: String,
    /// Decision.
    pub state: ForgeReviewState,
    /// Submission time, when the review was submitted.
    pub submitted_at: Option<DateTime<Utc>>,
    /// Review body, when one was left.
    pub body: Option<String>,
}

/// One label.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeLabel {
    /// Label text.
    pub name: String,
    /// Six-digit hex colour without `#`, when the forge provides one.
    pub color: Option<String>,
}

/// A row in the PR list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgePrSummary {
    /// Repository the PR belongs to.
    pub repo: ForgeRepoRef,
    /// PR/MR number (`#482`).
    pub number: u32,
    /// Title.
    pub title: String,
    /// Open draft merged or closed.
    pub state: ForgePrState,
    /// Author login.
    pub author: String,
    /// Source branch.
    pub head_branch: String,
    /// Target branch.
    pub base_branch: String,
    /// Head commit SHA, when known.
    pub head_sha: Option<String>,
    /// Last update on the forge.
    pub updated_at: DateTime<Utc>,
    /// Browser URL.
    pub web_url: String,
    /// Aggregate check state, when the list endpoint reports it.
    pub checks: ForgeChecksState,
    /// Aggregate review state, when known.
    pub review: ForgeReviewState,
    /// Whether the PR is a draft.
    pub draft: bool,
}

/// A pull request detail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgePullRequest {
    /// Summary fields shared with the list.
    pub summary: ForgePrSummary,
    /// Markdown body.
    pub body: String,
    /// Labels.
    pub labels: Vec<ForgeLabel>,
    /// Mergeability.
    pub mergeable: ForgeMergeable,
    /// Base commit SHA.
    pub base_sha: Option<String>,
    /// Lines added.
    pub additions: u32,
    /// Lines deleted.
    pub deletions: u32,
    /// Files changed.
    pub changed_files: u32,
    /// Whether the forge could count the changed files.
    ///
    /// `false` means the count is unknown (a diffstat page failed or hit its
    /// cap), not that the PR changes no files; clients show it as unknown.
    pub changed_files_known: bool,
    /// Whether the forge reported the line counts.
    ///
    /// `false` means additions and deletions are unknown rather than zero;
    /// clients show them as unknown instead of `+0 −0` (Azure DevOps does not
    /// report totals without downloading every file).
    pub line_counts_known: bool,
    /// Commits in the PR.
    pub commits: u32,
    /// When it was opened.
    pub created_at: DateTime<Utc>,
    /// Reviewers and their decisions.
    pub reviewers: Vec<ForgeReview>,
    /// Merge commit SHA when merged.
    pub merge_commit_sha: Option<String>,
}

/// One file in a diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ForgeFileStatus {
    /// New file.
    Added,
    /// Content changed.
    Modified,
    /// Removed file.
    Deleted,
    /// Moved or renamed.
    Renamed,
    /// Copied.
    Copied,
    /// Mode or type changed.
    Changed,
    /// The forge did not classify it.
    Unknown,
}

/// One changed file with its patch.
///
/// `patch` is `None` when the forge refuses to inline it (binary, generated,
/// or over the provider's size limit). `truncated` says the patch is present
/// but incomplete; `binary` says it has no text hunks at all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeFileDiff {
    /// Path after the change.
    pub path: String,
    /// Path before a rename, when different.
    pub previous_path: Option<String>,
    /// Change kind.
    pub status: ForgeFileStatus,
    /// Lines added.
    pub additions: u32,
    /// Lines deleted.
    pub deletions: u32,
    /// Unified diff hunks, when available.
    pub patch: Option<String>,
    /// The file has no text patch.
    pub binary: bool,
    /// The forge truncated the patch.
    pub truncated: bool,
}

/// One page of a diff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeDiffPage {
    /// Repository.
    pub repo: ForgeRepoRef,
    /// PR number.
    pub number: u32,
    /// Files in this page.
    pub files: Vec<ForgeFileDiff>,
    /// Page number, starting at 1.
    pub page: u32,
    /// Page size requested.
    pub per_page: u32,
    /// Another page exists.
    pub has_more: bool,
}

/// Which side of a diff an inline comment anchors to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ForgeDiffSide {
    /// The old file.
    Left,
    /// The new file.
    Right,
    /// The forge did not report a side.
    Unknown,
}

/// One inline review comment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeReviewComment {
    /// Provider comment id, as a string so any provider fits.
    pub comment_id: String,
    /// Thread/discussion id this comment belongs to.
    pub thread_id: Option<String>,
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
    /// File path the comment anchors to.
    pub path: String,
    /// Line number, when the forge reports one.
    pub line: Option<u32>,
    /// Side of the diff.
    pub side: ForgeDiffSide,
    /// Comment this is a reply to, when known.
    pub in_reply_to: Option<String>,
}

/// A resolved-or-open inline thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeReviewThread {
    /// Provider thread id.
    pub thread_id: String,
    /// File path.
    pub path: String,
    /// Line number, when known.
    pub line: Option<u32>,
    /// Side of the diff.
    pub side: ForgeDiffSide,
    /// Whether the conversation is resolved.
    pub resolved: bool,
    /// The anchored code is no longer in the diff.
    pub outdated: bool,
    /// Comments in chronological order.
    pub comments: Vec<ForgeReviewComment>,
}

/// A general (non-inline) comment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeComment {
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

/// Everything the conversation tab needs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeCommentPage {
    /// Repository.
    pub repo: ForgeRepoRef,
    /// PR number.
    pub number: u32,
    /// General comments, oldest first.
    pub comments: Vec<ForgeComment>,
    /// Inline threads, oldest first.
    pub threads: Vec<ForgeReviewThread>,
    /// Submitted reviews and their decisions.
    pub reviews: Vec<ForgeReview>,
    /// The forge has more comments than this page returned.
    pub has_more: bool,
}

/// One check, job or commit status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ForgeCheckStatus {
    /// Waiting in a queue.
    Queued,
    /// Running.
    InProgress,
    /// Finished successfully.
    Success,
    /// Finished with a failure.
    Failure,
    /// Finished without a verdict.
    Neutral,
    /// Cancelled by a user or the forge.
    Cancelled,
    /// Skipped by a condition.
    Skipped,
    /// Exceeded its time limit.
    TimedOut,
    /// Needs a manual action before it can run.
    ActionRequired,
    /// The forge reported an unknown state.
    Unknown,
}

impl ForgeCheckStatus {
    /// Whether the check is still running or queued.
    pub fn is_pending(self) -> bool {
        matches!(self, Self::Queued | Self::InProgress | Self::ActionRequired)
    }

    /// Whether the check failed.
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Failure | Self::TimedOut | Self::Cancelled)
    }

    /// Whether the check passed.
    pub fn is_success(self) -> bool {
        matches!(self, Self::Success | Self::Neutral | Self::Skipped)
    }
}

/// The aggregate state of a PR's checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ForgeChecksState {
    /// Every check finished successfully.
    Passing,
    /// At least one check failed.
    Failing,
    /// Checks are still running.
    Pending,
    /// The forge reports no checks, or cannot report them.
    Unknown,
}

/// One check entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeCheck {
    /// Workflow/job/context name.
    pub name: String,
    /// Current state.
    pub status: ForgeCheckStatus,
    /// Log or details URL.
    pub details_url: Option<String>,
    /// One-line description.
    pub description: Option<String>,
    /// When it started.
    pub started_at: Option<DateTime<Utc>>,
    /// When it finished.
    pub completed_at: Option<DateTime<Utc>>,
    /// Wall time in milliseconds, when both ends are known.
    pub duration_ms: Option<u64>,
    /// Whether the forge requires this check before merging. `None` when the
    /// forge or the token cannot say (GitHub without read access to the branch
    /// rules, a Gitea token that is not an admin).
    #[serde(default)]
    pub required: Option<bool>,
}

/// Whether the forge can re-run a PR's failed checks, and why not if it can't.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeRerunSupport {
    /// The forge exposes a re-run API for this check source.
    pub supported: bool,
    /// Why it does not, worded for a tooltip. `None` when supported.
    pub reason: Option<String>,
}

/// The checks tab and the agent context summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeChecks {
    /// Repository.
    pub repo: ForgeRepoRef,
    /// PR number.
    pub number: u32,
    /// Commit the checks ran against.
    pub head_sha: String,
    /// Aggregate state.
    pub state: ForgeChecksState,
    /// Total checks.
    pub total: u32,
    /// Checks that passed.
    pub passed: u32,
    /// Checks that failed.
    pub failed: u32,
    /// Checks still running.
    pub pending: u32,
    /// The checks themselves.
    pub checks: Vec<ForgeCheck>,
    /// Whether "Re-run failed" can work on this forge.
    pub rerun: ForgeRerunSupport,
    /// When the daemon fetched this snapshot.
    pub updated_at: DateTime<Utc>,
}

/// One commit of a pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeCommit {
    /// Full commit id.
    pub sha: String,
    /// First line of the message.
    pub title: String,
    /// The rest of the message, when there is one.
    pub body: Option<String>,
    /// Author name as recorded in the commit.
    pub author: String,
    /// Forge login of the author, when the forge links one.
    pub author_login: Option<String>,
    /// When the commit was authored.
    pub authored_at: Option<DateTime<Utc>>,
    /// Web page of the commit.
    pub web_url: Option<String>,
}

/// The Commits tab: a PR's commits, oldest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeCommits {
    /// Repository.
    pub repo: ForgeRepoRef,
    /// PR number.
    pub number: u32,
    /// The commits, oldest first, as far as the forge returned them.
    pub commits: Vec<ForgeCommit>,
    /// The PR has more commits than were fetched.
    pub truncated: bool,
}

/// A title/body pair for a new pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeNewPr {
    /// Title.
    pub title: String,
    /// Markdown body.
    pub body: String,
    /// Source branch, already pushed.
    pub head: String,
    /// Target branch.
    pub base: String,
    /// Open as a draft where the forge has a draft state.
    #[serde(default)]
    pub draft: bool,
}

/// Where a PR handoff should create a thread when none is linked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeAgentTarget {
    /// Host that runs the harness.
    pub host: crate::ids::HostId,
    /// Harness to use.
    pub harness: crate::common::HarnessKind,
    /// Model slug.
    pub model: String,
    /// Absolute workspace path.
    pub workspace_path: String,
}

/// Result of handing a PR to an agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeHandoff {
    /// Thread that received the context.
    pub thread_id: crate::ids::ThreadId,
    /// First turn, when the handoff created the thread.
    pub turn_id: Option<crate::ids::TurnId>,
    /// The markdown context block that was sent.
    pub context: String,
    /// The PR summary at handoff time.
    pub pr: ForgePrSummary,
}

/// A capabilities row for one forge kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeCapabilityEntry {
    /// Forge kind.
    pub forge: ForgeKind,
    /// What it supports.
    pub capabilities: ForgeCapabilities,
}

/// A started device authorization the client can poll.
///
/// The device code itself never crosses this contract; the daemon keeps it and
/// only hands the client the short code and verification URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ForgeDeviceFlow {
    /// Opaque id used to poll for completion.
    pub flow_id: String,
    /// Short code the user types at `verification_uri`.
    pub user_code: String,
    /// URL the user visits.
    pub verification_uri: String,
    /// Seconds until the code expires.
    #[ts(type = "number")]
    pub expires_in: u64,
    /// Suggested seconds between polls.
    #[ts(type = "number")]
    pub interval: u64,
}

/// One poll of a device authorization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ForgeDeviceFlowStatus {
    /// The user has not approved yet.
    Pending {
        /// Seconds to wait before polling again.
        #[ts(type = "number")]
        interval: u64,
    },
    /// The forge asked the client to slow down.
    SlowDown {
        /// Seconds to wait before polling again.
        #[ts(type = "number")]
        interval: u64,
    },
    /// The user approved and the account was stored.
    Complete {
        /// The stored account (never the token).
        account: ForgeAccount,
    },
    /// The code expired; start a new flow.
    Expired,
    /// The user denied the authorization.
    Denied {
        /// Provider or daemon explanation.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forge_kinds_round_trip_stable_slugs() {
        for kind in [
            ForgeKind::GitHub,
            ForgeKind::GitLab,
            ForgeKind::Gitea,
            ForgeKind::Forgejo,
            ForgeKind::Bitbucket,
            ForgeKind::AzureDevOps,
        ] {
            assert_eq!(ForgeKind::parse(kind.slug()), Some(kind));
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.slug()));
        }
        assert_eq!(
            ForgeKind::parse("Bitbucket Cloud"),
            Some(ForgeKind::Bitbucket)
        );
        assert_eq!(ForgeKind::parse("azure"), Some(ForgeKind::AzureDevOps));
    }

    #[test]
    fn azure_repo_urls_keep_the_git_segment() {
        let repo = ForgeRepoRef {
            forge: ForgeKind::AzureDevOps,
            base_url: "https://dev.azure.com/acme".into(),
            owner: "platform".into(),
            repo: "widget".into(),
        };
        assert_eq!(
            repo.web_url(),
            "https://dev.azure.com/acme/platform/_git/widget"
        );
    }

    #[test]
    fn repo_full_name_keeps_gitlab_namespaces() {
        let repo = ForgeRepoRef {
            forge: ForgeKind::GitLab,
            base_url: "https://gitlab.com".into(),
            owner: "team/platform".into(),
            repo: "api".into(),
        };
        assert_eq!(repo.full_name(), "team/platform/api");
        assert_eq!(repo.web_url(), "https://gitlab.com/team/platform/api");
    }

    #[test]
    fn filter_is_internally_tagged() {
        let value = serde_json::to_value(ForgePrFilter::ByBranch {
            branch: "h4rn/fix".into(),
        })
        .unwrap();
        assert_eq!(value["type"], "by_branch");
        assert_eq!(value["branch"], "h4rn/fix");
    }

    #[test]
    fn check_status_classes_are_exclusive() {
        assert!(ForgeCheckStatus::InProgress.is_pending());
        assert!(ForgeCheckStatus::Failure.is_failure());
        assert!(ForgeCheckStatus::Success.is_success());
        assert!(!ForgeCheckStatus::Unknown.is_success());
    }
}
