//! GitLab (gitlab.com and self-hosted) client.
//!
//! GitLab's REST v4 has everything the PR view needs: merge requests,
//! paginated diffs, discussions with resolvable inline notes, and pipelines
//! with jobs. There is no thread-resolution mutation in the v1 scope, matching
//! the capability matrix.
//!
//! Authentication is a personal access token in `PRIVATE-TOKEN`. GitLab does
//! not implement RFC 8628 device authorization; see `docs/forges.md`.

use async_trait::async_trait;
use h4rn_protocol::{
    ForgeCapabilities, ForgeCheck, ForgeChecks, ForgeChecksState, ForgeComment, ForgeCommentPage,
    ForgeCommit, ForgeCommits, ForgeDiffPage, ForgeDiffSide, ForgeFileDiff, ForgeFileStatus,
    ForgeKind, ForgeLabel, ForgeMergeable, ForgeNewPr, ForgePrFilter, ForgePrState, ForgePrSummary,
    ForgePullRequest, ForgeRepoRef, ForgeReview, ForgeReviewComment, ForgeReviewState,
    ForgeReviewThread,
};
use reqwest::Method;
use serde_json::{Value, json};
use url::form_urlencoded;

use crate::common::{
    duration_ms, encode_project_path, finish_checks, gitlab_check_status, optional_text,
    parse_datetime, parse_optional_datetime, split_message, text, validate_repo,
};
use crate::error::{ForgeError, Result};
use crate::http::{ForgeHttp, next_link};
use crate::remote::{api_base_url, validate_base_url};
use crate::{Forge, ForgeViewer, PrPage};

/// GitLab REST v4 client.
pub struct GitLabClient {
    http: ForgeHttp,
    api: String,
    base_url: String,
}

impl GitLabClient {
    /// Builds a client for one account.
    pub fn new(base_url: &str, token: &str, requests_per_minute: u32) -> Result<Self> {
        let base_url = validate_base_url(base_url)?;
        let api = api_base_url(ForgeKind::GitLab, &base_url);
        let auth = if token.trim().is_empty() {
            None
        } else {
            crate::http::auth_header("private-token", token.trim())
        };
        let http = ForgeHttp::new(
            ForgeKind::GitLab,
            &base_url,
            auth,
            "application/json",
            requests_per_minute,
        )?;
        Ok(Self {
            http,
            api,
            base_url,
        })
    }

    fn project_url(&self, repo: &ForgeRepoRef) -> String {
        format!("{}/projects/{}", self.api, encode_project_path(repo))
    }

    fn mr_url(&self, repo: &ForgeRepoRef, number: u32) -> String {
        format!("{}/merge_requests/{number}", self.project_url(repo))
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

    async fn fetch_mr(&self, repo: &ForgeRepoRef, number: u32) -> Result<Value> {
        self.http.get_json(&self.mr_url(repo, number)).await
    }
}

#[async_trait]
impl Forge for GitLabClient {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitLab
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn capabilities(&self) -> ForgeCapabilities {
        crate::capabilities(ForgeKind::GitLab)
    }

