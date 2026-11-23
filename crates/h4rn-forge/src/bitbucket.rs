//! Bitbucket Cloud client (`bitbucket.org`, API v2).
//!
//! Authentication is either an Atlassian API token sent as HTTP Basic with the
//! account email, or a workspace, project, repository or OAuth access token
//! sent as a bearer token. Atlassian retired app passwords in July 2026, so
//! this client no longer offers them. The daemon stores the token the same way
//! it stores every other forge credential.
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

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use h4rn_protocol::{
    ForgeCapabilities, ForgeCheck, ForgeChecks, ForgeChecksState, ForgeComment, ForgeCommentPage,
    ForgeCommit, ForgeCommits, ForgeDiffPage, ForgeDiffSide, ForgeIssue, ForgeIssueComment,
    ForgeIssueCommentPage, ForgeIssueFilter, ForgeIssuePage, ForgeIssueUpdate, ForgeKind,
    ForgeMergeable, ForgeNewPr, ForgePrFilter, ForgePrState, ForgePrSummary, ForgePullRequest,
    ForgeRepoRef, ForgeReview, ForgeReviewComment, ForgeReviewState, ForgeReviewThread,
};
use serde_json::{Value, json};

use crate::common::{
    bitbucket_check_status, duration_ms, finish_checks, optional_text, parse_datetime,
    parse_optional_datetime, split_message, text, validate_repo,
};
use crate::error::{ForgeError, Result};
use crate::http::{ForgeHttp, basic_auth_header};
use crate::remote::{api_base_url, validate_base_url_for};
use crate::{Forge, ForgeViewer, PrPage};

/// Learned page URLs kept per client. Only cursors a response actually
/// returned are stored, so this bounds memory whatever page is requested.
const CURSOR_ENTRIES: usize = 64;

/// Bitbucket Cloud REST v2 client.
pub struct BitbucketClient {
    http: ForgeHttp,
    api: String,
    base_url: String,
    username: Option<String>,
    /// Cursors learned from a collection's `next` link, keyed by the URL of
    /// the collection and the page the link addresses. Only learned cursors
    /// are stored, and at most [`CURSOR_ENTRIES`] of them, so asking for a
    /// distant page costs one lookup rather than a map sized by the page
    /// number.
    cursors: Mutex<LearnedCursors>,
}

/// Learned page URLs: a sparse, bounded LRU.
#[derive(Default)]
struct LearnedCursors {
    /// Insertion order for eviction; the oldest entry is first.
    order: VecDeque<(String, u32)>,
    urls: HashMap<(String, u32), String>,
}

impl LearnedCursors {
    fn get(&mut self, base: &str, page: u32) -> Option<String> {
        let key = (base.to_owned(), page);
        let url = self.urls.get(&key)?.clone();
        self.touch(&key);
        Some(url)
    }

    fn insert(&mut self, base: &str, page: u32, url: String) {
        let key = (base.to_owned(), page);
        if self.urls.insert(key.clone(), url).is_none() {
            self.order.push_back(key);
        } else {
            self.touch(&key);
        }
        while self.order.len() > CURSOR_ENTRIES {
            if let Some(oldest) = self.order.pop_front() {
                self.urls.remove(&oldest);
            }
        }
    }

    fn remove(&mut self, base: &str, page: u32) {
        let key = (base.to_owned(), page);
        if self.urls.remove(&key).is_some() {
            self.order.retain(|entry| entry != &key);
        }
    }

    fn touch(&mut self, key: &(String, u32)) {
        if let Some(index) = self.order.iter().position(|entry| entry == key) {
            if let Some(entry) = self.order.remove(index) {
                self.order.push_back(entry);
            }
        }
    }
}

