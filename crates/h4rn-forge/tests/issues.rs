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
use std::collections::BTreeSet;

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

    Mock::given(method("PATCH"))
        .and(path("/api/v3/repos/acme/widget/issues/12"))
        .and(body_json(json!({
            "state": "closed",
            "state_reason": "duplicate",
            "duplicate_issue_id": 9
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

type FieldIndex = std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>;

fn base_type_name(field_type: &gql_schema::Type<'_, String>) -> String {
    match field_type {
        gql_schema::Type::NamedType(name) => name.clone(),
        gql_schema::Type::ListType(inner) | gql_schema::Type::NonNullType(inner) => {
            base_type_name(inner)
        }
    }
}

fn schema_index() -> FieldIndex {
    let document = gql_schema::parse_schema::<String>(include_str!("linear_schema.graphql"))
        .expect("the vendored Linear schema parses");
    let mut objects = FieldIndex::new();
    for definition in document.definitions {
        let gql_schema::Definition::TypeDefinition(gql_schema::TypeDefinition::Object(object)) =
            definition
        else {
            continue;
        };
        let mut fields = std::collections::BTreeMap::new();
        for field in object.fields {
            fields.insert(field.name, base_type_name(&field.field_type));
        }
        objects.insert(object.name, fields);
    }
    objects
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

fn check_selections(index: &FieldIndex, parent: &str, set: &gql::SelectionSet<'_, String>) {
    for selection in &set.items {
        let gql::Selection::Field(field) = selection else {
            panic!("the daemon's documents use no fragments");
        };
        let fields = index
            .get(parent)
            .unwrap_or_else(|| panic!("{parent} is not an object in the vendored schema"));
        let child = fields
            .get(&field.name)
            .unwrap_or_else(|| panic!("Linear's schema has no {parent}.{} field", field.name));
        if !field.selection_set.items.is_empty() {
            check_selections(index, child, &field.selection_set);
        }
    }
}

fn check_document(document: &str) -> BTreeSet<String> {
    let index = schema_index();
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
    let declared = definitions
        .iter()
        .map(|definition| definition.name.clone())
        .collect::<BTreeSet<_>>();
    let mut used = BTreeSet::new();
    collect_selection_variables(selection_set, &mut used);
    assert_eq!(
        declared, used,
        "every declared variable is used, and every used variable is declared"
    );
    check_selections(&index, root, selection_set);
    declared
}

#[test]
fn every_linear_document_matches_the_published_schema() {
    let documents = [
        h4rn_forge::issues::LINEAR_LIST_QUERY,
        h4rn_forge::issues::LINEAR_SEARCH_QUERY,
        h4rn_forge::issues::LINEAR_DETAIL_QUERY,
        h4rn_forge::issues::LINEAR_COMMENT_MUTATION,
    ];
    for document in documents {
        check_document(document);
    }
    // The list document's team variable is used, not declared and ignored.
    let list = check_document(h4rn_forge::issues::LINEAR_LIST_QUERY);
    assert!(list.contains("teamId"), "{list:?}");
    let search = check_document(h4rn_forge::issues::LINEAR_SEARCH_QUERY);
    assert!(search.contains("filter"), "{search:?}");
}
