//! Gitea and Forgejo client (Gitea-compatible API v1).
//!
//! Forgejo is API-compatible with Gitea for everything the PR view needs; the
//! kind passed at construction only changes the label and the capability row.
//! Two differences are handled explicitly:
//!
//! - review threads have no resolved flag in the API, so threads are
//!   reconstructed from a review's comments and reported as unresolved;
//! - there is no nested reply endpoint, so a reply is posted as a general
//!   comment and the capability matrix says `nested_replies: false`.
//!
//! Diffs are requested as a raw unified patch and parsed locally, which works
//! on every Gitea 1.18+ and Forgejo version without depending on a newer JSON
//! files endpoint.

use async_trait::async_trait;
use h4rn_protocol::{
    ForgeCapabilities, ForgeCheck, ForgeChecks, ForgeChecksState, ForgeComment, ForgeCommentPage,
    ForgeCommit, ForgeCommits, ForgeDiffPage, ForgeDiffSide, ForgeFileDiff, ForgeFileStatus,
    ForgeIssue, ForgeIssueComment, ForgeIssueCommentPage, ForgeIssueFilter, ForgeIssuePage,
    ForgeIssueUpdate, ForgeKind, ForgeLabel, ForgeMergeable, ForgeNewPr, ForgePrFilter,
    ForgePrState, ForgePrSummary, ForgePullRequest, ForgeRepoRef, ForgeReview, ForgeReviewComment,
    ForgeReviewState, ForgeReviewThread,
};

use serde_json::{Value, json};

use crate::common::{
    context_matches, duration_ms, finish_checks, gitea_check_status, map_commit_gh_like,
    optional_text, parse_datetime, parse_optional_datetime, text, validate_repo,
};
use crate::error::{ForgeError, Result};
use crate::http::ForgeHttp;
use crate::remote::{api_base_url, validate_base_url};
use crate::{Forge, ForgeViewer, PrPage};

/// Gitea/Forgejo REST v1 client.
pub struct GiteaClient {
    kind: ForgeKind,
    http: ForgeHttp,
    api: String,
    base_url: String,
}

impl GiteaClient {
    /// Builds a client for one account.
    pub fn new(
        kind: ForgeKind,
        base_url: &str,
        token: &str,
        requests_per_minute: u32,
    ) -> Result<Self> {
        debug_assert!(matches!(kind, ForgeKind::Gitea | ForgeKind::Forgejo));
        let base_url = validate_base_url(base_url)?;
        let api = api_base_url(kind, &base_url);
        let auth = if token.trim().is_empty() {
            None
        } else {
            crate::http::auth_header("authorization", &format!("token {}", token.trim()))
        };
        let http = ForgeHttp::new(
            kind,
            &base_url,
            auth,
            "application/json",
            requests_per_minute,
        )?;
        Ok(Self {
            kind,
            http,
            api,
            base_url,
        })
    }

    fn repo_url(&self, repo: &ForgeRepoRef) -> String {
        format!(
            "{}/repos/{}/{}",
            self.api,
            urlencode(&repo.owner),
            urlencode(&repo.repo)
        )
    }

    fn pr_url(&self, repo: &ForgeRepoRef, number: u32) -> String {
        format!("{}/pulls/{number}", self.repo_url(repo))
    }

    async fn get_all(&self, url: &str, max_pages: usize) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        let mut page = 1;
        while page <= max_pages {
            let separator = if url.contains('?') { '&' } else { '?' };
            let current = format!("{url}{separator}limit=50&page={page}");
            let value = match self.http.get_json(&current).await {
                Ok(value) => value,
                Err(error) => {
                    if page == 1 {
                        return Err(error);
                    }
                    break;
                }
            };
            let Some(array) = value.as_array() else {
                break;
            };
            let count = array.len();
            values.extend(array.iter().cloned());
            if count < 50 {
                break;
            }
            page += 1;
        }
        Ok(values)
    }

    async fn fetch_pr(&self, repo: &ForgeRepoRef, number: u32) -> Result<Value> {
        self.http.get_json(&self.pr_url(repo, number)).await
    }

    /// The status-check patterns the base branch requires, or `None` when the
    /// protection rule cannot be read (it needs repository admin) or none is
    /// enabled. Unknown is better than a guess in either direction.
    async fn required_contexts(&self, repo: &ForgeRepoRef, base_ref: &str) -> Option<Vec<String>> {
        let url = format!(
            "{}/branch_protections/{}",
            self.repo_url(repo),
            urlencode(base_ref)
        );
        let value = self.http.get_json(&url).await.ok()?;
        if value["enable_status_check"].as_bool() != Some(true) {
            return None;
        }
        Some(
            value["status_check_contexts"]
                .as_array()?
                .iter()
                .filter_map(|context| context.as_str().map(str::to_owned))
                .collect(),
        )
    }
}

