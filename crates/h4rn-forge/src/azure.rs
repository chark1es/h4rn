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
//! client reads the latest iteration's changes, pages them, downloads the old
//! and new content of the files on the requested page (at most six requests at
//! once, from the iteration's `commonRefCommit` and `sourceRefCommit`), and
//! builds a unified diff locally with [`crate::diff::unified_diff`]. A page is
//! capped at eight files: the worst case is two content requests per file plus
//! the iteration and change-entry reads, 18 requests, which fits the UI's ten
//! second deadline inside the default 120-requests-per-minute account budget.
//! Files whose change carries no content difference (a pure rename, branch or
//! merge) download nothing. The PR detail only counts changed files from the
//! same metadata; it never downloads file contents.
//!
//! Checks combine the PR's commit statuses with its policy evaluations
//! (`api-version=7.1-preview.1`). Statuses carry an optional `iterationId`; the
//! client keeps PR-level statuses and those belonging to the current iteration
//! only, so a failure from an earlier push cannot outlive the code it tested.
//! Each policy evaluation is its own row (keyed by its evaluation and
//! configuration ids, so two policies that share a display name stay
//! distinct), blocking policies are `required`, and a policy outcome wins only
//! over a *status* with the same name. The project id comes from the PR's
//! `repository.project.id`. Re-running builds needs the Builds API and a
//! broader token; this client reports it as unsupported with the reason.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::Utc;
use futures::StreamExt;
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
/// Policy evaluations only exist on the preview version.
const POLICY_API_VERSION: &str = "7.1-preview.1";
/// File contents fetched at once for one diff page.
const CONTENT_CONCURRENCY: usize = 6;
/// Files in one diff page. Eight files need at most 18 requests per page, which
/// fits the UI's 10 s deadline at the default 120 requests/minute budget.
const DIFF_PAGE_FILES: u32 = 8;
/// Commits requested per page.
const COMMIT_PAGE_SIZE: u32 = 100;

/// One file content response. `exists` is false for a 404, which is how a
/// path that is absent on one side of the change is reported; an existing
/// empty file is present with empty text.
struct ItemContent {
    text: Option<String>,
    binary: bool,
    exists: bool,
}

/// One changed file, before its contents are fetched.
struct ChangedFile {
    path: String,
    original: String,
    status: ForgeFileStatus,
    /// Whether the change can alter the file's bytes. A pure rename, branch or
    /// merge keeps them, so its contents are never downloaded.
    content_changed: bool,
}

impl ItemContent {
    fn missing() -> Self {
        Self {
            text: None,
            binary: false,
            exists: false,
        }
    }

    fn present(text: Option<String>, binary: bool) -> Self {
        Self {
            text,
            binary,
            exists: true,
        }
    }

    fn text(&self) -> &str {
        self.text.as_deref().unwrap_or_default()
    }