impl BitbucketClient {
    /// Builds a client for one account.
    ///
    /// `username` (the Atlassian account email) selects basic auth with an API
    /// token. Without it the token is sent as a bearer token (OAuth or
    /// workspace, project or repository access token).
    pub fn new(
        base_url: &str,
        token: &str,
        username: Option<&str>,
        requests_per_minute: u32,
    ) -> Result<Self> {
        let base_url = validate_base_url_for(ForgeKind::Bitbucket, base_url)?;
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
            cursors: Mutex::new(LearnedCursors::default()),
        })
    }

    /// The URL to request for one page of a collection.
    ///
    /// Bitbucket links the next page in the response body, so the cursor it
    /// returned is carried forward verbatim instead of rebuilding `page=N`:
    /// the server may use an opaque cursor, and a rebuilt page number would
    /// then repeat rows or skip them. A page whose cursor was never learned
    /// falls back to the documented `page` parameter.
    fn page_url(&self, base: &str, page: u32) -> String {
        let learned = self
            .cursors
            .lock()
            .ok()
            .and_then(|mut cursors| cursors.get(base, page));
        learned.unwrap_or_else(|| with_query(base, &[("page", page.to_string())]))
    }

    /// Remembers the `next` link a page returned as the cursor for the page
    /// after it. A missing or unusable link forgets any cursor learned for
    /// that page earlier, so a stale entry cannot outlive the list it came
    /// from.
    fn remember_cursor(&self, base: &str, page: u32, next: Option<String>) {
        let Ok(mut cursors) = self.cursors.lock() else {
            return;
        };
        let next_page = page.saturating_add(1);
        match next {
            Some(url) => cursors.insert(base, next_page, url),
            None => cursors.remove(base, next_page),
        }
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
    ///
    /// The flag is true only when more data remains: the page cap was reached
    /// while a same-origin `next` link still existed.
    async fn get_pages(&self, url: &str, max_pages: usize) -> Result<(Vec<Value>, bool)> {
        let mut values = Vec::new();
        let mut next = Some(url.to_owned());
        let mut pages = 0;
        let mut has_more = false;
        while let Some(current) = next {
            if pages == max_pages {
                has_more = true;
                break;
            }
            pages += 1;
            let value = self.http.get_json(&current).await?;
            if let Some(array) = value["values"].as_array() {
                values.extend(array.iter().cloned());
            }
            next = optional_text(&value, "next")
                .filter(|next| crate::http::same_origin(next, &self.api));
        }
        Ok((values, has_more))
    }

    async fn fetch_pr(&self, repo: &ForgeRepoRef, number: u32) -> Result<Value> {
        self.http.get_json(&self.pr_url(repo, number)).await
    }

    /// The PR's diffstat, or `None` when it cannot be read in full.
    ///
    /// The totals are only reported when every page was read: a partial sum
    /// and a partial file count would be a wrong number, not an estimate.
    async fn diffstat(&self, repo: &ForgeRepoRef, number: u32) -> Option<(u32, u32, u32)> {
        let mut next = Some(format!(
            "{}/diffstat?pagelen=100",
            self.pr_url(repo, number)
        ));
        let mut additions = 0u32;
        let mut deletions = 0u32;
        let mut files = 0u32;
        let mut pages = 0;
        while let Some(current) = next {
            if pages == 5 {
                // A sixth page exists; stop and say the size is unknown.
                return None;
            }
            pages += 1;
            let value = self.http.get_json(&current).await.ok()?;
            let values = value["values"].as_array()?;
            for value in values {
                additions += value["lines_added"].as_u64().unwrap_or(0) as u32;
                deletions += value["lines_removed"].as_u64().unwrap_or(0) as u32;
                files += 1;
            }
            next = optional_text(&value, "next")
                .filter(|next| crate::http::same_origin(next, &self.api));
        }
        Some((additions, deletions, files))
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
    ///
    /// Bitbucket does not report a credential's scopes on this (or any)
    /// response, so the account row leaves them unknown instead of inventing
    /// a list from an `x-oauth-scopes` header, which is a GitHub header.
    async fn user_json(&self) -> Result<Value> {
        self.http.get_json(&format!("{}/user", self.api)).await
    }

    async fn user_viewer(&self) -> Result<ForgeViewer> {
        let value = self.user_json().await?;
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
            scopes: Vec::new(),
        })
    }

    /// The authenticated account's uuid, for the filters that need a user
    /// identity.
    ///
    /// Bitbucket documents `author.uuid` and `reviewers.uuid` as the stable
    /// identity fields. Nicknames and display names are not unique (two
    /// people can share a display name, and a token may expose no nickname),
    /// so the filters never match on them. A workspace, project or repository
    /// access token is a workspace, not a person: `/user` rejects it and the
    /// workspace slug is not an author. Those tokens get an honest "not
    /// supported" instead of an empty list.
    async fn filter_uuid(&self) -> Result<String> {
        let value = self.user_json().await.map_err(|error| match error {
            ForgeError::Unauthorized { .. } | ForgeError::Forbidden { .. } => {
                ForgeError::Unsupported(
                    "Mine and review-requested lists need a personal account; a workspace, project or repository access token has no user identity. Add an Atlassian API token with your account email to use them."
                        .into(),
                )
            }
            other => other,
        })?;
        optional_text(&value, "uuid")
            .map(|uuid| uuid.trim().to_owned())
            .filter(|uuid| !uuid.is_empty())
            .ok_or_else(|| {
                ForgeError::Parse(
                    "Bitbucket returned no account uuid, so the personal filters cannot be applied"
                        .into(),
                )
            })
    }

    /// Proves a repository-scoped token against the repository it was granted
    /// for. The workspace slug names the account, but it is not a person, so
    /// it cannot answer the Mine filter.
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
                    "the token cannot read an account; add a repository in the form so it can be validated against that repository"
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
        let mut query: Vec<(&str, String)> = vec![
            ("state", "OPEN".to_owned()),
            ("pagelen", "50".to_owned()),
            ("sort", "-updated_on".to_owned()),
        ];
        // Author, reviewer and source-branch filters all have a documented
        // `q` form, so the server does the filtering and a page maps one to
        // one onto a server page. Nothing is filtered locally, which is what
        // used to repeat rows when a later page re-read earlier results.
        match filter {
            ForgePrFilter::Open => {}
            ForgePrFilter::Mine => {
                // Match the account's uuid, not its nickname or display name:
                // two people can share a display name, and a token may expose
                // no nickname at all.
                let uuid = self.filter_uuid().await?;
                query.push(("q", format!("author.uuid = {}", quote(&uuid))));
            }
            ForgePrFilter::ReviewRequested => {
                let uuid = self.filter_uuid().await?;
                query.push(("q", format!("reviewers.uuid = {}", quote(&uuid))));
                // The PR collection omits `reviewers` by default; ask for the
                // documented partial-response addition so the row can show
                // who was asked even if the server's filter is lenient.
                query.push(("fields", "+values.reviewers".to_owned()));
            }
            ForgePrFilter::ByBranch { branch } => {
                query.push(("q", format!("source.branch.name = {}", quote(branch))));
            }
        }
        let base = with_query(&format!("{}/pullrequests", self.repo_url(repo)), &query);
        let url = self.page_url(&base, page);
        let value = self.http.get_json(&url).await?;
        let items = value["values"]
            .as_array()
            .ok_or_else(|| ForgeError::Parse("Bitbucket returned no PR list".into()))?;
        let prs = items
            .iter()
            .filter_map(|item| map_pr_summary(repo, item))
            .collect::<Vec<_>>();
        // A `next` link on another origin is never followed; treat it as the
        // end of the list rather than promising another page.
        let next =
            optional_text(&value, "next").filter(|next| crate::http::same_origin(next, &self.api));
        let has_more = next.is_some();
        self.remember_cursor(&base, page, next);
        Ok(PrPage {
            prs,
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
            pr.line_counts_known = true;
            pr.changed_files = files;
            pr.changed_files_known = true;
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
        let (values, has_more) = self.get_pages(&url, 10).await?;
        let (comments, threads) = group_comments(&values);
        let pr = self.fetch_pr(repo, number).await.unwrap_or(Value::Null);
        let reviews = map_reviewers(&pr);
        Ok(ForgeCommentPage {
            repo: repo.clone(),
            number,
            // Only true when the tenth page still linked a next page this
            // client did not read; a short page is the end even if it had
            // exactly as many rows as some earlier fetch.
            has_more,
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
        let (values, _) = self
            .get_pages(&url, 5)
            .await
            .unwrap_or_else(|_| (Vec::new(), false));
        let checks = values.iter().map(map_status).collect::<Vec<_>>();
        Ok(finish_checks(repo, number, sha, checks))
    }

    async fn list_commits(&self, repo: &ForgeRepoRef, number: u32) -> Result<ForgeCommits> {
        validate_repo(repo)?;
        let url = format!("{}/commits?pagelen=100", self.pr_url(repo, number));
        let (values, truncated) = self.get_pages(&url, 5).await?;
        let mut commits = values.iter().filter_map(map_commit).collect::<Vec<_>>();
        // Bitbucket does not promise an order; the tab reads oldest first.
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
                    "draft": request.draft,
                }),
            )
            .await?;
        map_pr_detail(repo, &value)
    }

    async fn list_issues(
        &self,
        _repo: &ForgeRepoRef,
        _filter: ForgeIssueFilter,
        _page: u32,
        _query: Option<&str>,
    ) -> Result<ForgeIssuePage> {
        Err(crate::issues::unavailable(ForgeKind::Bitbucket))
    }

    async fn get_issue(&self, _repo: &ForgeRepoRef, _number: u32) -> Result<ForgeIssue> {
        Err(crate::issues::unavailable(ForgeKind::Bitbucket))
    }

    async fn list_issue_comments(
        &self,
        _repo: &ForgeRepoRef,
        _number: u32,
        _cursor: Option<&str>,
    ) -> Result<ForgeIssueCommentPage> {
        Err(crate::issues::unavailable(ForgeKind::Bitbucket))
    }

    async fn comment_issue(
        &self,
        _repo: &ForgeRepoRef,
        _number: u32,
        _body: &str,
    ) -> Result<ForgeIssueComment> {
        Err(crate::issues::unavailable(ForgeKind::Bitbucket))
    }

    async fn update_issue(
        &self,
        _repo: &ForgeRepoRef,
        _number: u32,
        _update: &ForgeIssueUpdate,
    ) -> Result<ForgeIssue> {
        Err(crate::issues::unavailable(ForgeKind::Bitbucket))
    }
}

