//! Bitbucket Cloud client (`bitbucket.org`, API v2).
//!
//! Authentication is either an app password (or API token) basic-auth pair
//! with the account username, or a workspace/OAuth access token sent as a
//! bearer token. The daemon stores both halves the same way it stores every
//! other forge token.
//!
//! Review threads are rebuilt from the comment `parent` chain. Bitbucket's
//! newer API adds a `resolution` object to a comment; when it is present the
//! thread is reported resolved, otherwise unresolved. Bitbucket Cloud does
//! expose a resolve-comment endpoint (`POST .../comments/{id}/resolve`), but
//! this client does not call it yet, so `resolve_threads` stays `false`.
//!
//! Diffs are the PR's raw unified patch (`/pullrequests/{id}/diff`, which the
//! API redirects to the repository diff) parsed by [`crate::diff`]. Pagination
//! is applied locally, like Gitea and Forgejo.
//!
//! Bitbucket Server and Data Center are not supported; they use a different
//! API (`/rest/api/1.0`) and are rejected before this client is built.

use async_trait::async_trait;
use chrono::Utc;
use h4rn_protocol::{
    ForgeCapabilities, ForgeCheck, ForgeChecks, ForgeChecksState, ForgeComment, ForgeCommentPage,
    ForgeCommit, ForgeCommits, ForgeDiffPage, ForgeDiffSide, ForgeKind, ForgeMergeable, ForgeNewPr,
    ForgePrFilter, ForgePrState, ForgePrSummary, ForgePullRequest, ForgeRepoRef, ForgeReview,
    ForgeReviewComment, ForgeReviewState, ForgeReviewThread,
};
use serde_json::{Value, json};

use crate::common::{
    bitbucket_check_status, duration_ms, finish_checks, optional_text, parse_datetime,
    parse_optional_datetime, split_message, text, validate_repo,
};
use crate::error::{ForgeError, Result};
use crate::http::{ForgeHttp, basic_auth_header};
use crate::remote::{api_base_url, validate_base_url};
use crate::{Forge, ForgeViewer, PrPage};

/// Bitbucket Cloud REST v2 client.
pub struct BitbucketClient {
    http: ForgeHttp,
    api: String,
    base_url: String,
    username: Option<String>,
}

impl BitbucketClient {
    /// Builds a client for one account.
    ///
    /// `username` selects basic auth (app password / API token). Without it
    /// the token is sent as a bearer token (OAuth or workspace access token).
    pub fn new(
        base_url: &str,
        token: &str,
        username: Option<&str>,
        requests_per_minute: u32,
    ) -> Result<Self> {
        let base_url = validate_base_url(base_url)?;
        let api = api_base_url(ForgeKind::Bitbucket, &base_url);
        let username = username
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let auth = if token.trim().is_empty() {
            None
        } else if let Some(username) = username.as_deref() {
            basic_auth_header(username, token.trim())
        } else {
            crate::http::auth_header("authorization", &format!("Bearer {}", token.trim()))
        };
        let http = ForgeHttp::new(
            ForgeKind::Bitbucket,
            &base_url,
            auth,
            "application/json",
            requests_per_minute,
        )?;
        Ok(Self {
            http,
            api,
            base_url,
            username,
        })
    }

    fn repo_url(&self, repo: &ForgeRepoRef) -> String {
        format!(
            "{}/repositories/{}/{}",
            self.api,
            urlencode(&repo.owner),
            urlencode(&repo.repo)
        )
    }

    fn pr_url(&self, repo: &ForgeRepoRef, number: u32) -> String {
        format!("{}/pullrequests/{number}", self.repo_url(repo))
    }

