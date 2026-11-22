//! Azure DevOps Services client (`dev.azure.com` and legacy
//! `*.visualstudio.com`, REST 7.1).
//!
//! Authentication is a personal access token sent as HTTP Basic with an empty
//! username, as Azure documents. Azure DevOps Server is not supported: the
//! client only builds the Services URLs this crate detects.
//!
//! A `ForgeRepoRef` keeps the organization in `base_url`
//! (`https://dev.azure.com/acme`) and the project in `owner`, so every request
//! is `{base}/{project}/_apis/git/repositories/{repo}`.
//!
//! Diffs: Azure returns changed-file entries per iteration, not a patch. The
//! client reads the latest iteration's changes, downloads the old and new
//! content of each text file from the base and source commits, and builds a
//! unified diff locally with [`crate::diff::unified_diff`]. Pull request
//! additions/deletions come from the same construction, capped so a huge PR
//! cannot make the detail request unbounded.
//!
//! Checks combine the PR's commit statuses with its policy evaluations
//! (blocking policies are `required`). Re-running builds needs the Builds API
//! and a broader token; this client reports it as unsupported with the reason.

use async_trait::async_trait;
use chrono::Utc;
use h4rn_protocol::{
    ForgeCapabilities, ForgeCheck, ForgeChecks, ForgeChecksState, ForgeComment, ForgeCommentPage,
    ForgeCommit, ForgeCommits, ForgeDiffPage, ForgeDiffSide, ForgeFileDiff, ForgeFileStatus,
    ForgeKind, ForgeLabel, ForgeMergeable, ForgeNewPr, ForgePrFilter, ForgePrState, ForgePrSummary,
    ForgePullRequest, ForgeRepoRef, ForgeReview, ForgeReviewComment, ForgeReviewState,
    ForgeReviewThread,
};
use serde_json::{Value, json};

use crate::common::{
    azure_check_status, azure_policy_status, azure_thread_resolved, duration_ms, finish_checks,
    optional_text, parse_datetime, parse_optional_datetime, split_message, text, validate_repo,
};
use crate::error::{ForgeError, Result};
use crate::http::{ForgeHttp, basic_auth_header};
use crate::remote::validate_base_url_for;
use crate::{Forge, ForgeViewer, PrPage};

/// API version every request names.
const API_VERSION: &str = "7.1";
/// Changed files a PR detail will download to count lines.
const DETAIL_FILE_CAP: usize = 100;
/// Changed files a diff page will build.
const DIFF_FILE_CAP: usize = 500;

/// Azure DevOps Services REST client.
pub struct AzureDevOpsClient {
    http: ForgeHttp,
    /// Organization base, e.g. `https://dev.azure.com/acme`.
    api: String,
    base_url: String,
}

impl AzureDevOpsClient {
    /// Builds a client for one account.
    pub fn new(base_url: &str, token: &str, requests_per_minute: u32) -> Result<Self> {
        let base_url = validate_base_url_for(ForgeKind::AzureDevOps, base_url)?;
        let auth = if token.trim().is_empty() {
            None
        } else {
            basic_auth_header("", token.trim())
        };
        let http = ForgeHttp::new(
            ForgeKind::AzureDevOps,
            &base_url,
            auth,
            "application/json",
            requests_per_minute,
        )?;
        Ok(Self {
            api: base_url.clone(),
            http,
            base_url,
        })
    }

    fn repo_api(&self, repo: &ForgeRepoRef) -> String {
        format!(
            "{}/{}/_apis/git/repositories/{}",
            self.api,
            urlencode(&repo.owner),
            urlencode(&repo.repo)
        )
    }

    fn pr_api(&self, repo: &ForgeRepoRef, number: u32) -> String {
        format!("{}/pullrequests/{number}", self.repo_api(repo))
    }

    async fn fetch_pr(&self, repo: &ForgeRepoRef, number: u32) -> Result<Value> {
        self.http
            .get_json(&versioned(&self.pr_api(repo, number)))
            .await
    }

