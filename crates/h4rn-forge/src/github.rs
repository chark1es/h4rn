//! GitHub (github.com and GitHub Enterprise Server) client.
//!
//! REST handles listing, detail, diffs, comments and checks. GraphQL is used
//! for review threads because REST exposes flat inline comments with no thread
//! id and no resolved flag; the GraphQL `reviewThreads` connection has both.
//! If GraphQL is unavailable (very old GHES), the client falls back to grouping
//! REST comments by reply chain.

use async_trait::async_trait;
use chrono::Utc;
use h4rn_protocol::{
    ForgeCapabilities, ForgeCheck, ForgeChecks, ForgeChecksState, ForgeComment, ForgeCommentPage,
    ForgeCommits, ForgeDiffPage, ForgeDiffSide, ForgeFileDiff, ForgeFileStatus, ForgeIssue,
    ForgeIssueComment, ForgeIssueCommentPage, ForgeIssueFilter, ForgeIssuePage, ForgeIssueUpdate,
    ForgeKind, ForgeLabel, ForgeMergeable, ForgeNewPr, ForgePrFilter, ForgePrState, ForgePrSummary,
    ForgePullRequest, ForgeRepoRef, ForgeReview, ForgeReviewComment, ForgeReviewState,
    ForgeReviewThread,
};
use reqwest::Method;
use serde_json::{Value, json};
use url::form_urlencoded;

use crate::common::{
    duration_ms, finish_checks, github_check_status, map_commit_gh_like, optional_text,
    parse_datetime, parse_optional_datetime, text, validate_repo,
};
use crate::error::{ForgeError, Result};
use crate::http::{ForgeHttp, auth_header, next_link};
use crate::remote::{api_base_url, validate_base_url};
use crate::{Forge, ForgeViewer, PrPage};

/// GitHub REST and GraphQL client.
pub struct GitHubClient {
    http: ForgeHttp,
    api: String,
    graphql: String,
    base_url: String,
}

impl GitHubClient {
    /// Builds a client for one account. An empty token makes anonymous
    /// requests, which is enough for a read-only public-repository check.
    pub fn new(base_url: &str, token: &str, requests_per_minute: u32) -> Result<Self> {
        let base_url = validate_base_url(base_url)?;
        let api = api_base_url(ForgeKind::GitHub, &base_url);
        let graphql = graphql_url(&base_url);
        let auth = if token.trim().is_empty() {
            None
        } else {
            auth_header("authorization", &format!("Bearer {}", token.trim()))
        };
        let http = ForgeHttp::new(
            ForgeKind::GitHub,
            &base_url,
            auth,
            "application/vnd.github+json",
            requests_per_minute,
        )?;
        Ok(Self {
            http,
            api,
            graphql,
            base_url,
        })
    }

    fn repo_url(&self, repo: &ForgeRepoRef) -> String {
        format!(
            "{}/repos/{}/{}",
            self.api,
            encode_segment(&repo.owner),
            encode_segment(&repo.repo)
        )
    }

    async fn get_json(&self, url: &str) -> Result<Value> {
        self.http.get_json(url).await
    }

