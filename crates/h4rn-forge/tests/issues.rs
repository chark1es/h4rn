//! Wiremock tests for the issue surfaces: every forge's list, detail, comments
//! and write calls, plus Linear's GraphQL queries and mutations.
//!
//! Fixtures use the providers' own field names. Tokens are built at runtime and
//! never look like a real credential.

use h4rn_forge::issues::LinearClient;
use h4rn_forge::{
    AzureDevOpsClient, BitbucketClient, Forge, GitHubClient, GitLabClient, GiteaClient,
};
use h4rn_protocol::ForgeKind;
use h4rn_protocol::ForgeRepoRef;
use h4rn_protocol::issues::{
    ForgeIssueCloseReason, ForgeIssueFilter, ForgeIssueState, ForgeIssueUpdate, LinearStateType,
};
use std::collections::{BTreeMap, BTreeSet};

use graphql_parser::query as gql;
use graphql_parser::schema as gql_schema;
use serde_json::{Value, json};
use wiremock::matchers::{
    body_json, body_string_contains, header, method, path, query_param, query_param_is_missing,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn token() -> String {
    ["fixture", "tok", "en"].join("-")
}

fn repo(forge: ForgeKind, base_url: &str) -> ForgeRepoRef {
    ForgeRepoRef {
        forge,
        base_url: base_url.to_owned(),
        owner: "acme".into(),
        repo: "widget".into(),
    }
}

fn ok(value: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(value)
}

/// A list response that reports how many items the catalogue holds, as Gitea
/// and Forgejo do. The header is what lets a short page keep paging.
fn ok_with_total(value: Value, total: u64) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_json(value)
        .insert_header("x-total-count", total.to_string())
}

fn github_issue() -> Value {
    json!({
        "number": 12,
        "title": "Dark mode flickers",
        "state": "open",
        "state_reason": null,
        "user": {"login": "alice"},
        "labels": [{"name": "bug", "color": "d73a4a"}],
        "assignees": [{"login": "bob", "name": "Bob"}],
        "comments": 2,
        "updated_at": "2026-01-02T00:00:00Z",
        "created_at": "2026-01-01T00:00:00Z",
        "closed_at": null,
        "html_url": "https://github.com/acme/widget/issues/12",
        "body": "The list flashes."
    })
}

// ---------------------------------------------------------------------------
// GitHub
// ---------------------------------------------------------------------------