fn urlencode(value: &str) -> String {
    crate::http::encode_path_segment(value)
}

fn with_query(url: &str, query: &[(&str, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in query {
        serializer.append_pair(key, value);
    }
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}{}", serializer.finish())
}

/// Quotes one value for Bitbucket's documented `q` filter language.
fn quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn user_login(value: &Value) -> String {
    optional_text(value, "nickname")
        .or_else(|| optional_text(value, "username"))
        .or_else(|| optional_text(value, "display_name"))
        .unwrap_or_default()
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
        // The diffstat supplies the totals; when it cannot be read in full they
        // stay unknown rather than zero.
        line_counts_known: false,
        changed_files: 0,
        changed_files_known: false,
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
            // Only a line anchor can go stale. Bitbucket reports that as
            // `inline.outdated` on the root comment; a file-level note (no
            // `to`/`from` line) has no line that could have moved.
            outdated: line.is_some() && inline["outdated"].as_bool().unwrap_or(false),
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
        // The root anchors a line, so it is not a file-level note.
        assert!(!threads[0].outdated);
    }

    #[test]
    fn learned_cursors_are_sparse_and_bounded() {
        let client =
            BitbucketClient::new("https://bitbucket.test", "ATBB-token", None, 120).unwrap();
        let base = "https://bitbucket.test/2.0/repositories/acme/widget/pullrequests?state=OPEN";
        assert_eq!(client.page_url(base, 1), format!("{base}&page=1"));
        client.remember_cursor(base, 1, Some("opaque-cursor".into()));
        assert_eq!(client.page_url(base, 2), "opaque-cursor");
        // A page that was never learned uses the documented parameter; it does
        // not allocate a slot per intermediate page.
        assert!(client.page_url(base, 1_000_000).ends_with("&page=1000000"));
        for page in 2..=1_000 {
            client.remember_cursor(base, page, Some(format!("cursor-{page}")));
        }
        let cursors = client.cursors.lock().unwrap();
        assert_eq!(cursors.urls.len(), CURSOR_ENTRIES);
        assert_eq!(cursors.order.len(), CURSOR_ENTRIES);
    }

    #[test]
    fn bitbucket_reads_outdated_from_the_root_inline() {
        let comments = vec![
            json!({
                "id": 1,
                "content": {"raw": "moved"},
                "user": {"nickname": "bob"},
                "created_on": "2024-05-01T10:00:00Z",
                "inline": {"path": "src/lib.rs", "from": null, "to": 4, "outdated": true}
            }),
            json!({
                "id": 2,
                "content": {"raw": "file note"},
                "user": {"nickname": "bob"},
                "created_on": "2024-05-01T10:01:00Z",
                "inline": {"path": "src/lib.rs", "from": null, "to": null, "outdated": true}
            }),
        ];
        let (_, threads) = group_comments(&comments);
        assert_eq!(threads.len(), 2);
        assert!(threads[0].outdated);
        assert!(!threads[1].outdated);
    }

    #[test]
    fn bitbucket_file_level_comment_is_not_outdated() {
        let comments = vec![json!({
            "id": 7,
            "content": {"raw": "A note on the whole file"},
            "user": {"nickname": "bob"},
            "created_on": "2024-05-01T10:00:00Z",
            "inline": {"path": "src/lib.rs", "from": null, "to": null}
        })];
        let (_, threads) = group_comments(&comments);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].path, "src/lib.rs");
        assert_eq!(threads[0].line, None);
        assert!(
            !threads[0].outdated,
            "a file-level comment has no line that could move"
        );
    }
}