    async fn get_all(&self, url: &str, max_pages: usize) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        let mut next = Some(url.to_owned());
        let mut page = 0;
        while let Some(current) = next {
            page += 1;
            if page > max_pages {
                break;
            }
            let response = self
                .http
                .request(Method::GET, &current, None, false)
                .await?;
            let headers = response.headers.clone();
            let value = self.http.json(response)?;
            let Some(array) = value.as_array() else {
                break;
            };
            values.extend(array.iter().cloned());
            next = next_link(&headers);
        }
        Ok(values)
    }

    async fn fetch_pr(&self, repo: &ForgeRepoRef, number: u32) -> Result<Value> {
        self.get_json(&format!("{}/pulls/{number}", self.repo_url(repo)))
            .await
    }

    async fn fetch_reviews(&self, repo: &ForgeRepoRef, number: u32) -> Result<Vec<ForgeReview>> {
        let url = format!(
            "{}/pulls/{number}/reviews?per_page=100",
            self.repo_url(repo)
        );
        let values = self.get_all(&url, 3).await?;
        Ok(values.iter().filter_map(map_review).collect())
    }

    async fn review_threads(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<Option<Vec<ForgeReviewThread>>> {
        let query = r#"
query($owner: String!, $repo: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          isResolved
          isOutdated
          path
          line
          originalLine
          diffSide
          comments(first: 100) {
            nodes {
              id
              databaseId
              body
              createdAt
              updatedAt
              url
              author { login }
              replyTo { id }
            }
          }
        }
      }
    }
  }
}"#;
        let mut threads = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let body = json!({
                "query": query,
                "variables": {
                    "owner": repo.owner,
                    "repo": repo.repo,
                    "number": number,
                    "cursor": cursor,
                }
            });
            let value = match self.http.post_json(&self.graphql, &body).await {
                Ok(value) => value,
                Err(ForgeError::Unauthorized { .. } | ForgeError::Forbidden { .. }) => {
                    return Ok(None);
                }
                Err(_) => return Ok(None),
            };
            let connection = &value["data"]["repository"]["pullRequest"]["reviewThreads"];
            let Some(nodes) = connection["nodes"].as_array() else {
                return Ok(None);
            };
            for node in nodes {
                if let Some(thread) = map_review_thread(node) {
                    threads.push(thread);
                }
            }
            let has_next = connection["pageInfo"]["hasNextPage"]
                .as_bool()
                .unwrap_or(false);
            if !has_next {
                break;
            }
            cursor = connection["pageInfo"]["endCursor"]
                .as_str()
                .map(ToOwned::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(Some(threads))
    }

    fn fallback_threads(comments: &[Value]) -> Vec<ForgeReviewThread> {
        let mut roots: Vec<ForgeReviewThread> = Vec::new();
        let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for comment in comments {
            let Some(mapped) = map_review_comment(comment) else {
                continue;
            };
            let root_id = mapped
                .in_reply_to
                .clone()
                .unwrap_or_else(|| mapped.comment_id.clone());
            if let Some(position) = index.get(&root_id).copied() {
                roots[position].comments.push(mapped);
                continue;
            }
            let thread = ForgeReviewThread {
                thread_id: root_id.clone(),
                path: mapped.path.clone(),
                line: mapped.line,
                side: mapped.side,
                resolved: false,
                outdated: false,
                comments: vec![mapped],
            };
            index.insert(root_id, roots.len());
            roots.push(thread);
        }
        roots
    }

    /// Check names the base branch requires, or `None` when the token cannot
    /// read the rules (branch protection needs push access; rulesets are
    /// readable with plain read access, so both are tried).
    async fn required_contexts(&self, repo: &ForgeRepoRef, base_ref: &str) -> Option<Vec<String>> {
        let base = self.repo_url(repo);
        let branch = encode_segment(base_ref);
        let mut known = false;
        let mut names: Vec<String> = Vec::new();
        if let Ok(value) = self
            .get_json(&format!(
                "{base}/branches/{branch}/protection/required_status_checks"
            ))
            .await
        {
            known = true;
            if let Some(contexts) = value["contexts"].as_array() {
                names.extend(
                    contexts
                        .iter()
                        .filter_map(|c| c.as_str().map(str::to_owned)),
                );
            }
            if let Some(checks) = value["checks"].as_array() {
                names.extend(checks.iter().filter_map(|c| optional_text(c, "context")));
            }
        }
        if let Ok(value) = self
            .get_json(&format!("{base}/rules/branches/{branch}"))
            .await
            && let Some(rules) = value.as_array()
        {
            known = true;
            for rule in rules {
                if rule["type"] == "required_status_checks"
                    && let Some(checks) = rule["parameters"]["required_status_checks"].as_array()
                {
                    names.extend(checks.iter().filter_map(|c| optional_text(c, "context")));
                }
            }
        }
        known.then_some(names)
    }

    async fn head_sha(&self, repo: &ForgeRepoRef, number: u32) -> Result<String> {
        let pr = self.fetch_pr(repo, number).await?;
        pr["head"]["sha"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| ForgeError::Parse("the PR has no head SHA".into()))
    }
}

#[async_trait]
impl Forge for GitHubClient {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitHub
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn capabilities(&self) -> ForgeCapabilities {
        crate::capabilities(ForgeKind::GitHub)
    }

