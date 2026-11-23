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
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param};
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

    let comments = client.list_issue_comments(&repo, 12).await.unwrap();
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

    let comments = client.list_issue_comments(&repo, 5).await.unwrap();
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

    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/issues"))
        .and(query_param("type", "issues"))
        .and(query_param("assigned", "true"))
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

    Mock::given(method("PATCH"))
        .and(path("/api/v1/repos/acme/widget/issues/4"))
        .and(body_json(json!({
            "title": "Export CSV and JSON",
            "state": "closed",
            "labels": [2],
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

    let comments = client.list_issue_comments(&repo, 4).await.unwrap();
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

// ---------------------------------------------------------------------------
// Bitbucket Cloud
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bitbucket_lists_reads_and_writes_issues_when_the_tracker_is_on() {
    let server = MockServer::start().await;
    let base = server.uri();

    Mock::given(method("GET"))
        .and(path("/2.0/repositories/acme/widget"))
        .respond_with(ok(json!({"has_issues": true})))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/2.0/repositories/acme/widget/issues"))
        .respond_with(ok(json!({
            "values": [{
                "id": 6,
                "title": "Login loop",
                "status": "open",
                "content": {"raw": "Signing in loops."},
                "reporter": {"nickname": "alice", "display_name": "Alice"},
                "assignee": {"nickname": "bob", "display_name": "Bob"},
                "comment_count": 1,
                "created_on": "2026-04-01T00:00:00Z",
                "updated_on": "2026-04-02T00:00:00Z",
                "links": {"html": {"href": "https://bitbucket.org/acme/widget/issues/6"}}
            }],
            "next": null
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/2.0/repositories/acme/widget/issues/6"))
        .respond_with(ok(json!({
            "id": 6,
            "title": "Login loop",
            "status": "open",
            "content": {"raw": "Signing in loops."},
            "reporter": {"nickname": "alice", "display_name": "Alice"},
            "assignee": {"nickname": "bob", "display_name": "Bob"},
            "created_on": "2026-04-01T00:00:00Z",
            "updated_on": "2026-04-02T00:00:00Z"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/2.0/repositories/acme/widget/issues/6/comments"))
        .respond_with(ok(json!({
            "values": [{
                "id": 11,
                "content": {"raw": "Also on Safari."},
                "user": {"nickname": "carol", "display_name": "Carol"},
                "created_on": "2026-04-03T00:00:00Z",
                "updated_on": "2026-04-03T00:00:00Z"
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/2.0/repositories/acme/widget/issues/6/comments"))
        .and(body_json(json!({"content": {"raw": "Fixed."}})))
        .respond_with(ok(json!({
            "id": 12,
            "content": {"raw": "Fixed."},
            "user": {"nickname": "alice"},
            "created_on": "2026-04-04T00:00:00Z"
        })))
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path("/2.0/repositories/acme/widget/issues/6"))
        .and(body_json(json!({
            "title": "Login loop",
            "content": {"raw": "Signing in loops."},
            "assignee": {"nickname": "bob"},
            "status": "resolved"
        })))
        .respond_with(ok(json!({
            "id": 6,
            "title": "Login loop",
            "status": "resolved",
            "content": {"raw": "Signing in loops."},
            "reporter": {"nickname": "alice"},
            "assignee": {"nickname": "bob"},
            "created_on": "2026-04-01T00:00:00Z",
            "updated_on": "2026-04-05T00:00:00Z"
        })))
        .mount(&server)
        .await;

    let client = BitbucketClient::new(&base, &token(), Some("alice@example.test"), 600).unwrap();
    let repo = repo(ForgeKind::Bitbucket, &base);

    let page = client
        .list_issues(&repo, ForgeIssueFilter::Open, 1, None)
        .await
        .unwrap();
    assert_eq!(page.issues[0].number, 6);
    assert_eq!(page.issues[0].assignees[0].login, "bob");

    let detail = client.get_issue(&repo, 6).await.unwrap();
    assert_eq!(detail.body, "Signing in loops.");

    let comments = client.list_issue_comments(&repo, 6).await.unwrap();
    assert_eq!(comments.comments[0].body, "Also on Safari.");

    let posted = client.comment_issue(&repo, 6, "Fixed.").await.unwrap();
    assert_eq!(posted.comment_id, "12");

    let updated = client
        .update_issue(
            &repo,
            6,
            &ForgeIssueUpdate {
                state: Some(ForgeIssueState::Closed),
                close_reason: Some(ForgeIssueCloseReason::Completed),
                assignees: Some(vec!["bob".into()]),
                ..ForgeIssueUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.summary.state, ForgeIssueState::Closed);
    assert_eq!(updated.close_reason, Some(ForgeIssueCloseReason::Completed));
}

#[tokio::test]
async fn bitbucket_says_so_when_the_tracker_is_off() {
    let server = MockServer::start().await;
    let base = server.uri();
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/acme/widget"))
        .respond_with(ok(json!({"has_issues": false})))
        .mount(&server)
        .await;
    let client = BitbucketClient::new(&base, &token(), None, 600).unwrap();
    let error = client
        .list_issues(
            &repo(ForgeKind::Bitbucket, &base),
            ForgeIssueFilter::Open,
            1,
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("tracker is disabled"), "{error}");
}

// ---------------------------------------------------------------------------
// Azure DevOps
// ---------------------------------------------------------------------------

#[tokio::test]
async fn azure_lists_reads_and_writes_work_items_as_issues() {
    let server = MockServer::start().await;
    let base = format!("{}/acme", server.uri());

    Mock::given(method("POST"))
        .and(path("/acme/platform/_apis/wit/wiql"))
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
                    "System.State": "Closed",
                    "System.CreatedBy": {"uniqueName": "carol@example.test"},
                    "System.CreatedDate": "2026-05-01T00:00:00Z",
                    "System.ChangedDate": "2026-05-03T00:00:00Z"
                }
            }]
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
                "System.CreatedBy": {"uniqueName": "alice@example.test"},
                "System.CreatedDate": "2026-05-01T00:00:00Z",
                "System.ChangedDate": "2026-05-02T00:00:00Z",
                "System.Description": "Saving crashes."
            },
            "relations": [{
                "rel": "ArtifactLink",
                "url": "https://dev.azure.com/acme/platform/_git/widget/pullrequest/9",
                "attributes": {"name": "Pull request 9"}
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/wit/workItems/21/comments"))
        .respond_with(ok(json!({
            "comments": [{
                "id": 4,
                "text": "Reproduced on Linux.",
                "createdBy": {"uniqueName": "carol@example.test"},
                "createdDate": "2026-05-04T00:00:00Z",
                "modifiedDate": "2026-05-04T00:00:00Z"
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/acme/platform/_apis/wit/workItems/21/comments"))
        .and(body_json(json!({"text": "Fix in progress."})))
        .respond_with(ok(json!({
            "id": 5,
            "text": "Fix in progress.",
            "createdBy": {"uniqueName": "alice@example.test"},
            "createdDate": "2026-05-05T00:00:00Z"
        })))
        .mount(&server)
        .await;

    Mock::given(method("PATCH"))
        .and(path("/acme/platform/_apis/wit/workitems/21"))
        .and(header("content-type", "application/json-patch+json"))
        .and(body_json(json!([
            {"op": "add", "path": "/fields/System.Title", "value": "Crash when saving"},
            {"op": "add", "path": "/fields/System.State", "value": "Closed"}
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

    let comments = client.list_issue_comments(&repo, 21).await.unwrap();
    assert_eq!(comments.comments[0].body, "Reproduced on Linux.");

    let posted = client
        .comment_issue(&repo, 21, "Fix in progress.")
        .await
        .unwrap();
    assert_eq!(posted.comment_id, "5");

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
        "assignees": {"nodes": [{"displayName": "Alice"}]},
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
            "query": "query { viewer { name email organization { name } } }",
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
            "query": "query { teams(first: 100) { nodes { id key name } } }",
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
                "filter": {"team": {"id": {"eq": "team_1"}}, "state": {"type": {"in": ["backlog", "unstarted", "started"]}}},
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
            "variables": {"id": "iss_1"}
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
                "assignees": {"nodes": [{"displayName": "Alice"}]},
                "labels": {"nodes": [{"name": "backend"}]},
                "comments": {"nodes": [{
                    "id": "c1",
                    "body": "Working on it.",
                    "createdAt": "2026-06-03T00:00:00Z",
                    "updatedAt": null,
                    "user": {"displayName": "Bob"}
                }]},
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

    let detail = client.get_issue("iss_1").await.unwrap();
    assert_eq!(detail.comments[0].author, "Bob");
    assert_eq!(detail.linked_pull_requests.len(), 1);

    let posted = client.create_comment("iss_1", "Shipped.").await.unwrap();
    assert_eq!(posted.id, "c2");
}