    async fn viewer(&self) -> Result<ForgeViewer> {
        let value = self.http.get_json(&format!("{}/user", self.api)).await?;
        let login = text(&value, "username");
        if login.is_empty() {
            return Err(ForgeError::Parse("GitLab returned no username".into()));
        }
        Ok(ForgeViewer {
            login,
            display_name: optional_text(&value, "name"),
            scopes: optional_text(&value, "state")
                .map(|state| vec![state])
                .unwrap_or_default(),
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
        let mut query = vec![
            ("state", "opened".to_owned()),
            ("per_page", "50".to_owned()),
            ("page", page.to_string()),
            ("order_by", "updated_at".to_owned()),
            ("sort", "desc".to_owned()),
        ];
        match filter {
            ForgePrFilter::Open => {}
            ForgePrFilter::Mine => query.push(("scope", "created_by_me".to_owned())),
            ForgePrFilter::ByBranch { branch } => {
                query.push(("source_branch", branch.clone()));
            }
            ForgePrFilter::ReviewRequested => {
                let viewer = self.viewer().await?;
                let user = self
                    .http
                    .get_json(&format!(
                        "{}/users?username={}",
                        self.api,
                        urlencode(&viewer.login)
                    ))
                    .await?;
                let id = user
                    .as_array()
                    .and_then(|users| users.first())
                    .and_then(|user| user["id"].as_u64())
                    .ok_or_else(|| {
                        ForgeError::NotFound("could not resolve the GitLab viewer id".into())
                    })?;
                query.push(("reviewer_id", id.to_string()));
            }
        }
        let url = format!(
            "{}/merge_requests?{}",
            self.project_url(repo),
            encode_query(&query)
        );
        let value = self.http.get_json(&url).await?;
        let items = value
            .as_array()
            .ok_or_else(|| ForgeError::Parse("GitLab returned no merge request list".into()))?;
        let prs = items
            .iter()
            .filter_map(|item| map_mr_summary(repo, item))
            .collect::<Vec<_>>();
        Ok(PrPage {
            has_more: items.len() == 50,
            prs,
            page,
        })
    }

    async fn get_pr(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgePullRequest> {
        validate_repo(repo)?;
        let value = self.fetch_mr(repo, number).await?;
        let mut pr = map_mr_detail(repo, &value)?;
        pr.reviewers = self.reviewers(repo, number, &value).await;
        pr.summary.review = aggregate_review(&pr.reviewers);
        // GitLab omits line counts on the MR, so the detail sums the diff once.
        // An `overflow` answer, or a change GitLab collapsed or left out as too
        // large, means the list is incomplete: a partial sum would be a wrong
        // number, so the counts stay unknown instead.
        if let Ok((_, additions, deletions, files, truncated)) =
            self.change_stats(repo, number).await
            && !truncated
        {
            pr.additions = additions;
            pr.deletions = deletions;
            pr.line_counts_known = true;
            pr.changed_files = files;
            pr.changed_files_known = true;
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
        let url = format!(
            "{}/diffs?page={page}&per_page={per_page}",
            self.mr_url(repo, number)
        );
        let value = match self.http.get_json(&url).await {
            Ok(value) => value,
            Err(ForgeError::NotFound(_)) | Err(ForgeError::Http { status: 404, .. }) => {
                // Older GitLab exposes the diff only through `/changes`.
                let changes = self
                    .http
                    .get_json(&format!("{}/changes", self.mr_url(repo, number)))
                    .await?;
                let items = changes["changes"].as_array().cloned().unwrap_or_default();
                let (files, has_more) =
                    crate::diff::paginate(items.iter().map(map_change).collect(), page, per_page);
                return Ok(ForgeDiffPage {
                    repo: repo.clone(),
                    number,
                    files,
                    page,
                    per_page,
                    has_more,
                });
            }
            Err(error) => return Err(error),
        };
        let items = value
            .as_array()
            .ok_or_else(|| ForgeError::Parse("GitLab returned no diff list".into()))?;
        Ok(ForgeDiffPage {
            repo: repo.clone(),
            number,
            files: items.iter().map(map_change).collect(),
            page,
            per_page,
            has_more: items.len() == per_page as usize,
        })
    }

    async fn list_comments(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeCommentPage> {
        validate_repo(repo)?;
        let url = format!("{}/discussions?per_page=100", self.mr_url(repo, number));
        let discussions = self.get_all(&url, 5).await?;
        let mut comments = Vec::new();
        let mut threads = Vec::new();
        for discussion in &discussions {
            let notes = discussion["notes"].as_array().cloned().unwrap_or_default();
            let inline = notes.iter().any(|note| note.get("position").is_some());
            if inline {
                if let Some(thread) = map_discussion_thread(discussion, &notes) {
                    threads.push(thread);
                }
            } else {
                comments.extend(notes.iter().filter_map(map_note_comment));
            }
        }
        let mr = self.fetch_mr(repo, number).await.unwrap_or(Value::Null);
        let reviews = self.reviewers(repo, number, &mr).await;
        Ok(ForgeCommentPage {
            repo: repo.clone(),
            number,
            has_more: discussions.len() >= 500,
            comments,
            threads,
            reviews,
        })
    }

    async fn list_checks(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeChecks> {
        validate_repo(repo)?;
        let mr = self.fetch_mr(repo, number).await?;
        let sha = mr["sha"].as_str().unwrap_or_default().to_owned();
        let pipeline = if mr["head_pipeline"]["id"].as_u64().is_some() {
            mr["head_pipeline"].clone()
        } else {
            let url = format!("{}/pipelines?sha={sha}&per_page=1", self.project_url(repo));
            let value = self.http.get_json(&url).await?;
            value
                .as_array()
                .and_then(|pipelines| pipelines.first())
                .cloned()
                .unwrap_or(Value::Null)
        };
        let mut checks = Vec::new();
        if let Some(id) = pipeline["id"].as_u64() {
            let jobs_url = format!(
                "{}/pipelines/{id}/jobs?per_page=100",
                self.project_url(repo)
            );
            if let Ok(jobs) = self.get_all(&jobs_url, 5).await {
                for job in &jobs {
                    checks.push(map_job(job));
                }
            }
        }
        if checks.is_empty() && pipeline["status"].as_str().is_some() {
            checks.push(map_pipeline(&pipeline));
        }
        Ok(finish_checks(repo, number, sha, checks))
    }

    async fn list_commits(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeCommits> {
        validate_repo(repo)?;
        let url = format!("{}/commits?per_page=100", self.mr_url(repo, number));
        let values = self.get_all(&url, 5).await?;
        // GitLab lists the newest commit first; the tab reads oldest first.
        let commits = values
            .iter()
            .rev()
            .filter_map(|value| {
                let sha = optional_text(value, "id")?;
                let (title, body) = split_message(&text(value, "message"));
                Some(ForgeCommit {
                    sha,
                    title: optional_text(value, "title").unwrap_or(title),
                    body,
                    author: text(value, "author_name"),
                    author_login: None,
                    authored_at: parse_optional_datetime(
                        value.get("authored_date"),
                        "authored_date",
                    )
                    .ok()
                    .flatten(),
                    web_url: optional_text(value, "web_url"),
                })
            })
            .collect::<Vec<_>>();
        Ok(ForgeCommits {
            repo: repo.clone(),
            number,
            truncated: values.len() >= 500,
            commits,
        })
    }

    async fn rerun_failed_checks(&self, repo: &ForgeRepoRef, number: u32) -> Result<u32> {
        validate_repo(repo)?;
        let checks = self.list_checks(repo, number).await?;
        let failed = checks.failed;
        if failed == 0 {
            return Err(ForgeError::Conflict(
                "there are no failed checks to re-run".into(),
            ));
        }
        let mr = self.fetch_mr(repo, number).await?;
        let pipeline_id = mr["head_pipeline"]["id"].as_u64().ok_or_else(|| {
            ForgeError::Conflict("the merge request has no pipeline to retry".into())
        })?;
        // Retry re-queues only the pipeline's failed jobs.
        self.http
            .post_json(
                &format!("{}/pipelines/{pipeline_id}/retry", self.project_url(repo)),
                &json!({}),
            )
            .await?;
        Ok(failed)
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
        let Some(thread_id) = thread_id else {
            // A general note is a discussion too; GitLab creates one per note.
            let url = format!("{}/notes", self.mr_url(repo, number));
            let value = self.http.post_json(&url, &json!({"body": body})).await?;
            return Ok(ForgeReviewComment {
                comment_id: value["id"]
                    .as_u64()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| comment_id.to_owned()),
                thread_id: None,
                author: text(&value["author"], "username"),
                body: text(&value, "body"),
                created_at: parse_datetime(&value["created_at"], "created_at")
                    .unwrap_or_else(|_| chrono::Utc::now()),
                updated_at: parse_optional_datetime(value.get("updated_at"), "updated_at")
                    .ok()
                    .flatten(),
                web_url: None,
                path: String::new(),
                line: None,
                side: ForgeDiffSide::Unknown,
                in_reply_to: Some(comment_id.to_owned()),
            });
        };
        let url = format!(
            "{}/discussions/{thread_id}/notes",
            self.mr_url(repo, number)
        );
        let value = self.http.post_json(&url, &json!({"body": body})).await?;
        map_note_comment(&value)
            .map(|comment| ForgeReviewComment {
                comment_id: comment.comment_id,
                thread_id: Some(thread_id.to_owned()),
                author: comment.author,
                body: comment.body,
                created_at: comment.created_at,
                updated_at: comment.updated_at,
                web_url: comment.web_url,
                path: String::new(),
                line: None,
                side: ForgeDiffSide::Unknown,
                in_reply_to: Some(comment_id.to_owned()),
            })
            .ok_or_else(|| ForgeError::Parse("GitLab returned no note".into()))
    }

    async fn create_comment(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        body: &str,
    ) -> Result<ForgeComment> {
        validate_repo(repo)?;
        let url = format!("{}/notes", self.mr_url(repo, number));
        let value = self.http.post_json(&url, &json!({"body": body})).await?;
        map_note_comment(&value).ok_or_else(|| ForgeError::Parse("GitLab returned no note".into()))
    }

    async fn open_pr(&self, repo: &ForgeRepoRef, request: &ForgeNewPr) -> Result<ForgePullRequest> {
        validate_repo(repo)?;
        if request.head.trim().is_empty() || request.base.trim().is_empty() {
            return Err(ForgeError::InvalidRequest(
                "opening an MR needs a source and a target branch".into(),
            ));
        }
        let url = format!("{}/merge_requests", self.project_url(repo));
        let title = if request.draft {
            // GitLab marks a draft by the title prefix; its create endpoint
            // has no separate boolean field.
            format!("Draft: {}", request.title)
        } else {
            request.title.clone()
        };
        let value = self
            .http
            .post_json(
                &url,
                &json!({
                    "source_branch": request.head,
                    "target_branch": request.base,
                    "title": title,
                    "description": request.body,
                }),
            )
            .await?;
        map_mr_detail(repo, &value)
    }
}

impl GitLabClient {
    async fn reviewers(&self, repo: &ForgeRepoRef, number: u32, mr: &Value) -> Vec<ForgeReview> {
        let mut reviews = Vec::new();
        if let Some(approved) = mr["approved_by"].as_array() {
            for approval in approved {
                reviews.push(ForgeReview {
                    author: text(&approval["user"], "username"),
                    state: ForgeReviewState::Approved,
                    submitted_at: None,
                    body: None,
                });
            }
        }
        // Newer GitLab exposes an approvals endpoint with the same list.
        if reviews.is_empty()
            && let Ok(value) = self
                .http
                .get_json(&format!("{}/approvals", self.mr_url(repo, number)))
                .await
            && let Some(approved) = value["approved_by"].as_array()
        {
            for approval in approved {
                reviews.push(ForgeReview {
                    author: text(&approval["user"], "username"),
                    state: ForgeReviewState::Approved,
                    submitted_at: None,
                    body: None,
                });
            }
        }
        if let Some(reviewers) = mr["reviewers"].as_array() {
            for reviewer in reviewers {
                let login = text(reviewer, "username");
                if reviews.iter().any(|review| review.author == login) {
                    continue;
                }
                reviews.push(ForgeReview {
                    author: login,
                    state: ForgeReviewState::Pending,
                    submitted_at: None,
                    body: None,
                });
            }
        }
        reviews
    }

    /// The MR's changes, their summed line counts and whether GitLab cut the
    /// list off.
    ///
    /// When `overflow` is true the `changes` array is incomplete, and a change
    /// marked `collapsed` or `too_large` carries an incomplete patch of its
    /// own. Either way the line totals and file count would be partial; the
    /// caller keeps them unknown.
    async fn change_stats(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
    ) -> Result<(Value, u32, u32, u32, bool)> {
        let value = self
            .http
            .get_json(&format!("{}/changes", self.mr_url(repo, number)))
            .await?;
        let changes = value["changes"]
            .as_array()
            .cloned()
            .ok_or_else(|| ForgeError::Parse("GitLab returned no changes collection".into()))?;
        let mut additions = 0;
        let mut deletions = 0;
        for change in &changes {
            let (adds, dels) = count_diff(&text(change, "diff"));
            additions += adds;
            deletions += dels;
        }
        let files = changes.len() as u32;
        let overflow = value["overflow"].as_bool().unwrap_or(false);
        let truncated = overflow || changes.iter().any(change_truncated);
        Ok((value, additions, deletions, files, truncated))
    }
}

fn encode_query(pairs: &[(&str, String)]) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn urlencode(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
}

fn map_mr_summary(repo: &ForgeRepoRef, value: &Value) -> Option<ForgePrSummary> {
    let number = value["iid"].as_u64()? as u32;
    let updated_at = parse_datetime(&value["updated_at"], "updated_at").ok()?;
    let draft = value["draft"].as_bool().unwrap_or(false);
    let state = if value["state"].as_str() == Some("merged") {
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
        author: text(&value["author"], "username"),
        head_branch: text(value, "source_branch"),
        base_branch: text(value, "target_branch"),
        head_sha: optional_text(value, "sha"),
        updated_at,
        web_url: optional_text(value, "web_url").unwrap_or_default(),
        checks: ForgeChecksState::Unknown,
        review: ForgeReviewState::Pending,
        draft,
    })
}

fn map_mr_detail(repo: &ForgeRepoRef, value: &Value) -> Result<ForgePullRequest> {
    let summary = map_mr_summary(repo, value)
        .ok_or_else(|| ForgeError::Parse("GitLab returned an incomplete merge request".into()))?;
    let labels = value["labels"]
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .filter_map(|label| {
                    let name = label
                        .as_str()
                        .map(ToOwned::to_owned)
                        .or_else(|| optional_text(label, "name"))?;
                    let color = optional_text(label, "color")
                        .map(|color| color.trim_start_matches('#').to_owned());
                    Some(ForgeLabel { name, color })
                })
                .collect()
        })
        .unwrap_or_default();
    let mergeable = if value["has_conflicts"].as_bool().unwrap_or(false) {
        ForgeMergeable::Conflicting
    } else {
        match value["detailed_merge_status"].as_str() {
            Some("mergeable") | Some("ci_still_running") | Some("not_approved") => {
                ForgeMergeable::Mergeable
            }
            Some("conflict") | Some("cannot_be_merged") => ForgeMergeable::Conflicting,
            _ => match value["merge_status"].as_str() {
                Some("can_be_merged") => ForgeMergeable::Mergeable,
                Some("cannot_be_merged") => ForgeMergeable::Conflicting,
                _ => ForgeMergeable::Unknown,
            },
        }
    };
    let changed_files = value["changes_count"]
        .as_str()
        .and_then(|count| count.trim_end_matches('+').parse::<u32>().ok())
        .or_else(|| value["changes_count"].as_u64().map(|count| count as u32))
        .unwrap_or(0);
    Ok(ForgePullRequest {
        summary,
        body: text(value, "description"),
        labels,
        mergeable,
        base_sha: optional_text(&value["diff_refs"], "base_sha"),
        additions: 0,
        deletions: 0,
        // GitLab's merge request object carries no line totals; they only
        // exist per file, so the counts are unknown rather than zero.
        line_counts_known: false,
        changed_files,
        changed_files_known: true,
        commits: 0,
        created_at: parse_datetime(&value["created_at"], "created_at")
            .unwrap_or_else(|_| chrono::Utc::now()),
        reviewers: Vec::new(),
        merge_commit_sha: optional_text(value, "merge_commit_sha"),
    })
}

fn map_change(value: &Value) -> ForgeFileDiff {
    let diff = text(value, "diff");
    let (additions, deletions) = count_diff(&diff);
    let status = if value["new_file"].as_bool().unwrap_or(false) {
        ForgeFileStatus::Added
    } else if value["deleted_file"].as_bool().unwrap_or(false) {
        ForgeFileStatus::Deleted
    } else if value["renamed_file"].as_bool().unwrap_or(false) {
        ForgeFileStatus::Renamed
    } else {
        ForgeFileStatus::Modified
    };
    let path = if value["new_path"]
        .as_str()
        .is_some_and(|path| !path.is_empty())
    {
        text(value, "new_path")
    } else {
        text(value, "old_path")
    };
    ForgeFileDiff {
        path,
        previous_path: optional_text(value, "old_path"),
        status,
        additions,
        deletions,
        binary: diff.is_empty() && !value["new_file"].as_bool().unwrap_or(false),
        truncated: change_truncated(value),
        patch: (!diff.is_empty()).then_some(diff),
    }
}

/// Counts a change's added and removed lines by the first character inside a
/// hunk.
///
/// Only the file header before the first `@@` is a marker; inside a hunk a
/// content line that itself begins `+++` or `---` is still an added or removed
/// line.
fn count_diff(diff: &str) -> (u32, u32) {
    let mut additions = 0;
    let mut deletions = 0;
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if line.starts_with('+') {
            additions += 1;
        } else if line.starts_with('-') {
            deletions += 1;
        }
    }
    (additions, deletions)
}

/// Whether GitLab cut this change's patch short (`collapsed`) or omitted it
/// because it was too large (`too_large`).
fn change_truncated(value: &Value) -> bool {
    value["collapsed"].as_bool().unwrap_or(false) || value["too_large"].as_bool().unwrap_or(false)
}

fn map_note_comment(value: &Value) -> Option<ForgeComment> {
    if value["system"].as_bool().unwrap_or(false) {
        return None;
    }
    Some(ForgeComment {
        comment_id: value["id"].as_u64()?.to_string(),
        author: text(&value["author"], "username"),
        body: text(value, "body"),
        created_at: parse_datetime(&value["created_at"], "created_at").ok()?,
        updated_at: parse_optional_datetime(value.get("updated_at"), "updated_at")
            .ok()
            .flatten(),
        web_url: None,
    })
}

fn map_discussion_thread(value: &Value, notes: &[Value]) -> Option<ForgeReviewThread> {
    let first = notes.first()?;
    let position = &first["position"];
    let path = if position["new_path"]
        .as_str()
        .is_some_and(|path| !path.is_empty())
    {
        text(position, "new_path")
    } else {
        text(position, "old_path")
    };
    let side = if position["new_line"].as_u64().is_some() {
        ForgeDiffSide::Right
    } else if position["old_line"].as_u64().is_some() {
        ForgeDiffSide::Left
    } else {
        ForgeDiffSide::Unknown
    };
    let line = position["new_line"]
        .as_u64()
        .or_else(|| position["old_line"].as_u64())
        .map(|line| line as u32);
    let resolved = notes
        .iter()
        .any(|note| note["resolved"].as_bool().unwrap_or(false));
    let comments = notes
        .iter()
        .filter_map(|note| {
            map_note_comment(note).map(|comment| ForgeReviewComment {
                comment_id: comment.comment_id,
                thread_id: optional_text(value, "id"),
                author: comment.author,
                body: comment.body,
                created_at: comment.created_at,
                updated_at: comment.updated_at,
                web_url: comment.web_url,
                path: path.clone(),
                line,
                side,
                in_reply_to: None,
            })
        })
        .collect();
    Some(ForgeReviewThread {
        thread_id: optional_text(value, "id")?,
        path,
        line,
        side,
        resolved,
        outdated: false,
        comments,
    })
}

fn map_job(value: &Value) -> ForgeCheck {
    let started_at = parse_optional_datetime(value.get("started_at"), "started_at")
        .ok()
        .flatten();
    let completed_at = parse_optional_datetime(value.get("finished_at"), "finished_at")
        .ok()
        .flatten();
    ForgeCheck {
        name: text(value, "name"),
        status: gitlab_check_status(value["status"].as_str().unwrap_or_default()),
        details_url: optional_text(value, "web_url"),
        // A job that may not fail is what GitLab gates the merge on.
        required: value["allow_failure"].as_bool().map(|allowed| !allowed),
        description: optional_text(value, "failure_reason"),
        duration_ms: value["duration"]
            .as_f64()
            .map(|seconds| (seconds * 1000.0) as u64)
            .or_else(|| duration_ms(started_at.as_ref(), completed_at.as_ref())),
        started_at,
        completed_at,
    }
}

fn map_pipeline(value: &Value) -> ForgeCheck {
    ForgeCheck {
        name: "pipeline".into(),
        status: gitlab_check_status(value["status"].as_str().unwrap_or_default()),
        details_url: optional_text(value, "web_url"),
        required: None,
        description: optional_text(value, "ref"),
        started_at: None,
        completed_at: None,
        duration_ms: value["duration"]
            .as_f64()
            .map(|seconds| (seconds * 1000.0) as u64),
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
