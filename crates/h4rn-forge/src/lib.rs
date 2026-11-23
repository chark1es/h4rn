//! Provider-agnostic forge operations for the UI and the agent.
//!
//! The daemon owns one [`Forge`] per configured account. Every provider
//! implements the same normalised operations — list PRs, read detail and diff,
//! read comments and threads, read checks, write a reply or comment, open a PR
//! — so the protocol and the UI never branch on GitHub versus GitLab versus
//! Gitea/Forgejo versus Bitbucket Cloud versus Azure DevOps (ADR-001,
//! ADR-002).
//!
//! Credentials stay on the host that holds them (ADR-016). A [`Forge`] client
//! is constructed with a token the daemon loaded from its secret store; this
//! crate never persists one and never returns one from an operation.

#![deny(missing_docs)]

pub mod auth;
mod azure;
mod bitbucket;
mod common;
pub mod context;
mod diff;
mod error;
mod gitea;
mod github;
mod gitlab;
mod http;
pub mod issues;
pub mod remote;

use std::sync::Arc;

use async_trait::async_trait;
use h4rn_protocol::{
    ForgeAccount, ForgeCapabilities, ForgeChecks, ForgeComment, ForgeCommentPage, ForgeCommits,
    ForgeCredentialSource, ForgeDiffPage, ForgeIssue, ForgeIssueComment, ForgeIssueCommentPage,
    ForgeIssueFilter, ForgeIssuePage, ForgeIssueUpdate, ForgeKind, ForgeNewPr, ForgePrFilter,
    ForgePrSummary, ForgePullRequest, ForgeRepoRef, ForgeReviewComment,
};

pub use azure::AzureDevOpsClient;
pub use bitbucket::BitbucketClient;
pub use common::{finish_checks, validate_repo};
pub use context::pr_context_for_agent;
pub use diff::{paginate, parse_unified_diff, unified_diff};
pub use error::{ForgeError, Result};
pub use gitea::GiteaClient;
pub use github::GitHubClient;
pub use gitlab::GitLabClient;
pub use http::{DEFAULT_REQUESTS_PER_MINUTE, ForgeHttp, HttpResponse, next_link, page_of, redact};
pub use remote::{
    ForgeHost, ForgeHosts, api_base_url, base_urls_match, detect_remote, detect_repos,
    parse_remote, validate_base_url_for, validate_cloud_base_url,
};

/// The authenticated identity behind one account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeViewer {
    /// Login the forge reports.
    pub login: String,
    /// Human name, when the forge reports one.
    pub display_name: Option<String>,
    /// Scopes the token carries, when the forge reports them.
    pub scopes: Vec<String>,
}

/// One page of PR rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrPage {
    /// Matching PRs.
    pub prs: Vec<ForgePrSummary>,
    /// Page number returned.
    pub page: u32,
    /// Another page exists.
    pub has_more: bool,
}

/// Everything a forge client can do.
///
/// Implementations are stateless apart from their HTTP cache and rate limiter,
/// so the same client can serve several concurrent UI connections.
#[async_trait]
pub trait Forge: Send + Sync {
    /// Provider this client talks to.
    fn kind(&self) -> ForgeKind;

    /// Public web base URL this client was built for.
    fn base_url(&self) -> &str;

    /// Provider capabilities, used to hide impossible actions.
    fn capabilities(&self) -> ForgeCapabilities;

    /// Fetches the authenticated viewer, which also validates the token.
    async fn viewer(&self) -> Result<ForgeViewer>;

    /// Fetches the authenticated viewer with an optional repository hint.
    ///
    /// Forges whose credential is scoped to a repository (Bitbucket access
    /// tokens) cannot always call the account endpoint; the hint lets the
    /// client validate against that repository instead. The default ignores
    /// it.
    async fn viewer_for(&self, repo: Option<&ForgeRepoRef>) -> Result<ForgeViewer> {
        let _ = repo;
        self.viewer().await
    }