    /// Reads every page of a Bitbucket collection (it links the next page in
    /// the body, not a `Link` header). A `next` link on another origin is not
    /// followed: the request would carry this account's Authorization header.
    async fn get_pages(&self, url: &str, max_pages: usize) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        let mut next = Some(url.to_owned());
        let mut pages = 0;
        while let Some(current) = next {
            pages += 1;
            if pages > max_pages {
                break;
            }
            let value = self.http.get_json(&current).await?;
            if let Some(array) = value["values"].as_array() {
                values.extend(array.iter().cloned());
            }
            next = optional_text(&value, "next")
                .filter(|next| crate::http::same_origin(next, &self.api));
        }
        Ok(values)
    }

    async fn fetch_pr(&self, repo: &ForgeRepoRef, number: u32) -> Result<Value> {
        self.http.get_json(&self.pr_url(repo, number)).await
    }

    async fn diffstat(&self, repo: &ForgeRepoRef, number: u32) -> Option<(u32, u32, u32)> {
        let url = format!("{}/diffstat?pagelen=100", self.pr_url(repo, number));
        let values = self.get_pages(&url, 5).await.ok()?;
        let mut additions = 0u32;
        let mut deletions = 0u32;
        for value in &values {
            additions += value["lines_added"].as_u64().unwrap_or(0) as u32;
            deletions += value["lines_removed"].as_u64().unwrap_or(0) as u32;
        }
        Some((additions, deletions, values.len() as u32))
    }

    async fn commit_count(&self, repo: &ForgeRepoRef, number: u32) -> Option<u32> {
        let url = format!("{}/commits?pagelen=1", self.pr_url(repo, number));
        let value = self.http.get_json(&url).await.ok()?;
        value["size"]
            .as_u64()
            .map(|size| size as u32)
            .or_else(|| Some(value["values"].as_array()?.len() as u32))
    }

    /// `GET /user`, which needs the account scope.
    async fn user_viewer(&self) -> Result<ForgeViewer> {
        let url = format!("{}/user", self.api);
        let response = self
            .http
            .request(reqwest::Method::GET, &url, None, false)
            .await?;
        let scopes = response
            .headers
            .get("x-oauth-scopes")
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|scope| !scope.is_empty())
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let value = self.http.json(response)?;
        let login = optional_text(&value, "nickname")
            .or_else(|| optional_text(&value, "username"))
            .or_else(|| optional_text(&value, "display_name"))
            .unwrap_or_default();
        if login.is_empty() {
            return Err(ForgeError::Parse("Bitbucket returned no username".into()));
        }
        Ok(ForgeViewer {
            login,
            display_name: optional_text(&value, "display_name"),
            scopes,
        })
    }

    /// Proves a repository-scoped token against the repository it was granted
    /// for. The workspace slug is the account login, which is what the Mine
    /// filter compares against.
    async fn repository_viewer(&self, repo: &ForgeRepoRef) -> Option<ForgeViewer> {
        self.http.get_json(&self.repo_url(repo)).await.ok()?;
        Some(ForgeViewer {
            login: repo.owner.clone(),
            display_name: None,
            scopes: Vec::new(),
        })
    }

    /// `GET /user/workspaces` for workspace access tokens and OAuth tokens
    /// that cannot read `/user`. The slug names the account.
    async fn workspace_viewer(&self) -> Result<ForgeViewer> {
        let url = format!("{}/user/workspaces?pagelen=1", self.api);
        let value = self.http.get_json(&url).await?;
        let workspace = value["values"]
            .as_array()
            .and_then(|values| values.first())
            .map(|value| &value["workspace"])
            .ok_or_else(|| ForgeError::Unauthorized {
                forge: ForgeKind::Bitbucket,
                message:
                    "the token cannot read an account; add the account scope or use an app password"
                        .into(),
            })?;
        let login = optional_text(workspace, "slug")
            .or_else(|| optional_text(workspace, "name"))
            .or_else(|| self.username.clone())
            .unwrap_or_default();
        if login.is_empty() {
            return Err(ForgeError::Parse(
                "Bitbucket returned no workspace for this token".into(),
            ));
        }
        Ok(ForgeViewer {
            login,
            display_name: optional_text(workspace, "name"),
            scopes: Vec::new(),
        })
    }
}