    /// The text as a diff side: `None` when the path does not exist there, an
    /// empty string when it exists and is empty.
    fn side(&self) -> Option<&str> {
        self.exists.then(|| self.text())
    }
}

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

    /// The latest iteration, whose `commonRefCommit` and `sourceRefCommit`
    /// describe the two sides of the diff.
    async fn latest_iteration(&self, repo: &ForgeRepoRef, number: u32) -> Result<Value> {
        let value = self
            .http
            .get_json(&versioned(&format!(
                "{}/iterations",
                self.pr_api(repo, number)
            )))
            .await?;
        let iterations = value
            .as_array()
            .cloned()
            .or_else(|| value["value"].as_array().cloned())
            .unwrap_or_default();
        iterations
            .into_iter()
            .filter(|iteration| iteration["id"].as_u64().is_some())
            .max_by_key(|iteration| iteration["id"].as_u64().unwrap_or(0))
            .ok_or_else(|| ForgeError::Parse("the pull request has no iterations to diff".into()))
    }

    /// One iteration's change entries, with `nextSkip`/`nextTop` paging.
    async fn change_entries(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        iteration: &Value,
    ) -> Result<Vec<Value>> {
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
            next = value["nextSkip"]
                .as_u64()
                .filter(|skip| *skip > 0)
                .map(|skip| {
                    let top = value["nextTop"].as_u64().unwrap_or(200);
                    versioned(&format!(
                        "{}/iterations/{id}/changes?$top={top}&$skip={skip}",
                        self.pr_api(repo, number)
                    ))
                });
        }
        Ok(entries)
    }

    /// File content and metadata at one commit. A 404 means the path does not
    /// exist there (an added or deleted file's other side).
    async fn item_content(
        &self,
        repo: &ForgeRepoRef,
        path: &str,
        commit: &str,
    ) -> Result<ItemContent> {
        let url = format!(
            "{}/items?path={}&includeContent=true&includeContentMetadata=true&$format=json&versionDescriptor.version={}&versionDescriptor.versionType=commit&api-version={API_VERSION}",
            self.repo_api(repo),
            urlencode(&format!("/{}", path.trim_start_matches('/'))),
            urlencode(commit)
        );
        match self.http.get_json(&url).await {
            Ok(value) => Ok(ItemContent::present(
                optional_text(&value, "content"),
                value["contentMetadata"]["isBinary"]
                    .as_bool()
                    .or_else(|| value["isBinary"].as_bool())
                    .unwrap_or(false),
            )),
            Err(ForgeError::NotFound(_)) | Err(ForgeError::Http { status: 404, .. }) => {
                Ok(ItemContent::missing())
            }
            Err(error) => Err(error),
        }
    }

    /// Fetches the old and new contents for one page of files, at most
    /// [`CONTENT_CONCURRENCY`] requests at a time. A cache keyed by path and
    /// commit keeps a rename from downloading the same blob twice, and a file
    /// whose change keeps its bytes (a pure rename) downloads nothing.
    async fn page_file_diffs(
        &self,
        repo: &ForgeRepoRef,
        files: &[ChangedFile],
        old_ref: &str,
        new_ref: &str,
    ) -> Result<Vec<ForgeFileDiff>> {
        let mut requests: Vec<(String, String)> = Vec::new();
        for file in files {
            if !file.content_changed {
                continue;
            }
            if file.status != ForgeFileStatus::Added {
                requests.push((file.original.clone(), old_ref.to_owned()));
            }
            if file.status != ForgeFileStatus::Deleted {
                requests.push((file.path.clone(), new_ref.to_owned()));
            }
        }
        requests.sort();
        requests.dedup();

        let fetched =
            futures::stream::iter(requests.into_iter().map(|(path, commit)| async move {
                let content = self.item_content(repo, &path, &commit).await;
                ((path, commit), content)
            }))
            .buffer_unordered(CONTENT_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;

        let mut cache: HashMap<(String, String), ItemContent> = HashMap::new();
        for (key, content) in fetched {
            cache.insert(key, content?);
        }

        let mut diffs = Vec::with_capacity(files.len());
        let missing = ItemContent::missing();
        for file in files {
            let old = if file.status == ForgeFileStatus::Added || !file.content_changed {
                &missing
            } else {
                cache
                    .get(&(file.original.clone(), old_ref.to_owned()))
                    .unwrap_or(&missing)
            };
            let new = if file.status == ForgeFileStatus::Deleted || !file.content_changed {
                &missing
            } else {
                cache
                    .get(&(file.path.clone(), new_ref.to_owned()))
                    .unwrap_or(&missing)
            };
            let binary = old.binary || new.binary;
            let patch = if binary || !file.content_changed {
                None
            } else {
                crate::diff::unified_diff(&file.original, &file.path, old.side(), new.side())
            };
            let (additions, deletions) = patch.as_deref().map(count_patch).unwrap_or((0, 0));
            diffs.push(ForgeFileDiff {
                path: file.path.clone(),
                previous_path: (file.status == ForgeFileStatus::Renamed
                    && file.original != file.path)
                    .then(|| file.original.clone()),
                status: file.status,
                additions,
                deletions,
                patch,
                binary,
                truncated: false,
            });
        }
        Ok(diffs)
    }

    /// One page of commits, plus the server's continuation token.
    async fn commit_page(
        &self,
        repo: &ForgeRepoRef,
        number: u32,
        top: u32,
        token: Option<&str>,
    ) -> Result<(Vec<Value>, Option<String>)> {
        let mut query = vec![("$top", top.to_string())];
        if let Some(token) = token {
            query.push(("continuationToken", token.to_owned()));
        }
        let url = with_version(&format!("{}/commits", self.pr_api(repo, number)), &query);
        let response = self
            .http
            .request(reqwest::Method::GET, &url, None, false)
            .await?;
        let next = response
            .headers
            .get("x-ms-continuationtoken")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let value = self.http.json(response)?;
        let commits = value
            .as_array()
            .cloned()
            .or_else(|| value["value"].as_array().cloned())
            .unwrap_or_default();
        Ok((commits, next))
    }

    /// Every commit, following `continuationToken` for up to five pages.
    async fn all_commits(&self, repo: &ForgeRepoRef, number: u32) -> Result<(Vec<Value>, bool)> {
        let mut all = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..5 {
            let (commits, next) = self
                .commit_page(repo, number, COMMIT_PAGE_SIZE, token.as_deref())
                .await?;
            all.extend(commits);
            match next {
                Some(next) => token = Some(next),
                None => return Ok((all, false)),
            }
        }
        Ok((all, true))
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
        // Count the changed files from the iteration metadata only. Building
        // the patch here would download every file and blow the detail
        // request's budget; the Diff tab builds one page on demand.
        if let Ok(iteration) = self.latest_iteration(repo, number).await
            && let Ok(entries) = self.change_entries(repo, number, &iteration).await
        {
            pr.changed_files = changed_files(&entries).len() as u32;
        }
        pr.commits = self
            .all_commits(repo, number)
            .await
            .map(|(commits, _)| commits.len() as u32)
            .unwrap_or(0);
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
        // Azure needs one content request per side of every changed file, so a
        // large page blows the request budget and the UI's deadline. Eight
        // files keep the worst case within both.
        let per_page = per_page.clamp(1, DIFF_PAGE_FILES);
        let page = page.max(1);
        let iteration = self.latest_iteration(repo, number).await?;
        let old_ref = iteration["commonRefCommit"]["commitId"]
            .as_str()
            .ok_or_else(|| {
                ForgeError::Parse("the iteration has no common ref commit to diff".into())
            })?;
        let new_ref = iteration["sourceRefCommit"]["commitId"]
            .as_str()
            .ok_or_else(|| {
                ForgeError::Parse("the iteration has no source ref commit to diff".into())
            })?;
        let entries = self.change_entries(repo, number, &iteration).await?;
        let changed = changed_files(&entries);
        let total = changed.len();
        let start = ((page - 1) * per_page) as usize;
        let end = (start + per_page as usize).min(total);
        let files = if start < total {
            self.page_file_diffs(repo, &changed[start..end], old_ref, new_ref)
                .await?
        } else {
            Vec::new()
        };
        Ok(ForgeDiffPage {
            repo: repo.clone(),
            number,
            files,
            page,
            per_page,
            has_more: end < total,
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
        // The current iteration scopes the iteration-level statuses; the
        // PR-level statuses (no `iterationId`) always stay.
        let current_iteration = self
            .latest_iteration(repo, number)
            .await
            .ok()
            .and_then(|iteration| iteration["id"].as_u64());
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
                if status_is_current(status, current_iteration) {
                    checks.push(map_status(status));
                }
            }
        }
        // A policy row is never merged with another policy row: two
        // configurations can share a display name (an approved optional policy
        // and a rejected blocking one), and collapsing them would hide the
        // failure. A policy only reconciles with a *status* of the same name,
        // because the policy is the branch gate.
        let status_count = checks.len();
        if let Some(project_id) = optional_text(&pr["repository"]["project"], "id") {
            let artifact = urlencode(&format!(
                "vstfs:///CodeReview/CodeReviewId/{project_id}/{number}"
            ));
            let url = format!(
                "{}/{}/_apis/policy/evaluations?artifactId={artifact}&api-version={POLICY_API_VERSION}",
                self.api,
                urlencode(&repo.owner)
            );
            if let Ok(value) = self.http.get_json(&url).await {
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                // A status is reconciled with at most one policy; a second
                // same-named policy stays its own row.
                let mut reconciled: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                for evaluation in value["value"].as_array().into_iter().flatten() {
                    let policy = &evaluation["configuration"];
                    let evaluation_id = optional_text(evaluation, "evaluationId")
                        .or_else(|| optional_text(policy, "id"))
                        .or_else(|| policy["id"].as_u64().map(|id| id.to_string()))
                        .unwrap_or_default();
                    if !evaluation_id.is_empty() && !seen.insert(evaluation_id) {
                        continue;
                    }
                    let type_name = optional_text(&policy["type"], "displayName");
                    let name = optional_text(&policy["settings"], "displayName")
                        .or_else(|| type_name.clone())
                        .unwrap_or_else(|| "policy".into());
                    let required = policy["isBlocking"].as_bool();
                    let status =
                        azure_policy_status(evaluation["status"].as_str().unwrap_or_default());
                    if !reconciled.contains(&name)
                        && let Some(existing) = checks[..status_count]
                            .iter_mut()
                            .find(|check| check.name == name)
                    {
                        existing.required = required;
                        existing.status = status;
                        if existing.description.is_none() {
                            existing.description = type_name;
                        }
                        reconciled.insert(name);
                        continue;
                    }
                    checks.push(ForgeCheck {
                        name,
                        status,
                        details_url: None,
                        description: type_name,
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
        let (values, truncated) = self.all_commits(repo, number).await?;
        let mut commits: Vec<ForgeCommit> = values
            .iter()
            .filter_map(|value| map_commit(repo, value))
            .collect();
        // Azure does not promise an order; the Commits tab reads oldest first.
        commits.sort_by_key(|commit| commit.authored_at);
        Ok(ForgeCommits {
            repo: repo.clone(),
            number,
            truncated,
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
        let parent = comment_id.parse::<u64>().map_err(|_| {
            ForgeError::InvalidRequest(format!("invalid comment id {comment_id:?}"))
        })?;
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
    crate::http::encode_path_segment(value)
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
    // Azure does not update a PR's creation date, so a closed PR uses its
    // closed date and an active one keeps the creation date. `lastMergeCommit`
    // is the merge attempt, not a modification time, so it is not a fallback.
    let updated_at = parse_optional_datetime(value.get("closedDate"), "closedDate")
        .ok()
        .flatten()
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

fn changed_files(entries: &[Value]) -> Vec<ChangedFile> {
    let mut files = Vec::new();
    for entry in entries {
        let item = &entry["item"];
        if item["isFolder"].as_bool().unwrap_or(false) {
            continue;
        }
        if let Some(kind) = optional_text(item, "gitObjectType")
            && !kind.eq_ignore_ascii_case("blob")
        {
            continue;
        }
        let change_type = entry["changeType"].as_str().unwrap_or_default();
        let status = change_status(change_type);
        let source = optional_text(entry, "originalPath")
            .or_else(|| optional_text(entry, "sourceServerItem"));
        // A deleted entry may have no `item.path`; the source fields name it.
        let Some(path) = optional_text(item, "path").or_else(|| {
            (status == ForgeFileStatus::Deleted)
                .then(|| source.clone())
                .flatten()
        }) else {
            continue;
        };
        let path = path.trim_start_matches('/').to_owned();
        let original = source
            .map(|path| path.trim_start_matches('/').to_owned())
            .unwrap_or_else(|| path.clone());
        files.push(ChangedFile {
            path,
            original,
            status,
            content_changed: change_has_content_difference(change_type),
        });
    }
    files
}

/// The change-type tokens, lowercased. `changeType` is a comma-separated list
/// of `VersionControlChangeType` values ("rename, edit"), so tokens are matched
/// exactly: `undelete` is not a deletion, it restores a file.
fn change_tokens(change_type: &str) -> Vec<String> {
    change_type
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn change_status(change_type: &str) -> ForgeFileStatus {
    let tokens = change_tokens(change_type);
    let has = |token: &str| tokens.iter().any(|value| value == token);
    if has("add") || has("undelete") {
        ForgeFileStatus::Added
    } else if has("delete") {
        ForgeFileStatus::Deleted
    } else if has("rename") || has("sourcerename") || has("targetrename") {
        ForgeFileStatus::Renamed
    } else if has("edit") || has("encoding") || has("lock") || has("property") || has("rollback") {
        ForgeFileStatus::Modified
    } else {
        ForgeFileStatus::Unknown
    }
}

/// Whether a change can alter the file's bytes. A pure rename, branch, merge
/// or lock keeps them, so the diff needs no content request for it.
fn change_has_content_difference(change_type: &str) -> bool {
    change_tokens(change_type).iter().any(|token| {
        matches!(
            token.as_str(),
            "add" | "delete" | "edit" | "encoding" | "undelete" | "rollback" | "property"
        )
    })
}

/// A status belongs to the current checks when it is PR-level (no
/// `iterationId`) or was recorded for the current iteration.
fn status_is_current(status: &Value, current_iteration: Option<u64>) -> bool {
    match status["iterationId"].as_u64() {
        None => true,
        Some(iteration) => Some(iteration) == current_iteration,
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
        // `undelete` restores a file; matching the substring "delete" would
        // report it as a removal.
        assert_eq!(change_status("undelete"), ForgeFileStatus::Added);
        assert_eq!(change_status("sourceRename"), ForgeFileStatus::Renamed);
        assert_eq!(change_status("targetRename"), ForgeFileStatus::Renamed);
        assert_eq!(change_status("rollback, delete"), ForgeFileStatus::Deleted);
    }

    #[test]
    fn only_content_changes_need_a_content_fetch() {
        assert!(change_has_content_difference("add"));
        assert!(change_has_content_difference("edit"));
        assert!(change_has_content_difference("rename, edit"));
        assert!(change_has_content_difference("delete"));
        assert!(change_has_content_difference("undelete"));
        assert!(!change_has_content_difference("rename"));
        assert!(!change_has_content_difference("branch"));
        assert!(!change_has_content_difference("merge"));
    }

    #[test]
    fn statuses_from_older_iterations_are_dropped() {
        let current = Some(2);
        assert!(status_is_current(&json!({"state": "succeeded"}), current));
        assert!(status_is_current(&json!({"iterationId": 2}), current));
        assert!(!status_is_current(&json!({"iterationId": 1}), current));
        // With no current iteration only PR-level statuses survive.
        assert!(status_is_current(&json!({"state": "failed"}), None));
        assert!(!status_is_current(&json!({"iterationId": 1}), None));
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

    #[test]
    fn names_are_encoded_as_single_path_segments() {
        assert_eq!(urlencode("My Project"), "My%20Project");
        assert_eq!(urlencode("a/b"), "a%2Fb");
        assert_eq!(urlencode("widget"), "widget");
    }
}