    /// Lists PRs with the requested filter.
    async fn list_prs(
        &self,
        repo: &ForgeRepoRef,
        filter: &ForgePrFilter,
        page: u32,
    ) -> Result<PrPage>;

    /// Fetches one PR's detail.
    async fn get_pr(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgePullRequest>;

    /// Fetches one page of the PR diff.
    async fn get_diff(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        page: u32,
        per_page: u32,
    ) -> Result<ForgeDiffPage>;

    /// Fetches general comments, inline threads and reviews.
    async fn list_comments(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeCommentPage>;

    /// Fetches the head commit's checks.
    async fn list_checks(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeChecks>;

    /// Lists the PR's commits, oldest first.
    async fn list_commits(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeCommits>;

    /// Re-runs the PR's failed checks and returns how many runs, suites or
    /// pipelines were re-requested.
    ///
    /// Forges whose CI is plain commit statuses answer
    /// [`ForgeError::Unsupported`]; see [`rerun_support`].
    async fn rerun_failed_checks(&self, repo: &ForgeRepoRef, number: u32) -> Result<u32> {
        let _ = (repo, number);
        Err(ForgeError::Unsupported(
            rerun_support(self.kind())
                .reason
                .unwrap_or_else(|| "this forge cannot re-run checks".into()),
        ))
    }

    /// Replies to a comment or a review thread.
    async fn reply(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        thread_id: Option<&str>,
        comment_id: &str,
        body: &str,
    ) -> Result<ForgeReviewComment>;

    /// Posts a general PR comment.
    async fn create_comment(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        body: &str,
    ) -> Result<ForgeComment>;

    /// Opens a pull request whose head branch already exists.
    async fn open_pr(&self, repo: &ForgeRepoRef, request: &ForgeNewPr) -> Result<ForgePullRequest>;

    /// Lists issues from the repository's tracker.
    ///
    /// A forge whose issue tracker is disabled for this repository answers
    /// [`ForgeError::Unsupported`] with the reason; it never returns an empty
    /// list that would look like a tracker with no issues.
    async fn list_issues(
        &self,
        repo: &ForgeRepoRef,
        filter: ForgeIssueFilter,
        page: u32,
        query: Option<&str>,
    ) -> Result<ForgeIssuePage> {
        let _ = (repo, filter, page, query);
        Err(ForgeError::Unsupported(format!(
            "{} does not expose issues",
            self.kind().display_name()
        )))
    }

    /// Reads one issue.
    async fn get_issue(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeIssue> {
        let _ = (repo, number);
        Err(ForgeError::Unsupported(format!(
            "{} does not expose issues",
            self.kind().display_name()
        )))
    }

    /// Reads one issue's comments.
    async fn list_issue_comments(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        cursor: Option<&str>,
    ) -> Result<ForgeIssueCommentPage> {
        let _ = (repo, number, cursor);
        Err(ForgeError::Unsupported(format!(
            "{} does not expose issues",
            self.kind().display_name()
        )))
    }

    /// Posts a comment on an issue.
    async fn comment_issue(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        body: &str,
    ) -> Result<ForgeIssueComment> {
        let _ = (repo, number, body);
        Err(ForgeError::Unsupported(format!(
            "{} does not expose issues",
            self.kind().display_name()
        )))
    }

    /// Changes an issue's title, body, state, labels or assignees.
    async fn update_issue(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        update: &ForgeIssueUpdate,
    ) -> Result<ForgeIssue> {
        let _ = (repo, number, update);
        Err(ForgeError::Unsupported(format!(
            "{} does not expose issues",
            self.kind().display_name()
        )))
    }
}

/// Builds a client for one stored account.
///
/// `token` is supplied by the daemon's secret store; it is not part of
/// [`ForgeAccount`] and never crosses the wire.
pub fn client_for(account: &ForgeAccount, token: &str) -> Result<Arc<dyn Forge>> {
    client_for_with_budget(account, token, http::DEFAULT_REQUESTS_PER_MINUTE)
}

/// Builds a client with an explicit local request budget.
pub fn client_for_with_budget(
    account: &ForgeAccount,
    token: &str,
    requests_per_minute: u32,
) -> Result<Arc<dyn Forge>> {
    let client: Arc<dyn Forge> = match account.forge {
        ForgeKind::GitHub => Arc::new(GitHubClient::new(
            &account.base_url,
            token,
            requests_per_minute,
        )?),
        ForgeKind::GitLab => Arc::new(GitLabClient::new(
            &account.base_url,
            token,
            requests_per_minute,
        )?),
        ForgeKind::Gitea => Arc::new(GiteaClient::new(
            ForgeKind::Gitea,
            &account.base_url,
            token,
            requests_per_minute,
        )?),
        ForgeKind::Forgejo => Arc::new(GiteaClient::new(
            ForgeKind::Forgejo,
            &account.base_url,
            token,
            requests_per_minute,
        )?),
        ForgeKind::Bitbucket => {
            // The source remembers which scheme validated at add time, so a
            // workspace access token stays Bearer after a daemon restart even
            // though the account row also carries the viewer login.
            let basic = matches!(account.source, ForgeCredentialSource::BitbucketApiToken)
                || (matches!(account.source, ForgeCredentialSource::Pat)
                    && account.username.is_some());
            Arc::new(BitbucketClient::new(
                &account.base_url,
                token,
                if basic {
                    account.username.as_deref()
                } else {
                    None
                },
                requests_per_minute,
            )?)
        }
        ForgeKind::AzureDevOps => Arc::new(AzureDevOpsClient::new(
            &account.base_url,
            token,
            requests_per_minute,
        )?),
    };
    Ok(client)
}

/// Capabilities for one forge kind, without constructing a client.
pub fn capabilities(kind: ForgeKind) -> ForgeCapabilities {
    match kind {
        ForgeKind::GitHub => ForgeCapabilities {
            device_flow: true,
            review_threads: true,
            resolve_threads: true,
            nested_replies: true,
            inline_review_comments: true,
            checks: true,
            pipelines: false,
            diff_pagination: true,
            graphql: true,
            rerun_checks: true,
            conditional_poll: true,
            issues: issues::capabilities(ForgeKind::GitHub),
        },
        ForgeKind::GitLab => ForgeCapabilities {
            device_flow: false,
            review_threads: true,
            resolve_threads: true,
            nested_replies: true,
            inline_review_comments: true,
            checks: true,
            pipelines: true,
            diff_pagination: true,
            graphql: false,
            rerun_checks: true,
            conditional_poll: false,
            issues: issues::capabilities(ForgeKind::GitLab),
        },
        ForgeKind::Gitea => ForgeCapabilities {
            device_flow: false,
            review_threads: true,
            resolve_threads: false,
            nested_replies: false,
            inline_review_comments: true,
            checks: true,
            pipelines: false,
            diff_pagination: false,
            graphql: false,
            rerun_checks: false,
            conditional_poll: false,
            issues: issues::capabilities(ForgeKind::Gitea),
        },
        ForgeKind::Forgejo => ForgeCapabilities {
            device_flow: false,
            review_threads: true,
            resolve_threads: false,
            nested_replies: false,
            inline_review_comments: true,
            checks: true,
            pipelines: false,
            diff_pagination: false,
            graphql: false,
            rerun_checks: false,
            conditional_poll: false,
            issues: issues::capabilities(ForgeKind::Forgejo),
        },
        ForgeKind::Bitbucket => ForgeCapabilities {
            device_flow: false,
            review_threads: true,
            // Resolution is read from the comment when the API reports it;
            // Bitbucket has a resolve-comment endpoint, but this client does
            // not call it yet.
            resolve_threads: false,
            nested_replies: true,
            inline_review_comments: true,
            checks: true,
            pipelines: false,
            diff_pagination: false,
            graphql: false,
            rerun_checks: false,
            conditional_poll: false,
            issues: issues::capabilities(ForgeKind::Bitbucket),
        },
        ForgeKind::AzureDevOps => ForgeCapabilities {
            device_flow: false,
            review_threads: true,
            resolve_threads: false,
            nested_replies: true,
            inline_review_comments: true,
            checks: true,
            pipelines: false,
            diff_pagination: false,
            graphql: false,
            // Re-queuing a pipeline needs the Builds API and a broader token.
            rerun_checks: false,
            conditional_poll: false,
            issues: issues::capabilities(ForgeKind::AzureDevOps),
        },
    }
}

/// Whether a forge can re-run failed checks, with the reason when it cannot.
pub fn rerun_support(kind: ForgeKind) -> h4rn_protocol::ForgeRerunSupport {
    let supported = capabilities(kind).rerun_checks;
    h4rn_protocol::ForgeRerunSupport {
        supported,
        reason: (!supported).then(|| match kind {
            ForgeKind::Gitea | ForgeKind::Forgejo => format!(
                "{} posts CI results as commit statuses and has no re-run API; re-run it in {}'s Actions tab",
                kind.display_name(),
                kind.display_name()
            ),
            ForgeKind::Bitbucket => "Bitbucket Cloud has no API to re-run build statuses; re-run the pipeline in Bitbucket"
                .to_owned(),
            ForgeKind::AzureDevOps => "Azure DevOps can only re-queue a pipeline through the Builds API with extra scopes; re-run it in Azure Pipelines"
                .to_owned(),
            _ => format!("{} has no API to re-run checks", kind.display_name()),
        }),
    }
}

/// One capabilities row per supported forge.
pub fn capability_entries() -> Vec<h4rn_protocol::ForgeCapabilityEntry> {
    [
        ForgeKind::GitHub,
        ForgeKind::GitLab,
        ForgeKind::Gitea,
        ForgeKind::Forgejo,
        ForgeKind::Bitbucket,
        ForgeKind::AzureDevOps,
    ]
    .into_iter()
    .map(|forge| h4rn_protocol::ForgeCapabilityEntry {
        forge,
        capabilities: capabilities(forge),
    })
    .collect()
}

/// Crate name, used by startup diagnostics.
pub const CRATE_NAME: &str = "h4rn-forge";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_is_wired() {
        assert_eq!(CRATE_NAME, "h4rn-forge");
    }

    #[test]
    fn capability_matrix_is_complete() {
        let entries = capability_entries();
        assert_eq!(entries.len(), 6);
        assert!(capabilities(ForgeKind::GitHub).device_flow);
        assert!(!capabilities(ForgeKind::GitLab).device_flow);
        assert!(capabilities(ForgeKind::GitLab).pipelines);
        assert!(!capabilities(ForgeKind::Gitea).nested_replies);
        assert!(capabilities(ForgeKind::Bitbucket).nested_replies);
        assert!(!capabilities(ForgeKind::Bitbucket).resolve_threads);
        assert!(capabilities(ForgeKind::AzureDevOps).inline_review_comments);
    }

    #[test]
    fn only_github_advertises_conditional_pr_polling() {
        assert!(capabilities(ForgeKind::GitHub).conditional_poll);
        for kind in [
            ForgeKind::GitLab,
            ForgeKind::Gitea,
            ForgeKind::Forgejo,
            ForgeKind::Bitbucket,
            ForgeKind::AzureDevOps,
        ] {
            assert!(!capabilities(kind).conditional_poll, "{kind:?}");
        }
    }

    #[test]
    fn rerun_is_offered_only_where_the_forge_has_an_api() {
        assert!(rerun_support(ForgeKind::GitHub).supported);
        assert!(rerun_support(ForgeKind::GitLab).supported);
        for kind in [
            ForgeKind::Gitea,
            ForgeKind::Forgejo,
            ForgeKind::Bitbucket,
            ForgeKind::AzureDevOps,
        ] {
            let support = rerun_support(kind);
            assert!(!support.supported);
            assert!(
                support
                    .reason
                    .as_deref()
                    .is_some_and(|reason| !reason.is_empty()),
                "{support:?}"
            );
        }
        assert!(
            rerun_support(ForgeKind::Gitea)
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("commit statuses"))
        );
    }
}