    async fn viewer(&self) -> Result<ForgeViewer> {
        let response = self
            .http
            .request(Method::GET, &format!("{}/user", self.api), None, false)
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
        let login = text(&value, "login");
        if login.is_empty() {
            return Err(ForgeError::Parse("GitHub returned no login".into()));
        }
        Ok(ForgeViewer {
            login,
            display_name: optional_text(&value, "name"),
            scopes,
        })
    }

    async fn list_prs(
        &self,
        repo: &ForgeRepoRef,
        filter: &ForgePrFilter,
        page: u32,
    ) -> Result<PrPage> {
        validate_repo(repo)?;
        let page = page.max(1);
        let (url, search) = match filter {
            ForgePrFilter::Open => (
                format!(
                    "{}/pulls?state=open&per_page=50&page={page}&sort=updated&direction=desc",
                    self.repo_url(repo)
                ),
                false,
            ),
            ForgePrFilter::ByBranch { branch } => (
                format!(
                    "{}/pulls?state=open&per_page=50&page={page}&head={}",
                    self.repo_url(repo),
                    urlencode(&format!("{}:{branch}", repo.owner))
                ),
                false,
            ),
            ForgePrFilter::Mine | ForgePrFilter::ReviewRequested => {
                let qualifier = match filter {
                    ForgePrFilter::Mine => "author:@me",
                    _ => "review-requested:@me",
                };
                let query = format!(
                    "repo:{}/{} is:pr is:open {qualifier}",
                    repo.owner, repo.repo
                );
                (
                    format!(
                        "{}/search/issues?q={}&per_page=50&page={page}&sort=updated&order=desc",
                        self.api,
                        urlencode(&query)
                    ),
                    true,
                )
            }
        };
        let value = self.get_json(&url).await?;
        if search {
            let items = value["items"]
                .as_array()
                .ok_or_else(|| ForgeError::Parse("GitHub search returned no items".into()))?;
            let total = value["total_count"].as_u64().unwrap_or(items.len() as u64);
            let prs = items
                .iter()
                .filter_map(|item| map_search_item(repo, item))
                .collect::<Vec<_>>();
            Ok(PrPage {
                has_more: total > u64::from(page) * 50,
                prs,
                page,
            })
        } else {
            let items = value
                .as_array()
                .ok_or_else(|| ForgeError::Parse("GitHub returned no PR list".into()))?;
            let prs = items
                .iter()
                .filter_map(|item| map_pr_summary(repo, item))
                .collect::<Vec<_>>();
            Ok(PrPage {
                has_more: items.len() == 50,
                prs,
                page,
            })
        }
    }

