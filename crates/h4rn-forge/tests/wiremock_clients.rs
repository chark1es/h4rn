//! Wiremock tests for the three forge API shapes.
//!
//! Fixtures are trimmed but realistic: field names, nesting and nulls match the
//! provider responses the clients parse. The live tests at the bottom are
//! ignored by default and gated on environment variables; `scripts/test-forges.sh`
//! runs them against Docker when the user asks.

use h4rn_forge::{Forge, ForgeError, GitHubClient, GitLabClient, GiteaClient};
use h4rn_protocol::{ForgeDiffSide, ForgeKind, ForgePrFilter, ForgePrState, ForgeRepoRef};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn repo(forge: ForgeKind, base_url: &str) -> ForgeRepoRef {
    ForgeRepoRef {
        forge,
        base_url: base_url.to_owned(),
        owner: "acme".into(),
        repo: "widget".into(),
    }
}

fn json_body(value: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(value)
}

// ---------------------------------------------------------------------------
// GitHub
// ---------------------------------------------------------------------------

#[tokio::test]
async fn github_reads_rest_and_graphql_into_normalised_types() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v3/user"))
        .respond_with(
            json_body(json!({"login": "alice", "name": "Alice Example", "id": 1}))
                .insert_header("x-oauth-scopes", "repo, read:user"),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/pulls"))
        .and(query_param("state", "open"))
        .respond_with(json_body(json!([github_pr_summary()])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/pulls/42"))
        .respond_with(json_body(github_pr_detail()))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/pulls/42/reviews"))
        .respond_with(json_body(json!([{
            "id": 7,
            "user": {"login": "bob"},
            "state": "CHANGES_REQUESTED",
            "submitted_at": "2024-05-01T12:00:00Z",
            "body": "Please add a test."
        }])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/pulls/42/files"))
        .respond_with(json_body(json!([{
            "filename": "src/lib.rs",
            "previous_filename": "src/old.rs",
            "status": "renamed",
            "additions": 12,
            "deletions": 3,
            "changes": 15,
            "patch": "@@ -1,2 +1,2 @@\n-old\n+new"
        }])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/issues/42/comments"))
        .respond_with(json_body(json!([{
            "id": 9001,
            "user": {"login": "carol"},
            "body": "Looks good overall.",
            "created_at": "2024-05-01T10:00:00Z",
            "updated_at": null,
            "html_url": "https://github.com/acme/widget/pull/42#issuecomment-9001"
        }])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/pulls/42/comments"))
        .respond_with(json_body(json!([{
            "id": 1001,
            "user": {"login": "bob"},
            "body": "This can panic.",
            "path": "src/lib.rs",
            "line": 12,
            "original_line": 12,
            "side": "RIGHT",
            "in_reply_to_id": null,
            "created_at": "2024-05-01T11:00:00Z",
            "html_url": "https://github.com/acme/widget/pull/42#discussion_r1001"
        }])))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/graphql"))
        .respond_with(json_body(json!({
            "data": {"repository": {"pullRequest": {"reviewThreads": {
                "pageInfo": {"hasNextPage": false, "endCursor": null},
                "nodes": [{
                    "id": "PRRT_1",
                    "isResolved": false,
                    "isOutdated": false,
                    "path": "src/lib.rs",
                    "line": 12,
                    "originalLine": 12,
                    "diffSide": "RIGHT",
                    "comments": {"nodes": [{
                        "id": "PRRC_1",
                        "databaseId": 1001,
                        "body": "This can panic.",
                        "createdAt": "2024-05-01T11:00:00Z",
                        "updatedAt": null,
                        "url": "https://github.com/acme/widget/pull/42#discussion_r1001",
                        "author": {"login": "bob"},
                        "replyTo": null
                    }]}
                }]
            }}}}
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/commits/abc123/check-runs"))
        .respond_with(json_body(json!({
            "total_count": 1,
            "check_runs": [{
                "name": "ci/test",
                "status": "completed",
                "conclusion": "success",
                "details_url": "https://github.com/acme/widget/actions/runs/1",
                "output": {"title": "all tests passed", "summary": ""},
                "started_at": "2024-05-01T10:00:00Z",
                "completed_at": "2024-05-01T10:05:00Z"
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/commits/abc123/status"))
        .respond_with(json_body(json!({
            "state": "failure",
            "statuses": [{
                "context": "ci/lint",
                "state": "failure",
                "target_url": "https://ci.example.test/1",
                "description": "lint failed",
                "created_at": "2024-05-01T10:00:00Z",
                "updated_at": "2024-05-01T10:01:00Z"
            }]
        })))
        .mount(&server)
        .await;

    let client = GitHubClient::new(&server.uri(), "ghp_test", 120).unwrap();
    let repo = repo(ForgeKind::GitHub, &server.uri());

    let viewer = client.viewer().await.unwrap();
    assert_eq!(viewer.login, "alice");
    assert_eq!(viewer.display_name.as_deref(), Some("Alice Example"));
    assert_eq!(viewer.scopes, vec!["repo", "read:user"]);

    let page = client
        .list_prs(&repo, &ForgePrFilter::Open, 1)
        .await
        .unwrap();
    assert_eq!(page.page, 1);
    assert_eq!(page.prs.len(), 1);
    assert_eq!(page.prs[0].number, 42);
    assert_eq!(page.prs[0].state, ForgePrState::Open);

    let pr = client.get_pr(&repo, 42).await.unwrap();
    assert_eq!(pr.summary.title, "Fix the widget");
    assert_eq!(pr.additions, 12);
    assert_eq!(pr.changed_files, 2);
    assert_eq!(pr.reviewers.len(), 1);
    assert_eq!(pr.reviewers[0].author, "bob");

    let diff = client.get_diff(&repo, 42, 1, 30).await.unwrap();
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "src/lib.rs");
    assert_eq!(diff.files[0].previous_path.as_deref(), Some("src/old.rs"));

    let comments = client.list_comments(&repo, 42).await.unwrap();
    assert_eq!(comments.comments.len(), 1);
    assert_eq!(comments.comments[0].author, "carol");
    assert_eq!(comments.threads.len(), 1);
    assert_eq!(comments.threads[0].thread_id, "PRRT_1");
    assert_eq!(comments.threads[0].comments[0].side, ForgeDiffSide::Right);
    assert_eq!(comments.reviews.len(), 1);

    let checks = client.list_checks(&repo, 42).await.unwrap();
    assert_eq!(checks.head_sha, "abc123");
    assert_eq!(checks.total, 2);
    assert_eq!(checks.passed, 1);
    assert_eq!(checks.failed, 1);
    assert_eq!(checks.state, h4rn_protocol::ForgeChecksState::Failing);
}

#[tokio::test]
async fn github_etag_revalidation_serves_the_cached_body() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/pulls"))
        .respond_with(json_body(json!([github_pr_summary()])).insert_header("etag", "\"v1\""))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/pulls"))
        .and(header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;

    let client = GitHubClient::new(&server.uri(), "ghp_test", 120).unwrap();
    let repo = repo(ForgeKind::GitHub, &server.uri());
    let first = client
        .list_prs(&repo, &ForgePrFilter::Open, 1)
        .await
        .unwrap();
    let second = client
        .list_prs(&repo, &ForgePrFilter::Open, 1)
        .await
        .unwrap();
    assert_eq!(first.prs.len(), 1);
    assert_eq!(second.prs.len(), 1);
    server.verify().await;
}

#[tokio::test]
async fn github_rate_limit_surfaces_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/pulls"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "42")
                .insert_header("x-ratelimit-remaining", "0"),
        )
        .mount(&server)
        .await;

    let client = GitHubClient::new(&server.uri(), "ghp_test", 120).unwrap();
    let repo = repo(ForgeKind::GitHub, &server.uri());
    match client.list_prs(&repo, &ForgePrFilter::Open, 1).await {
        Err(ForgeError::RateLimited {
            retry_after_secs, ..
        }) => assert_eq!(retry_after_secs, Some(42)),
        other => panic!("expected a rate limit error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// GitLab
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gitlab_reads_merge_requests_discussions_and_jobs() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v4/user"))
        .respond_with(json_body(json!({
            "id": 9,
            "username": "alice",
            "name": "Alice Example",
            "state": "active"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v4/projects/[^/]+/merge_requests$"))
        .respond_with(json_body(json!([gitlab_mr()])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v4/projects/[^/]+/merge_requests/7$"))
        .respond_with(json_body(gitlab_mr_detail()))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(
            r"^/api/v4/projects/[^/]+/merge_requests/7/changes$",
        ))
        .respond_with(json_body(json!({
            "changes": [{
                "old_path": "src/lib.rs",
                "new_path": "src/lib.rs",
                "new_file": false,
                "renamed_file": false,
                "deleted_file": false,
                "diff": "@@ -1,2 +1,2 @@\n-old\n+new\n+more"
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(
            r"^/api/v4/projects/[^/]+/merge_requests/7/diffs$",
        ))
        .respond_with(json_body(json!([{
            "old_path": "src/lib.rs",
            "new_path": "src/lib.rs",
            "new_file": false,
            "renamed_file": false,
            "deleted_file": false,
            "diff": "@@ -1,2 +1,2 @@\n-old\n+new\n+more"
        }])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(
            r"^/api/v4/projects/[^/]+/merge_requests/7/discussions$",
        ))
        .respond_with(json_body(json!([{
            "id": "disc-1",
            "notes": [{
                "id": 3001,
                "author": {"username": "bob"},
                "body": "Needs a null check.",
                "created_at": "2024-05-01T11:00:00Z",
                "system": false,
                "position": {"new_path": "src/lib.rs", "new_line": 12}
            }]
        }])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v4/projects/[^/]+/pipelines$"))
        .respond_with(json_body(json!([])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v4/projects/[^/]+/pipelines/900/jobs$"))
        .respond_with(json_body(json!([{
            "id": 1,
            "name": "test",
            "stage": "test",
            "status": "failed",
            "web_url": "https://gitlab.com/acme/widget/-/jobs/1",
            "failure_reason": "script_failure",
            "started_at": "2024-05-01T10:00:00Z",
            "finished_at": "2024-05-01T10:02:00Z",
            "duration": 120.0
        }])))
        .mount(&server)
        .await;

    let client = GitLabClient::new(&server.uri(), "glpat-test", 120).unwrap();
    let repo = repo(ForgeKind::GitLab, &server.uri());

    let viewer = client.viewer().await.unwrap();
    assert_eq!(viewer.login, "alice");
    assert_eq!(viewer.scopes, vec!["active"]);

    let page = client
        .list_prs(&repo, &ForgePrFilter::Open, 1)
        .await
        .unwrap();
    assert_eq!(page.prs.len(), 1);
    assert_eq!(page.prs[0].number, 7);
    assert_eq!(page.prs[0].author, "alice");

    let pr = client.get_pr(&repo, 7).await.unwrap();
    assert_eq!(pr.summary.head_branch, "alice/fix");
    assert_eq!(pr.additions, 2);
    assert_eq!(pr.deletions, 1);
    assert_eq!(pr.changed_files, 1);
    assert_eq!(pr.reviewers[0].author, "bob");

    let diff = client.get_diff(&repo, 7, 1, 30).await.unwrap();
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].additions, 2);

    let comments = client.list_comments(&repo, 7).await.unwrap();
    assert_eq!(comments.threads.len(), 1);
    assert_eq!(comments.threads[0].thread_id, "disc-1");
    assert_eq!(comments.threads[0].line, Some(12));

    let checks = client.list_checks(&repo, 7).await.unwrap();
    assert_eq!(checks.head_sha, "abc123");
    assert_eq!(checks.failed, 1);
    assert_eq!(checks.total, 1);
}

// ---------------------------------------------------------------------------
// Gitea / Forgejo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gitea_reads_pulls_raw_diff_comments_and_statuses() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .respond_with(json_body(json!({
            "login": "alice",
            "full_name": "Alice Example",
            "id": 3
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/pulls"))
        .and(query_param("state", "open"))
        .respond_with(json_body(json!([gitea_pr()])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/pulls/5"))
        .respond_with(json_body(gitea_pr_detail()))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/pulls/5.diff"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/plain; charset=utf-8")
                .set_body_string(
                    "diff --git a/src/lib.rs b/src/lib.rs\n\
                     index 1111111..2222222 100644\n\
                     --- a/src/lib.rs\n\
                     +++ b/src/lib.rs\n\
                     @@ -1,2 +1,2 @@\n\
                     -old\n\
                     +new\n",
                ),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/issues/5/comments"))
        .respond_with(json_body(json!([{
            "id": 8001,
            "user": {"login": "carol"},
            "body": "Thanks!",
            "created_at": "2024-05-01T10:00:00Z",
            "html_url": "https://gitea.example.test/acme/widget/pulls/5#issuecomment-8001"
        }])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/pulls/5/reviews"))
        .respond_with(json_body(json!([{
            "id": 11,
            "state": "REQUEST_CHANGES",
            "user": {"login": "bob"},
            "submitted_at": "2024-05-01T11:00:00Z",
            "body": "Needs work."
        }])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(
            "/api/v1/repos/acme/widget/pulls/5/reviews/11/comments",
        ))
        .respond_with(json_body(json!([{
            "id": 7001,
            "path": "src/lib.rs",
            "position": 12,
            "body": "This can panic.",
            "user": {"login": "bob"},
            "created_at": "2024-05-01T11:05:00Z"
        }])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/commits/abc123/statuses"))
        .respond_with(json_body(json!([{
            "context": "ci/test",
            "status": "success",
            "target_url": "https://ci.example.test/1",
            "description": "all tests passed",
            "created_at": "2024-05-01T10:00:00Z",
            "updated_at": "2024-05-01T10:01:00Z"
        }])))
        .mount(&server)
        .await;

    let client = GiteaClient::new(ForgeKind::Gitea, &server.uri(), "token", 120).unwrap();
    let repo = repo(ForgeKind::Gitea, &server.uri());

    let viewer = client.viewer().await.unwrap();
    assert_eq!(viewer.login, "alice");

    let page = client
        .list_prs(&repo, &ForgePrFilter::Open, 1)
        .await
        .unwrap();
    assert_eq!(page.prs.len(), 1);
    assert_eq!(page.prs[0].number, 5);

    let pr = client.get_pr(&repo, 5).await.unwrap();
    assert_eq!(pr.summary.title, "Fix the widget");
    assert_eq!(pr.reviewers.len(), 1);
    assert_eq!(
        pr.reviewers[0].state,
        h4rn_protocol::ForgeReviewState::ChangesRequested
    );

    let diff = client.get_diff(&repo, 5, 1, 30).await.unwrap();
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "src/lib.rs");
    assert_eq!(diff.files[0].additions, 1);
    assert_eq!(diff.files[0].deletions, 1);

    let comments = client.list_comments(&repo, 5).await.unwrap();
    assert_eq!(comments.comments.len(), 1);
    assert_eq!(comments.threads.len(), 1);
    assert_eq!(comments.threads[0].path, "src/lib.rs");

    let checks = client.list_checks(&repo, 5).await.unwrap();
    assert_eq!(checks.total, 1);
    assert_eq!(checks.passed, 1);
}

// ---------------------------------------------------------------------------
// Live tests (ignored)
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "reads a real GitHub repository; run with H4RN_GITHUB_TOKEN"]
async fn live_github_read_only() {
    let token = std::env::var("H4RN_GITHUB_TOKEN").unwrap_or_default();
    let client = GitHubClient::new("https://github.com", &token, 30).unwrap();
    let repo = ForgeRepoRef {
        forge: ForgeKind::GitHub,
        base_url: "https://github.com".into(),
        owner: "pingdotgg".into(),
        repo: "t3code".into(),
    };
    let page = client
        .list_prs(&repo, &ForgePrFilter::Open, 1)
        .await
        .expect("live GitHub list should succeed");
    assert!(page.page >= 1);
}

#[tokio::test]
#[ignore = "requires H4RN_GITEA_URL, H4RN_GITEA_TOKEN and H4RN_GITEA_REPO"]
async fn live_gitea_read_and_write() {
    let base = std::env::var("H4RN_GITEA_URL").expect("H4RN_GITEA_URL");
    let token = std::env::var("H4RN_GITEA_TOKEN").expect("H4RN_GITEA_TOKEN");
    let full = std::env::var("H4RN_GITEA_REPO").expect("H4RN_GITEA_REPO");
    let (owner, name) = full.split_once('/').expect("owner/repo");
    let number: u32 = std::env::var("H4RN_GITEA_PR")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let kind = match std::env::var("H4RN_GITEA_KIND").as_deref() {
        Ok("forgejo") => ForgeKind::Forgejo,
        _ => ForgeKind::Gitea,
    };
    let client = GiteaClient::new(kind, &base, &token, 120).unwrap();
    let repo = ForgeRepoRef {
        forge: kind,
        base_url: base,
        owner: owner.to_owned(),
        repo: name.to_owned(),
    };

    let viewer = client.viewer().await.expect("viewer");
    assert!(!viewer.login.is_empty());
    let page = client
        .list_prs(&repo, &ForgePrFilter::Open, 1)
        .await
        .expect("list");
    assert!(page.prs.iter().any(|pr| pr.number == number));
    let pr = client.get_pr(&repo, number).await.expect("detail");
    assert_eq!(pr.summary.number, number);
    client.get_diff(&repo, number, 1, 10).await.expect("diff");
    let page = client.list_comments(&repo, number).await.expect("comments");
    let general = client
        .create_comment(&repo, number, "h4rn forge smoke test")
        .await
        .expect("comment");
    assert!(!general.comment_id.is_empty());
    let _ = client
        .reply(
            &repo,
            number,
            page.threads.first().map(|thread| thread.thread_id.as_str()),
            page.threads
                .first()
                .and_then(|thread| thread.comments.first())
                .map(|comment| comment.comment_id.as_str())
                .unwrap_or("0"),
            "h4rn forge smoke reply",
        )
        .await
        .expect("reply");
    let checks = client.list_checks(&repo, number).await.expect("checks");
    // Commit statuses carry no re-run API and, for a repo admin's token on an
    // unprotected branch, no required marks.
    assert!(!checks.rerun.supported);
    assert!(matches!(
        client.rerun_failed_checks(&repo, number).await,
        Err(ForgeError::Unsupported(_))
    ));
    assert!(checks.checks.iter().all(|check| check.required.is_none()));

    // The seed script made two commits on the branch, oldest first: the file
    // commit, then one whose message has a body.
    let commits = client.list_commits(&repo, number).await.expect("commits");
    let titles: Vec<&str> = commits.commits.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["add the feature file", "add the second file"]);
    assert_eq!(
        commits.commits[1].body.as_deref(),
        Some("It has a body line.")
    );
    assert!(commits.commits.iter().all(|c| c.sha.len() == 40));
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn github_pr_summary() -> Value {
    json!({
        "number": 42,
        "title": "Fix the widget",
        "state": "open",
        "draft": false,
        "user": {"login": "alice"},
        "head": {"ref": "alice/fix", "sha": "abc123", "repo": {"full_name": "acme/widget"}},
        "base": {"ref": "main", "sha": "def456"},
        "updated_at": "2024-05-01T12:00:00Z",
        "html_url": "https://github.com/acme/widget/pull/42",
        "body": "Fixes the widget."
    })
}

fn github_pr_detail() -> Value {
    let mut value = github_pr_summary();
    value["body"] = json!("Fixes the widget and adds a regression test.");
    value["labels"] = json!([{"name": "bug", "color": "d73a4a"}]);
    value["mergeable"] = json!(true);
    value["additions"] = json!(12);
    value["deletions"] = json!(3);
    value["changed_files"] = json!(2);
    value["commits"] = json!(1);
    value["created_at"] = json!("2024-05-01T09:00:00Z");
    value["merge_commit_sha"] = Value::Null;
    value
}

fn gitlab_mr() -> Value {
    json!({
        "iid": 7,
        "title": "Fix the widget",
        "state": "opened",
        "draft": false,
        "author": {"username": "alice"},
        "source_branch": "alice/fix",
        "target_branch": "main",
        "sha": "abc123",
        "updated_at": "2024-05-01T12:00:00Z",
        "web_url": "https://gitlab.com/acme/widget/-/merge_requests/7",
        "description": "Fixes the widget."
    })
}

fn gitlab_mr_detail() -> Value {
    let mut value = gitlab_mr();
    value["description"] = json!("Fixes the widget and adds a regression test.");
    value["labels"] = json!(["bug"]);
    value["has_conflicts"] = json!(false);
    value["detailed_merge_status"] = json!("mergeable");
    value["changes_count"] = json!("1");
    value["diff_refs"] = json!({"base_sha": "def456", "head_sha": "abc123", "start_sha": "def456"});
    value["created_at"] = json!("2024-05-01T09:00:00Z");
    value["approved_by"] = json!([{"user": {"username": "bob"}}]);
    value["reviewers"] = json!([{"username": "carol"}]);
    value["head_pipeline"] = json!({
        "id": 900,
        "status": "failed",
        "web_url": "https://gitlab.com/acme/widget/-/pipelines/900"
    });
    value["merge_commit_sha"] = Value::Null;
    value
}

fn gitea_pr() -> Value {
    json!({
        "number": 5,
        "title": "Fix the widget",
        "state": "open",
        "draft": false,
        "user": {"login": "alice"},
        "head": {"ref": "alice/fix", "sha": "abc123"},
        "base": {"ref": "main", "sha": "def456"},
        "updated_at": "2024-05-01T12:00:00Z",
        "html_url": "https://gitea.example.test/acme/widget/pulls/5",
        "body": "Fixes the widget."
    })
}

fn gitea_pr_detail() -> Value {
    let mut value = gitea_pr();
    value["body"] = json!("Fixes the widget and adds a regression test.");
    value["labels"] = json!([{"name": "bug", "color": "#d73a4a"}]);
    value["mergeable"] = json!(true);
    value["additions"] = json!(12);
    value["deletions"] = json!(3);
    value["changed_files"] = json!(2);
    value["created_at"] = json!("2024-05-01T09:00:00Z");
    value["merge_commit_sha"] = Value::Null;
    value
}