    async fn identity_id(&self) -> Result<String> {
        let value = self
            .http
            .get_json(&versioned(&format!("{}/_apis/connectionData", self.api)))
            .await?;
        optional_text(&value["authenticatedUser"], "id")
            .ok_or_else(|| ForgeError::Parse("Azure DevOps returned no authenticated user".into()))
    }

    async fn project_id(&self, repo: &ForgeRepoRef) -> Result<String> {
        let value = self
            .http
            .get_json(&versioned(&format!(
                "{}/_apis/projects/{}",
                self.api,
                urlencode(&repo.owner)
            )))
            .await?;
        optional_text(&value, "id")
            .ok_or_else(|| ForgeError::Parse("Azure DevOps returned no project id".into()))
    }

    /// Reads every page of a `{count, value}` collection with `$top`/`$skip`.
    async fn get_all(&self, url: &str, page_size: u32, max_pages: usize) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        let mut skip = 0u64;
        for _ in 0..max_pages {
            let current = with_query(
                url,
                &[
                    ("$top", page_size.to_string()),
                    ("$skip", skip.to_string()),
                    ("api-version", API_VERSION.to_owned()),
                ],
            );
            let value = self.http.get_json(&current).await?;
            let Some(array) = value["value"].as_array().or_else(|| value.as_array()) else {
                break;
            };
            let count = array.len();
            values.extend(array.iter().cloned());
            if count < page_size as usize {
                break;
            }
            skip += count as u64;
        }
        Ok(values)
    }

    /// The latest iteration's change entries, with paging handled.
    async fn change_entries(&self, repo: &ForgeRepoRef, number: u32) -> Result<Vec<Value>> {
        let iterations = self
            .http
            .get_json(&versioned(&format!(
                "{}/iterations",
                self.pr_api(repo, number)
            )))
            .await?;
        let iteration = iterations["value"]
            .as_array()
            .and_then(|values| {
                values
                    .iter()
                    .filter_map(|value| value["id"].as_u64().map(|id| (id, value)))
                    .max_by_key(|(id, _)| *id)
                    .map(|(_, value)| value)
            })
            .ok_or_else(|| {
                ForgeError::Parse("the pull request has no iterations to diff".into())
            })?;
        let id = iteration["id"].as_u64().unwrap_or(1);
        let url = versioned(&format!(
            "{}/iterations/{id}/changes?$top=200&$skip=0",
            self.pr_api(repo, number)
        ));
        let mut entries = Vec::new();
        let mut next = Some(url);
        let mut pages = 0;
        while let Some(current) = next {
            pages += 1;
            if pages > 10 {
                break;
            }
            let value = self.http.get_json(&current).await?;
            if let Some(array) = value["changeEntries"].as_array() {
                entries.extend(array.iter().cloned());
            }
            next = value["nextSkip"].as_u64().map(|skip| {
                let top = value["nextTop"].as_u64().unwrap_or(200);
                versioned(&format!(
                    "{}/iterations/{id}/changes?$top={top}&$skip={skip}",
                    self.pr_api(repo, number)
                ))
            });
        }
        Ok(entries)
    }

    /// File contents at one commit, or `None` when the path does not exist
    /// there (an added or deleted file's other side).
    async fn item_content(
        &self,
        repo: &ForgeRepoRef,
        path: &str,
        commit: &str,
    ) -> Result<Option<String>> {
        let url = format!(
            "{}/items?path={}&includeContent=true&versionDescriptor.version={}&versionDescriptor.versionType=commit&api-version={API_VERSION}",
            self.repo_api(repo),
            urlencode(&format!("/{path}")),
            urlencode(commit)
        );
        match self.http.get_json(&url).await {
            Ok(value) => Ok(optional_text(&value, "content")),
            Err(ForgeError::NotFound(_)) | Err(ForgeError::Http { status: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn collect_file_diffs(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        cap: usize,
    ) -> Result<Vec<ForgeFileDiff>> {
        let pr = self.fetch_pr(repo, number).await?;
        let base = pr["lastMergeTargetCommit"]["commitId"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| ForgeError::Parse("the pull request has no base commit".into()))?;
        let head = pr["lastMergeSourceCommit"]["commitId"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| ForgeError::Parse("the pull request has no source commit".into()))?;
        let entries = self.change_entries(repo, number).await?;
        let mut files = Vec::new();
        for entry in entries.iter().take(cap) {
            let item = &entry["item"];
            if item["isFolder"].as_bool().unwrap_or(false) {
                continue;
            }
            let Some(path) = optional_text(item, "path") else {
                continue;
            };
            let path = path.trim_start_matches('/').to_owned();
            let change_type = entry["changeType"].as_str().unwrap_or_default();
            let status = change_status(change_type);
            let original = optional_text(entry, "originalPath")
                .or_else(|| optional_text(item, "originalPath"))
                .map(|path| path.trim_start_matches('/').to_owned())
                .unwrap_or_else(|| path.clone());
            let old_content = if status == ForgeFileStatus::Added {
                None
            } else {
                self.item_content(repo, &original, &base).await?
            };
            let new_content = if status == ForgeFileStatus::Deleted {
                None
            } else {
                self.item_content(repo, &path, &head).await?
            };
            let old_text = old_content.unwrap_or_default();
            let new_text = new_content.unwrap_or_default();
            let binary = old_text.as_bytes().contains(&0) || new_text.as_bytes().contains(&0);
            let patch = if binary {
                None
            } else {
                crate::diff::unified_diff(&original, &path, &old_text, &new_text)
            };
            let (additions, deletions) = patch.as_deref().map(count_patch).unwrap_or((0, 0));
            files.push(ForgeFileDiff {
                path: path.clone(),
                previous_path: (status == ForgeFileStatus::Renamed && original != path)
                    .then(|| original.clone()),
                status,
                additions,
                deletions,
                patch,
                binary,
                truncated: false,
            });
        }
        Ok(files)
    }

    async fn commit_count(&self, repo: &ForgeRepoRef, number: u32) -> Option<u32> {
        let url = versioned(&format!("{}/commits?$top=1", self.pr_api(repo, number)));
        let value = self.http.get_json(&url).await.ok()?;
        value["count"]
            .as_u64()
            .map(|count| count as u32)
            .or_else(|| Some(value["value"].as_array()?.len() as u32))
    }
}

#[async_trait]
impl Forge for AzureDevOpsClient {
    fn kind(&self) -> ForgeKind {
        ForgeKind::AzureDevOps
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn capabilities(&self) -> ForgeCapabilities {
        crate::capabilities(ForgeKind::AzureDevOps)
    }

    async fn viewer(&self) -> Result<ForgeViewer> {
        let value = self
            .http
            .get_json(&versioned(&format!("{}/_apis/connectionData", self.api)))
            .await?;
        let user = &value["authenticatedUser"];
        let login = optional_text(user, "providerDisplayName")
            .or_else(|| optional_text(user, "customDisplayName"))
            .unwrap_or_default();
        if login.is_empty() {
            return Err(ForgeError::Parse(
                "Azure DevOps returned no display name for the token".into(),
            ));
        }
        Ok(ForgeViewer {
            login: login.clone(),
            display_name: Some(login),
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
        let skip = (page - 1) * 50;
        let mut query: Vec<(&str, String)> = vec![
            ("searchCriteria.status", "active".to_owned()),
            ("$top", "50".to_owned()),
            ("$skip", skip.to_string()),
        ];
        match filter {
            ForgePrFilter::Open => {}
            ForgePrFilter::Mine => {
                query.push(("searchCriteria.creatorId", self.identity_id().await?));
            }
            ForgePrFilter::ReviewRequested => {
                query.push(("searchCriteria.reviewerId", self.identity_id().await?));
            }
            ForgePrFilter::ByBranch { branch } => {
                query.push((
                    "searchCriteria.sourceRefName",
                    format!("refs/heads/{branch}"),
                ));
            }
        }
        let url = with_version(&format!("{}/pullrequests", self.repo_api(repo)), &query);
        let value = self.http.get_json(&url).await?;
        let items = value["value"]
            .as_array()
            .ok_or_else(|| ForgeError::Parse("Azure DevOps returned no PR list".into()))?;
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

    async fn get_pr(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgePullRequest> {
        validate_repo(repo)?;
        let value = self.fetch_pr(repo, number).await?;
        let mut pr = map_pr_detail(repo, &value)?;
        if let Ok(files) = self.collect_file_diffs(repo, number, DETAIL_FILE_CAP).await {
            pr.additions = files.iter().map(|file| file.additions).sum();
            pr.deletions = files.iter().map(|file| file.deletions).sum();
            pr.changed_files = files.len() as u32;
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
        let files = self.collect_file_diffs(repo, number, DIFF_FILE_CAP).await?;
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

    async fn list_comments(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeCommentPage> {
        validate_repo(repo)?;
        let value = self
            .http
            .get_json(&versioned(&format!(
                "{}/threads",
                self.pr_api(repo, number)
            )))
            .await?;
        let pr = self.fetch_pr(repo, number).await.unwrap_or(Value::Null);
        let pr_url = links_html(&pr);
        let threads = value["value"].as_array().cloned().unwrap_or_default();
        let mut comments = Vec::new();
        let mut review_threads = Vec::new();
        for thread in &threads {
            let context = &thread["threadContext"];
            let path = optional_text(context, "filePath")
                .unwrap_or_default()
                .trim_start_matches('/')
                .to_owned();
            let thread_id = thread["id"]
                .as_u64()
                .map(|id| id.to_string())
                .unwrap_or_default();
            if thread_id.is_empty() {
                continue;
            }
            let mapped = map_thread_comments(thread, &thread_id, &path, pr_url.as_deref());
            if path.is_empty() {
                comments.extend(mapped.into_iter().map(|comment| ForgeComment {
                    comment_id: comment.comment_id,
                    author: comment.author,
                    body: comment.body,
                    created_at: comment.created_at,
                    updated_at: comment.updated_at,
                    web_url: comment.web_url,
                }));
                continue;
            }
            let (line, side) = match (
                context["rightFileStart"]["line"].as_u64(),
                context["leftFileStart"]["line"].as_u64(),
            ) {
                (Some(line), _) => (Some(line as u32), ForgeDiffSide::Right),
                (None, Some(line)) => (Some(line as u32), ForgeDiffSide::Left),
                _ => (None, ForgeDiffSide::Unknown),
            };
            let mapped = mapped
                .into_iter()
                .map(|mut comment| {
                    comment.line = line;
                    comment.side = side;
                    comment
                })
                .collect();
            review_threads.push(ForgeReviewThread {
                thread_id: thread_id.clone(),
                path,
                line,
                side,
                resolved: azure_thread_resolved(thread["status"].as_str().unwrap_or_default()),
                outdated: false,
                comments: mapped,
            });
        }
        comments.sort_by_key(|comment| comment.created_at);
        review_threads.sort_by_key(|thread| {
            thread
                .comments
                .first()
                .map(|comment| comment.created_at)
                .unwrap_or_else(Utc::now)
        });
        let reviews = map_reviewers(&pr);
        Ok(ForgeCommentPage {
            repo: repo.clone(),
            number,
            has_more: threads.len() >= 500,
            comments,
            threads: review_threads,
            reviews,
        })
    }

    async fn list_checks(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeChecks> {
        validate_repo(repo)?;
        let pr = self.fetch_pr(repo, number).await?;
        let sha = pr["lastMergeSourceCommit"]["commitId"]
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| ForgeError::Parse("the pull request has no source commit".into()))?;
        let mut checks = Vec::new();
        if let Ok(value) = self
            .http
            .get_json(&versioned(&format!(
                "{}/statuses",
                self.pr_api(repo, number)
            )))
            .await
        {
            for status in value["value"].as_array().into_iter().flatten() {
                checks.push(map_status(status));
            }
        }
        // Blocking policies are what the branch gates on. Statuses often carry
        // the same context, so a matching row only gains `required`.
        if let Ok(project_id) = self.project_id(repo).await {
            let artifact = urlencode(&format!(
                "vstfs:///CodeReview/CodeReviewId/{project_id}/{number}"
            ));
            let url = format!(
                "{}/{}/_apis/policy/evaluations?artifactId={artifact}&api-version={API_VERSION}",
                self.api,
                urlencode(&repo.owner)
            );
            if let Ok(value) = self.http.get_json(&url).await {
                for evaluation in value["value"].as_array().into_iter().flatten() {
                    let policy = &evaluation["configuredPolicy"];
                    let name = optional_text(&policy["settings"], "displayName")
                        .or_else(|| optional_text(&policy["type"], "displayName"))
                        .unwrap_or_else(|| "policy".into());
                    let required = policy["isBlocking"].as_bool();
                    if let Some(existing) = checks.iter_mut().find(|check| check.name == name) {
                        existing.required = required;
                        continue;
                    }
                    checks.push(ForgeCheck {
                        name,
                        status: azure_policy_status(
                            evaluation["status"].as_str().unwrap_or_default(),
                        ),
                        details_url: None,
                        description: optional_text(&policy["type"], "displayName"),
                        started_at: None,
                        completed_at: None,
                        duration_ms: None,
                        required,
                    });
                }
            }
        }
        Ok(finish_checks(repo, number, sha, checks))
    }

    async fn list_commits(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeCommits> {
        validate_repo(repo)?;
        let url = format!("{}/commits", self.pr_api(repo, number));
        let values = self.get_all(&url, 100, 5).await?;
        let mut commits: Vec<ForgeCommit> = values
            .iter()
            .filter_map(|value| map_commit(repo, value))
            .collect();
        // Azure does not promise an order; the Commits tab reads oldest first.
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
        let Some(thread_id) = thread_id else {
            return Err(ForgeError::Unsupported(
                "Azure DevOps replies need the thread id".into(),
            ));
        };
        let parent = comment_id.parse::<u64>().unwrap_or(0);
        let url = versioned(&format!(
            "{}/threads/{thread_id}/comments",
            self.pr_api(repo, number)
        ));
        let value = self
            .http
            .post_json(
                &url,
                &json!({
                    "content": body,
                    "parentCommentId": parent,
                    "commentType": "text",
                }),
            )
            .await?;
        let mut comment = map_review_comment(&value, thread_id, None)
            .ok_or_else(|| ForgeError::Parse("Azure DevOps returned no comment".into()))?;
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
        let url = versioned(&format!("{}/threads", self.pr_api(repo, number)));
        let value = self
            .http
            .post_json(
                &url,
                &json!({
                    "comments": [{
                        "parentCommentId": 0,
                        "content": body,
                        "commentType": "text"
                    }],
                    "status": "active",
                }),
            )
            .await?;
        let thread_id = value["id"]
            .as_u64()
            .map(|id| id.to_string())
            .unwrap_or_default();
        let pr = self.fetch_pr(repo, number).await.unwrap_or(Value::Null);
        let mapped = map_thread_comments(&value, &thread_id, "", links_html(&pr).as_deref());
        mapped
            .into_iter()
            .next()
            .map(|comment| ForgeComment {
                comment_id: comment.comment_id,
                author: comment.author,
                body: comment.body,
                created_at: comment.created_at,
                updated_at: comment.updated_at,
                web_url: comment.web_url,
            })
            .ok_or_else(|| ForgeError::Parse("Azure DevOps returned no comment".into()))
    }

    async fn open_pr(&self, repo: &ForgeRepoRef, request: &ForgeNewPr) -> Result<ForgePullRequest> {
        validate_repo(repo)?;
        if request.head.trim().is_empty() || request.base.trim().is_empty() {
            return Err(ForgeError::InvalidRequest(
                "opening a pull request needs a source and a target branch".into(),
            ));
        }
        let url = versioned(&format!("{}/pullrequests", self.repo_api(repo)));
        let value = self
            .http
            .post_json(
                &url,
                &json!({
                    "sourceRefName": format!("refs/heads/{}", request.head),
                    "targetRefName": format!("refs/heads/{}", request.base),
                    "title": request.title,
                    "description": request.body,
                }),
            )
            .await?;
        map_pr_detail(repo, &value)
    }
}

fn versioned(url: &str) -> String {
    with_query(url, &[("api-version", API_VERSION.to_owned())])
}

fn with_query(url: &str, query: &[(&str, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in query {
        serializer.append_pair(key, value);
    }
    let encoded = serializer.finish();
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}{encoded}")
}

fn with_version(url: &str, query: &[(&str, String)]) -> String {
    let mut pairs = query.to_vec();
    pairs.push(("api-version", API_VERSION.to_owned()));
    with_query(url, &pairs)
}

fn urlencode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
}

fn ref_branch(value: &str) -> String {
    value.trim_start_matches("refs/heads/").to_owned()
}

fn links_html(value: &Value) -> Option<String> {
    optional_text(&value["_links"]["web"], "href")
}

fn pr_state(value: &Value) -> ForgePrState {
    match value["status"].as_str().unwrap_or_default() {
        "completed" => ForgePrState::Merged,
        "abandoned" => ForgePrState::Closed,
        _ if value["isDraft"].as_bool().unwrap_or(false) => ForgePrState::Draft,
        _ => ForgePrState::Open,
    }
}

fn map_pr_summary(repo: &ForgeRepoRef, value: &Value) -> Option<ForgePrSummary> {
    let number = value["pullRequestId"].as_u64()? as u32;
    let updated_at = parse_optional_datetime(value.get("closedDate"), "closedDate")
        .ok()
        .flatten()
        .or_else(|| {
            parse_optional_datetime(
                value["lastMergeCommit"]["author"].get("date"),
                "lastMergeCommit.author.date",
            )
            .ok()
            .flatten()
        })
        .or_else(|| {
            parse_optional_datetime(value.get("creationDate"), "creationDate")
                .ok()
                .flatten()
        })?;
    Some(ForgePrSummary {
        repo: repo.clone(),
        number,
        title: text(value, "title"),
        state: pr_state(value),
        author: optional_text(&value["createdBy"], "displayName")
            .or_else(|| optional_text(&value["createdBy"], "uniqueName"))
            .unwrap_or_default(),
        head_branch: ref_branch(&text(value, "sourceRefName")),
        base_branch: ref_branch(&text(value, "targetRefName")),
        head_sha: optional_text(&value["lastMergeSourceCommit"], "commitId"),
        updated_at,
        web_url: links_html(value).unwrap_or_default(),
        checks: ForgeChecksState::Unknown,
        review: ForgeReviewState::Pending,
        draft: value["isDraft"].as_bool().unwrap_or(false),
    })
}

fn map_pr_detail(repo: &ForgeRepoRef, value: &Value) -> Result<ForgePullRequest> {
    let summary = map_pr_summary(repo, value).ok_or_else(|| {
        ForgeError::Parse("Azure DevOps returned an incomplete pull request".into())
    })?;
    let labels = value["labels"]
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .filter_map(|label| {
                    optional_text(label, "name").map(|name| ForgeLabel { name, color: None })
                })
                .collect()
        })
        .unwrap_or_default();
    let mergeable = match value["mergeStatus"].as_str().unwrap_or_default() {
        "conflicts" => ForgeMergeable::Conflicting,
        "succeeded" => ForgeMergeable::Mergeable,
        _ => ForgeMergeable::Unknown,
    };
    let merge_commit_sha = (value["status"].as_str() == Some("completed"))
        .then(|| optional_text(&value["lastMergeCommit"], "commitId"))
        .flatten();
    Ok(ForgePullRequest {
        summary,
        body: text(value, "description"),
        labels,
        mergeable,
        base_sha: optional_text(&value["lastMergeTargetCommit"], "commitId"),
        additions: 0,
        deletions: 0,
        changed_files: 0,
        commits: 0,
        created_at: parse_datetime(&value["creationDate"], "creationDate")
            .unwrap_or_else(|_| Utc::now()),
        reviewers: Vec::new(),
        merge_commit_sha,
    })
}

fn map_reviewers(pr: &Value) -> Vec<ForgeReview> {
    let mut reviews = Vec::new();
    for reviewer in pr["reviewers"].as_array().into_iter().flatten() {
        let login = optional_text(reviewer, "displayName")
            .or_else(|| optional_text(reviewer, "uniqueName"))
            .unwrap_or_default();
        if login.is_empty()
            || reviews
                .iter()
                .any(|review: &ForgeReview| review.author == login)
        {
            continue;
        }
        let state = match reviewer["vote"].as_i64().unwrap_or(0) {
            vote if vote >= 5 => ForgeReviewState::Approved,
            -10 => ForgeReviewState::ChangesRequested,
            // -5 (waiting for the author), 0 (no vote yet) and anything else
            // are all "not decided" rather than a comment.
            _ => ForgeReviewState::Pending,
        };
        reviews.push(ForgeReview {
            author: login,
            state,
            submitted_at: None,
            body: None,
        });
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

/// A review comment from a thread. `pr_url` is the PR's browser URL when one
/// is known; the discussion deep link is appended for the comment.
fn map_review_comment(
    value: &Value,
    thread_id: &str,
    path: Option<&str>,
) -> Option<ForgeReviewComment> {
    if value["isDeleted"].as_bool().unwrap_or(false) {
        return None;
    }
    if value["commentType"].as_str() == Some("system") {
        return None;
    }
    let author = optional_text(&value["author"], "displayName")
        .or_else(|| optional_text(&value["author"], "uniqueName"))
        .unwrap_or_default();
    Some(ForgeReviewComment {
        comment_id: value["id"].as_u64()?.to_string(),
        thread_id: Some(thread_id.to_owned()),
        author,
        body: text(value, "content"),
        created_at: parse_datetime(&value["publishedDate"], "publishedDate").ok()?,
        updated_at: parse_optional_datetime(value.get("lastUpdatedDate"), "lastUpdatedDate")
            .ok()
            .flatten(),
        web_url: None,
        path: path.unwrap_or_default().to_owned(),
        line: None,
        side: ForgeDiffSide::Unknown,
        in_reply_to: value["parentCommentId"]
            .as_u64()
            .filter(|id| *id > 0)
            .map(|id| id.to_string()),
    })
}

fn map_thread_comments(
    thread: &Value,
    thread_id: &str,
    path: &str,
    pr_url: Option<&str>,
) -> Vec<ForgeReviewComment> {
    thread["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|comment| {
            let mut mapped = map_review_comment(comment, thread_id, Some(path))?;
            if let Some(url) = pr_url {
                mapped.web_url = Some(format!("{url}?discussionId={thread_id}"));
            }
            Some(mapped)
        })
        .collect()
}

fn change_status(change_type: &str) -> ForgeFileStatus {
    if change_type.contains("add") {
        ForgeFileStatus::Added
    } else if change_type.contains("delete") {
        ForgeFileStatus::Deleted
    } else if change_type.contains("rename") {
        ForgeFileStatus::Renamed
    } else if change_type.contains("edit") {
        ForgeFileStatus::Modified
    } else {
        ForgeFileStatus::Unknown
    }
}

fn count_patch(patch: &str) -> (u32, u32) {
    let mut additions = 0;
    let mut deletions = 0;
    let mut in_hunk = false;
    for line in patch.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            additions += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions += 1;
        }
    }
    (additions, deletions)
}

/// Azure commit dates are RFC 3339; the Commits tab reads oldest first.
fn map_commit(repo: &ForgeRepoRef, value: &Value) -> Option<ForgeCommit> {
    let sha = optional_text(value, "commitId")?;
    let (title, body) = split_message(&text(value, "comment"));
    let web_url = format!("{}/commit/{sha}", repo.web_url());
    Some(ForgeCommit {
        sha,
        title,
        body,
        author: optional_text(&value["author"], "name").unwrap_or_default(),
        author_login: optional_text(&value["author"], "email"),
        authored_at: parse_optional_datetime(value["author"].get("date"), "author.date")
            .ok()
            .flatten(),
        web_url: Some(web_url),
    })
}

fn map_status(value: &Value) -> ForgeCheck {
    let created_at = parse_optional_datetime(value.get("creationDate"), "creationDate")
        .ok()
        .flatten();
    let updated_at = parse_optional_datetime(value.get("updatedDate"), "updatedDate")
        .ok()
        .flatten();
    ForgeCheck {
        name: optional_text(&value["context"], "name")
            .or_else(|| optional_text(&value["context"], "genre"))
            .unwrap_or_default(),
        status: azure_check_status(value["state"].as_str().unwrap_or_default()),
        details_url: optional_text(value, "targetUrl"),
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

    fn repo() -> ForgeRepoRef {
        ForgeRepoRef {
            forge: ForgeKind::AzureDevOps,
            base_url: "https://dev.azure.com/acme".into(),
            owner: "platform".into(),
            repo: "widget".into(),
        }
    }

    fn summary(status: &str, draft: bool) -> ForgePrSummary {
        map_pr_summary(
            &repo(),
            &json!({
                "pullRequestId": 7,
                "title": "Fix",
                "status": status,
                "isDraft": draft,
                "createdBy": {"displayName": "Alice"},
                "creationDate": "2024-05-01T09:00:00Z",
                "sourceRefName": "refs/heads/fix",
                "targetRefName": "refs/heads/main",
                "lastMergeSourceCommit": {"commitId": "abc"}
            }),
        )
        .unwrap()
    }

    #[test]
    fn azure_states_cover_active_completed_and_abandoned() {
        assert_eq!(summary("active", false).state, ForgePrState::Open);
        assert_eq!(summary("active", true).state, ForgePrState::Draft);
        assert_eq!(summary("completed", false).state, ForgePrState::Merged);
        assert_eq!(summary("abandoned", false).state, ForgePrState::Closed);
    }

    #[test]
    fn azure_change_types_cover_add_delete_rename_and_edit() {
        assert_eq!(change_status("add"), ForgeFileStatus::Added);
        assert_eq!(change_status("delete"), ForgeFileStatus::Deleted);
        assert_eq!(change_status("rename, edit"), ForgeFileStatus::Renamed);
        assert_eq!(change_status("edit"), ForgeFileStatus::Modified);
        assert_eq!(change_status("something"), ForgeFileStatus::Unknown);
    }

    #[test]
    fn azure_letters_resolved_thread_statuses() {
        assert!(azure_thread_resolved("fixed"));
        assert!(azure_thread_resolved("closed"));
        assert!(azure_thread_resolved("wontFix"));
        assert!(azure_thread_resolved("byDesign"));
        assert!(!azure_thread_resolved("active"));
        assert!(!azure_thread_resolved("pending"));
    }
}