#[async_trait]
impl Forge for BitbucketClient {
    fn kind(&self) -> ForgeKind {
        ForgeKind::Bitbucket
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn capabilities(&self) -> ForgeCapabilities {
        crate::capabilities(ForgeKind::Bitbucket)
    }

    async fn viewer(&self) -> Result<ForgeViewer> {
        self.viewer_for(None).await
    }

    /// Bitbucket's `/user` needs the `account` (or `read:user:bitbucket`)
    /// scope, which repository, project and workspace access tokens do not
    /// always carry. When `/user` rejects the token, a repository-scoped
    /// request proves the token first (when the caller has a repo), then
    /// `/user/workspaces` names the workspace for tokens that may read it.
    async fn viewer_for(&self, repo: Option<&ForgeRepoRef>) -> Result<ForgeViewer> {
        match self.user_viewer().await {
            Ok(viewer) => Ok(viewer),
            Err(error @ (ForgeError::Unauthorized { .. } | ForgeError::Forbidden { .. })) => {
                if let Some(repo) = repo
                    && let Some(viewer) = self.repository_viewer(repo).await
                {
                    return Ok(viewer);
                }
                match self.workspace_viewer().await {
                    Ok(viewer) => Ok(viewer),
                    // Keep the honest auth error from `/user`, with the scan
                    // the UI shows for tokens that cannot read an account.
                    Err(_) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    async fn list_prs(
        &self,
        repo: &ForgeRepoRef,
        filter: &ForgePrFilter,
        page: u32,
    ) -> Result<PrPage> {
        validate_repo(repo)?;
        let page = page.max(1);
        let base = self.repo_url(repo);
        if matches!(filter, ForgePrFilter::Open) {
            let url =
                format!("{base}/pullrequests?state=OPEN&pagelen=50&page={page}&sort=-updated_on");
            let value = self.http.get_json(&url).await?;
            let items = value["values"]
                .as_array()
                .ok_or_else(|| ForgeError::Parse("Bitbucket returned no PR list".into()))?;
            let prs = items
                .iter()
                .filter_map(|item| map_pr_summary(repo, item))
                .collect::<Vec<_>>();
            return Ok(PrPage {
                has_more: optional_text(&value, "next").is_some(),
                prs,
                page,
            });
        }

        // Bitbucket's list endpoint has no reviewer/author filter that maps
        // exactly onto the UI's filters, so open pages are filtered locally.
        let viewer = match filter {
            ForgePrFilter::Mine | ForgePrFilter::ReviewRequested => {
                Some(self.viewer_for(Some(repo)).await?)
            }
            _ => None,
        };
        let mut matches = Vec::new();
        let mut current_page = page;
        let mut has_more = false;
        for _ in 0..4 {
            let url = format!(
                "{base}/pullrequests?state=OPEN&pagelen=50&page={current_page}&sort=-updated_on"
            );
            let value = self.http.get_json(&url).await?;
            let Some(items) = value["values"].as_array() else {
                break;
            };
            has_more = optional_text(&value, "next").is_some();
            for item in items {
                let Some(pr) = map_pr_summary(repo, item) else {
                    continue;
                };
                let keep = match filter {
                    ForgePrFilter::Mine => viewer
                        .as_ref()
                        .is_some_and(|viewer| same_user(&item["author"], &viewer.login)),
                    ForgePrFilter::ReviewRequested => viewer.as_ref().is_some_and(|viewer| {
                        item["reviewers"].as_array().is_some_and(|reviewers| {
                            reviewers
                                .iter()
                                .any(|reviewer| same_user(reviewer, &viewer.login))
                        })
                    }),
                    ForgePrFilter::ByBranch { branch } => pr.head_branch == *branch,
                    ForgePrFilter::Open => true,
                };
                if keep {
                    matches.push(pr);
                    if matches.len() >= 50 {
                        break;
                    }
                }
            }
            if matches.len() >= 50 || !has_more {
                break;
            }
            current_page += 1;
        }
        Ok(PrPage {
            prs: matches,
            page,
            has_more,
        })
    }

    async fn get_pr(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgePullRequest> {
        validate_repo(repo)?;
        let value = self.fetch_pr(repo, number).await?;
        let mut pr = map_pr_detail(repo, &value)?;
        if let Some((additions, deletions, files)) = self.diffstat(repo, number).await {
            pr.additions = additions;
            pr.deletions = deletions;
            pr.changed_files = files;
        }
        pr.commits = self.commit_count(repo, number).await.unwrap_or(0);
        pr.reviewers = map_reviewers(&value);
        pr.summary.review = aggregate_review(&pr.reviewers);
        Ok(pr)
    }

    async fn get_diff(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        page: u32,
        per_page: u32,
    ) -> Result<ForgeDiffPage> {
        validate_repo(repo)?;
        let per_page = per_page.clamp(1, 100);
        let page = page.max(1);
        let url = format!("{}/diff", self.pr_url(repo, number));
        let (body, content_type) = self.http.get_text(&url).await?;
        // Some proxies negotiate a JSON wrapper around the same patch.
        let patch = if content_type
            .as_deref()
            .is_some_and(|value| value.contains("json"))
        {
            let value: Value = serde_json::from_str(&body)
                .map_err(|error| ForgeError::Parse(format!("Bitbucket diff: {error}")))?;
            optional_text(&value, "diff")
                .or_else(|| optional_text(&value, "patch"))
                .unwrap_or_default()
        } else {
            body
        };
        let (files, has_more) =
            crate::diff::paginate(crate::diff::parse_unified_diff(&patch), page, per_page);
        Ok(ForgeDiffPage {
            repo: repo.clone(),
            number,
            files,
            page,
            per_page,
            has_more,
        })
    }

    async fn list_comments(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeCommentPage> {
        validate_repo(repo)?;
        let url = format!("{}/comments?pagelen=100", self.pr_url(repo, number));
        let values = self.get_pages(&url, 10).await?;
        let (comments, threads) = group_comments(&values);
        let pr = self.fetch_pr(repo, number).await.unwrap_or(Value::Null);
        let reviews = map_reviewers(&pr);
        Ok(ForgeCommentPage {
            repo: repo.clone(),
            number,
            has_more: values.len() >= 1_000,
            comments,
            threads,
            reviews,
        })
    }

    async fn list_checks(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeChecks> {
        validate_repo(repo)?;
        let pr = self.fetch_pr(repo, number).await?;
        let sha = pr["source"]["commit"]["hash"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| ForgeError::Parse("the pull request has no source commit".into()))?;
        let url = format!(
            "{}/commit/{}/statuses?pagelen=100",
            self.repo_url(repo),
            urlencode(&sha)
        );
        let values = self.get_pages(&url, 5).await.unwrap_or_default();
        let checks = values.iter().map(map_status).collect::<Vec<_>>();
        Ok(finish_checks(repo, number, sha, checks))
    }

    async fn list_commits(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeCommits> {
        validate_repo(repo)?;
        let url = format!("{}/commits?pagelen=100", self.pr_url(repo, number));
        let values = self.get_pages(&url, 5).await?;
        let mut commits = values.iter().filter_map(map_commit).collect::<Vec<_>>();
        // Bitbucket does not promise an order; the tab reads oldest first.
        commits.sort_by_key(|commit| commit.authored_at);
        Ok(ForgeCommits {
            repo: repo.clone(),
            number,
            truncated: values.len() >= 500,
            commits,
        })
    }

    async fn reply(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        thread_id: Option<&str>,
        comment_id: &str,
        body: &str,
    ) -> Result<ForgeReviewComment> {
        validate_repo(repo)?;
        let parent = comment_id.parse::<u64>().map_err(|_| {
            ForgeError::InvalidRequest(format!("invalid comment id {comment_id:?}"))
        })?;
        let url = format!("{}/comments", self.pr_url(repo, number));
        let value = self
            .http
            .post_json(
                &url,
                &json!({
                    "content": {"raw": body},
                    "parent": {"id": parent},
                }),
            )
            .await?;
        let mut comment = map_review_comment(&value)
            .ok_or_else(|| ForgeError::Parse("Bitbucket returned no comment".into()))?;
        comment.thread_id = thread_id.map(ToOwned::to_owned);
        comment.in_reply_to = Some(comment_id.to_owned());
        Ok(comment)
    }

    async fn create_comment(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        body: &str,
    ) -> Result<ForgeComment> {
        validate_repo(repo)?;
        let url = format!("{}/comments", self.pr_url(repo, number));
        let value = self
            .http
            .post_json(&url, &json!({"content": {"raw": body}}))
            .await?;
        map_comment(&value).ok_or_else(|| ForgeError::Parse("Bitbucket returned no comment".into()))
    }

    async fn open_pr(&self, repo: &ForgeRepoRef, request: &ForgeNewPr) -> Result<ForgePullRequest> {
        validate_repo(repo)?;
        if request.head.trim().is_empty() || request.base.trim().is_empty() {
            return Err(ForgeError::InvalidRequest(
                "opening a pull request needs a source and a destination branch".into(),
            ));
        }
        let url = format!("{}/pullrequests", self.repo_url(repo));
        let value = self
            .http
            .post_json(
                &url,
                &json!({
                    "title": request.title,
                    "description": request.body,
                    "source": {"branch": {"name": request.head}},
                    "destination": {"branch": {"name": request.base}},
                    "close_source_branch": false,
                }),
            )
            .await?;
        map_pr_detail(repo, &value)
    }
}

fn urlencode(value: &str) -> String {
    crate::http::encode_path_segment(value)
}

fn user_login(value: &Value) -> String {
    optional_text(value, "nickname")
        .or_else(|| optional_text(value, "username"))
        .or_else(|| optional_text(value, "display_name"))
        .unwrap_or_default()
}

fn same_user(value: &Value, login: &str) -> bool {
    user_login(value) == login
        || optional_text(value, "uuid") == Some(login.to_owned())
        || optional_text(value, "account_id") == Some(login.to_owned())
}

fn pr_state(value: &Value) -> ForgePrState {
    match value["state"].as_str().unwrap_or_default() {
        "MERGED" => ForgePrState::Merged,
        "DECLINED" | "SUPERSEDED" => ForgePrState::Closed,
        _ if value["draft"].as_bool().unwrap_or(false) => ForgePrState::Draft,
        _ => ForgePrState::Open,
    }
}

fn links_html(value: &Value) -> Option<String> {
    optional_text(&value["links"]["html"], "href")
}

fn map_pr_summary(repo: &ForgeRepoRef, value: &Value) -> Option<ForgePrSummary> {
    let number = value["id"].as_u64()? as u32;
    let updated_at = parse_datetime(&value["updated_on"], "updated_on").ok()?;
    Some(ForgePrSummary {
        repo: repo.clone(),
        number,
        title: text(value, "title"),
        state: pr_state(value),
        author: user_login(&value["author"]),
        head_branch: text(&value["source"]["branch"], "name"),
        base_branch: text(&value["destination"]["branch"], "name"),
        head_sha: optional_text(&value["source"]["commit"], "hash"),
        updated_at,
        web_url: links_html(value).unwrap_or_default(),
        checks: ForgeChecksState::Unknown,
        review: ForgeReviewState::Pending,
        draft: value["draft"].as_bool().unwrap_or(false),
    })
}

fn map_pr_detail(repo: &ForgeRepoRef, value: &Value) -> Result<ForgePullRequest> {
    let summary = map_pr_summary(repo, value)
        .ok_or_else(|| ForgeError::Parse("Bitbucket returned an incomplete pull request".into()))?;
    let body = optional_text(&value["summary"], "raw")
        .or_else(|| optional_text(value, "description"))
        .unwrap_or_default();
    Ok(ForgePullRequest {
        summary,
        body,
        labels: Vec::new(),
        // Bitbucket computes mergeability when the merge is attempted; the PR
        // object does not carry a conflict flag.
        mergeable: ForgeMergeable::Unknown,
        base_sha: optional_text(&value["destination"]["commit"], "hash"),
        additions: 0,
        deletions: 0,
        changed_files: 0,
        commits: 0,
        created_at: parse_datetime(&value["created_on"], "created_on")
            .unwrap_or_else(|_| Utc::now()),
        reviewers: Vec::new(),
        merge_commit_sha: optional_text(&value["merge_commit"], "hash"),
    })
}

/// Builds reviewers from the PR's participants and requested reviewers.
fn map_reviewers(pr: &Value) -> Vec<ForgeReview> {
    let mut reviews: Vec<ForgeReview> = Vec::new();
    let mut push = |login: String, state: ForgeReviewState, at: Option<chrono::DateTime<Utc>>| {
        if login.is_empty() || reviews.iter().any(|review| review.author == login) {
            return;
        }
        reviews.push(ForgeReview {
            author: login,
            state,
            submitted_at: at,
            body: None,
        });
    };

    if let Some(participants) = pr["participants"].as_array() {
        for participant in participants {
            let login = user_login(&participant["user"]);
            let state = match participant["state"].as_str().unwrap_or_default() {
                "approved" => ForgeReviewState::Approved,
                "changes_requested" => ForgeReviewState::ChangesRequested,
                _ if participant["role"].as_str() == Some("REVIEWER") => ForgeReviewState::Pending,
                _ => continue,
            };
            let at = parse_optional_datetime(participant.get("participated_on"), "participated_on")
                .ok()
                .flatten();
            push(login, state, at);
        }
    }
    if let Some(reviewers) = pr["reviewers"].as_array() {
        for reviewer in reviewers {
            push(user_login(reviewer), ForgeReviewState::Pending, None);
        }
    }
    reviews
}

fn aggregate_review(reviews: &[ForgeReview]) -> ForgeReviewState {
    if reviews
        .iter()
        .any(|review| review.state == ForgeReviewState::ChangesRequested)
    {
        ForgeReviewState::ChangesRequested
    } else if reviews
        .iter()
        .any(|review| review.state == ForgeReviewState::Approved)
    {
        ForgeReviewState::Approved
    } else if reviews
        .iter()
        .any(|review| review.state == ForgeReviewState::Commented)
    {
        ForgeReviewState::Commented
    } else {
        ForgeReviewState::Pending
    }
}

fn comment_id(value: &Value) -> Option<u64> {
    value["id"].as_u64()
}

fn comment_author(value: &Value) -> String {
    user_login(&value["user"])
}

fn comment_web_url(value: &Value) -> Option<String> {
    links_html(value)
}

fn map_comment(value: &Value) -> Option<ForgeComment> {
    if value["deleted"].as_bool().unwrap_or(false) {
        return None;
    }
    Some(ForgeComment {
        comment_id: comment_id(value)?.to_string(),
        author: comment_author(value),
        body: text(&value["content"], "raw"),
        created_at: parse_datetime(&value["created_on"], "created_on").ok()?,
        updated_at: parse_optional_datetime(value.get("updated_on"), "updated_on")
            .ok()
            .flatten(),
        web_url: comment_web_url(value),
    })
}

fn map_review_comment(value: &Value) -> Option<ForgeReviewComment> {
    if value["deleted"].as_bool().unwrap_or(false) {
        return None;
    }
    let inline = &value["inline"];
    let path = optional_text(inline, "path").unwrap_or_default();
    let (line, side) = match (inline["to"].as_u64(), inline["from"].as_u64()) {
        (Some(to), _) => (Some(to as u32), ForgeDiffSide::Right),
        (None, Some(from)) => (Some(from as u32), ForgeDiffSide::Left),
        _ => (None, ForgeDiffSide::Unknown),
    };
    Some(ForgeReviewComment {
        comment_id: comment_id(value)?.to_string(),
        thread_id: None,
        author: comment_author(value),
        body: text(&value["content"], "raw"),
        created_at: parse_datetime(&value["created_on"], "created_on").ok()?,
        updated_at: parse_optional_datetime(value.get("updated_on"), "updated_on")
            .ok()
            .flatten(),
        web_url: comment_web_url(value),
        path,
        line,
        side,
        in_reply_to: value["parent"]["id"].as_u64().map(|id| id.to_string()),
    })
}

/// Walks `parent` chains, then splits comments into inline threads and general
/// comments. A thread's resolution comes from its root comment.
fn group_comments(values: &[Value]) -> (Vec<ForgeComment>, Vec<ForgeReviewThread>) {
    use std::collections::HashMap;

    let mut by_id: HashMap<u64, &Value> = HashMap::new();
    for value in values {
        if let Some(id) = comment_id(value) {
            by_id.insert(id, value);
        }
    }

    let mut general: Vec<ForgeComment> = Vec::new();
    let mut thread_order: Vec<u64> = Vec::new();
    let mut thread_comments: HashMap<u64, Vec<ForgeReviewComment>> = HashMap::new();
    let mut roots: HashMap<u64, &Value> = HashMap::new();

    for value in values {
        if value["deleted"].as_bool().unwrap_or(false) {
            continue;
        }
        let Some(root) = root_comment(value, &by_id) else {
            continue;
        };
        let Some(root_id) = comment_id(root) else {
            continue;
        };
        let inline_path = optional_text(&root["inline"], "path").unwrap_or_default();
        if inline_path.is_empty() {
            if let Some(comment) = map_comment(value) {
                general.push(comment);
            }
            continue;
        }
        if let std::collections::hash_map::Entry::Vacant(entry) = roots.entry(root_id) {
            entry.insert(root);
            thread_order.push(root_id);
        }
        if let Some(mut comment) = map_review_comment(value) {
            comment.thread_id = Some(root_id.to_string());
            thread_comments.entry(root_id).or_default().push(comment);
        }
    }

    general.sort_by_key(|comment| comment.created_at);

    let mut threads = Vec::new();
    for root_id in thread_order {
        let Some(root) = roots.get(&root_id) else {
            continue;
        };
        let Some(mut comments) = thread_comments.remove(&root_id) else {
            continue;
        };
        comments.sort_by_key(|comment| comment.created_at);
        let inline = &root["inline"];
        let path = optional_text(inline, "path").unwrap_or_default();
        let (line, side) = match (inline["to"].as_u64(), inline["from"].as_u64()) {
            (Some(to), _) => (Some(to as u32), ForgeDiffSide::Right),
            (None, Some(from)) => (Some(from as u32), ForgeDiffSide::Left),
            _ => (None, ForgeDiffSide::Unknown),
        };
        threads.push(ForgeReviewThread {
            thread_id: root_id.to_string(),
            path,
            line,
            side,
            resolved: !root["resolution"].is_null(),
            outdated: inline["to"].as_u64().is_none() && inline["from"].as_u64().is_none(),
            comments,
        });
    }
    (general, threads)
}

/// Follows `parent` ids to the first comment in the chain.
fn root_comment<'a>(
    mut value: &'a Value,
    by_id: &std::collections::HashMap<u64, &'a Value>,
) -> Option<&'a Value> {
    let mut guard = 0;
    while let Some(parent) = value["parent"]["id"].as_u64() {
        let Some(next) = by_id.get(&parent).copied() else {
            break;
        };
        value = next;
        guard += 1;
        if guard > 100 {
            break;
        }
    }
    Some(value)
}

fn map_commit(value: &Value) -> Option<ForgeCommit> {
    let sha = optional_text(value, "hash")?;
    let (title, body) = split_message(&text(value, "message"));
    let author = optional_text(&value["author"]["user"], "display_name")
        .or_else(|| optional_text(&value["author"], "raw"))
        .unwrap_or_default();
    Some(ForgeCommit {
        sha,
        title,
        body,
        author,
        author_login: optional_text(&value["author"]["user"], "nickname"),
        authored_at: parse_optional_datetime(value.get("date"), "date")
            .ok()
            .flatten(),
        web_url: links_html(value),
    })
}

fn map_status(value: &Value) -> ForgeCheck {
    let created_at = parse_optional_datetime(value.get("created_on"), "created_on")
        .ok()
        .flatten();
    let updated_at = parse_optional_datetime(value.get("updated_on"), "updated_on")
        .ok()
        .flatten();
    ForgeCheck {
        name: optional_text(value, "name")
            .or_else(|| optional_text(value, "key"))
            .unwrap_or_default(),
        status: bitbucket_check_status(value["state"].as_str().unwrap_or_default()),
        details_url: optional_text(value, "url"),
        required: None,
        description: optional_text(value, "description"),
        duration_ms: duration_ms(created_at.as_ref(), updated_at.as_ref()),
        started_at: created_at,
        completed_at: updated_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bitbucket_maps_open_merged_draft_and_declined() {
        assert_eq!(pr_state(&json!({"state": "OPEN"})), ForgePrState::Open);
        assert_eq!(
            pr_state(&json!({"state": "OPEN", "draft": true})),
            ForgePrState::Draft
        );
        assert_eq!(pr_state(&json!({"state": "MERGED"})), ForgePrState::Merged);
        assert_eq!(
            pr_state(&json!({"state": "DECLINED"})),
            ForgePrState::Closed
        );
        assert_eq!(
            pr_state(&json!({"state": "SUPERSEDED"})),
            ForgePrState::Closed
        );
    }

    #[test]
    fn bitbucket_groups_replies_and_reads_resolution() {
        let comments = vec![
            json!({
                "id": 1,
                "content": {"raw": "root"},
                "user": {"nickname": "bob"},
                "created_on": "2024-05-01T10:00:00Z",
                "inline": {"path": "src/lib.rs", "from": null, "to": 4},
                "resolution": {"user": {"nickname": "alice"}}
            }),
            json!({
                "id": 2,
                "parent": {"id": 1},
                "content": {"raw": "reply"},
                "user": {"nickname": "alice"},
                "created_on": "2024-05-01T10:05:00Z"
            }),
        ];
        let (general, threads) = group_comments(&comments);
        assert!(general.is_empty());
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id, "1");
        assert!(threads[0].resolved);
        assert_eq!(threads[0].comments.len(), 2);
        assert_eq!(threads[0].comments[1].in_reply_to.as_deref(), Some("1"));
    }
}