#[async_trait]
impl Forge for GiteaClient {
    fn kind(&self) -> ForgeKind {
        self.kind
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn capabilities(&self) -> ForgeCapabilities {
        crate::capabilities(self.kind)
    }

    async fn viewer(&self) -> Result<ForgeViewer> {
        let value = self.http.get_json(&format!("{}/user", self.api)).await?;
        let login = text(&value, "login");
        if login.is_empty() {
            return Err(ForgeError::Parse("Gitea returned no login".into()));
        }
        Ok(ForgeViewer {
            login,
            display_name: optional_text(&value, "full_name"),
            scopes: Vec::new(),
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
        let viewer = match filter {
            ForgePrFilter::Mine | ForgePrFilter::ReviewRequested => Some(self.viewer().await?),
            _ => None,
        };
        let mut url = format!(
            "{}/pulls?state=open&limit=50&page={page}",
            self.repo_url(repo)
        );
        if let ForgePrFilter::ByBranch { branch } = filter {
            // Gitea has no head filter; it is applied after the page is read.
            let _ = branch;
        }
        if matches!(filter, ForgePrFilter::Open) {
            let value = self.http.get_json(&url).await?;
            let items = value
                .as_array()
                .ok_or_else(|| ForgeError::Parse("Gitea returned no PR list".into()))?;
            let prs = items
                .iter()
                .filter_map(|item| map_pr_summary(repo, item))
                .collect::<Vec<_>>();
            return Ok(PrPage {
                has_more: items.len() == 50,
                prs,
                page,
            });
        }

        let mut matches = Vec::new();
        let mut current_page = page;
        let mut has_more = false;
        for _ in 0..4 {
            url = format!(
                "{}/pulls?state=open&limit=50&page={current_page}",
                self.repo_url(repo)
            );
            let value = self.http.get_json(&url).await?;
            let Some(items) = value.as_array() else {
                break;
            };
            has_more = items.len() == 50;
            for item in items {
                let Some(pr) = map_pr_summary(repo, item) else {
                    continue;
                };
                let keep = match filter {
                    ForgePrFilter::Mine => viewer
                        .as_ref()
                        .is_some_and(|viewer| pr.author == viewer.login),
                    ForgePrFilter::ReviewRequested => viewer.as_ref().is_some_and(|viewer| {
                        item["requested_reviewers"]
                            .as_array()
                            .is_some_and(|reviewers| {
                                reviewers
                                    .iter()
                                    .any(|reviewer| text(reviewer, "login") == viewer.login)
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
        let reviews = self
            .get_all(&format!("{}/reviews", self.pr_url(repo, number)), 3)
            .await;
        if let Ok(reviews) = reviews {
            pr.reviewers = reviews.iter().filter_map(map_review).collect();
            pr.summary.review = aggregate_review(&pr.reviewers);
        }
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
        let url = format!("{}.diff", self.pr_url(repo, number));
        match self.http.get_text(&url).await {
            Ok((body, content_type)) => {
                if content_type
                    .as_deref()
                    .is_some_and(|value| value.contains("json"))
                {
                    return self.files_fallback(repo, number, page, per_page).await;
                }
                let (files, has_more) =
                    crate::diff::paginate(crate::diff::parse_unified_diff(&body), page, per_page);
                Ok(ForgeDiffPage {
                    repo: repo.clone(),
                    number,
                    files,
                    page,
                    per_page,
                    has_more,
                })
            }
            Err(ForgeError::NotFound(_)) | Err(ForgeError::Http { status: 404, .. }) => {
                self.files_fallback(repo, number, page, per_page).await
            }
            Err(error) => Err(error),
        }
    }

    async fn list_comments(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeCommentPage> {
        validate_repo(repo)?;
        let comments = self
            .get_all(
                &format!("{}/issues/{number}/comments", self.repo_url(repo)),
                5,
            )
            .await?;
        let comments = comments.iter().filter_map(map_comment).collect::<Vec<_>>();
        let reviews = self
            .get_all(&format!("{}/reviews", self.pr_url(repo, number)), 3)
            .await
            .unwrap_or_default();
        let reviews = reviews.iter().filter_map(map_review).collect::<Vec<_>>();

        let mut threads = Vec::new();
        let review_values = self
            .get_all(&format!("{}/reviews", self.pr_url(repo, number)), 3)
            .await
            .unwrap_or_default();
        for review in &review_values {
            let Some(review_id) = review["id"].as_u64() else {
                continue;
            };
            let url = format!("{}/reviews/{review_id}/comments", self.pr_url(repo, number));
            if let Ok(values) = self.get_all(&url, 2).await
                && let Some(thread) = map_review_comments_thread(review_id, &values)
            {
                threads.push(thread);
            }
        }
        if threads.is_empty()
            && let Ok(values) = self
                .get_all(&format!("{}/comments", self.pr_url(repo, number)), 3)
                .await
        {
            threads = group_comments_into_threads(&values);
        }
        Ok(ForgeCommentPage {
            repo: repo.clone(),
            number,
            has_more: comments.len() >= 250,
            comments,
            threads,
            reviews,
        })
    }

    async fn list_checks(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeChecks> {
        validate_repo(repo)?;
        let pr = self.fetch_pr(repo, number).await?;
        let sha = pr["head"]["sha"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| ForgeError::Parse("the PR has no head SHA".into()))?;
        let url = format!("{}/commits/{sha}/statuses?limit=100", self.repo_url(repo));
        let values = self.get_all(&url, 2).await.unwrap_or_default();
        let mut checks = values.iter().map(map_status).collect::<Vec<_>>();
        if checks.is_empty()
            && let Ok(value) = self
                .http
                .get_json(&format!("{}/commits/{sha}/status", self.repo_url(repo)))
                .await
            && let Some(statuses) = value["statuses"].as_array()
        {
            checks = statuses.iter().map(map_status).collect();
        }
        if let Some(base_ref) = pr["base"]["ref"].as_str()
            && let Some(required) = self.required_contexts(repo, base_ref).await
        {
            for check in &mut checks {
                check.required = Some(
                    required
                        .iter()
                        .any(|pattern| context_matches(pattern, &check.name)),
                );
            }
        }
        Ok(finish_checks(repo, number, sha, checks))
    }

    async fn list_commits(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeCommits> {
        validate_repo(repo)?;
        let url = format!(
            "{}/commits?limit=50&verification=false&files=false",
            self.pr_url(repo, number)
        );
        let values = self.get_all(&url, 10).await?;
        let mut commits: Vec<ForgeCommit> = values.iter().filter_map(map_commit_gh_like).collect();
        // Gitea answers newest first; the tab reads oldest first.
        commits.reverse();
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
        // Gitea and Forgejo expose no nested reply endpoint. Posting the body
        // as a general comment is the documented degraded behaviour; the
        // comment still appears in the conversation and can be handed on.
        let url = format!("{}/issues/{number}/comments", self.repo_url(repo));
        let value = self.http.post_json(&url, &json!({"body": body})).await?;
        Ok(ForgeReviewComment {
            comment_id: value["id"]
                .as_u64()
                .map(|id| id.to_string())
                .unwrap_or_else(|| comment_id.to_owned()),
            thread_id: thread_id.map(ToOwned::to_owned),
            author: text(&value["user"], "login"),
            body: text(&value, "body"),
            created_at: parse_datetime(&value["created_at"], "created_at")
                .unwrap_or_else(|_| chrono::Utc::now()),
            updated_at: parse_optional_datetime(value.get("updated_at"), "updated_at")
                .ok()
                .flatten(),
            web_url: optional_text(&value, "html_url"),
            path: String::new(),
            line: None,
            side: ForgeDiffSide::Unknown,
            in_reply_to: Some(comment_id.to_owned()),
        })
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
        map_comment(&value).ok_or_else(|| ForgeError::Parse("Gitea returned no comment".into()))
    }

    async fn open_pr(&self, repo: &ForgeRepoRef, request: &ForgeNewPr) -> Result<ForgePullRequest> {
        validate_repo(repo)?;
        if request.head.trim().is_empty() || request.base.trim().is_empty() {
            return Err(ForgeError::InvalidRequest(
                "opening a PR needs a head and a base branch".into(),
            ));
        }
        let url = format!("{}/pulls", self.repo_url(repo));
        // Gitea's `CreatePullRequestOption` has no draft field. A pull request
        // is a draft when its title starts with `WIP:` or `[WIP]`, so the flag
        // travels in the title exactly as it does for GitLab.
        let title = if request.draft {
            crate::common::draft_title(&request.title, "WIP: ", &["WIP:", "[WIP]"])
        } else {
            request.title.clone()
        };
        let value = self
            .http
            .post_json(
                &url,
                &json!({
                    "title": title,
                    "body": request.body,
                    "head": request.head,
                    "base": request.base,
                }),
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
        crate::issues::gitea::list(&self.http, &self.api, repo, filter, page, query).await
    }

    async fn get_issue(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeIssue> {
        crate::issues::gitea::get(&self.http, &self.api, repo, number).await
    }

    async fn list_issue_comments(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<ForgeIssueCommentPage> {
        crate::issues::gitea::comments(&self.http, &self.api, repo, number).await
    }

    async fn comment_issue(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        body: &str,
    ) -> Result<ForgeIssueComment> {
        crate::issues::gitea::comment(&self.http, &self.api, repo, number, body).await
    }

    async fn update_issue(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        update: &ForgeIssueUpdate,
    ) -> Result<ForgeIssue> {
        crate::issues::gitea::update(&self.http, &self.api, repo, number, update).await
    }
}

impl GiteaClient {
    async fn files_fallback(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        page: u32,
        per_page: u32,
    ) -> Result<ForgeDiffPage> {
        let url = format!("{}/files", self.pr_url(repo, number));
        let value = self.http.get_json(&url).await?;
        let items = value
            .as_array()
            .ok_or_else(|| ForgeError::Parse("Gitea returned no diff files".into()))?;
        let files = items.iter().map(map_file).collect::<Vec<_>>();
        let (files, has_more) = crate::diff::paginate(files, page, per_page);
        Ok(ForgeDiffPage {
            repo: repo.clone(),
            number,
            files,
            page,
            per_page,
            has_more,
        })
    }
}

fn urlencode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
}

fn map_pr_summary(repo: &ForgeRepoRef, value: &Value) -> Option<ForgePrSummary> {
    let number = value["number"].as_u64()? as u32;
    let updated_at = parse_datetime(&value["updated_at"], "updated_at").ok()?;
    let draft = value["draft"].as_bool().unwrap_or(false);
    let state = if value["merged_at"].as_str().is_some()
        || value["merged"].as_bool().unwrap_or(false)
        || value["state"].as_str() == Some("merged")
    {
        ForgePrState::Merged
    } else if value["state"].as_str() == Some("closed") {
        ForgePrState::Closed
    } else if draft {
        ForgePrState::Draft
    } else {
        ForgePrState::Open
    };
    Some(ForgePrSummary {
        repo: repo.clone(),
        number,
        title: text(value, "title"),
        state,
        author: text(&value["user"], "login"),
        head_branch: text(&value["head"], "ref"),
        base_branch: text(&value["base"], "ref"),
        head_sha: optional_text(&value["head"], "sha"),
        updated_at,
        web_url: optional_text(value, "html_url").unwrap_or_default(),
        checks: ForgeChecksState::Unknown,
        review: ForgeReviewState::Pending,
        draft,
    })
}

fn map_pr_detail(repo: &ForgeRepoRef, value: &Value) -> Result<ForgePullRequest> {
    let summary = map_pr_summary(repo, value)
        .ok_or_else(|| ForgeError::Parse("Gitea returned an incomplete PR".into()))?;
    let labels = value["labels"]
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .map(|label| ForgeLabel {
                    name: text(label, "name"),
                    color: optional_text(label, "color")
                        .map(|color| color.trim_start_matches('#').to_owned()),
                })
                .collect()
        })
        .unwrap_or_default();
    let mergeable = match value["mergeable"].as_bool() {
        Some(true) => ForgeMergeable::Mergeable,
        Some(false) => ForgeMergeable::Conflicting,
        None => ForgeMergeable::Unknown,
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
        commits: 0,
        created_at: parse_datetime(&value["created_at"], "created_at")
            .unwrap_or_else(|_| chrono::Utc::now()),
        reviewers: Vec::new(),
        merge_commit_sha: optional_text(value, "merge_commit_sha"),
    })
}

fn map_file(value: &Value) -> ForgeFileDiff {
    let status = match value["status"].as_str().unwrap_or_default() {
        "added" => ForgeFileStatus::Added,
        "removed" | "deleted" => ForgeFileStatus::Deleted,
        "renamed" => ForgeFileStatus::Renamed,
        "changed" => ForgeFileStatus::Changed,
        _ => ForgeFileStatus::Modified,
    };
    let patch = optional_text(value, "patch");
    let additions = value["additions"].as_u64().unwrap_or(0) as u32;
    let deletions = value["deletions"].as_u64().unwrap_or(0) as u32;
    ForgeFileDiff {
        path: optional_text(value, "filename")
            .or_else(|| optional_text(value, "new_path"))
            .unwrap_or_default(),
        previous_path: optional_text(value, "previous_filename")
            .or_else(|| optional_text(value, "old_path")),
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

fn map_review(value: &Value) -> Option<ForgeReview> {
    let state = match value["state"].as_str().unwrap_or_default() {
        "APPROVED" => ForgeReviewState::Approved,
        "REQUEST_CHANGES" => ForgeReviewState::ChangesRequested,
        "COMMENT" => ForgeReviewState::Commented,
        "DISMISSED" => ForgeReviewState::Dismissed,
        "PENDING" => return None,
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

fn map_review_comments_thread(review_id: u64, values: &[Value]) -> Option<ForgeReviewThread> {
    let first = values.first()?;
    let path = optional_text(first, "path").unwrap_or_default();
    let line = first["original_position"]
        .as_u64()
        .or_else(|| first["position"].as_u64())
        .map(|line| line as u32);
    let comments = values
        .iter()
        .filter_map(|value| {
            Some(ForgeReviewComment {
                comment_id: value["id"].as_u64()?.to_string(),
                thread_id: Some(review_id.to_string()),
                author: text(&value["user"], "login"),
                body: text(value, "body"),
                created_at: parse_datetime(&value["created_at"], "created_at").ok()?,
                updated_at: parse_optional_datetime(value.get("updated_at"), "updated_at")
                    .ok()
                    .flatten(),
                web_url: optional_text(value, "html_url"),
                path: optional_text(value, "path").unwrap_or_else(|| path.clone()),
                line,
                side: ForgeDiffSide::Right,
                in_reply_to: None,
            })
        })
        .collect();
    Some(ForgeReviewThread {
        thread_id: review_id.to_string(),
        path,
        line,
        side: ForgeDiffSide::Right,
        resolved: false,
        outdated: false,
        comments,
    })
}

fn group_comments_into_threads(values: &[Value]) -> Vec<ForgeReviewThread> {
    let mut threads = Vec::new();
    for value in values {
        let Some(comment_id) = value["id"].as_u64() else {
            continue;
        };
        let path = optional_text(value, "path").unwrap_or_default();
        if path.is_empty() {
            continue;
        }
        let comment = ForgeReviewComment {
            comment_id: comment_id.to_string(),
            thread_id: Some(comment_id.to_string()),
            author: text(&value["user"], "login"),
            body: text(value, "body"),
            created_at: parse_datetime(&value["created_at"], "created_at")
                .unwrap_or_else(|_| chrono::Utc::now()),
            updated_at: parse_optional_datetime(value.get("updated_at"), "updated_at")
                .ok()
                .flatten(),
            web_url: optional_text(value, "html_url"),
            path: path.clone(),
            line: value["position"].as_u64().map(|line| line as u32),
            side: ForgeDiffSide::Right,
            in_reply_to: None,
        };
        threads.push(ForgeReviewThread {
            thread_id: comment_id.to_string(),
            path,
            line: comment.line,
            side: ForgeDiffSide::Right,
            resolved: false,
            outdated: false,
            comments: vec![comment],
        });
    }
    threads
}

fn map_status(value: &Value) -> ForgeCheck {
    let created_at = parse_optional_datetime(value.get("created_at"), "created_at")
        .ok()
        .flatten();
    let updated_at = parse_optional_datetime(value.get("updated_at"), "updated_at")
        .ok()
        .flatten();
    ForgeCheck {
        name: text(value, "context"),
        status: gitea_check_status(value["status"].as_str().unwrap_or_default()),
        details_url: optional_text(value, "target_url"),
        required: None,
        description: optional_text(value, "description"),
        duration_ms: duration_ms(created_at.as_ref(), updated_at.as_ref()),
        started_at: created_at,
        completed_at: updated_at,
    }
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
