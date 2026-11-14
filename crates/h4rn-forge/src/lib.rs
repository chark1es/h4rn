//! Provider-agnostic forge operations for the UI and the agent.
//!
//! The daemon owns one [`Forge`] per configured account. Every provider
//! implements the same normalised operations — list PRs, read detail and diff,
//! read comments and threads, read checks, write a reply or comment, open a PR
//! — so the protocol and the UI never branch on GitHub versus GitLab versus
//! Gitea/Forgejo (ADR-001, ADR-002).
//!
//! Credentials stay on the host that holds them (ADR-016). A [`Forge`] client
//! is constructed with a token the daemon loaded from its secret store; this
//! crate never persists one and never returns one from an operation.

#![deny(missing_docs)]

pub mod auth;
mod common;
pub mod context;
mod diff;
mod error;
mod gitea;
mod github;
mod gitlab;
mod http;
pub mod remote;

use std::sync::Arc;

use async_trait::async_trait;
use h4rn_protocol::{
    ForgeAccount, ForgeCapabilities, ForgeChecks, ForgeComment, ForgeCommentPage, ForgeCommits,
    ForgeDiffPage, ForgeKind, ForgeNewPr, ForgePrFilter, ForgePrSummary, ForgePullRequest,
    ForgeRepoRef, ForgeReviewComment,
};

pub use common::{finish_checks, validate_repo};
pub use context::pr_context_for_agent;
pub use diff::{paginate, parse_unified_diff};
pub use error::{ForgeError, Result};
pub use gitea::GiteaClient;
pub use github::GitHubClient;
pub use gitlab::GitLabClient;
pub use http::{DEFAULT_REQUESTS_PER_MINUTE, ForgeHttp, HttpResponse, next_link, page_of, redact};
pub use remote::{ForgeHost, ForgeHosts, api_base_url, detect_remote, detect_repos, parse_remote};

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
        assert_eq!(entries.len(), 4);
        assert!(capabilities(ForgeKind::GitHub).device_flow);
        assert!(!capabilities(ForgeKind::GitLab).device_flow);
        assert!(capabilities(ForgeKind::GitLab).pipelines);
        assert!(!capabilities(ForgeKind::Gitea).nested_replies);
    }

    #[test]
    fn rerun_is_offered_only_where_the_forge_has_an_api() {
        assert!(rerun_support(ForgeKind::GitHub).supported);
        assert!(rerun_support(ForgeKind::GitLab).supported);
        for kind in [ForgeKind::Gitea, ForgeKind::Forgejo] {
            let support = rerun_support(kind);
            assert!(!support.supported);
            assert!(
                support
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("commit statuses")),
                "{support:?}"
            );
        }
    }
}