    async fn get_pr(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgePullRequest> {
        validate_repo(repo)?;
        let value = self.fetch_pr(repo, number).await?;
        let mut pr = map_pr_detail(repo, &value)?;
        pr.reviewers = self.fetch_reviews(repo, number).await.unwrap_or_default();
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
        let url = format!(
            "{}/pulls/{number}/files?per_page={per_page}&page={page}",
            self.repo_url(repo)
        );
        let value = self.get_json(&url).await?;
        let items = value
            .as_array()
            .ok_or_else(|| ForgeError::Parse("GitHub returned no diff files".into()))?;
        let files = items.iter().map(map_file_diff).collect::<Vec<_>>();
        Ok(ForgeDiffPage {
            repo: repo.clone(),
            number,
            has_more: items.len() == per_page as usize,
            files,
            page,
            per_page,
        })
    }

    async fn list_comments(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeCommentPage> {
        validate_repo(repo)?;
        let base = self.repo_url(repo);
        let general = self
            .get_all(&format!("{base}/issues/{number}/comments?per_page=100"), 5)
            .await?;
        let inline = self
            .get_all(&format!("{base}/pulls/{number}/comments?per_page=100"), 5)
            .await?;
        let reviews = self.fetch_reviews(repo, number).await?;
        let comments = general.iter().filter_map(map_comment).collect::<Vec<_>>();
        let threads = match self.review_threads(repo, number).await? {
            Some(threads) => threads,
            None => Self::fallback_threads(&inline),
        };
        let has_more = general.len() >= 500 || inline.len() >= 500;
        Ok(ForgeCommentPage {
            repo: repo.clone(),
            number,
            comments,
            threads,
            reviews,
            has_more,
        })
    }

    async fn list_checks(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeChecks> {
        validate_repo(repo)?;
        let pr = self.fetch_pr(repo, number).await?;
        let sha = pr["head"]["sha"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| ForgeError::Parse("the PR has no head SHA".into()))?;
        let base = self.repo_url(repo);
        let mut checks = Vec::new();

        if let Ok(value) = self
            .get_json(&format!("{base}/commits/{sha}/check-runs?per_page=100"))
            .await
        {
            if let Some(runs) = value["check_runs"].as_array() {
                for run in runs {
                    checks.push(map_check_run(run));
                }
            }
        }
        if let Ok(value) = self.get_json(&format!("{base}/commits/{sha}/status")).await {
            if let Some(contexts) = value["statuses"].as_array() {
                for context in contexts {
                    let name = text(context, "context");
                    if checks.iter().any(|check| check.name == name) {
                        continue;
                    }
                    checks.push(map_status_context(context));
                }
            }
        }
        if let Some(base_ref) = pr["base"]["ref"].as_str()
            && let Some(required) = self.required_contexts(repo, base_ref).await
        {
            for check in &mut checks {
                check.required = Some(required.iter().any(|name| name == &check.name));
            }
        }
        Ok(finish_checks(repo, number, sha, checks))
    }

    async fn list_commits(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeCommits> {
        validate_repo(repo)?;
        let url = format!(
            "{}/pulls/{number}/commits?per_page=100",
            self.repo_url(repo)
        );
        let values = self.get_all(&url, 5).await?;
        Ok(ForgeCommits {
            repo: repo.clone(),
            number,
            truncated: values.len() >= 500,
            commits: values.iter().filter_map(map_commit_gh_like).collect(),
        })
    }

    async fn rerun_failed_checks(&self, repo: &ForgeRepoRef, number: u32) -> Result<u32> {
        validate_repo(repo)?;
        let sha = self.head_sha(repo, number).await?;
        let base = self.repo_url(repo);
        let value = self
            .get_json(&format!("{base}/commits/{sha}/check-runs?per_page=100"))
            .await?;
        let mut suites: Vec<u64> = Vec::new();
        for run in value["check_runs"].as_array().into_iter().flatten() {
            let failed = github_check_status(
                run["status"].as_str().unwrap_or_default(),
                run["conclusion"].as_str(),
            )
            .is_failure();
            if let Some(id) = run["check_suite"]["id"].as_u64()
                && failed
                && !suites.contains(&id)
            {
                suites.push(id);
            }
        }
        if suites.is_empty() {
            return Err(ForgeError::Conflict(
                "there are no failed checks to re-run".into(),
            ));
        }
        for id in &suites {
            self.http
                .post_json(&format!("{base}/check-suites/{id}/rerequest"), &json!({}))
                .await?;
        }
        Ok(suites.len() as u32)
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
        let base = self.repo_url(repo);
        let url = format!("{base}/pulls/{number}/comments/{comment_id}/replies");
        let value = match self.http.post_json(&url, &json!({"body": body})).await {
            Ok(value) => value,
            Err(ForgeError::NotFound(_))
            | Err(ForgeError::Http {
                status: 404 | 422, ..
            }) => {
                let fallback = format!("{base}/pulls/{number}/comments");
                self.http
                    .post_json(&fallback, &json!({"body": body, "in_reply_to": comment_id}))
                    .await?
            }
            Err(error) => return Err(error),
        };
        let mut comment = map_review_comment(&value)
            .ok_or_else(|| ForgeError::Parse("GitHub returned no review comment".into()))?;
        if let Some(thread_id) = thread_id {
            comment.thread_id = Some(thread_id.to_owned());
        }
        Ok(comment)
    }

    async fn create_comment(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        body: &str,
    ) -> Result<ForgeComment> {
        validate_repo(repo)?;
        let url = format!("{}/issues/{number}/comments", self.repo_url(repo));
        let value = self.http.post_json(&url, &json!({"body": body})).await?;
        map_comment(&value).ok_or_else(|| ForgeError::Parse("GitHub returned no comment".into()))
    }

    async fn open_pr(&self, repo: &ForgeRepoRef, request: &ForgeNewPr) -> Result<ForgePullRequest> {
        self.open_pr_within(repo, request, crate::http::DEFAULT_REQUEST_TIMEOUT)
            .await
    }

    async fn open_pr_within(
        &self,
        repo: &ForgeRepoRef,
        request: &ForgeNewPr,
        budget: std::time::Duration,
    ) -> Result<ForgePullRequest> {
        validate_repo(repo)?;
        if request.head.trim().is_empty() || request.base.trim().is_empty() {
            return Err(ForgeError::InvalidRequest(
                "opening a PR needs a head and a base branch".into(),
            ));
        }
        let url = format!("{}/pulls", self.repo_url(repo));
        let value = self
            .http
            .post_json_within(
                &url,
                &json!({
                    "title": request.title,
                    "body": request.body,
                    "head": request.head,
                    "base": request.base,
                    "draft": request.draft,
                }),
                budget,
            )
            .await?;
        map_pr_detail(repo, &value)
    }

    async fn list_issues(
        &self,
        repo: &ForgeRepoRef,
        filter: ForgeIssueFilter,
        page: u32,
        query: Option<&str>,
    ) -> Result<ForgeIssuePage> {
        crate::issues::github::list(&self.http, &self.api, repo, filter, page, query).await
    }

    async fn get_issue(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeIssue> {
        crate::issues::github::get(&self.http, &self.api, repo, number).await
    }

    async fn list_issue_comments(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        cursor: Option<&str>,
    ) -> Result<ForgeIssueCommentPage> {
        crate::issues::github::comments(&self.http, &self.api, repo, number, cursor).await
    }

    async fn comment_issue(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        body: &str,
    ) -> Result<ForgeIssueComment> {
        crate::issues::github::comment(&self.http, &self.api, repo, number, body).await
    }

    async fn update_issue(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        update: &ForgeIssueUpdate,
    ) -> Result<ForgeIssue> {
        crate::issues::github::update(&self.http, &self.api, repo, number, update).await
    }
}

fn graphql_url(base_url: &str) -> String {
    if base_url.eq_ignore_ascii_case("https://github.com") {
        "https://api.github.com/graphql".to_owned()
    } else {
        format!("{}/api/graphql", base_url.trim_end_matches('/'))
    }
}

fn encode_segment(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
}

fn urlencode(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
}

fn pr_state(value: &Value) -> ForgePrState {
    if value["merged_at"].as_str().is_some()
        || value["merged"].as_bool().unwrap_or(false)
        || value["state"].as_str() == Some("merged")
    {
        ForgePrState::Merged
    } else if value["draft"].as_bool().unwrap_or(false) {
        ForgePrState::Draft
    } else if value["state"].as_str() == Some("closed") {
        ForgePrState::Closed
    } else {
        ForgePrState::Open
    }
}

fn map_pr_summary(repo: &ForgeRepoRef, value: &Value) -> Option<ForgePrSummary> {
    let number = value["number"].as_u64()? as u32;
    let title = text(value, "title");
    let author = text(&value["user"], "login");
    let head_branch = text(&value["head"], "ref");
    let base_branch = text(&value["base"], "ref");
    let head_sha = optional_text(&value["head"], "sha");
    let updated_at = parse_datetime(&value["updated_at"], "updated_at").ok()?;
    let web_url = optional_text(value, "html_url").unwrap_or_default();
    Some(ForgePrSummary {
        repo: repo.clone(),
        number,
        title,
        state: pr_state(value),
        author,
        head_branch,
        base_branch,
        head_sha,
        updated_at,
        web_url,
        checks: ForgeChecksState::Unknown,
        review: ForgeReviewState::Pending,
        draft: value["draft"].as_bool().unwrap_or(false),
    })
}

fn map_search_item(repo: &ForgeRepoRef, value: &Value) -> Option<ForgePrSummary> {
    let number = value["number"].as_u64()? as u32;
    let updated_at = parse_datetime(&value["updated_at"], "updated_at").ok()?;
    let pull = &value["pull_request"];
    Some(ForgePrSummary {
        repo: repo.clone(),
        number,
        title: text(value, "title"),
        state: if value["draft"].as_bool().unwrap_or(false) {
            ForgePrState::Draft
        } else if value["state"].as_str() == Some("closed") {
            ForgePrState::Closed
        } else {
            ForgePrState::Open
        },
        author: text(&value["user"], "login"),
        head_branch: String::new(),
        base_branch: String::new(),
        head_sha: None,
        updated_at,
        web_url: optional_text(value, "html_url")
            .or_else(|| optional_text(pull, "html_url"))
            .unwrap_or_default(),
        checks: ForgeChecksState::Unknown,
        review: ForgeReviewState::Pending,
        draft: value["draft"].as_bool().unwrap_or(false),
    })
}

fn map_pr_detail(repo: &ForgeRepoRef, value: &Value) -> Result<ForgePullRequest> {
    let summary = map_pr_summary(repo, value)
        .ok_or_else(|| ForgeError::Parse("GitHub returned an incomplete PR".into()))?;
    let labels = value["labels"]
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .map(|label| ForgeLabel {
                    name: text(label, "name"),
                    color: optional_text(label, "color"),
                })
                .collect()
        })
        .unwrap_or_default();
    let mergeable = match value["mergeable"].as_bool() {
        Some(true) => ForgeMergeable::Mergeable,
        Some(false) => ForgeMergeable::Conflicting,
        None => match value["mergeable_state"].as_str() {
            Some("dirty") => ForgeMergeable::Conflicting,
            Some("clean" | "unstable" | "blocked" | "behind" | "has_hooks") => {
                ForgeMergeable::Mergeable
            }
            _ => ForgeMergeable::Unknown,
        },
    };
    Ok(ForgePullRequest {
        summary,
        body: text(value, "body"),
        labels,
        mergeable,
        base_sha: optional_text(&value["base"], "sha"),
        additions: value["additions"].as_u64().unwrap_or(0) as u32,
        deletions: value["deletions"].as_u64().unwrap_or(0) as u32,
        line_counts_known: true,
        changed_files: value["changed_files"].as_u64().unwrap_or(0) as u32,
        changed_files_known: true,
        commits: value["commits"].as_u64().unwrap_or(0) as u32,
        created_at: parse_datetime(&value["created_at"], "created_at")
            .unwrap_or_else(|_| Utc::now()),
        reviewers: Vec::new(),
        merge_commit_sha: optional_text(value, "merge_commit_sha"),
    })
}

fn map_file_diff(value: &Value) -> ForgeFileDiff {
    let status = match value["status"].as_str().unwrap_or_default() {
        "added" => ForgeFileStatus::Added,
        "removed" => ForgeFileStatus::Deleted,
        "renamed" => ForgeFileStatus::Renamed,
        "copied" => ForgeFileStatus::Copied,
        "changed" => ForgeFileStatus::Changed,
        "modified" => ForgeFileStatus::Modified,
        _ => ForgeFileStatus::Unknown,
    };
    let additions = value["additions"].as_u64().unwrap_or(0) as u32;
    let deletions = value["deletions"].as_u64().unwrap_or(0) as u32;
    let patch = optional_text(value, "patch");
    ForgeFileDiff {
        path: text(value, "filename"),
        previous_path: optional_text(value, "previous_filename"),
        status,
        additions,
        deletions,
        binary: patch.is_none() && additions == 0 && deletions == 0,
        truncated: false,
        patch,
    }
}

fn map_comment(value: &Value) -> Option<ForgeComment> {
    Some(ForgeComment {
        comment_id: value["id"].as_u64()?.to_string(),
        author: text(&value["user"], "login"),
        body: text(value, "body"),
        created_at: parse_datetime(&value["created_at"], "created_at").ok()?,
        updated_at: parse_optional_datetime(value.get("updated_at"), "updated_at")
            .ok()
            .flatten(),
        web_url: optional_text(value, "html_url"),
    })
}

fn map_review_comment(value: &Value) -> Option<ForgeReviewComment> {
    let side = match value["side"].as_str() {
        Some("LEFT") => ForgeDiffSide::Left,
        Some("RIGHT") => ForgeDiffSide::Right,
        _ => ForgeDiffSide::Unknown,
    };
    Some(ForgeReviewComment {
        comment_id: value["id"].as_u64()?.to_string(),
        thread_id: None,
        author: text(&value["user"], "login"),
        body: text(value, "body"),
        created_at: parse_datetime(&value["created_at"], "created_at").ok()?,
        updated_at: parse_optional_datetime(value.get("updated_at"), "updated_at")
            .ok()
            .flatten(),
        web_url: optional_text(value, "html_url"),
        path: text(value, "path"),
        line: value["line"]
            .as_u64()
            .or_else(|| value["original_line"].as_u64())
            .map(|line| line as u32),
        side,
        in_reply_to: value["in_reply_to_id"].as_u64().map(|id| id.to_string()),
    })
}

fn map_review_thread(value: &Value) -> Option<ForgeReviewThread> {
    let side = match value["diffSide"].as_str() {
        Some("LEFT") => ForgeDiffSide::Left,
        Some("RIGHT") => ForgeDiffSide::Right,
        _ => ForgeDiffSide::Unknown,
    };
    let comments = value["comments"]["nodes"]
        .as_array()?
        .iter()
        .filter_map(|comment| {
            let comment_id = comment["databaseId"]
                .as_u64()
                .map(|id| id.to_string())
                .or_else(|| optional_text(comment, "id"))?;
            Some(ForgeReviewComment {
                comment_id,
                thread_id: optional_text(value, "id"),
                author: text(&comment["author"], "login"),
                body: text(comment, "body"),
                created_at: parse_datetime(&comment["createdAt"], "createdAt").ok()?,
                updated_at: parse_optional_datetime(comment.get("updatedAt"), "updatedAt")
                    .ok()
                    .flatten(),
                web_url: optional_text(comment, "url"),
                path: text(value, "path"),
                line: value["line"]
                    .as_u64()
                    .or_else(|| value["originalLine"].as_u64())
                    .map(|line| line as u32),
                side,
                in_reply_to: comment["replyTo"]["id"].as_str().map(ToOwned::to_owned),
            })
        })
        .collect::<Vec<_>>();
    Some(ForgeReviewThread {
        thread_id: optional_text(value, "id")?,
        path: text(value, "path"),
        line: value["line"]
            .as_u64()
            .or_else(|| value["originalLine"].as_u64())
            .map(|line| line as u32),
        side,
        resolved: value["isResolved"].as_bool().unwrap_or(false),
        outdated: value["isOutdated"].as_bool().unwrap_or(false),
        comments,
    })
}

fn map_review(value: &Value) -> Option<ForgeReview> {
    let state = match value["state"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "approved" => ForgeReviewState::Approved,
        "changes_requested" => ForgeReviewState::ChangesRequested,
        "commented" => ForgeReviewState::Commented,
        "dismissed" => ForgeReviewState::Dismissed,
        "pending" => ForgeReviewState::Pending,
        _ => ForgeReviewState::Pending,
    };
    Some(ForgeReview {
        author: text(&value["user"], "login"),
        state,
        submitted_at: parse_optional_datetime(value.get("submitted_at"), "submitted_at")
            .ok()
            .flatten(),
        body: optional_text(value, "body").filter(|body| !body.is_empty()),
    })
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

fn map_check_run(value: &Value) -> ForgeCheck {
    let started_at = parse_optional_datetime(value.get("started_at"), "started_at")
        .ok()
        .flatten();
    let completed_at = parse_optional_datetime(value.get("completed_at"), "completed_at")
        .ok()
        .flatten();
    ForgeCheck {
        name: text(value, "name"),
        status: github_check_status(
            value["status"].as_str().unwrap_or_default(),
            value["conclusion"].as_str(),
        ),
        details_url: optional_text(value, "html_url")
            .or_else(|| optional_text(value, "details_url")),
        required: None,
        description: optional_text(&value["output"], "title")
            .or_else(|| optional_text(&value["output"], "summary")),
        duration_ms: duration_ms(started_at.as_ref(), completed_at.as_ref()),
        started_at,
        completed_at,
    }
}

fn map_status_context(value: &Value) -> ForgeCheck {
    let updated_at = parse_optional_datetime(value.get("updated_at"), "updated_at")
        .ok()
        .flatten();
    let created_at = parse_optional_datetime(value.get("created_at"), "created_at")
        .ok()
        .flatten();
    ForgeCheck {
        name: text(value, "context"),
        status: github_check_status(value["state"].as_str().unwrap_or_default(), None),
        details_url: optional_text(value, "target_url"),
        required: None,
        description: optional_text(value, "description"),
        duration_ms: duration_ms(created_at.as_ref(), updated_at.as_ref()),
        started_at: created_at,
        completed_at: updated_at,
    }
}