#[tokio::test]
async fn github_lists_reads_and_writes_issues() {
    let server = MockServer::start().await;
    let base = server.uri();
    let api = "/api/v3";

    // The plain list endpoint includes PRs; the row must be filtered out.
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/issues"))
        .and(query_param("state", "open"))
        .respond_with(ok(json!([
            github_issue(),
            {"number": 13, "title": "A PR", "pull_request": {"url": "x"}}
        ])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/issues/12"))
        .respond_with(ok(github_issue()))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/issues/12/timeline"))
        .respond_with(ok(json!([{
            "event": "cross-referenced",
            "source": {"issue": {
                "number": 7,
                "title": "Fix flicker",
                "state": "open",
                "html_url": "https://github.com/acme/widget/pull/7",
                "pull_request": {"merged_at": null}
            }}
        }])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/issues/12/comments"))
        .respond_with(ok(json!([{
            "id": 90,
            "user": {"login": "carol"},
            "body": "Reproduced.",
            "created_at": "2026-01-03T00:00:00Z",
            "html_url": "https://github.com/acme/widget/issues/12#issuecomment-90"
        }])))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/v3/repos/acme/widget/issues/12/comments"))
        .and(body_json(json!({"body": "Confirmed."})))
        .respond_with(ok(json!({
            "id": 91,
            "user": {"login": "alice"},
            "body": "Confirmed.",
            "created_at": "2026-01-04T00:00:00Z"
        })))
        .mount(&server)
        .await;

    Mock::given(method("PATCH"))
        .and(path("/api/v3/repos/acme/widget/issues/12"))
        .and(body_json(json!({
            "title": "Dark mode flickers on load",
            "state": "closed",
            "state_reason": "completed",
            "labels": ["bug"]
        })))
        .respond_with(ok({
            let mut issue = github_issue();
            issue["state"] = json!("closed");
            issue["state_reason"] = json!("completed");
            issue["closed_at"] = json!("2026-01-05T00:00:00Z");
            issue
        }))
        .mount(&server)
        .await;

    let client = GitHubClient::new(&base, &token(), 600).unwrap();
    let repo = repo(ForgeKind::GitHub, &base);

    let page = client
        .list_issues(&repo, ForgeIssueFilter::Open, 1, None)
        .await
        .unwrap();
    assert_eq!(page.issues.len(), 1);
    assert_eq!(page.issues[0].number, 12);
    assert_eq!(page.issues[0].labels[0].name, "bug");

    let detail = client.get_issue(&repo, 12).await.unwrap();
    assert_eq!(detail.body, "The list flashes.");
    assert_eq!(detail.linked_pull_requests.len(), 1);
    assert_eq!(detail.linked_pull_requests[0].number, 7);

    let comments = client.list_issue_comments(&repo, 12, None).await.unwrap();
    assert_eq!(comments.comments[0].author, "carol");
    assert!(comments.comments[0].web_url.is_some());

    let posted = client.comment_issue(&repo, 12, "Confirmed.").await.unwrap();
    assert_eq!(posted.comment_id, "91");

    let updated = client
        .update_issue(
            &repo,
            12,
            &ForgeIssueUpdate {
                title: Some("Dark mode flickers on load".into()),
                state: Some(ForgeIssueState::Closed),
                close_reason: Some(ForgeIssueCloseReason::Completed),
                labels: Some(vec!["bug".into()]),
                ..ForgeIssueUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.summary.state, ForgeIssueState::Closed);
    assert_eq!(updated.close_reason, Some(ForgeIssueCloseReason::Completed));
    let _ = api;
}

#[tokio::test]
async fn github_closing_as_a_duplicate_names_the_issue_it_duplicates() {
    let server = MockServer::start().await;
    let base = server.uri();

    // The person picks issue number 9; GitHub's API wants that issue's
    // repository-wide id, which is not the number.
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/issues/9"))
        .respond_with(ok(json!({
            "id": 1234567890123u64,
            "number": 9,
            "title": "Original bug"
        })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/api/v3/repos/acme/widget/issues/12"))
        .and(body_json(json!({
            "state": "closed",
            "state_reason": "duplicate",
            "duplicate_issue_id": 1234567890123u64
        })))
        .respond_with(ok({
            let mut issue = github_issue();
            issue["state"] = json!("closed");
            issue["state_reason"] = json!("duplicate");
            issue["closed_at"] = json!("2026-01-05T00:00:00Z");
            issue
        }))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/issues/12/timeline"))
        .respond_with(ok(json!([])))
        .mount(&server)
        .await;

    let client = GitHubClient::new(&base, &token(), 600).unwrap();
    let repo = repo(ForgeKind::GitHub, &base);
    let updated = client
        .update_issue(
            &repo,
            12,
            &ForgeIssueUpdate {
                state: Some(ForgeIssueState::Closed),
                close_reason: Some(ForgeIssueCloseReason::Duplicate),
                duplicate_issue_id: Some(9),
                ..ForgeIssueUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.close_reason, Some(ForgeIssueCloseReason::Duplicate));
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.url.path() == "/api/v3/repos/acme/widget/issues/9"),
        "the picked number was resolved to its id"
    );

    // A duplicate close without the number is refused before the request.
    let missing = client
        .update_issue(
            &repo,
            12,
            &ForgeIssueUpdate {
                state: Some(ForgeIssueState::Closed),
                close_reason: Some(ForgeIssueCloseReason::Duplicate),
                ..ForgeIssueUpdate::default()
            },
        )
        .await
        .unwrap_err();
    assert!(missing.to_string().contains("duplicates"), "{missing}");
}

#[tokio::test]
async fn github_search_carries_the_involvement_qualifier() {
    let server = MockServer::start().await;
    let base = server.uri();
    Mock::given(method("GET"))
        .and(path("/api/v3/search/issues"))
        .and(query_param(
            "q",
            "repo:acme/widget is:issue is:open assignee:@me",
        ))
        .respond_with(ok(json!({
            "total_count": 1,
            "items": [github_issue()]
        })))
        .mount(&server)
        .await;
    let client = GitHubClient::new(&base, &token(), 600).unwrap();
    let page = client
        .list_issues(
            &repo(ForgeKind::GitHub, &base),
            ForgeIssueFilter::Assigned,
            1,
            None,
        )
        .await
        .unwrap();
    assert_eq!(page.issues.len(), 1);
}

#[tokio::test]
async fn github_involvement_searches_only_open_issues() {
    // "Opened by me" and "Mentioning me" are work views like "Assigned to
    // me": each search closes the state, so closed issues do not crowd them.
    let server = MockServer::start().await;
    let base = server.uri();
    for qualifier in ["author:@me", "mentions:@me"] {
        Mock::given(method("GET"))
            .and(path("/api/v3/search/issues"))
            .and(query_param(
                "q",
                format!("repo:acme/widget is:issue is:open {qualifier}"),
            ))
            .respond_with(ok(json!({"total_count": 0, "items": []})))
            .mount(&server)
            .await;
    }
    let client = GitHubClient::new(&base, &token(), 600).unwrap();
    let repo = repo(ForgeKind::GitHub, &base);
    for filter in [ForgeIssueFilter::Created, ForgeIssueFilter::Mentioning] {
        let page = client.list_issues(&repo, filter, 1, None).await.unwrap();
        assert!(page.issues.is_empty(), "{filter:?}");
    }
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path() == "/api/v3/search/issues")
            .count(),
        2
    );
}

#[tokio::test]
async fn github_timeline_follows_a_cross_reference_on_the_second_page() {
    let server = MockServer::start().await;
    let base = server.uri();

    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/issues/12"))
        .respond_with(ok(github_issue()))
        .mount(&server)
        .await;
    // GitHub pages the timeline with `Link`; the first page holds no
    // cross-reference, so only the second page carries the PR.
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/issues/12/timeline"))
        .and(query_param_is_missing("page"))
        .respond_with(ok(json!([])).insert_header(
            "link",
            format!(
                "<{base}/api/v3/repos/acme/widget/issues/12/timeline?per_page=100&page=2>; rel=\"next\""
            ),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/issues/12/timeline"))
        .and(query_param("page", "2"))
        .respond_with(ok(json!([{
            "event": "cross-referenced",
            "source": {"issue": {
                "number": 7,
                "title": "Fix flicker",
                "state": "open",
                "html_url": "https://github.com/acme/widget/pull/7",
                "pull_request": {"merged_at": null}
            }}
        }])))
        .mount(&server)
        .await;

    let client = GitHubClient::new(&base, &token(), 600).unwrap();
    let detail = client
        .get_issue(&repo(ForgeKind::GitHub, &base), 12)
        .await
        .unwrap();
    assert_eq!(detail.linked_pull_requests.len(), 1);
    assert_eq!(detail.linked_pull_requests[0].number, 7);
}

#[tokio::test]
async fn github_timeline_stops_after_ten_pages() {
    let server = MockServer::start().await;
    let base = server.uri();

    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/issues/12"))
        .respond_with(ok(github_issue()))
        .mount(&server)
        .await;
    // A server that never stops offering a next page must not make the read
    // walk forever.
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/issues/12/timeline"))
        .respond_with(ok(json!([])).insert_header(
            "link",
            format!(
                "<{base}/api/v3/repos/acme/widget/issues/12/timeline?per_page=100>; rel=\"next\""
            ),
        ))
        .expect(10)
        .mount(&server)
        .await;

    let client = GitHubClient::new(&base, &token(), 600).unwrap();
    let detail = client
        .get_issue(&repo(ForgeKind::GitHub, &base), 12)
        .await
        .unwrap();
    assert!(detail.linked_pull_requests.is_empty());
}

// ---------------------------------------------------------------------------
// GitLab
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gitlab_lists_reads_and_writes_issues() {
    let server = MockServer::start().await;
    let base = server.uri();
    let project = "/api/v4/projects/acme%2Fwidget";

    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidget/issues"))
        .and(query_param("state", "opened"))
        .and(query_param("scope", "assigned_to_me"))
        .respond_with(ok(json!([{
            "iid": 5,
            "title": "Slow search",
            "state": "opened",
            "author": {"username": "alice"},
            "labels": ["performance"],
            "assignees": [{"username": "bob", "name": "Bob"}],
            "user_notes_count": 3,
            "updated_at": "2026-02-02T00:00:00Z",
            "web_url": "https://gitlab.com/acme/widget/-/issues/5"
        }])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidget/issues/5"))
        .respond_with(ok(json!({
            "iid": 5,
            "title": "Slow search",
            "state": "opened",
            "author": {"username": "alice"},
            "labels": ["performance"],
            "assignees": [{"username": "bob", "name": "Bob"}],
            "user_notes_count": 3,
            "updated_at": "2026-02-02T00:00:00Z",
            "created_at": "2026-02-01T00:00:00Z",
            "closed_at": null,
            "description": "Search takes 4s.",
            "web_url": "https://gitlab.com/acme/widget/-/issues/5"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(
            "/api/v4/projects/acme%2Fwidget/issues/5/related_merge_requests",
        ))
        .respond_with(ok(json!([{
            "iid": 8,
            "title": "Index issues",
            "state": "opened",
            "web_url": "https://gitlab.com/acme/widget/-/merge_requests/8"
        }])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidget/issues/5/notes"))
        .respond_with(ok(json!([
            {"id": 1, "system": true, "body": "changed the description", "author": {"username": "alice"}, "created_at": "2026-02-01T00:00:00Z"},
            {"id": 2, "system": false, "body": "Same here.", "author": {"username": "carol"}, "created_at": "2026-02-03T00:00:00Z"}
        ])))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/v4/projects/acme%2Fwidget/issues/5/notes"))
        .and(body_json(json!({"body": "Fixed in !8."})))
        .respond_with(ok(json!({
            "id": 3,
            "body": "Fixed in !8.",
            "author": {"username": "alice"},
            "created_at": "2026-02-04T00:00:00Z"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v4/projects/acme%2Fwidget/members/all"))
        .and(query_param("query", "bob"))
        .respond_with(ok(json!([{"id": 42, "username": "bob"}])))
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/api/v4/projects/acme%2Fwidget/issues/5"))
        .and(body_json(json!({
            "title": "Search is slow",
            "state_event": "close",
            "labels": "performance",
            "assignee_ids": [42]
        })))
        .respond_with(ok(json!({
            "iid": 5,
            "title": "Search is slow",
            "state": "closed",
            "author": {"username": "alice"},
            "labels": ["performance"],
            "assignees": [{"username": "bob", "name": "Bob"}],
            "updated_at": "2026-02-02T00:00:00Z",
            "created_at": "2026-02-01T00:00:00Z",
            "closed_at": "2026-02-05T00:00:00Z",
            "description": "Search takes 4s."
        })))
        .mount(&server)
        .await;

    let client = GitLabClient::new(&base, &token(), 600).unwrap();
    let repo = repo(ForgeKind::GitLab, &base);

    let page = client
        .list_issues(&repo, ForgeIssueFilter::Assigned, 1, None)
        .await
        .unwrap();
    assert_eq!(page.issues.len(), 1);
    assert_eq!(page.issues[0].labels[0].name, "performance");

    let detail = client.get_issue(&repo, 5).await.unwrap();
    assert_eq!(detail.linked_pull_requests[0].number, 8);

    let comments = client.list_issue_comments(&repo, 5, None).await.unwrap();
    assert_eq!(comments.comments.len(), 1, "system notes are hidden");

    let posted = client
        .comment_issue(&repo, 5, "Fixed in !8.")
        .await
        .unwrap();
    assert_eq!(posted.comment_id, "3");

    let updated = client
        .update_issue(
            &repo,
            5,
            &ForgeIssueUpdate {
                title: Some("Search is slow".into()),
                state: Some(ForgeIssueState::Closed),
                labels: Some(vec!["performance".into()]),
                assignees: Some(vec!["bob".into()]),
                ..ForgeIssueUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.summary.state, ForgeIssueState::Closed);
    let _ = project;
}

// ---------------------------------------------------------------------------
// Gitea / Forgejo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gitea_lists_reads_and_writes_issues() {
    let server = MockServer::start().await;
    let base = server.uri();

    // The authenticated login, which the issue-list filters name.
    Mock::given(method("GET"))
        .and(path("/api/v1/user"))
        .respond_with(ok(json!({"login": "me"})))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/issues"))
        .and(query_param("type", "issues"))
        .and(query_param("assigned_by", "me"))
        .respond_with(ok(json!([{
            "number": 4,
            "title": "Export CSV",
            "state": "open",
            "user": {"login": "alice"},
            "labels": [{"id": 1, "name": "feature", "color": "00aabb"}],
            "assignees": [{"login": "bob"}],
            "comments": 1,
            "updated_at": "2026-03-02T00:00:00Z",
            "html_url": "https://gitea.com/acme/widget/issues/4"
        }])))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/issues/4"))
        .respond_with(ok(json!({
            "number": 4,
            "title": "Export CSV",
            "state": "open",
            "user": {"login": "alice"},
            "labels": [{"id": 1, "name": "feature", "color": "00aabb"}],
            "assignees": [{"login": "bob"}],
            "comments": 1,
            "updated_at": "2026-03-02T00:00:00Z",
            "created_at": "2026-03-01T00:00:00Z",
            "closed_at": null,
            "body": "A CSV button.",
            "html_url": "https://gitea.com/acme/widget/issues/4"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/issues/4/comments"))
        .respond_with(ok(json!([{
            "id": 7,
            "user": {"login": "carol"},
            "body": "+1",
            "created_at": "2026-03-03T00:00:00Z"
        }])))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/v1/repos/acme/widget/issues/4/comments"))
        .and(body_json(json!({"body": "Shipping soon."})))
        .respond_with(ok(json!({
            "id": 8,
            "user": {"login": "alice"},
            "body": "Shipping soon.",
            "created_at": "2026-03-04T00:00:00Z"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/labels"))
        .respond_with(ok(
            json!([{"id": 1, "name": "feature"}, {"id": 2, "name": "bug"}]),
        ))
        .mount(&server)
        .await;

    // Labels go through their own replacement endpoint, by label id.
    Mock::given(method("PUT"))
        .and(path("/api/v1/repos/acme/widget/issues/4/labels"))
        .and(body_json(json!({"labels": [2]})))
        .respond_with(ok(json!([{"id": 2, "name": "bug"}])))
        .mount(&server)
        .await;

    Mock::given(method("PATCH"))
        .and(path("/api/v1/repos/acme/widget/issues/4"))
        .and(body_json(json!({
            "title": "Export CSV and JSON",
            "state": "closed",
            "assignees": ["bob"]
        })))
        .respond_with(ok(json!({"number": 4})))
        .mount(&server)
        .await;

    let client = GiteaClient::new(ForgeKind::Gitea, &base, &token(), 600).unwrap();
    let repo = repo(ForgeKind::Gitea, &base);

    let page = client
        .list_issues(&repo, ForgeIssueFilter::Assigned, 1, None)
        .await
        .unwrap();
    assert_eq!(page.issues[0].assignees[0].login, "bob");

    let detail = client.get_issue(&repo, 4).await.unwrap();
    assert_eq!(detail.body, "A CSV button.");

    let comments = client.list_issue_comments(&repo, 4, None).await.unwrap();
    assert_eq!(comments.comments[0].body, "+1");

    let posted = client
        .comment_issue(&repo, 4, "Shipping soon.")
        .await
        .unwrap();
    assert_eq!(posted.comment_id, "8");

    let updated = client
        .update_issue(
            &repo,
            4,
            &ForgeIssueUpdate {
                title: Some("Export CSV and JSON".into()),
                state: Some(ForgeIssueState::Closed),
                labels: Some(vec!["bug".into()]),
                assignees: Some(vec!["bob".into()]),
                ..ForgeIssueUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.summary.number, 4);
}

#[tokio::test]
async fn gitea_comments_are_one_complete_page() {
    let server = MockServer::start().await;
    let base = server.uri();

    // Gitea's `GET /issues/{index}/comments` is not paged: its swagger lists
    // only owner, repo, index, since and before, and the server ignores
    // `limit`/`page`. A 51-comment discussion must arrive in one response,
    // with no follow-up request.
    let comments: Vec<Value> = (1..=51)
        .map(|id| {
            json!({
                "id": id,
                "user": {"login": "carol"},
                "body": format!("Comment {id}"),
                "created_at": "2026-03-03T00:00:00Z"
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/issues/4/comments"))
        .and(query_param_is_missing("limit"))
        .and(query_param_is_missing("page"))
        .respond_with(ok(Value::Array(comments)))
        .expect(2)
        .mount(&server)
        .await;

    let client = GiteaClient::new(ForgeKind::Gitea, &base, &token(), 600).unwrap();
    let repo = repo(ForgeKind::Gitea, &base);
    let page = client.list_issue_comments(&repo, 4, None).await.unwrap();
    assert_eq!(page.comments.len(), 51);
    assert!(!page.has_more);
    assert_eq!(page.next_cursor, None);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "one complete page needs exactly one request"
    );
    // Even a cursor handed back cannot produce another page.
    let again = client
        .list_issue_comments(&repo, 4, Some("2"))
        .await
        .unwrap();
    assert_eq!(again.comments.len(), 51);
    assert!(!again.has_more);
    assert_eq!(again.next_cursor, None);
}

#[tokio::test]
async fn gitea_resolves_a_label_on_the_second_catalogue_page() {
    let server = MockServer::start().await;
    let base = server.uri();

    let first_page: Vec<Value> = (1..=50)
        .map(|id| json!({"id": id, "name": format!("label-{id}")}))
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/labels"))
        .and(query_param("page", "1"))
        .respond_with(ok_with_total(Value::Array(first_page), 51))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/labels"))
        .and(query_param("page", "2"))
        .respond_with(ok_with_total(json!([{"id": 51, "name": "release"}]), 51))
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/api/v1/repos/acme/widget/issues/4/labels"))
        .and(body_json(json!({"labels": [51]})))
        .respond_with(ok(json!([{"id": 51, "name": "release"}])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/issues/4"))
        .respond_with(ok(json!({
            "number": 4,
            "title": "Export CSV",
            "state": "open",
            "user": {"login": "alice"},
            "labels": [{"id": 51, "name": "release"}],
            "updated_at": "2026-03-02T00:00:00Z"
        })))
        .mount(&server)
        .await;

    let client = GiteaClient::new(ForgeKind::Gitea, &base, &token(), 600).unwrap();
    let repo = repo(ForgeKind::Gitea, &base);
    let updated = client
        .update_issue(
            &repo,
            4,
            &ForgeIssueUpdate {
                labels: Some(vec!["release".into()]),
                ..ForgeIssueUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.summary.labels[0].name, "release");
}

/// A Gitea server whose `MAX_RESPONSE_ITEMS` is 30 caps every page at 30 even
/// though this client asks for 50. A 30-row page is not the last page.
fn gitea_issue_rows(start: u32, count: u32) -> Value {
    Value::Array(
        (start..start + count)
            .map(|number| {
                json!({
                    "number": number,
                    "title": format!("Issue {number}"),
                    "state": "open",
                    "user": {"login": "alice"},
                    "labels": [],
                    "assignees": [],
                    "comments": 0,
                    "updated_at": "2026-03-02T00:00:00Z",
                    "html_url": format!("https://gitea.com/acme/widget/issues/{number}")
                })
            })
            .collect(),
    )
}

#[tokio::test]
async fn gitea_keeps_paging_when_the_server_page_is_smaller_than_requested() {
    let server = MockServer::start().await;
    let base = server.uri();

    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/issues"))
        .and(query_param("page", "1"))
        .respond_with(ok_with_total(gitea_issue_rows(1, 30), 70))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/issues"))
        .and(query_param("page", "2"))
        .respond_with(ok_with_total(gitea_issue_rows(31, 30), 70))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/issues"))
        .and(query_param("page", "3"))
        .respond_with(ok_with_total(gitea_issue_rows(61, 10), 70))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/issues"))
        .and(query_param("page", "4"))
        .respond_with(ok_with_total(gitea_issue_rows(1, 0), 70))
        .mount(&server)
        .await;

    let client = GiteaClient::new(ForgeKind::Gitea, &base, &token(), 600).unwrap();
    let repo = repo(ForgeKind::Gitea, &base);
    let first = client
        .list_issues(&repo, ForgeIssueFilter::Open, 1, None)
        .await
        .unwrap();
    assert_eq!(first.issues.len(), 30);
    assert!(
        first.has_more,
        "a short page from a clamped server still has a next page"
    );

    let second = client
        .list_issues(&repo, ForgeIssueFilter::Open, 2, None)
        .await
        .unwrap();
    assert_eq!(second.issues.len(), 30);
    assert!(second.has_more, "the last row lives on a third short page");

    let third = client
        .list_issues(&repo, ForgeIssueFilter::Open, 3, None)
        .await
        .unwrap();
    assert_eq!(third.issues.last().map(|issue| issue.number), Some(70));

    // A count without a Link header cannot name the server's page size, so it
    // may ask once more; the empty page is what ends the listing.
    let fourth = client
        .list_issues(&repo, ForgeIssueFilter::Open, 4, None)
        .await
        .unwrap();
    assert!(fourth.issues.is_empty());
    assert!(!fourth.has_more);
}

#[tokio::test]
async fn gitea_finds_a_label_past_a_smaller_catalogue_page() {
    let server = MockServer::start().await;
    let base = server.uri();

    let first_page: Vec<Value> = (1..=30)
        .map(|id| json!({"id": id, "name": format!("label-{id}")}))
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/labels"))
        .and(query_param("page", "1"))
        .respond_with(ok_with_total(Value::Array(first_page), 45))
        .mount(&server)
        .await;
    let second_page: Vec<Value> = (31..=45)
        .map(|id| {
            json!({
                "id": id,
                "name": if id == 45 { "release".to_owned() } else { format!("label-{id}") }
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/labels"))
        .and(query_param("page", "2"))
        .respond_with(ok_with_total(Value::Array(second_page), 45))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/repos/acme/widget/issues/4/labels"))
        .and(body_json(json!({"labels": [45]})))
        .respond_with(ok(json!([{"id": 45, "name": "release"}])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/issues/4"))
        .respond_with(ok(json!({
            "number": 4,
            "title": "Export CSV",
            "state": "open",
            "user": {"login": "alice"},
            "labels": [{"id": 45, "name": "release"}],
            "updated_at": "2026-03-02T00:00:00Z"
        })))
        .mount(&server)
        .await;

    let client = GiteaClient::new(ForgeKind::Gitea, &base, &token(), 600).unwrap();
    let repo = repo(ForgeKind::Gitea, &base);
    let updated = client
        .update_issue(
            &repo,
            4,
            &ForgeIssueUpdate {
                labels: Some(vec!["release".into()]),
                ..ForgeIssueUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.summary.labels[0].name, "release");
    let label_pages = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/api/v1/repos/acme/widget/labels")
        .count();
    assert_eq!(
        label_pages, 2,
        "the short first page did not end the catalogue"
    );
}

#[tokio::test]
async fn gitea_replaces_a_label_from_the_organisation_catalogue() {
    let server = MockServer::start().await;
    let base = server.uri();

    // The repository carries one label; the requested one belongs to the
    // organisation that owns it. `ReplaceIssueLabels` accepts both.
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/labels"))
        .and(query_param("page", "1"))
        .respond_with(ok_with_total(json!([{"id": 1, "name": "feature"}]), 1))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/orgs/acme/labels"))
        .and(query_param("page", "1"))
        .respond_with(ok_with_total(json!([{"id": 51, "name": "release"}]), 1))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/repos/acme/widget/issues/4/labels"))
        .and(body_json(json!({"labels": [51]})))
        .respond_with(ok(json!([{"id": 51, "name": "release"}])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/issues/4"))
        .respond_with(ok(json!({
            "number": 4,
            "title": "Export CSV",
            "state": "open",
            "user": {"login": "alice"},
            "labels": [{"id": 51, "name": "release"}],
            "updated_at": "2026-03-02T00:00:00Z"
        })))
        .mount(&server)
        .await;

    let client = GiteaClient::new(ForgeKind::Gitea, &base, &token(), 600).unwrap();
    let updated = client
        .update_issue(
            &repo(ForgeKind::Gitea, &base),
            4,
            &ForgeIssueUpdate {
                labels: Some(vec!["release".into()]),
                ..ForgeIssueUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.summary.labels[0].name, "release");
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.url.path() == "/api/v1/orgs/acme/labels"),
        "the organisation catalogue was read"
    );
}

#[tokio::test]
async fn gitea_stops_label_paging_at_an_empty_page_without_a_count() {
    let server = MockServer::start().await;
    let base = server.uri();

    // No `X-Total-Count`: the client asks for 50 rows, the server returns 30,
    // so only an empty page proves the catalogue ended. A missing label must
    // not page forever.
    for (page, start) in [(1u32, 1u32), (2, 31)] {
        let rows: Vec<Value> = (start..start + 30)
            .map(|id| json!({"id": id, "name": format!("label-{id}")}))
            .collect();
        Mock::given(method("GET"))
            .and(path("/api/v1/repos/acme/widget/labels"))
            .and(query_param("page", page.to_string()))
            .respond_with(ok(Value::Array(rows)))
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/labels"))
        .and(query_param("page", "3"))
        .respond_with(ok(json!([])))
        .mount(&server)
        .await;

    let client = GiteaClient::new(ForgeKind::Gitea, &base, &token(), 600).unwrap();
    let error = client
        .update_issue(
            &repo(ForgeKind::Gitea, &base),
            4,
            &ForgeIssueUpdate {
                labels: Some(vec!["missing".into()]),
                ..ForgeIssueUpdate::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("no label called missing"),
        "{error}"
    );

    let requests = server.received_requests().await.unwrap();
    let pages = requests
        .iter()
        .filter(|request| request.url.path() == "/api/v1/repos/acme/widget/labels")
        .count();
    assert_eq!(pages, 3, "the empty page ends the catalogue");
    assert!(
        !requests
            .iter()
            .any(|request| request.method.as_str() == "PUT"),
        "no replacement is sent without every id"
    );
}

// ---------------------------------------------------------------------------
// Bitbucket Cloud
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bitbucket_reports_issues_as_unavailable_without_calling_it() {
    // Atlassian removed Bitbucket Cloud's issue tracker API in August 2026.
    // The client must say so before any request, and the capability row must
    // not advertise a surface that no longer exists.
    let server = MockServer::start().await;
    let base = server.uri();
    let client = BitbucketClient::new(&base, &token(), None, 600).unwrap();
    let repo = repo(ForgeKind::Bitbucket, &base);
    let error = client
        .list_issues(&repo, ForgeIssueFilter::Open, 1, None)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("removed Bitbucket Cloud's issue API"),
        "{error}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    let capabilities = h4rn_forge::issues::capabilities(ForgeKind::Bitbucket);
    assert!(!capabilities.read);
    assert!(!capabilities.write);
}

// ---------------------------------------------------------------------------
// Azure DevOps
// ---------------------------------------------------------------------------

fn azure_states() -> Value {
    json!({
        "count": 5,
        "value": [
            {"name": "New", "category": "Proposed"},
            {"name": "Active", "category": "InProgress"},
            {"name": "Resolved", "category": "Resolved"},
            {"name": "Done", "category": "Completed"},
            {"name": "Removed", "category": "Removed"}
        ]
    })
}

async fn azure_states_mocks(server: &MockServer) {
    // The project's own type catalogue, not a guessed pair of names.
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitemtypes"))
        .respond_with(ok(json!({
            "count": 2,
            "value": [
                {"name": "Issue", "referenceName": "Microsoft.VSTS.WorkItemTypes.Issue"},
                {"name": "Bug", "referenceName": "Microsoft.VSTS.WorkItemTypes.Bug"}
            ]
        })))
        .mount(server)
        .await;
    for kind in ["Issue", "Bug"] {
        Mock::given(method("GET"))
            .and(path(format!(
                "/acme/platform/_apis/wit/workitemtypes/{kind}/states"
            )))
            .respond_with(ok(azure_states()))
            .mount(server)
            .await;
    }
}

#[tokio::test]
async fn azure_lists_reads_and_writes_work_items_as_issues() {
    let server = MockServer::start().await;
    let base = format!("{}/acme", server.uri());
    azure_states_mocks(&server).await;

    // Scrum names its finished state Done; the WIQL must use the category,
    // not the GitHub-style names Closed or Active.
    Mock::given(method("POST"))
        .and(path("/acme/platform/_apis/wit/wiql"))
        .and(body_string_contains(
            "[System.State] NOT IN ('Done', 'Removed')",
        ))
        .respond_with(ok(json!({
            "workItems": [{"id": 21}, {"id": 22}]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitems"))
        .and(query_param("ids", "21,22"))
        .respond_with(ok(json!({
            "count": 2,
            "value": [{
                "id": 21,
                "fields": {
                    "System.Title": "Crash on save",
                    "System.State": "Active",
                    "System.WorkItemType": "Bug",
                    "System.CreatedBy": {"uniqueName": "alice@example.test", "displayName": "Alice"},
                    "System.AssignedTo": {"uniqueName": "bob@example.test", "displayName": "Bob"},
                    "System.Tags": "bug; urgent",
                    "System.CommentCount": 2,
                    "System.CreatedDate": "2026-05-01T00:00:00Z",
                    "System.ChangedDate": "2026-05-02T00:00:00Z",
                    "System.Description": "Saving crashes."
                },
                "_links": {"html": {"href": "https://dev.azure.com/acme/platform/_workitems/edit/21"}}
            }, {
                "id": 22,
                "fields": {
                    "System.Title": "A bug",
                    "System.State": "Done",
                    "System.WorkItemType": "Bug",
                    "System.CreatedBy": {"uniqueName": "carol@example.test"},
                    "System.CreatedDate": "2026-05-01T00:00:00Z",
                    "System.ChangedDate": "2026-05-09T00:00:00Z"
                }
            }]
        })))
        .mount(&server)
        .await;

    // A linked pull request names its own repository by id; the artifact's
    // project and repository are resolved to that repository's web URL.
    Mock::given(method("GET"))
        .and(path(
            "/acme/6f1c2e40-aaaa-bbbb-cccc-0123456789ab/_apis/git/repositories/9d2a1b7c-1111-2222-3333-444455556666",
        ))
        .respond_with(ok(json!({
            "name": "widget",
            "webUrl": format!("{base}/platform/_git/widget")
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitems/21"))
        .respond_with(ok(json!({
            "id": 21,
            "fields": {
                "System.Title": "Crash on save",
                "System.State": "Active",
                "System.WorkItemType": "Bug",
                "System.CreatedBy": {"uniqueName": "alice@example.test"},
                "System.CreatedDate": "2026-05-01T00:00:00Z",
                "System.ChangedDate": "2026-05-02T00:00:00Z",
                "System.Description": "Saving crashes."
            },
            "relations": [{
                "rel": "ArtifactLink",
                "url": "vstfs:///Git/PullRequestId/6f1c2e40-aaaa-bbbb-cccc-0123456789ab%2F9d2a1b7c-1111-2222-3333-444455556666%2F9",
                "attributes": {"name": "Pull request 9"}
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitems/22"))
        .respond_with(ok(json!({
            "id": 22,
            "fields": {
                "System.Title": "A bug",
                "System.State": "Done",
                "System.WorkItemType": "Bug",
                "System.CreatedDate": "2026-05-01T00:00:00Z",
                "System.ChangedDate": "2026-05-09T00:00:00Z",
                "Microsoft.VSTS.Common.ClosedDate": "2026-05-04T00:00:00Z",
                "Microsoft.VSTS.Common.StateChangeDate": "2026-05-04T00:00:00Z"
            }
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workItems/21/comments"))
        .and(query_param_is_missing("continuationToken"))
        .respond_with(ok(json!({
            "comments": [{
                "id": 4,
                "text": "Reproduced on Linux.",
                "createdBy": {"uniqueName": "carol@example.test"},
                "createdDate": "2026-05-04T00:00:00Z",
                "modifiedDate": "2026-05-04T00:00:00Z"
            }],
            "continuationToken": "tok_2"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workItems/21/comments"))
        .and(query_param("continuationToken", "tok_2"))
        .respond_with(ok(json!({
            "comments": [{
                "id": 5,
                "text": "Also on Linux 6.9.",
                "createdBy": {"uniqueName": "dave@example.test"},
                "createdDate": "2026-05-05T00:00:00Z"
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/acme/platform/_apis/wit/workItems/21/comments"))
        .and(body_json(json!({"text": "Fix in progress."})))
        .respond_with(ok(json!({
            "id": 6,
            "text": "Fix in progress.",
            "createdBy": {"uniqueName": "alice@example.test"},
            "createdDate": "2026-05-06T00:00:00Z"
        })))
        .mount(&server)
        .await;

    // Closing uses the Completed category's own state name.
    Mock::given(method("PATCH"))
        .and(path("/acme/platform/_apis/wit/workitems/21"))
        .and(header("content-type", "application/json-patch+json"))
        .and(body_json(json!([
            {"op": "add", "path": "/fields/System.Title", "value": "Crash when saving"},
            {"op": "add", "path": "/fields/System.State", "value": "Done"}
        ])))
        .respond_with(ok(json!({"id": 21})))
        .mount(&server)
        .await;

    let client = AzureDevOpsClient::new(&base, &token(), 600).unwrap();
    let repo = ForgeRepoRef {
        forge: ForgeKind::AzureDevOps,
        base_url: base.clone(),
        owner: "platform".into(),
        repo: "widget".into(),
    };

    let page = client
        .list_issues(&repo, ForgeIssueFilter::Open, 1, None)
        .await
        .unwrap();
    assert_eq!(page.issues.len(), 2);
    assert_eq!(page.issues[0].labels.len(), 2);
    assert_eq!(page.issues[1].state, ForgeIssueState::Closed);

    let detail = client.get_issue(&repo, 21).await.unwrap();
    assert_eq!(detail.linked_pull_requests[0].number, 9);
    assert_eq!(
        detail.linked_pull_requests[0].web_url,
        format!("{base}/platform/_git/widget/pullrequest/9")
    );

    // The closure date is the real one, not the last edit.
    let closed = client.get_issue(&repo, 22).await.unwrap();
    assert_eq!(
        closed.closed_at.map(|date| date.to_rfc3339()),
        Some("2026-05-04T00:00:00+00:00".into())
    );
    assert_ne!(
        closed.closed_at.map(|date| date.to_rfc3339()),
        closed.summary.updated_at.map(|date| date.to_rfc3339())
    );

    let comments = client.list_issue_comments(&repo, 21, None).await.unwrap();
    assert_eq!(comments.comments[0].body, "Reproduced on Linux.");
    assert_eq!(comments.next_cursor.as_deref(), Some("tok_2"));

    let more = client
        .list_issue_comments(&repo, 21, comments.next_cursor.as_deref())
        .await
        .unwrap();
    assert_eq!(more.comments[0].body, "Also on Linux 6.9.");
    assert!(more.next_cursor.is_none());

    let posted = client
        .comment_issue(&repo, 21, "Fix in progress.")
        .await
        .unwrap();
    assert_eq!(posted.comment_id, "6");

    let updated = client
        .update_issue(
            &repo,
            21,
            &ForgeIssueUpdate {
                title: Some("Crash when saving".into()),
                state: Some(ForgeIssueState::Closed),
                ..ForgeIssueUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.summary.number, 21);
}

#[tokio::test]
async fn azure_skips_a_type_this_process_does_not_define() {
    let server = MockServer::start().await;
    let base = format!("{}/acme", server.uri());

    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitemtypes"))
        .respond_with(ok(json!({
            "count": 2,
            "value": [
                {"name": "Issue", "referenceName": "Microsoft.VSTS.WorkItemTypes.Issue"},
                {"name": "Bug", "referenceName": "Microsoft.VSTS.WorkItemTypes.Bug"}
            ]
        })))
        .mount(&server)
        .await;
    // Basic has no Bug and Scrum has no Issue: the states endpoint answers
    // 404 for the missing type instead of failing the whole list.
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitemtypes/Issue/states"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "message": "The work item type Issue does not exist in this project"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitemtypes/Bug/states"))
        .respond_with(ok(azure_states()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/acme/platform/_apis/wit/wiql"))
        .and(body_string_contains("[System.WorkItemType] = 'Bug'"))
        .respond_with(ok(json!({"workItems": [{"id": 31}]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitems"))
        .respond_with(ok(json!({
            "count": 1,
            "value": [{
                "id": 31,
                "fields": {
                    "System.Title": "The only type this process has",
                    "System.State": "Active",
                    "System.WorkItemType": "Bug",
                    "System.ChangedDate": "2026-05-02T00:00:00Z"
                }
            }]
        })))
        .mount(&server)
        .await;

    let client = AzureDevOpsClient::new(&base, &token(), 600).unwrap();
    let repo = ForgeRepoRef {
        forge: ForgeKind::AzureDevOps,
        base_url: base.clone(),
        owner: "platform".into(),
        repo: "widget".into(),
    };
    let page = client
        .list_issues(&repo, ForgeIssueFilter::Open, 1, None)
        .await
        .expect("the present type still lists");
    assert_eq!(page.issues.len(), 1);
    assert_eq!(page.issues[0].number, 31);
    let wiql = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|request| request.url.path() == "/acme/platform/_apis/wit/wiql")
        .expect("the WIQL request");
    let body = String::from_utf8_lossy(&wiql.body);
    assert!(!body.contains("'Issue'"), "{body}");
}

#[tokio::test]
async fn azure_recognises_an_inherited_type_by_its_real_reference_name() {
    let server = MockServer::start().await;
    let base = format!("{}/acme", server.uri());

    // An inherited process keeps the system reference names, and a renamed
    // copy keeps the `<Process>.<type>` one. `Microsoft.Issue` and
    // `Microsoft.Bug` are not names Azure ever returns.
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitemtypes"))
        .respond_with(ok(json!({
            "count": 3,
            "value": [
                {"name": "Defect", "referenceName": "Acme.Bug"},
                {"name": "Story", "referenceName": "Microsoft.VSTS.WorkItemTypes.UserStory"},
                {"name": "Ghost", "referenceName": "Microsoft.Issue"}
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitemtypes/Defect/states"))
        .respond_with(ok(azure_states()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/acme/platform/_apis/wit/wiql"))
        .and(body_string_contains("[System.WorkItemType] = 'Defect'"))
        .respond_with(ok(json!({"workItems": [{"id": 51}]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitems"))
        .respond_with(ok(json!({
            "count": 1,
            "value": [{
                "id": 51,
                "fields": {
                    "System.Title": "A renamed bug",
                    "System.State": "Active",
                    "System.WorkItemType": "Defect",
                    "System.ChangedDate": "2026-05-02T00:00:00Z"
                }
            }]
        })))
        .mount(&server)
        .await;

    let client = AzureDevOpsClient::new(&base, &token(), 600).unwrap();
    let repo = ForgeRepoRef {
        forge: ForgeKind::AzureDevOps,
        base_url: base.clone(),
        owner: "platform".into(),
        repo: "widget".into(),
    };
    let page = client
        .list_issues(&repo, ForgeIssueFilter::Open, 1, None)
        .await
        .expect("the inherited Bug copy lists");
    assert_eq!(page.issues.len(), 1);
    assert_eq!(page.issues[0].number, 51);

    let requests = server.received_requests().await.unwrap();
    let wiql = requests
        .iter()
        .find(|request| request.url.path() == "/acme/platform/_apis/wit/wiql")
        .expect("the WIQL request");
    let body = String::from_utf8_lossy(&wiql.body);
    assert!(!body.contains("'Story'"), "{body}");
    assert!(!body.contains("'Ghost'"), "{body}");
    assert!(
        !requests
            .iter()
            .any(|request| request.url.path().contains("/Story/")),
        "a non-issue type must not be read"
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.url.path().contains("/Ghost/")),
        "the fake Microsoft.Issue name must not be read"
    );
}

#[tokio::test]
async fn azure_closes_each_type_with_its_own_finished_state() {
    let server = MockServer::start().await;
    let base = format!("{}/acme", server.uri());

    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitemtypes"))
        .respond_with(ok(json!({
            "count": 2,
            "value": [
                {"name": "Issue", "referenceName": "Microsoft.VSTS.WorkItemTypes.Issue"},
                {"name": "Bug", "referenceName": "Microsoft.VSTS.WorkItemTypes.Bug"}
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitemtypes/Issue/states"))
        .respond_with(ok(json!({
            "value": [
                {"name": "To Do", "category": "Proposed"},
                {"name": "Doing", "category": "InProgress"},
                {"name": "Closed", "category": "Completed"}
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitemtypes/Bug/states"))
        .respond_with(ok(azure_states()))
        .mount(&server)
        .await;

    // The closed prompt is scoped to each type's own state names.
    Mock::given(method("POST"))
        .and(path("/acme/platform/_apis/wit/wiql"))
        .and(body_string_contains(
            "[System.WorkItemType] = 'Issue' AND [System.State] IN ('Closed')",
        ))
        .and(body_string_contains(
            "[System.WorkItemType] = 'Bug' AND [System.State] IN ('Done', 'Removed')",
        ))
        .respond_with(ok(json!({"workItems": [{"id": 41}, {"id": 42}]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitems"))
        .respond_with(ok(json!({
            "count": 2,
            "value": [{
                "id": 41,
                "fields": {
                    "System.Title": "An issue",
                    "System.State": "Closed",
                    "System.WorkItemType": "Issue",
                    "System.ChangedDate": "2026-05-02T00:00:00Z"
                }
            }, {
                "id": 42,
                "fields": {
                    "System.Title": "A bug",
                    "System.State": "Done",
                    "System.WorkItemType": "Bug",
                    "System.ChangedDate": "2026-05-02T00:00:00Z"
                }
            }]
        })))
        .mount(&server)
        .await;

    let client = AzureDevOpsClient::new(&base, &token(), 600).unwrap();
    let repo = ForgeRepoRef {
        forge: ForgeKind::AzureDevOps,
        base_url: base.clone(),
        owner: "platform".into(),
        repo: "widget".into(),
    };
    let page = client
        .list_issues(&repo, ForgeIssueFilter::Closed, 1, None)
        .await
        .expect("closed items list");
    assert_eq!(page.issues.len(), 2);
    assert!(
        page.issues
            .iter()
            .all(|issue| issue.state == ForgeIssueState::Closed)
    );

    // Reopening the Issue moves it to its type's in-progress state, Doing.
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workitems/41"))
        .respond_with(ok(json!({
            "id": 41,
            "fields": {
                "System.Title": "An issue",
                "System.State": "Closed",
                "System.WorkItemType": "Issue",
                "System.ChangedDate": "2026-05-02T00:00:00Z"
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/acme/platform/_apis/wit/workitems/41"))
        .and(body_string_contains("Doing"))
        .respond_with(ok(json!({"id": 41})))
        .mount(&server)
        .await;
    let reopened = client
        .update_issue(
            &repo,
            41,
            &ForgeIssueUpdate {
                state: Some(ForgeIssueState::Open),
                ..ForgeIssueUpdate::default()
            },
        )
        .await
        .expect("a work item reopens with its own type's state");
    assert_eq!(reopened.summary.number, 41);
}

#[tokio::test]
async fn azure_percent_encodes_a_project_name_with_a_space() {
    let server = MockServer::start().await;
    let base = format!("{}/acme", server.uri());

    // A team project may contain a space. A path segment encodes it as `%20`;
    // form encoding's `+` is a literal plus in a path, so the request 404s.
    let project = "/acme/Platform%20Team";
    Mock::given(method("GET"))
        .and(path(format!("{project}/_apis/wit/workitemtypes")))
        .respond_with(ok(json!({
            "count": 1,
            "value": [
                {"name": "Bug", "referenceName": "Microsoft.VSTS.WorkItemTypes.Bug"}
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{project}/_apis/wit/workitemtypes/Bug/states"
        )))
        .respond_with(ok(azure_states()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{project}/_apis/wit/wiql")))
        .respond_with(ok(json!({"workItems": [{"id": 21}]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{project}/_apis/wit/workitems")))
        .and(query_param("ids", "21"))
        .respond_with(ok(json!({
            "count": 1,
            "value": [{
                "id": 21,
                "fields": {
                    "System.Title": "Crash on save",
                    "System.State": "Active",
                    "System.WorkItemType": "Bug",
                    "System.ChangedDate": "2026-05-02T00:00:00Z"
                }
            }]
        })))
        .mount(&server)
        .await;

    let client = AzureDevOpsClient::new(&base, &token(), 600).unwrap();
    let repo = ForgeRepoRef {
        forge: ForgeKind::AzureDevOps,
        base_url: base.clone(),
        owner: "Platform Team".into(),
        repo: "widget".into(),
    };
    let page = client
        .list_issues(&repo, ForgeIssueFilter::Open, 1, None)
        .await
        .expect("a project with a space reads through encoded paths");
    assert_eq!(page.issues.len(), 1);
    // The fallback browser link encodes the same name the same way.
    assert_eq!(
        page.issues[0].web_url,
        format!("{base}/Platform%20Team/_workitems/edit/21")
    );
    assert!(
        !server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.url.path().contains("Platform+Team")),
        "a path never carries form encoding"
    );
}

// ---------------------------------------------------------------------------
// Linear
// ---------------------------------------------------------------------------

fn linear_issue() -> Value {
    json!({
        "id": "iss_1",
        "identifier": "ENG-12",
        "title": "Rate limit the import",
        "description": "Imports hammer the API.",
        "createdAt": "2026-06-01T00:00:00Z",
        "updatedAt": "2026-06-02T00:00:00Z",
        "url": "https://linear.app/acme/issue/ENG-12",
        "state": {"name": "In Progress", "type": "started"},
        "team": {"id": "team_1", "key": "ENG"},
        // Linear exposes a single assignee, not a connection of them.
        "assignee": {"displayName": "Alice"},
        "labels": {"nodes": [{"name": "backend"}]}
    })
}

#[tokio::test]
async fn linear_validates_lists_reads_and_comments() {
    let server = MockServer::start().await;
    let endpoint = format!("{}/graphql", server.uri());
    let api_key = token();

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(header("authorization", api_key.as_str()))
        .and(body_json(json!({
            "query": h4rn_forge::issues::LINEAR_VIEWER_QUERY,
            "variables": {}
        })))
        .respond_with(ok(json!({
            "data": {"viewer": {"name": "Alice", "email": "alice@example.test", "organization": {"name": "Acme"}}}
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_json(json!({
            "query": h4rn_forge::issues::LINEAR_TEAMS_QUERY,
            "variables": {}
        })))
        .respond_with(ok(json!({
            "data": {"teams": {"nodes": [{"id": "team_1", "key": "ENG", "name": "Engineering"}]}}
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_json(json!({
            "query": h4rn_forge::issues::LINEAR_LIST_QUERY,
            "variables": {
                "teamId": "team_1",
                "filter": {"state": {"type": {"in": ["triage", "backlog", "unstarted", "started"]}}},
                "first": 50,
                "after": null
            }
        })))
        .respond_with(ok(json!({
            "data": {"issues": {
                "nodes": [linear_issue()],
                "pageInfo": {"hasNextPage": true, "endCursor": "cursor_2"}
            }}
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_json(json!({
            "query": h4rn_forge::issues::LINEAR_DETAIL_QUERY,
            "variables": {"id": "iss_1", "after": null}
        })))
        .respond_with(ok(json!({
            "data": {"issue": {
                "id": "iss_1",
                "identifier": "ENG-12",
                "title": "Rate limit the import",
                "description": "Imports hammer the API.",
                "createdAt": "2026-06-01T00:00:00Z",
                "updatedAt": "2026-06-02T00:00:00Z",
                "url": "https://linear.app/acme/issue/ENG-12",
                "state": {"name": "In Progress", "type": "started"},
                "team": {"id": "team_1", "key": "ENG"},
                "assignee": {"displayName": "Alice"},
                "labels": {"nodes": [{"name": "backend"}]},
                "comments": {
                    "nodes": [{
                        "id": "c1",
                        "body": "Working on it.",
                        "createdAt": "2026-06-03T00:00:00Z",
                        "updatedAt": null,
                        "user": {"displayName": "Bob"}
                    }],
                    "pageInfo": {"hasNextPage": true, "endCursor": "comments_2"}
                },
                "attachments": {"nodes": [{
                    "title": "Rate limit",
                    "url": "https://github.com/acme/widget/pull/15",
                    "metadata": {"state": "open"}
                }]}
            }}
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_json(json!({
            "query": h4rn_forge::issues::LINEAR_COMMENT_MUTATION,
            "variables": {"issueId": "iss_1", "body": "Shipped."}
        })))
        .respond_with(ok(json!({
            "data": {"commentCreate": {"success": true, "comment": {
                "id": "c2",
                "body": "Shipped.",
                "createdAt": "2026-06-04T00:00:00Z",
                "updatedAt": null,
                "user": {"displayName": "Alice"}
            }}}
        })))
        .mount(&server)
        .await;

    let client = LinearClient::with_endpoint(&api_key, &endpoint).unwrap();
    let viewer = client.viewer().await.unwrap();
    assert_eq!(viewer.name, "Acme");
    assert_eq!(viewer.email.as_deref(), Some("alice@example.test"));

    let teams = client.teams().await.unwrap();
    assert_eq!(teams[0].key, "ENG");

    let page = client
        .list_issues("team_1", ForgeIssueFilter::Open, None, None)
        .await
        .unwrap();
    assert_eq!(page.issues[0].identifier, "ENG-12");
    assert_eq!(page.issues[0].state_type, LinearStateType::Started);
    assert_eq!(page.next_cursor.as_deref(), Some("cursor_2"));

    let detail = client.get_issue("iss_1", None).await.unwrap();
    assert_eq!(detail.comments[0].author, "Bob");
    assert_eq!(detail.comments_next_cursor.as_deref(), Some("comments_2"));
    assert_eq!(detail.linked_pull_requests.len(), 1);

    let posted = client.create_comment("iss_1", "Shipped.").await.unwrap();
    assert_eq!(posted.id, "c2");
}

#[tokio::test]
async fn linear_search_carries_the_same_filter_as_the_list() {
    let server = MockServer::start().await;
    let endpoint = format!("{}/graphql", server.uri());
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_json(json!({
            "query": h4rn_forge::issues::LINEAR_SEARCH_QUERY,
            "variables": {
                "teamId": "team_1",
                "term": "import",
                "filter": {
                    "state": {"type": {"in": ["triage", "backlog", "unstarted", "started"]}},
                    "assignee": {"isMe": {"eq": true}}
                },
                "first": 50,
                "after": null
            }
        })))
        .respond_with(ok(json!({
            "data": {"searchIssues": {
                "nodes": [linear_issue()],
                "pageInfo": {"hasNextPage": false, "endCursor": null}
            }}
        })))
        .mount(&server)
        .await;
    let client = LinearClient::with_endpoint(&token(), &endpoint).unwrap();
    let page = client
        .list_issues("team_1", ForgeIssueFilter::Assigned, None, Some("import"))
        .await
        .unwrap();
    assert_eq!(page.issues.len(), 1);
    assert_eq!(page.issues[0].assignees, vec!["Alice".to_owned()]);
}

async fn linear_status_error(status: u16, body: Value) -> String {
    linear_status_error_with_key(status, body, &token()).await
}

async fn linear_status_error_with_key(status: u16, body: Value, key: &str) -> String {
    let server = MockServer::start().await;
    let endpoint = format!("{}/graphql", server.uri());
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    let client = LinearClient::with_endpoint(key, &endpoint).unwrap();
    client.viewer().await.unwrap_err().to_string()
}

#[tokio::test]
async fn linear_errors_never_echo_the_configured_key() {
    let key = ["fixture", "lin", "ear", "key"].join("-");
    let message = format!("the key {key} is not valid");
    let error = linear_status_error_with_key(401, json!({"message": message}), &key).await;
    assert!(!error.contains(&key), "{error}");
    assert!(error.contains("[redacted]"), "{error}");
    assert!(error.contains("Linear"), "{error}");

    // A GraphQL error carried with HTTP 200 takes the same path.
    let server = MockServer::start().await;
    let endpoint = format!("{}/graphql", server.uri());
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ok(json!({"errors": [{"message": message}]})))
        .mount(&server)
        .await;
    let client = LinearClient::with_endpoint(&key, &endpoint).unwrap();
    let error = client.viewer().await.unwrap_err().to_string();
    assert!(!error.contains(&key), "{error}");
    assert!(error.contains("[redacted]"), "{error}");
}

#[tokio::test]
async fn linear_forbidden_and_rate_limited_are_not_github_token_errors() {
    let forbidden = linear_status_error(403, json!({"message": "workspace restricted"})).await;
    assert!(forbidden.contains("Linear refused"), "{forbidden}");
    assert!(!forbidden.contains("GitHub"), "{forbidden}");
    assert!(!forbidden.contains("token"), "{forbidden}");

    let limited = linear_status_error(429, json!({"message": "slow down"})).await;
    assert!(limited.contains("Linear is rate limiting"), "{limited}");
    assert!(!limited.contains("GitHub"), "{limited}");
}

// ---------------------------------------------------------------------------
// Linear schema conformance
// ---------------------------------------------------------------------------

/// A GraphQL type reference, shared by the schema and the documents so the two
/// can be compared structurally.
#[derive(Clone, Debug, PartialEq)]
enum Ty {
    Named(String),
    List(Box<Ty>),
    NonNull(Box<Ty>),
}

impl Ty {
    fn from_gql(value: &gql::Type<'_, String>) -> Self {
        match value {
            gql::Type::NamedType(name) => Ty::Named(name.clone()),
            gql::Type::ListType(inner) => Ty::List(Box::new(Ty::from_gql(inner))),
            gql::Type::NonNullType(inner) => Ty::NonNull(Box::new(Ty::from_gql(inner))),
        }
    }

    /// The type with its top-level non-null wrapper removed.
    fn nullable(&self) -> &Ty {
        match self {
            Ty::NonNull(inner) => inner,
            other => other,
        }
    }

    /// The named type at the base of any list or non-null wrappers.
    fn base(&self) -> Option<&str> {
        match self {
            Ty::Named(name) => Some(name),
            Ty::List(inner) | Ty::NonNull(inner) => inner.base(),
        }
    }
}

/// Whether a variable declared as `variable` may be used where `expected` is
/// required. A non-null variable satisfies a nullable argument; the reverse
/// does not. Scalars never substitute for one another.
fn accepts(variable: &Ty, expected: &Ty) -> bool {
    match expected {
        Ty::NonNull(expected_inner) => match variable {
            Ty::NonNull(variable_inner) => accepts(variable_inner, expected_inner),
            _ => false,
        },
        _ => match variable {
            Ty::NonNull(variable_inner) => accepts(variable_inner, expected),
            Ty::List(variable_inner) => match expected {
                Ty::List(expected_inner) => accepts(variable_inner, expected_inner),
                _ => false,
            },
            Ty::Named(variable_name) => {
                matches!(expected, Ty::Named(expected_name) if variable_name == expected_name)
            }
        },
    }
}

#[derive(Clone, Debug)]
struct FieldShape {
    ty: Ty,
    arguments: BTreeMap<String, Ty>,
}

#[derive(Clone, Debug)]
struct InputShape {
    ty: Ty,
    required: bool,
}

/// The parts of the published schema a document check reads.
#[derive(Default)]
struct SchemaIndex {
    fields: BTreeMap<String, BTreeMap<String, FieldShape>>,
    inputs: BTreeMap<String, BTreeMap<String, InputShape>>,
    enums: BTreeMap<String, BTreeSet<String>>,
    scalars: BTreeSet<String>,
    types: BTreeSet<String>,
}

fn schema_index() -> &'static SchemaIndex {
    static INDEX: std::sync::OnceLock<SchemaIndex> = std::sync::OnceLock::new();
    INDEX.get_or_init(|| {
        let document = gql_schema::parse_schema::<String>(include_str!("linear_schema.graphql"))
            .expect("the vendored Linear schema parses");
        let mut index = SchemaIndex::default();
        for definition in document.definitions {
            match definition {
                gql_schema::Definition::TypeDefinition(gql_schema::TypeDefinition::Object(
                    object,
                )) => {
                    index.types.insert(object.name.clone());
                    let mut fields = BTreeMap::new();
                    for field in object.fields {
                        let arguments = field
                            .arguments
                            .into_iter()
                            .map(|argument| (argument.name, Ty::from_gql(&argument.value_type)))
                            .collect();
                        fields.insert(
                            field.name,
                            FieldShape {
                                ty: Ty::from_gql(&field.field_type),
                                arguments,
                            },
                        );
                    }
                    index.fields.insert(object.name, fields);
                }
                gql_schema::Definition::TypeDefinition(
                    gql_schema::TypeDefinition::InputObject(input),
                ) => {
                    index.types.insert(input.name.clone());
                    let fields = input
                        .fields
                        .into_iter()
                        .map(|field| {
                            let ty = Ty::from_gql(&field.value_type);
                            let required = matches!(ty, Ty::NonNull(_));
                            (field.name, InputShape { ty, required })
                        })
                        .collect();
                    index.inputs.insert(input.name, fields);
                }
                gql_schema::Definition::TypeDefinition(gql_schema::TypeDefinition::Enum(
                    enumeration,
                )) => {
                    index.types.insert(enumeration.name.clone());
                    index.enums.insert(
                        enumeration.name,
                        enumeration
                            .values
                            .into_iter()
                            .map(|value| value.name)
                            .collect(),
                    );
                }
                gql_schema::Definition::TypeDefinition(gql_schema::TypeDefinition::Scalar(
                    scalar,
                )) => {
                    index.types.insert(scalar.name.clone());
                    index.scalars.insert(scalar.name);
                }
                gql_schema::Definition::TypeDefinition(gql_schema::TypeDefinition::Interface(
                    interface,
                )) => {
                    index.types.insert(interface.name);
                }
                gql_schema::Definition::TypeDefinition(gql_schema::TypeDefinition::Union(
                    union,
                )) => {
                    index.types.insert(union.name);
                }
                _ => {}
            }
        }
        index
    })
}

fn collect_value_variables(value: &gql::Value<'_, String>, used: &mut BTreeSet<String>) {
    match value {
        gql::Value::Variable(name) => {
            used.insert(name.clone());
        }
        gql::Value::List(items) => {
            for item in items {
                collect_value_variables(item, used);
            }
        }
        gql::Value::Object(fields) => {
            for value in fields.values() {
                collect_value_variables(value, used);
            }
        }
        _ => {}
    }
}

fn collect_selection_variables(set: &gql::SelectionSet<'_, String>, used: &mut BTreeSet<String>) {
    for selection in &set.items {
        let gql::Selection::Field(field) = selection else {
            panic!("the daemon's documents use no fragments");
        };
        for (_, value) in &field.arguments {
            collect_value_variables(value, used);
        }
        collect_selection_variables(&field.selection_set, used);
    }
}

/// Checks one argument value against the type the schema declares for it.
///
/// This is the part field-name checks miss: an `ID` used where the schema
/// takes a `String`, a required input left out, or an enum value that does not
/// exist all fail here instead of at runtime.
fn check_value(
    schema: &SchemaIndex,
    expected: &Ty,
    value: &gql::Value<'_, String>,
    variables: &BTreeMap<String, Ty>,
    at: &str,
) {
    match value {
        gql::Value::Variable(name) => {
            let declared = variables
                .get(name)
                .unwrap_or_else(|| panic!("{at} uses the undeclared variable ${name}"));
            assert!(
                accepts(declared, expected),
                "{at} passes ${name} ({declared:?}) where the schema takes {expected:?}"
            );
        }
        gql::Value::Null => assert!(
            !matches!(expected, Ty::NonNull(_)),
            "{at} passes null to a required {expected:?}"
        ),
        gql::Value::Int(_) => assert!(
            matches!(
                expected.nullable(),
                Ty::Named(name) if matches!(name.as_str(), "Int" | "Float" | "ID" | "BigInt")
            ),
            "{at} passes an int where the schema takes {expected:?}"
        ),
        gql::Value::Float(_) => assert!(
            matches!(
                expected.nullable(),
                Ty::Named(name) if matches!(name.as_str(), "Float" | "ID")
            ),
            "{at} passes a float where the schema takes {expected:?}"
        ),
        gql::Value::String(_) => {
            let accepted = match expected.nullable() {
                Ty::Named(name) => {
                    name == "String" || name == "ID" || schema.scalars.contains(name)
                }
                _ => false,
            };
            assert!(
                accepted,
                "{at} passes a string where the schema takes {expected:?}"
            );
        }
        gql::Value::Boolean(_) => assert!(
            matches!(expected.nullable(), Ty::Named(name) if name == "Boolean"),
            "{at} passes a boolean where the schema takes {expected:?}"
        ),
        gql::Value::Enum(name) => {
            let Ty::Named(type_name) = expected.nullable() else {
                panic!("{at} passes an enum where the schema takes {expected:?}");
            };
            let values = schema
                .enums
                .get(type_name)
                .unwrap_or_else(|| panic!("{at}: {type_name} is not an enum"));
            assert!(
                values.contains(name),
                "{at} passes {name}, which {type_name} does not define"
            );
        }
        gql::Value::List(items) => {
            let Ty::List(inner) = expected.nullable() else {
                panic!("{at} passes a list where the schema takes {expected:?}");
            };
            for item in items {
                check_value(schema, inner, item, variables, at);
            }
        }
        gql::Value::Object(fields) => {
            let Ty::Named(type_name) = expected.nullable() else {
                panic!("{at} passes an object where the schema takes {expected:?}");
            };
            let input = schema
                .inputs
                .get(type_name)
                .unwrap_or_else(|| panic!("{at}: {type_name} is not an input object"));
            for (key, value) in fields {
                let field = input
                    .get(key)
                    .unwrap_or_else(|| panic!("{at}: {type_name} has no {key} field"));
                check_value(schema, &field.ty, value, variables, at);
            }
            for (key, field) in input {
                assert!(
                    !field.required || fields.contains_key(key),
                    "{at}: required input {type_name}.{key} is missing"
                );
            }
        }
    }
}

fn check_selections(
    schema: &SchemaIndex,
    parent: &str,
    set: &gql::SelectionSet<'_, String>,
    variables: &BTreeMap<String, Ty>,
) {
    for selection in &set.items {
        let gql::Selection::Field(field) = selection else {
            panic!("the daemon's documents use no fragments");
        };
        let fields = schema
            .fields
            .get(parent)
            .unwrap_or_else(|| panic!("{parent} is not an object in the vendored schema"));
        let shape = fields
            .get(&field.name)
            .unwrap_or_else(|| panic!("Linear's schema has no {parent}.{} field", field.name));
        for (name, value) in &field.arguments {
            let expected = shape.arguments.get(name).unwrap_or_else(|| {
                panic!(
                    "Linear's schema has no {parent}.{}({name}:) argument",
                    field.name
                )
            });
            check_value(
                schema,
                expected,
                value,
                variables,
                &format!("{parent}.{}.{name}", field.name),
            );
        }
        if !field.selection_set.items.is_empty() {
            let child = shape
                .ty
                .base()
                .unwrap_or_else(|| panic!("{parent}.{} is not an object", field.name));
            check_selections(schema, child, &field.selection_set, variables);
        }
    }
}

fn check_document(document: &str) -> BTreeSet<String> {
    let schema = schema_index();
    let parsed = gql::parse_query::<String>(document).expect("the document parses");
    let gql::Definition::Operation(operation) = &parsed.definitions[0] else {
        panic!("expected one operation");
    };
    let (root, definitions, selection_set) = match operation {
        gql::OperationDefinition::Query(query) => {
            ("Query", &query.variable_definitions, &query.selection_set)
        }
        gql::OperationDefinition::Mutation(mutation) => (
            "Mutation",
            &mutation.variable_definitions,
            &mutation.selection_set,
        ),
        _ => panic!("expected a query or mutation"),
    };
    let variables = definitions
        .iter()
        .map(|definition| {
            let ty = Ty::from_gql(&definition.var_type);
            let base = ty
                .base()
                .unwrap_or_else(|| panic!("${} has no named type", definition.name));
            let built_in = matches!(base, "ID" | "String" | "Int" | "Float" | "Boolean");
            assert!(
                built_in || schema.types.contains(base),
                "${} is declared as {ty:?}, which the schema does not define",
                definition.name
            );
            (definition.name.clone(), ty)
        })
        .collect::<BTreeMap<_, _>>();
    let declared = variables.keys().cloned().collect::<BTreeSet<_>>();
    let mut used = BTreeSet::new();
    collect_selection_variables(selection_set, &mut used);
    assert_eq!(
        declared, used,
        "every declared variable is used, and every used variable is declared"
    );
    check_selections(schema, root, selection_set, &variables);
    declared
}

#[test]
fn every_linear_document_matches_the_published_schema() {
    let documents = [
        h4rn_forge::issues::LINEAR_VIEWER_QUERY,
        h4rn_forge::issues::LINEAR_TEAMS_QUERY,
        h4rn_forge::issues::LINEAR_LIST_QUERY,
        h4rn_forge::issues::LINEAR_SEARCH_QUERY,
        h4rn_forge::issues::LINEAR_DETAIL_QUERY,
        h4rn_forge::issues::LINEAR_ISSUE_TEAM_QUERY,
        h4rn_forge::issues::LINEAR_COMMENT_MUTATION,
    ];
    for document in documents {
        check_document(document);
    }
    // The list document's team variable is an ID inside the team filter; the
    // search document passes the team to `searchIssues`, whose argument is a
    // String.
    let list = check_document(h4rn_forge::issues::LINEAR_LIST_QUERY);
    assert!(list.contains("teamId"), "{list:?}");
    let search = check_document(h4rn_forge::issues::LINEAR_SEARCH_QUERY);
    assert!(search.contains("filter"), "{search:?}");
}
