//! Bitbucket Cloud and Azure DevOps against recorded API responses.
//!
//! Every fixture is a trimmed response with the provider's own field names,
//! taken from the documented Bitbucket Cloud REST 2.0 and Azure DevOps REST
//! 7.1 schemas; no test reaches the network beyond the local wiremock server.

use base64::Engine as _;
use chrono::{DateTime, Utc};
use h4rn_forge::{
    AzureDevOpsClient, BitbucketClient, DEFAULT_REQUESTS_PER_MINUTE, Forge, ForgeError,
};
use h4rn_protocol::{
    ForgeChecksState, ForgeDiffSide, ForgeFileStatus, ForgeKind, ForgeMergeable, ForgePrFilter,
    ForgePrState, ForgeRepoRef, ForgeReviewState,
};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BB_PR: &str = include_str!("fixtures/bitbucket_pr_42.json");
const BB_LIST: &str = include_str!("fixtures/bitbucket_pr_list.json");
const BB_COMMITS: &str = include_str!("fixtures/bitbucket_commits.json");
const BB_COMMENTS: &str = include_str!("fixtures/bitbucket_comments.json");
const BB_COMMENTS_OUTDATED: &str = include_str!("fixtures/bitbucket_comments_outdated.json");
const BB_DIFFSTAT: &str = include_str!("fixtures/bitbucket_diffstat.json");
const BB_STATUSES: &str = include_str!("fixtures/bitbucket_statuses.json");
const BB_DIFF: &str = include_str!("fixtures/bitbucket_diff.txt");

const AZ_PR: &str = include_str!("fixtures/azure_pr_7.json");
const AZ_LIST: &str = include_str!("fixtures/azure_pr_list.json");
const AZ_ITERATIONS: &str = include_str!("fixtures/azure_iterations.json");
const AZ_CHANGES: &str = include_str!("fixtures/azure_changes.json");
const AZ_THREADS: &str = include_str!("fixtures/azure_threads.json");
const AZ_STATUSES: &str = include_str!("fixtures/azure_statuses.json");
const AZ_POLICIES: &str = include_str!("fixtures/azure_policies.json");
const AZ_COMMITS: &str = include_str!("fixtures/azure_commits.json");
const AZ_CONNECTION: &str = include_str!("fixtures/azure_connection.json");
const AZ_STATUSES_ITERATIONS: &str = include_str!("fixtures/azure_statuses_iterations.json");
const AZ_POLICIES_SAME_NAME: &str = include_str!("fixtures/azure_policies_same_name.json");

const AZ_BASE: &str = "bbbb1111cccc2222dddd3333eeee4444ffff5555";
const AZ_HEAD: &str = "aaaa1111bbbb2222cccc3333dddd4444eeee5555";

fn fixture(raw: &str) -> Value {
    serde_json::from_str(raw).expect("fixture is JSON")
}

fn ok(raw: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(fixture(raw))
}

fn text_body(raw: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/plain; charset=utf-8")
        .set_body_string(raw)
}

fn bitbucket_repo(base_url: &str) -> ForgeRepoRef {
    ForgeRepoRef {
        forge: ForgeKind::Bitbucket,
        base_url: base_url.to_owned(),
        owner: "acme".into(),
        repo: "widget".into(),
    }
}

fn azure_repo(base_url: &str) -> ForgeRepoRef {
    ForgeRepoRef {
        forge: ForgeKind::AzureDevOps,
        base_url: format!("{base_url}/acme"),
        owner: "platform".into(),
        repo: "widget".into(),
    }
}

async fn bitbucket_server() -> MockServer {
    let server = MockServer::start().await;
    let api = "/2.0/repositories/acme/widget";
    Mock::given(method("GET"))
        .and(path("/2.0/user"))
        // Bitbucket does not report scopes; this GitHub-style header must not
        // be read as one.
        .respond_with(
            ok(r#"{"type":"user","display_name":"Alice Example","nickname":"alice","uuid":"{alice-uuid}","account_id":"557058:alice"}"#)
                .insert_header("x-oauth-scopes", "account, repository"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{api}/pullrequests")))
        .and(query_param("state", "OPEN"))
        .respond_with(ok(BB_LIST))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{api}/pullrequests/42")))
        .respond_with(ok(BB_PR))
        .mount(&server)
        .await;
    // The documented diffstat route redirects to the repository diffstat too.
    Mock::given(method("GET"))
        .and(path(format!("{api}/pullrequests/42/diffstat")))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "location",
            format!("{}{api}/diffstat/aaaa1111..bbbb1111", server.uri()),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{api}/diffstat/aaaa1111..bbbb1111")))
        .respond_with(ok(BB_DIFFSTAT))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{api}/pullrequests/42/commits")))
        .respond_with(ok(BB_COMMITS))
        .mount(&server)
        .await;
    // The documented route redirects to the repository diff.
    Mock::given(method("GET"))
        .and(path(format!("{api}/pullrequests/42/diff")))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "location",
            format!("{}{api}/diff/aaaa1111..bbbb1111", server.uri()),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{api}/diff/aaaa1111..bbbb1111")))
        .respond_with(text_body(BB_DIFF))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{api}/pullrequests/42/comments")))
        .respond_with(ok(BB_COMMENTS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{api}/commit/aaaa1111bbbb2222cccc3333dddd4444eeee5555/statuses"
        )))
        .respond_with(ok(BB_STATUSES))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn bitbucket_reads_pull_requests_threads_and_checks() {
    let server = bitbucket_server().await;
    let client = BitbucketClient::new(&server.uri(), "ATBB-token", None, 120).unwrap();
    let repo = bitbucket_repo(&server.uri());

    let viewer = client.viewer().await.unwrap();
    assert_eq!(viewer.login, "alice");
    assert_eq!(viewer.display_name.as_deref(), Some("Alice Example"));
    assert!(
        viewer.scopes.is_empty(),
        "Bitbucket reports no scopes; an x-oauth-scopes header is not one"
    );

    let page = client
        .list_prs(&repo, &ForgePrFilter::Open, 1)
        .await
        .unwrap();
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request
                .url
                .query_pairs()
                .any(|(key, value)| key == "state" && value == "OPEN")),
        "Bitbucket lists open PRs with state=OPEN"
    );
    assert_eq!(page.prs.len(), 2);
    assert_eq!(page.prs[0].state, ForgePrState::Open);
    assert_eq!(page.prs[1].state, ForgePrState::Draft);
    assert_eq!(page.prs[0].head_branch, "alice/fix");
    assert!(!page.has_more);

    let pr = client.get_pr(&repo, 42).await.unwrap();
    assert_eq!(pr.body, "Fixes the widget and adds a regression test.");
    assert_eq!(pr.additions, 5);
    assert_eq!(pr.deletions, 1);
    assert_eq!(pr.changed_files, 2);
    assert!(pr.changed_files_known);
    assert!(pr.line_counts_known, "the complete diffstat reports totals");
    assert_eq!(pr.commits, 2);
    assert_eq!(pr.reviewers.len(), 2);
    assert_eq!(pr.summary.review, ForgeReviewState::ChangesRequested);

    let diff = client.get_diff(&repo, 42, 1, 30).await.unwrap();
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "src/lib.rs");
    assert_eq!(diff.files[0].additions, 2);
    assert_eq!(diff.files[0].deletions, 1);

    let comments = client.list_comments(&repo, 42).await.unwrap();
    assert_eq!(comments.comments.len(), 1);
    assert_eq!(comments.comments[0].author, "carol");
    assert_eq!(comments.threads.len(), 1);
    let thread = &comments.threads[0];
    assert_eq!(thread.thread_id, "9100");
    assert_eq!(thread.path, "src/lib.rs");
    assert_eq!(thread.line, Some(12));
    assert_eq!(thread.side, ForgeDiffSide::Right);
    assert!(thread.resolved, "the root comment reports a resolution");
    assert_eq!(thread.comments.len(), 2);
    assert_eq!(thread.comments[1].in_reply_to.as_deref(), Some("9100"));
    assert_eq!(thread.comments[1].body, "Good catch, fixed.");

    let checks = client.list_checks(&repo, 42).await.unwrap();
    assert_eq!(checks.total, 2);
    assert_eq!(checks.passed, 1);
    assert_eq!(checks.failed, 1);
    assert_eq!(checks.state, ForgeChecksState::Failing);
    assert!(!checks.rerun.supported);
    assert!(
        checks
            .rerun
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("Bitbucket")),
        "{:?}",
        checks.rerun
    );
    let error = client.rerun_failed_checks(&repo, 42).await.unwrap_err();
    assert!(matches!(error, ForgeError::Unsupported(_)), "{error:?}");

    let commits = client.list_commits(&repo, 42).await.unwrap();
    assert_eq!(commits.commits.len(), 2);
    assert_eq!(commits.commits[0].title, "Fix the panic");
    assert_eq!(
        commits.commits[0].body.as_deref(),
        Some("Reproduces the panic on empty input.")
    );
    assert_eq!(commits.commits[0].author_login.as_deref(), Some("alice"));
}

#[tokio::test]
async fn bitbucket_sends_basic_or_bearer_auth() {
    let basic = MockServer::start().await;
    let expected = base64::engine::general_purpose::STANDARD.encode("alice@example.test:s3cret");
    Mock::given(method("GET"))
        .and(path("/2.0/user"))
        .and(header("authorization", format!("Basic {expected}")))
        .respond_with(ok(
            r#"{"type":"user","display_name":"Alice Example","nickname":"alice","uuid":"{alice-uuid}"}"#,
        ))
        .expect(1)
        .mount(&basic)
        .await;
    let client =
        BitbucketClient::new(&basic.uri(), "s3cret", Some("alice@example.test"), 120).unwrap();
    assert_eq!(client.viewer().await.unwrap().login, "alice");

    let bearer = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2.0/user"))
        .and(header("authorization", "Bearer ATBB-workspace"))
        .respond_with(ok(
            r#"{"type":"user","display_name":"Alice Example","nickname":"alice","uuid":"{alice-uuid}"}"#,
        ))
        .expect(1)
        .mount(&bearer)
        .await;
    let client = BitbucketClient::new(&bearer.uri(), "ATBB-workspace", None, 120).unwrap();
    assert_eq!(client.viewer().await.unwrap().login, "alice");
}

#[tokio::test]
async fn bitbucket_validates_tokens_that_cannot_read_user() {
    // A workspace access token gets a 403 from /user; /user/workspaces names
    // the workspace, whose slug becomes the account login.
    let workspace = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2.0/user"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "type": "error",
            "error": {"message": "Your credentials lack the account scope"}
        })))
        .mount(&workspace)
        .await;
    Mock::given(method("GET"))
        .and(path("/2.0/user/workspaces"))
        .respond_with(ok(
            r#"{"pagelen":1,"values":[{"type":"workspace_access","administrator":true,"workspace":{"type":"workspace","slug":"acme","name":"Acme","uuid":"{workspace-uuid}"}}]}"#,
        ))
        .mount(&workspace)
        .await;
    let client = BitbucketClient::new(&workspace.uri(), "ATBB-workspace", None, 120).unwrap();
    let viewer = client.viewer().await.unwrap();
    assert_eq!(viewer.login, "acme");
    assert_eq!(viewer.display_name.as_deref(), Some("Acme"));
    // The login is a workspace, not a person, so the personal filters say so.
    let repo = bitbucket_repo(&workspace.uri());
    let error = client
        .list_prs(&repo, &ForgePrFilter::Mine, 1)
        .await
        .unwrap_err();
    assert!(matches!(error, ForgeError::Unsupported(_)), "{error:?}");

    // A repository access token can read neither account endpoint. A
    // repository request proves it, and the workspace slug is used for Mine.
    let repository = MockServer::start().await;
    for endpoint in ["/2.0/user", "/2.0/user/workspaces"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "type": "error",
                "error": {"message": "Unauthorized"}
            })))
            .mount(&repository)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/acme/widget"))
        .respond_with(ok(
            r#"{"type":"repository","name":"widget","full_name":"acme/widget","uuid":"{widget-uuid}","is_private":true}"#,
        ))
        .mount(&repository)
        .await;
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/acme/widget/pullrequests"))
        .respond_with(ok(
            r#"{"size":1,"page":1,"pagelen":1,"values":[{"type":"pullrequest","id":42,"title":"Fix the widget","state":"OPEN","draft":false,"author":{"type":"user","display_name":"Alice Example","nickname":"alice","uuid":"{alice-uuid}"},"source":{"branch":{"name":"alice/fix"},"commit":{"hash":"aaaa1111"}},"destination":{"branch":{"name":"main"},"commit":{"hash":"bbbb1111"}},"created_on":"2024-05-01T09:00:00+00:00","updated_on":"2024-05-01T12:00:00+00:00","links":{"html":{"href":"https://bitbucket.org/acme/widget/pull-requests/42"}}}]}"#,
        ))
        .mount(&repository)
        .await;
    let repo = bitbucket_repo(&repository.uri());
    let client = BitbucketClient::new(&repository.uri(), "ATBB-repo", None, 120).unwrap();
    assert_eq!(client.viewer_for(Some(&repo)).await.unwrap().login, "acme");
    // The workspace slug is not a person: a personal filter says so instead of
    // returning an empty list that looks like "no pull requests".
    for filter in [ForgePrFilter::Mine, ForgePrFilter::ReviewRequested] {
        let error = client.list_prs(&repo, &filter, 1).await.unwrap_err();
        assert!(matches!(error, ForgeError::Unsupported(_)), "{error:?}");
        assert!(
            error.to_string().contains("access token"),
            "the reason names the credential: {error}"
        );
    }
}

#[tokio::test]
async fn bitbucket_follows_same_origin_next_but_not_a_cross_origin_one() {
    let same = MockServer::start().await;
    let api = "/2.0/repositories/acme/widget/pullrequests/42/commits";
    let commits = fixture(BB_COMMITS);
    let commits = commits["values"].as_array().unwrap();
    Mock::given(method("GET"))
        .and(path(api))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "pagelen": 1,
            "values": [commits[0].clone()],
            "next": format!("{}{api}?page=2", same.uri())
        })))
        .mount(&same)
        .await;
    Mock::given(method("GET"))
        .and(path(api))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "pagelen": 1,
            "values": [commits[1].clone()]
        })))
        .mount(&same)
        .await;
    let client = BitbucketClient::new(&same.uri(), "ATBB-token", None, 120).unwrap();
    let repo = bitbucket_repo(&same.uri());
    let listed = client.list_commits(&repo, 42).await.unwrap();
    assert_eq!(listed.commits.len(), 2);

    let first = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(api))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "pagelen": 1,
            "values": [commits[0].clone()],
            "next": format!("{}{api}?page=2", other.uri())
        })))
        .mount(&first)
        .await;
    let client = BitbucketClient::new(&first.uri(), "ATBB-token", None, 120).unwrap();
    let repo = bitbucket_repo(&first.uri());
    let listed = client.list_commits(&repo, 42).await.unwrap();
    assert_eq!(
        listed.commits.len(),
        1,
        "pagination stops at another origin"
    );
    assert!(
        other.received_requests().await.unwrap().is_empty(),
        "a cross-origin next link must not receive the token"
    );
}

fn bitbucket_pr(id: u32, author: &str) -> Value {
    json!({
        "type": "pullrequest",
        "id": id,
        "title": format!("Fix {id}"),
        "state": "OPEN",
        "draft": false,
        "author": {"type": "user", "display_name": "Alice Example", "nickname": author, "uuid": "{alice-uuid}"},
        "source": {"branch": {"name": "alice/fix"}, "commit": {"hash": "aaaa1111"}},
        "destination": {"branch": {"name": "main"}, "commit": {"hash": "bbbb1111"}},
        "created_on": "2024-05-01T09:00:00+00:00",
        "updated_on": "2024-05-01T12:00:00+00:00",
        "links": {"html": {"href": format!("https://bitbucket.org/acme/widget/pull-requests/{id}")}}
    })
}

#[tokio::test]
async fn bitbucket_filtered_lists_use_the_documented_q_filter() {
    let server = MockServer::start().await;
    let api = "/2.0/repositories/acme/widget/pullrequests";
    Mock::given(method("GET"))
        .and(path("/2.0/user"))
        .respond_with(ok(
            r#"{"type":"user","display_name":"Alice Example","nickname":"alice","uuid":"{alice-uuid}"}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(api))
        .respond_with(ok(r#"{"pagelen":2,"values":[],"page":1}"#))
        .mount(&server)
        .await;
    let client = BitbucketClient::new(&server.uri(), "ATBB-token", None, 120).unwrap();
    let repo = bitbucket_repo(&server.uri());

    for (filter, expected) in [
        (
            ForgePrFilter::Mine,
            "author.uuid = \"{alice-uuid}\"".to_owned(),
        ),
        (
            ForgePrFilter::ReviewRequested,
            "reviewers.uuid = \"{alice-uuid}\"".to_owned(),
        ),
        (
            ForgePrFilter::ByBranch {
                branch: "alice/fix".into(),
            },
            "source.branch.name = \"alice/fix\"".to_owned(),
        ),
    ] {
        let _ = client.list_prs(&repo, &filter, 3).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let last = requests
            .iter()
            .rev()
            .find(|request| request.url.path() == api)
            .expect("a PR list request");
        let query: std::collections::HashMap<_, _> = last
            .url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(query.get("q").map(String::as_str), Some(expected.as_str()));
        assert_eq!(query.get("page").map(String::as_str), Some("3"));
        if matches!(filter, ForgePrFilter::ReviewRequested) {
            assert_eq!(
                query.get("fields").map(String::as_str),
                Some("+values.reviewers"),
                "reviewer rows ask for the documented partial-response addition"
            );
        }
    }
}

#[tokio::test]
async fn bitbucket_personal_filters_match_the_account_uuid() {
    // The viewer has no nickname, so its login falls back to the display name.
    // Two accounts can share that name; only the uuid identifies the viewer.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2.0/user"))
        .respond_with(ok(
            r#"{"type":"user","display_name":"Alex Kim","uuid":"{viewer-uuid}"}"#,
        ))
        .mount(&server)
        .await;
    let api = "/2.0/repositories/acme/widget/pullrequests";
    let viewer_pr = json!({
        "type": "pullrequest",
        "id": 1,
        "title": "Mine",
        "state": "OPEN",
        "author": {"type": "user", "display_name": "Alex Kim", "uuid": "{viewer-uuid}"},
        "source": {"branch": {"name": "alex/fix"}, "commit": {"hash": "aaaa1111"}},
        "destination": {"branch": {"name": "main"}, "commit": {"hash": "bbbb1111"}},
        "created_on": "2024-05-01T09:00:00+00:00",
        "updated_on": "2024-05-01T12:00:00+00:00"
    });
    let namesake_pr = json!({
        "type": "pullrequest",
        "id": 2,
        "title": "Someone else's",
        "state": "OPEN",
        "author": {"type": "user", "display_name": "Alex Kim", "uuid": "{other-uuid}"},
        "source": {"branch": {"name": "other/fix"}, "commit": {"hash": "cccc1111"}},
        "destination": {"branch": {"name": "main"}, "commit": {"hash": "bbbb1111"}},
        "created_on": "2024-05-01T09:00:00+00:00",
        "updated_on": "2024-05-01T12:00:00+00:00"
    });
    for (q, page) in [
        (
            "author.uuid = \"{viewer-uuid}\"",
            json!({"pagelen": 1, "page": 1, "values": [viewer_pr.clone()]}),
        ),
        (
            "reviewers.uuid = \"{viewer-uuid}\"",
            json!({"pagelen": 1, "page": 1, "values": [viewer_pr.clone()]}),
        ),
        // A display-name match would return both people; mount it so a
        // regression reads as two rows instead of a 404.
        (
            "author.nickname = \"Alex Kim\" OR author.display_name = \"Alex Kim\"",
            json!({"pagelen": 2, "page": 1, "values": [viewer_pr.clone(), namesake_pr]}),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(api))
            .and(query_param("q", q))
            .respond_with(ResponseTemplate::new(200).set_body_json(page))
            .mount(&server)
            .await;
    }

    let client = BitbucketClient::new(&server.uri(), "ATBB-token", None, 120).unwrap();
    let repo = bitbucket_repo(&server.uri());
    for filter in [ForgePrFilter::Mine, ForgePrFilter::ReviewRequested] {
        let page = client.list_prs(&repo, &filter, 1).await.unwrap();
        assert_eq!(
            page.prs.len(),
            1,
            "{filter:?} matched a namesake instead of the uuid"
        );
        assert_eq!(page.prs[0].number, 1);
    }
    let requests = server.received_requests().await.unwrap();
    let filter_queries: Vec<String> = requests
        .iter()
        .filter(|request| request.url.path() == api)
        .filter_map(|request| {
            request
                .url
                .query_pairs()
                .find(|(key, _)| key == "q")
                .map(|(_, value)| value.into_owned())
        })
        .collect();
    assert_eq!(
        filter_queries,
        [
            "author.uuid = \"{viewer-uuid}\"",
            "reviewers.uuid = \"{viewer-uuid}\""
        ],
        "the filters never fall back to the display name"
    );
}

#[tokio::test]
async fn bitbucket_falls_back_to_the_page_parameter_for_a_distant_page() {
    // No cursor was learned for this page. The documented `page` parameter is
    // the fallback; the request must not depend on having walked every page
    // before it.
    let server = MockServer::start().await;
    let api = "/2.0/repositories/acme/widget/pullrequests";
    Mock::given(method("GET"))
        .and(path(api))
        .and(query_param("page", "1000000"))
        .respond_with(ok(r#"{"pagelen":50,"page":1000000,"values":[]}"#))
        .expect(1)
        .mount(&server)
        .await;
    let client = BitbucketClient::new(&server.uri(), "ATBB-token", None, 120).unwrap();
    let repo = bitbucket_repo(&server.uri());
    let page = client
        .list_prs(&repo, &ForgePrFilter::Open, 1_000_000)
        .await
        .unwrap();
    assert!(page.prs.is_empty());
    assert!(!page.has_more);
}

#[tokio::test]
async fn bitbucket_reads_outdated_from_the_api() {
    let server = MockServer::start().await;
    let api = "/2.0/repositories/acme/widget/pullrequests/42";
    Mock::given(method("GET"))
        .and(path(api))
        .respond_with(ok(BB_PR))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{api}/comments")))
        .respond_with(ok(BB_COMMENTS_OUTDATED))
        .mount(&server)
        .await;
    let client = BitbucketClient::new(&server.uri(), "ATBB-token", None, 120).unwrap();
    let repo = bitbucket_repo(&server.uri());
    let comments = client.list_comments(&repo, 42).await.unwrap();
    assert_eq!(comments.threads.len(), 2);
    let anchored = &comments.threads[0];
    assert_eq!(anchored.line, Some(20));
    assert!(
        anchored.outdated,
        "the root comment reports inline.outdated"
    );
    let file_level = &comments.threads[1];
    assert_eq!(file_level.line, None);
    assert!(
        !file_level.outdated,
        "a file-level note has no line that could be outdated, even when the API says so"
    );
}

#[tokio::test]
async fn bitbucket_server_pages_do_not_repeat_pull_requests() {
    // Server-side filtering means page two resumes where page one ended; no
    // row is returned twice across the UI's "load more".
    let server = MockServer::start().await;
    let api = "/2.0/repositories/acme/widget/pullrequests";
    Mock::given(method("GET"))
        .and(path("/2.0/user"))
        .respond_with(ok(
            r#"{"type":"user","display_name":"Alice Example","nickname":"alice","uuid":"{alice-uuid}"}"#,
        ))
        .mount(&server)
        .await;
    // The second page is addressed by the opaque cursor the first page
    // returned, not by a rebuilt `page=2`: Bitbucket may use a cursor that
    // carries filters a page number cannot reproduce.
    Mock::given(method("GET"))
        .and(path(api))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "pagelen": 2,
            "page": 1,
            "values": [bitbucket_pr(1, "alice"), bitbucket_pr(2, "alice")],
            "next": format!("{}{api}?cursor=opaque-next", server.uri())
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(api))
        .and(query_param("cursor", "opaque-next"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "pagelen": 2,
            "page": 2,
            "values": [bitbucket_pr(3, "alice")]
        })))
        .mount(&server)
        .await;

    let client = BitbucketClient::new(&server.uri(), "ATBB-token", None, 120).unwrap();
    let repo = bitbucket_repo(&server.uri());
    let first = client
        .list_prs(&repo, &ForgePrFilter::Mine, 1)
        .await
        .unwrap();
    let second = client
        .list_prs(&repo, &ForgePrFilter::Mine, 2)
        .await
        .unwrap();
    assert!(first.has_more);
    assert!(!second.has_more);
    let ids: Vec<u32> = first
        .prs
        .iter()
        .chain(second.prs.iter())
        .map(|pr| pr.number)
        .collect();
    let unique: std::collections::HashSet<u32> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len(), "pages repeated a row: {ids:?}");
    assert_eq!(ids, [1, 2, 3]);
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request
                .url
                .query_pairs()
                .any(|(key, value)| key == "cursor" && value == "opaque-next")),
        "page two follows the cursor the first page returned"
    );
}

#[tokio::test]
async fn bitbucket_comments_report_more_only_when_a_next_page_exists() {
    let server = MockServer::start().await;
    let api = "/2.0/repositories/acme/widget/pullrequests/42";
    Mock::given(method("GET"))
        .and(path(api))
        .respond_with(ok(BB_PR))
        .mount(&server)
        .await;
    // Ten short pages, and the tenth still links a next page: the client stops
    // at its page cap, so there really is more. A length threshold would miss
    // this and an exact-size list would overstate it.
    let page_body = |page: u32| {
        json!({
            "pagelen": 1,
            "values": [{
                "type": "pullrequest_comment",
                "id": page,
                "content": {"raw": format!("comment {page}")},
                "user": {"type": "user", "display_name": "Alice", "nickname": "alice"},
                "created_on": format!("2024-05-01T10:{page:02}:00Z")
            }],
            "next": format!("{}{api}/comments?page={}", server.uri(), page + 1)
        })
    };
    Mock::given(method("GET"))
        .and(path(format!("{api}/comments")))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page_body(1)))
        .mount(&server)
        .await;
    for page in 2..=10 {
        Mock::given(method("GET"))
            .and(path(format!("{api}/comments")))
            .and(query_param("page", page.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(page_body(page)))
            .mount(&server)
            .await;
    }
    let client = BitbucketClient::new(&server.uri(), "ATBB-token", None, 120).unwrap();
    let repo = bitbucket_repo(&server.uri());
    let comments = client.list_comments(&repo, 42).await.unwrap();
    assert_eq!(comments.comments.len(), 10);
    assert!(comments.has_more, "the last page read linked another page");
}

#[tokio::test]
async fn bitbucket_reports_unknown_size_when_diffstat_paging_hits_its_cap() {
    let server = MockServer::start().await;
    let api = "/2.0/repositories/acme/widget/pullrequests/42/diffstat";
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/acme/widget/pullrequests/42"))
        .respond_with(ok(BB_PR))
        .mount(&server)
        .await;
    for page in 1..=5 {
        let given = Mock::given(method("GET")).and(path(api));
        let given = if page == 1 {
            given.and(query_param_is_missing("page"))
        } else {
            given.and(query_param("page", page.to_string()))
        };
        given
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "pagelen": 100,
                "values": [{
                    "type": "diffstat",
                    "status": "modified",
                    "lines_added": 3,
                    "lines_removed": 1,
                    "old": {"path": format!("src/{page}.rs")},
                    "new": {"path": format!("src/{page}.rs")}
                }],
                // Even the fifth page has a successor, so the totals would be
                // partial and must be reported as unknown instead.
                "next": format!("{}{api}?page={}", server.uri(), page + 1)
            })))
            .mount(&server)
            .await;
    }
    let client = BitbucketClient::new(&server.uri(), "ATBB-token", None, 120).unwrap();
    let repo = bitbucket_repo(&server.uri());
    let pr = client.get_pr(&repo, 42).await.unwrap();
    assert_eq!(pr.additions, 0);
    assert_eq!(pr.deletions, 0);
    assert_eq!(pr.changed_files, 0);
    assert!(
        !pr.changed_files_known,
        "a capped diffstat is unknown, not an empty PR"
    );
    assert!(
        !pr.line_counts_known,
        "a capped diffstat cannot report line totals"
    );
}

/// An Azure server with only the checks sources mounted, so a test can pick
/// the statuses and policy evaluations it needs.
async fn azure_checks_server(statuses: Value, policies: Value) -> MockServer {
    let server = MockServer::start().await;
    let repo = "/acme/platform/_apis/git/repositories/widget";
    let pr = format!("{repo}/pullrequests/7");
    Mock::given(method("GET"))
        .and(path(&pr))
        .respond_with(ok(AZ_PR))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/iterations")))
        .respond_with(ok(AZ_ITERATIONS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/statuses")))
        .respond_with(ResponseTemplate::new(200).set_body_json(statuses))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/policy/evaluations"))
        .and(query_param("api-version", "7.1-preview.1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(policies))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn azure_same_name_policies_stay_distinct_and_a_rejection_fails_the_pr() {
    let server = azure_checks_server(
        json!({"count": 0, "value": []}),
        fixture(AZ_POLICIES_SAME_NAME),
    )
    .await;
    let client = AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&server.uri());

    let checks = client.list_checks(&repo, 7).await.unwrap();
    let rows: Vec<_> = checks
        .checks
        .iter()
        .filter(|check| check.name == "Build validation")
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "two configurations that share a display name are two rows"
    );
    assert!(
        rows.iter().any(|row| row.required == Some(true)
            && row.status == h4rn_protocol::ForgeCheckStatus::Failure),
        "the blocking rejection keeps its own row"
    );
    assert!(
        rows.iter().any(|row| row.required == Some(false)
            && row.status == h4rn_protocol::ForgeCheckStatus::Success),
        "the approved optional policy keeps its own row"
    );
    assert_eq!(checks.failed, 1);
    assert_eq!(checks.state, ForgeChecksState::Failing);
}

#[tokio::test]
async fn azure_drops_statuses_from_older_iterations() {
    let server = azure_checks_server(
        fixture(AZ_STATUSES_ITERATIONS),
        json!({"count": 0, "value": []}),
    )
    .await;
    let client = AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&server.uri());

    let checks = client.list_checks(&repo, 7).await.unwrap();
    let build: Vec<_> = checks
        .checks
        .iter()
        .filter(|check| check.name == "ci/build")
        .collect();
    assert_eq!(build.len(), 1, "only the current iteration's status stays");
    assert_eq!(build[0].status, h4rn_protocol::ForgeCheckStatus::Success);
    assert_eq!(
        checks
            .checks
            .iter()
            .filter(|check| check.name == "license/cla")
            .count(),
        1,
        "a PR-level status has no iteration and always stays"
    );
    assert_eq!(checks.failed, 0);
    assert_eq!(checks.state, ForgeChecksState::Passing);
}

#[tokio::test]
async fn azure_checks_require_both_value_collections() {
    let green = json!({
        "count": 1,
        "value": [{
            "id": 1,
            "state": "succeeded",
            "context": {"name": "ci/build", "genre": "continuous-integration"},
            "creationDate": "2024-05-01T11:00:00.000Z",
            "updatedDate": "2024-05-01T11:05:00.000Z"
        }]
    });
    let empty = json!({"count": 0, "value": []});
    let client_for = |server: &MockServer| {
        AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat", 120).unwrap()
    };
    let repo_for = |server: &MockServer| azure_repo(&server.uri());

    // A valid empty collection stays empty: no rows, so no passing aggregate.
    let server = azure_checks_server(empty.clone(), empty.clone()).await;
    let checks = client_for(&server)
        .list_checks(&repo_for(&server), 7)
        .await
        .unwrap();
    assert_eq!(checks.total, 0);
    assert_eq!(checks.state, ForgeChecksState::Unknown);

    // An empty policy collection is a real answer next to green statuses.
    let server = azure_checks_server(green.clone(), empty.clone()).await;
    let checks = client_for(&server)
        .list_checks(&repo_for(&server), 7)
        .await
        .unwrap();
    assert_eq!(checks.total, 1);
    assert_eq!(checks.state, ForgeChecksState::Passing);

    // A missing `value` is not an empty answer: it is half the data, and a
    // green status list must not become a pass from it.
    let server = azure_checks_server(green.clone(), json!({"count": 0})).await;
    let error = client_for(&server)
        .list_checks(&repo_for(&server), 7)
        .await
        .unwrap_err();
    assert!(matches!(error, ForgeError::Parse(_)), "{error:?}");

    let server = azure_checks_server(json!({"count": 0}), empty).await;
    let error = client_for(&server)
        .list_checks(&repo_for(&server), 7)
        .await
        .unwrap_err();
    assert!(matches!(error, ForgeError::Parse(_)), "{error:?}");
}

#[tokio::test]
async fn azure_checks_fail_when_the_policy_request_fails() {
    // Green statuses must not become "Passing" when the branch gate could not
    // be read: the request fails instead of reporting a pass from half the
    // data.
    let server = MockServer::start().await;
    let repo_path = "/acme/platform/_apis/git/repositories/widget";
    let pr = format!("{repo_path}/pullrequests/7");
    Mock::given(method("GET"))
        .and(path(&pr))
        .respond_with(ok(AZ_PR))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/iterations")))
        .respond_with(ok(AZ_ITERATIONS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/statuses")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "count": 1,
            "value": [{
                "id": 1,
                "state": "succeeded",
                "context": {"name": "ci/build", "genre": "continuous-integration"},
                "creationDate": "2024-05-01T11:00:00.000Z",
                "updatedDate": "2024-05-01T11:05:00.000Z"
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/policy/evaluations"))
        .respond_with(ResponseTemplate::new(500).set_body_string("policy service unavailable"))
        .mount(&server)
        .await;

    let client = AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&server.uri());
    let error = client.list_checks(&repo, 7).await.unwrap_err();
    assert!(
        matches!(error, ForgeError::Network(_)),
        "a failed policy read is an error, not a passing aggregate: {error:?}"
    );
}

fn fork_pr() -> Value {
    let mut pr = fixture(AZ_PR);
    pr["forkSource"] = json!({
        "repository": {
            "id": "fork-guid",
            "name": "widget-fork",
            "project": {"id": "project-guid", "name": "platform"},
            "isFork": true
        },
        "commit": {"commitId": AZ_HEAD},
        "refName": "refs/heads/alice/fix"
    });
    pr
}

/// A fork PR server with one added file whose source side exists only in the
/// fork's repository.
async fn azure_fork_server() -> MockServer {
    let server = MockServer::start().await;
    let target = "/acme/platform/_apis/git/repositories/widget";
    let pr = format!("{target}/pullrequests/7");
    Mock::given(method("GET"))
        .and(path(&pr))
        .respond_with(ResponseTemplate::new(200).set_body_json(fork_pr()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/iterations")))
        .respond_with(ok(AZ_ITERATIONS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/iterations/2/changes")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "changeEntries": [{
                "changeTrackingId": 1,
                "changeId": 1,
                "changeType": "add",
                "item": {
                    "objectId": "3333333333333333333333333333333333333333",
                    "gitObjectType": "blob",
                    "path": "/src/fork.rs"
                }
            }]
        })))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn azure_fork_prs_read_the_source_side_from_the_fork() {
    let server = azure_fork_server().await;
    Mock::given(method("GET"))
        .and(path(
            "/acme/platform/_apis/git/repositories/widget-fork/items",
        ))
        .and(query_param("path", "/src/fork.rs"))
        .and(query_param("versionDescriptor.version", AZ_HEAD))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "/src/fork.rs",
            "gitObjectType": "blob",
            "content": "from the fork\n",
            "contentMetadata": {"isBinary": false, "encoding": 65001, "contentType": "text/plain"}
        })))
        .mount(&server)
        .await;

    let client = AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&server.uri());
    let diff = client.get_diff(&repo, 7, 1, 10).await.unwrap();
    let added = diff.files.iter().find(|f| f.path == "src/fork.rs").unwrap();
    let patch = added
        .patch
        .as_deref()
        .expect("the added file's contents come from the fork");
    assert!(patch.contains("+from the fork"), "{patch}");
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request.url.path().contains("/widget-fork/items")),
        "the source side is read from the fork repository"
    );
}

#[tokio::test]
async fn azure_unexpected_item_404_is_an_error() {
    // The fork's item is missing. That is a wrong repository or commit, not an
    // absent side: reporting the file as new/deleted would invent a diff.
    let server = azure_fork_server().await;
    let client = AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&server.uri());
    let error = client.get_diff(&repo, 7, 1, 10).await.unwrap_err();
    assert!(
        matches!(error, ForgeError::NotFound(_)),
        "an unexpected 404 is an error: {error:?}"
    );
}

#[tokio::test]
async fn azure_hundred_file_pr_pages_within_the_deadline() {
    // A 100-file PR at the UI's default page request (30) through the real
    // 120 requests/minute limiter: every page must finish inside the UI's 10 s
    // deadline, and Azure's smaller page size must keep the request count
    // bounded. The files are added, so each needs exactly one content request.
    let server = MockServer::start().await;
    let repo_path = "/acme/platform/_apis/git/repositories/widget";
    let pr = format!("{repo_path}/pullrequests/7");
    Mock::given(method("GET"))
        .and(path(&pr))
        .respond_with(ok(AZ_PR))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/iterations")))
        .respond_with(ok(AZ_ITERATIONS))
        .mount(&server)
        .await;
    let changes: Vec<Value> = (1..=100)
        .map(|index| {
            json!({
                "changeTrackingId": index,
                "changeId": index,
                "changeType": "add",
                "item": {
                    "objectId": format!("{index:040}"),
                    "gitObjectType": "blob",
                    "path": format!("/src/gen/file{index:03}.txt")
                }
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path(format!("{pr}/iterations/2/changes")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"changeEntries": changes})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo_path}/items")))
        .and(query_param("includeContentMetadata", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "gitObjectType": "blob",
            "content": "line\n",
            "contentMetadata": {"isBinary": false, "encoding": 65001, "contentType": "text/plain"}
        })))
        .mount(&server)
        .await;

    let client = AzureDevOpsClient::new(
        &format!("{}/acme", server.uri()),
        "pat",
        DEFAULT_REQUESTS_PER_MINUTE,
    )
    .unwrap();
    let repo = azure_repo(&server.uri());

    let mut seen = 0;
    for page in 1..=13 {
        let started = std::time::Instant::now();
        let diff = client.get_diff(&repo, 7, page, 30).await.unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "page {page} took {elapsed:?}, over the UI's 10 s deadline"
        );
        assert_eq!(diff.per_page, 8, "Azure clamps the page size");
        let expected = if page < 13 { 8 } else { 4 };
        assert_eq!(diff.files.len(), expected, "page {page}");
        assert_eq!(diff.has_more, page < 13, "page {page}");
        let total = server.received_requests().await.unwrap().len();
        let page_requests = total - seen;
        seen = total;
        assert!(
            page_requests <= 20,
            "page {page} made {page_requests} requests; the default budget allows 20 in 10 s"
        );
    }
    assert_eq!(
        seen,
        100 + 13 * 3,
        "100 contents plus the PR, iteration and change reads per page"
    );
}

async fn azure_server() -> MockServer {
    let server = MockServer::start().await;
    let repo = "/acme/platform/_apis/git/repositories/widget";
    let pr = format!("{repo}/pullrequests/7");

    Mock::given(method("GET"))
        .and(path("/acme/_apis/connectionData"))
        .respond_with(ok(AZ_CONNECTION))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/pullrequests")))
        .and(query_param("searchCriteria.status", "active"))
        .respond_with(ok(AZ_LIST))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(&pr))
        .respond_with(ok(AZ_PR))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/iterations")))
        .respond_with(ok(AZ_ITERATIONS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/iterations/2/changes")))
        .respond_with(ok(AZ_CHANGES))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/threads")))
        .respond_with(ok(AZ_THREADS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/statuses")))
        .respond_with(ok(AZ_STATUSES))
        .mount(&server)
        .await;
    let commits = fixture(AZ_COMMITS);
    Mock::given(method("GET"))
        .and(path(format!("{pr}/commits")))
        .and(query_param_is_missing("continuationToken"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-ms-continuationtoken", "token-2")
                .set_body_json(json!([commits[0].clone()])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/commits")))
        .and(query_param("continuationToken", "token-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([commits[1].clone()])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/items")))
        .and(query_param("includeContentMetadata", "true"))
        .and(query_param("path", "/src/lib.rs"))
        .and(query_param("versionDescriptor.version", AZ_BASE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "/src/lib.rs",
            "gitObjectType": "blob",
            "content": "one\ntwo\nthree\n",
            "contentMetadata": {"isBinary": false, "encoding": 65001, "contentType": "text/plain"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/items")))
        .and(query_param("includeContentMetadata", "true"))
        .and(query_param("path", "/src/lib.rs"))
        .and(query_param("versionDescriptor.version", AZ_HEAD))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "/src/lib.rs",
            "gitObjectType": "blob",
            "content": "one\ntwo\nTHREE\nfour\n",
            "contentMetadata": {"isBinary": false, "encoding": 65001, "contentType": "text/plain"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/items")))
        .and(query_param("includeContentMetadata", "true"))
        .and(query_param("path", "/src/new.rs"))
        .and(query_param("versionDescriptor.version", AZ_HEAD))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "/src/new.rs",
            "gitObjectType": "blob",
            "content": "hello\nnew\n",
            "contentMetadata": {"isBinary": false, "encoding": 65001, "contentType": "text/plain"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/items")))
        .and(query_param("includeContentMetadata", "true"))
        .and(query_param("path", "/src/old.rs"))
        .and(query_param("versionDescriptor.version", AZ_BASE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "/src/old.rs",
            "gitObjectType": "blob",
            "content": "bye\n",
            "contentMetadata": {"isBinary": false, "encoding": 65001, "contentType": "text/plain"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/items")))
        .and(query_param("includeContentMetadata", "true"))
        .and(query_param("path", "/src/old_name.rs"))
        .and(query_param("versionDescriptor.version", AZ_BASE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "/src/old_name.rs",
            "gitObjectType": "blob",
            "content": "same\n",
            "contentMetadata": {"isBinary": false, "encoding": 65001, "contentType": "text/plain"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/items")))
        .and(query_param("includeContentMetadata", "true"))
        .and(query_param("path", "/src/new_name.rs"))
        .and(query_param("versionDescriptor.version", AZ_HEAD))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "/src/new_name.rs",
            "gitObjectType": "blob",
            "content": "same\nchanged\n",
            "contentMetadata": {"isBinary": false, "encoding": 65001, "contentType": "text/plain"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/items")))
        .and(query_param("includeContentMetadata", "true"))
        .and(query_param("path", "/assets/logo.png"))
        .and(query_param("versionDescriptor.version", AZ_BASE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "/assets/logo.png",
            "gitObjectType": "blob",
            "content": "PNG old bytes",
            "contentMetadata": {"isBinary": true, "encoding": 0, "contentType": "image/png"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/items")))
        .and(query_param("includeContentMetadata", "true"))
        .and(query_param("path", "/assets/logo.png"))
        .and(query_param("versionDescriptor.version", AZ_HEAD))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "/assets/logo.png",
            "gitObjectType": "blob",
            "content": "PNG new bytes",
            "contentMetadata": {"isBinary": true, "encoding": 0, "contentType": "image/png"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/policy/evaluations"))
        .and(query_param("api-version", "7.1-preview.1"))
        .and(query_param(
            "artifactId",
            "vstfs:///CodeReview/CodeReviewId/project-guid/7",
        ))
        .respond_with(ok(AZ_POLICIES))
        .mount(&server)
        .await;
    server
}

fn item_paths(requests: &[wiremock::Request]) -> Vec<String> {
    requests
        .iter()
        .filter(|request| request.url.path().ends_with("/items"))
        .filter_map(|request| {
            request
                .url
                .query_pairs()
                .find(|(key, _)| key == "path")
                .map(|(_, value)| value.into_owned())
        })
        .collect()
}

#[tokio::test]
async fn azure_reads_pull_requests_diff_threads_and_checks() {
    let server = azure_server().await;
    let client = AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&server.uri());

    let viewer = client.viewer().await.unwrap();
    assert_eq!(viewer.login, "Alice Example");

    let page = client
        .list_prs(&repo, &ForgePrFilter::Open, 1)
        .await
        .unwrap();
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request
                .url
                .query_pairs()
                .any(|(key, value)| { key == "searchCriteria.status" && value == "active" })),
        "Azure DevOps lists active PRs with searchCriteria.status=active"
    );
    assert_eq!(page.prs.len(), 1);
    assert_eq!(page.prs[0].state, ForgePrState::Open);
    assert_eq!(page.prs[0].head_branch, "alice/fix");
    assert_eq!(page.prs[0].base_branch, "main");
    assert_eq!(page.prs[0].author, "Alice Example");
    assert_eq!(
        page.prs[0].updated_at,
        DateTime::<Utc>::from_timestamp(1714554000, 0).unwrap(),
        "an active PR keeps its creation date; lastMergeCommit is not an update"
    );

    let pr = client.get_pr(&repo, 7).await.unwrap();
    assert_eq!(pr.body, "Fixes the widget and adds a regression test.");
    assert_eq!(pr.labels.len(), 1);
    assert_eq!(pr.mergeable, ForgeMergeable::Mergeable);
    // The detail counts changed files from iteration metadata and never
    // downloads a file's contents.
    assert_eq!(pr.additions, 0);
    assert_eq!(pr.deletions, 0);
    assert_eq!(pr.changed_files, 5);
    assert!(pr.changed_files_known);
    assert!(
        !pr.line_counts_known,
        "Azure reports no line totals without downloading every file"
    );
    assert_eq!(pr.commits, 2);
    assert_eq!(pr.reviewers.len(), 3);
    assert_eq!(pr.summary.review, ForgeReviewState::ChangesRequested);
    assert_eq!(pr.merge_commit_sha, None);
    assert!(
        item_paths(&server.received_requests().await.unwrap()).is_empty(),
        "get_pr must not build the patch"
    );

    let diff = client.get_diff(&repo, 7, 1, 10).await.unwrap();
    assert_eq!(diff.files.len(), 5);
    let lib = diff.files.iter().find(|f| f.path == "src/lib.rs").unwrap();
    assert_eq!(lib.additions, 2);
    assert_eq!(lib.deletions, 1);
    assert!(lib.patch.as_deref().unwrap().contains("+THREE\n"));
    let added = diff.files.iter().find(|f| f.path == "src/new.rs").unwrap();
    assert_eq!(added.status, ForgeFileStatus::Added);
    let added_patch = added.patch.as_deref().unwrap();
    assert!(added_patch.contains("--- /dev/null"));
    assert!(
        added_patch.contains("@@ -0,0 +1,2 @@"),
        "an added file has no old lines: {added_patch}"
    );
    let deleted = diff.files.iter().find(|f| f.path == "src/old.rs").unwrap();
    assert_eq!(deleted.status, ForgeFileStatus::Deleted);
    let deleted_patch = deleted.patch.as_deref().unwrap();
    assert!(deleted_patch.contains("+++ /dev/null"));
    assert!(
        deleted_patch.contains("@@ -1,1 +0,0 @@"),
        "a deleted file has no new lines: {deleted_patch}"
    );
    let renamed = diff
        .files
        .iter()
        .find(|f| f.path == "src/new_name.rs")
        .unwrap();
    assert_eq!(renamed.status, ForgeFileStatus::Renamed);
    assert_eq!(renamed.previous_path.as_deref(), Some("src/old_name.rs"));
    assert!(
        renamed.patch.is_none(),
        "a pure rename keeps its bytes, so there is nothing to show"
    );
    assert!(
        !item_paths(&server.received_requests().await.unwrap())
            .iter()
            .any(|path| path == "/src/old_name.rs"),
        "a pure rename downloads neither side"
    );
    let binary = diff
        .files
        .iter()
        .find(|f| f.path == "assets/logo.png")
        .unwrap();
    assert!(binary.binary, "contentMetadata.isBinary marks the file");
    assert!(binary.patch.is_none());
    assert_eq!(binary.additions, 0);

    let comments = client.list_comments(&repo, 7).await.unwrap();
    assert_eq!(comments.comments.len(), 1);
    assert_eq!(comments.comments[0].body, "Thanks!");
    assert_eq!(comments.threads.len(), 1);
    let thread = &comments.threads[0];
    assert_eq!(thread.thread_id, "101");
    assert_eq!(thread.path, "src/lib.rs");
    assert_eq!(thread.line, Some(12));
    assert_eq!(thread.side, ForgeDiffSide::Right);
    assert!(!thread.resolved);
    assert_eq!(thread.comments.len(), 2);
    assert_eq!(thread.comments[1].in_reply_to.as_deref(), Some("1"));

    let checks = client.list_checks(&repo, 7).await.unwrap();
    assert_eq!(checks.head_sha, AZ_HEAD);
    assert_eq!(checks.total, 3);
    assert_eq!(checks.passed, 1);
    assert_eq!(checks.failed, 2);
    let by_name = |name: &str| checks.checks.iter().find(|c| c.name == name).unwrap();
    assert_eq!(by_name("Build validation").required, Some(true));
    // The status says succeeded, the blocking policy rejected it: the policy
    // outcome wins.
    assert_eq!(
        by_name("Build validation").status,
        h4rn_protocol::ForgeCheckStatus::Failure
    );
    assert_eq!(
        by_name("ci/lint").status,
        h4rn_protocol::ForgeCheckStatus::Failure
    );
    assert_eq!(by_name("Minimum reviewers").required, Some(false));
    assert!(!checks.rerun.supported);
    assert!(
        checks
            .rerun
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("Builds API")),
        "{:?}",
        checks.rerun
    );

    let commits = client.list_commits(&repo, 7).await.unwrap();
    let titles: Vec<&str> = commits.commits.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["Add a test", "Fix the panic"]);
    assert_eq!(
        commits.commits[1].body.as_deref(),
        Some("Reproduces the panic on empty input.")
    );
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| request
                .url
                .query_pairs()
                .any(|(key, value)| key == "continuationToken" && value == "token-2")),
        "the second commit page uses the server's continuationToken"
    );
}

#[tokio::test]
async fn azure_diff_page_fetches_only_its_files() {
    let server = azure_server().await;
    let client = AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&server.uri());

    let first = client.get_diff(&repo, 7, 1, 2).await.unwrap();
    assert_eq!(first.files.len(), 2);
    assert_eq!(
        first
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        ["src/lib.rs", "src/new.rs"]
    );
    assert!(first.has_more);
    let mut requested = item_paths(&server.received_requests().await.unwrap());
    requested.sort();
    assert_eq!(requested, ["/src/lib.rs", "/src/lib.rs", "/src/new.rs"]);

    let second = client.get_diff(&repo, 7, 2, 2).await.unwrap();
    assert_eq!(
        second
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        ["src/old.rs", "src/new_name.rs"]
    );
    assert!(second.has_more, "the binary file is still on page 3");
    let mut requested = item_paths(&server.received_requests().await.unwrap());
    requested.sort();
    assert_eq!(
        requested,
        ["/src/lib.rs", "/src/lib.rs", "/src/new.rs", "/src/old.rs"],
        "the second page fetches only the files whose content can differ"
    );
}

#[tokio::test]
async fn azure_encodes_project_names_as_path_segments() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/acme/My%20Project/_apis/git/repositories/widget/pullrequests/7",
        ))
        .respond_with(ok(AZ_PR))
        .mount(&server)
        .await;
    let client = AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat", 120).unwrap();
    let repo = ForgeRepoRef {
        forge: ForgeKind::AzureDevOps,
        base_url: format!("{}/acme", server.uri()),
        owner: "My Project".into(),
        repo: "widget".into(),
    };
    let pr = client.get_pr(&repo, 7).await.unwrap();
    assert_eq!(pr.summary.number, 7);
}

#[tokio::test]
async fn azure_sends_a_basic_pat_with_an_empty_username() {
    let server = MockServer::start().await;
    let expected = base64::engine::general_purpose::STANDARD.encode(":pat-token");
    Mock::given(method("GET"))
        .and(path("/acme/_apis/connectionData"))
        .and(header("authorization", format!("Basic {expected}")))
        .respond_with(ok(AZ_CONNECTION))
        .expect(1)
        .mount(&server)
        .await;
    let client =
        AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat-token", 120).unwrap();
    assert_eq!(client.viewer().await.unwrap().login, "Alice Example");
}

#[tokio::test]
async fn azure_mine_and_reviewer_filters_use_the_viewer_identity() {
    let server = azure_server().await;
    let client = AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&server.uri());
    for filter in [ForgePrFilter::Mine, ForgePrFilter::ReviewRequested] {
        let page = client.list_prs(&repo, &filter, 1).await.unwrap();
        assert_eq!(page.prs.len(), 1, "{filter:?}");
    }
}

#[tokio::test]
async fn azure_rejects_a_malformed_comment_id() {
    // The comment id goes into the JSON body, the thread id into the path;
    // both are parsed first, so a bad id costs no request at all.
    let server = MockServer::start().await;
    let client = AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&server.uri());
    for comment_id in ["not-a-number", "-1", "99999999999999999999"] {
        let error = client
            .reply(&repo, 7, Some("101"), comment_id, "Fixed.")
            .await
            .unwrap_err();
        assert!(
            matches!(error, ForgeError::InvalidRequest(_)),
            "comment id {comment_id:?}: {error:?}"
        );
    }
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "an invalid comment id must not send a request"
    );
}

#[tokio::test]
async fn azure_rejects_a_thread_id_before_building_the_url() {
    // The id is a caller-supplied string. Parsing it as a positive integer
    // before formatting the path keeps `../` and similar text out of the URL,
    // and an invalid id costs no request at all.
    let server = MockServer::start().await;
    let client = AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&server.uri());
    for thread_id in [
        "abc",
        "101/../iterations",
        "0",
        "-1",
        "99999999999999999999",
    ] {
        let error = client
            .reply(&repo, 7, Some(thread_id), "1", "Fixed.")
            .await
            .unwrap_err();
        assert!(
            matches!(error, ForgeError::InvalidRequest(_)),
            "thread id {thread_id:?}: {error:?}"
        );
    }
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "an invalid id must not be turned into a request path"
    );
}

#[tokio::test]
async fn azure_redacts_the_configured_pat_in_an_error_body() {
    // Azure can quote the credential it rejected. Both the verbatim PAT and the
    // Base64 Basic credential must be gone before the error leaves the client.
    let pat = "azp/7Kq+2f9Xr3Lm0";
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!(":{pat}"));
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(
            "/acme/platform/_apis/git/repositories/widget/pullrequests/7",
        ))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "message": format!(
                "The personal access token \"{pat}\" was rejected: Authorization: Basic {encoded}"
            )
        })))
        .mount(&server)
        .await;
    let client = AzureDevOpsClient::new(&format!("{}/acme", server.uri()), pat, 120).unwrap();
    let repo = azure_repo(&server.uri());
    let message = client.get_pr(&repo, 7).await.unwrap_err().to_string();
    assert!(!message.contains(pat), "{message}");
    assert!(!message.contains(&encoded), "{message}");
    assert!(message.contains("[redacted]"), "{message}");
}

/// Mounts the PR and its latest iteration, then answers the change entries
/// with `body`.
async fn azure_changes_server(body: Value) -> MockServer {
    let server = MockServer::start().await;
    let repo = "/acme/platform/_apis/git/repositories/widget";
    let pr = format!("{repo}/pullrequests/7");
    Mock::given(method("GET"))
        .and(path(&pr))
        .respond_with(ok(AZ_PR))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/iterations")))
        .respond_with(ok(AZ_ITERATIONS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/iterations/2/changes")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn azure_missing_change_entries_are_unknown_not_empty() {
    let missing = azure_changes_server(json!({"nextSkip": 0})).await;
    let client = AzureDevOpsClient::new(&format!("{}/acme", missing.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&missing.uri());
    let pr = client.get_pr(&repo, 7).await.unwrap();
    assert!(
        !pr.changed_files_known,
        "a 200 without changeEntries is not a zero-file PR"
    );
    assert_eq!(pr.changed_files, 0);
    let error = client.get_diff(&repo, 7, 1, 10).await.unwrap_err();
    assert!(matches!(error, ForgeError::Parse(_)), "{error:?}");

    let empty = azure_changes_server(json!({"changeEntries": [], "nextSkip": 0})).await;
    let client = AzureDevOpsClient::new(&format!("{}/acme", empty.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&empty.uri());
    let pr = client.get_pr(&repo, 7).await.unwrap();
    assert_eq!(pr.changed_files, 0);
    assert!(pr.changed_files_known, "a real empty list is known");
    let diff = client.get_diff(&repo, 7, 1, 10).await.unwrap();
    assert!(diff.files.is_empty());
    assert!(!diff.has_more);
}

#[tokio::test]
async fn azure_missing_threads_are_an_error_not_no_comments() {
    let missing = MockServer::start().await;
    let pr = "/acme/platform/_apis/git/repositories/widget/pullrequests/7";
    Mock::given(method("GET"))
        .and(path(pr))
        .respond_with(ok(AZ_PR))
        .mount(&missing)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/threads")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"count": 0})))
        .mount(&missing)
        .await;
    let client = AzureDevOpsClient::new(&format!("{}/acme", missing.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&missing.uri());
    let error = client.list_comments(&repo, 7).await.unwrap_err();
    assert!(matches!(error, ForgeError::Parse(_)), "{error:?}");

    let empty = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(pr))
        .respond_with(ok(AZ_PR))
        .mount(&empty)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{pr}/threads")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"count": 0, "value": []})))
        .mount(&empty)
        .await;
    let client = AzureDevOpsClient::new(&format!("{}/acme", empty.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&empty.uri());
    let page = client.list_comments(&repo, 7).await.unwrap();
    assert!(page.comments.is_empty());
    assert!(page.threads.is_empty());
}

#[tokio::test]
async fn bitbucket_rejects_an_invalid_comment_id_before_posting() {
    let server = MockServer::start().await;
    let client = BitbucketClient::new(&server.uri(), "ATBB-token", None, 120).unwrap();
    let repo = bitbucket_repo(&server.uri());
    for comment_id in [
        "not-a-number",
        "-1",
        "99999999999999999999",
        "9100/../../comments",
    ] {
        let error = client
            .reply(&repo, 42, Some("9100"), comment_id, "Fixed.")
            .await
            .unwrap_err();
        assert!(
            matches!(error, ForgeError::InvalidRequest(_)),
            "comment id {comment_id:?}: {error:?}"
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn bitbucket_writes_replies_comments_and_pull_requests() {
    let server = bitbucket_server().await;
    let api = "/2.0/repositories/acme/widget";
    Mock::given(method("POST"))
        .and(path(format!("{api}/pullrequests/42/comments")))
        .and(wiremock::matchers::body_partial_json(
            json!({"parent": {"id": 9100}}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "type": "pullrequest_comment",
            "id": 9200,
            "content": {"raw": "Fixed.", "markup": "markdown", "html": "<p>Fixed.</p>"},
            "user": {"type": "user", "display_name": "Alice Example", "nickname": "alice", "uuid": "{alice-uuid}"},
            "created_on": "2024-05-01T12:00:00.123456+00:00",
            "inline": {"path": "src/lib.rs", "from": null, "to": 12}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{api}/pullrequests/42/comments")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "type": "pullrequest_comment",
            "id": 9201,
            "content": {"raw": "Thanks!", "markup": "markdown", "html": "<p>Thanks!</p>"},
            "user": {"type": "user", "display_name": "Alice Example", "nickname": "alice", "uuid": "{alice-uuid}"},
            "created_on": "2024-05-01T12:01:00.123456+00:00"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{api}/pullrequests")))
        .and(wiremock::matchers::body_partial_json(json!({
            "source": {"branch": {"name": "alice/fix"}},
            "destination": {"branch": {"name": "main"}}
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(fixture(BB_PR)))
        .expect(1)
        .mount(&server)
        .await;

    let client = BitbucketClient::new(&server.uri(), "ATBB-token", None, 120).unwrap();
    let repo = bitbucket_repo(&server.uri());
    let reply = client
        .reply(&repo, 42, Some("9100"), "9100", "Fixed.")
        .await
        .unwrap();
    assert_eq!(reply.comment_id, "9200");
    assert_eq!(reply.thread_id.as_deref(), Some("9100"));
    assert_eq!(reply.in_reply_to.as_deref(), Some("9100"));
    assert_eq!(reply.path, "src/lib.rs");

    let comment = client.create_comment(&repo, 42, "Thanks!").await.unwrap();
    assert_eq!(comment.comment_id, "9201");

    let opened = client
        .open_pr(
            &repo,
            &h4rn_protocol::ForgeNewPr {
                title: "Fix the widget".into(),
                body: "Fixes the widget.".into(),
                head: "alice/fix".into(),
                base: "main".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(opened.summary.number, 42);
}

#[tokio::test]
async fn azure_writes_replies_comments_and_pull_requests() {
    let server = azure_server().await;
    let pr = "/acme/platform/_apis/git/repositories/widget/pullrequests/7";
    Mock::given(method("POST"))
        .and(path(format!("{pr}/threads/101/comments")))
        .and(wiremock::matchers::body_partial_json(
            json!({"parentCommentId": 1, "content": "Fixed."}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 5,
            "parentCommentId": 1,
            "author": {"displayName": "Alice Example"},
            "content": "Fixed.",
            "publishedDate": "2024-05-01T12:00:00.000Z",
            "lastUpdatedDate": "2024-05-01T12:00:00.000Z",
            "commentType": "text"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{pr}/threads")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 104,
            "status": "active",
            "comments": [{
                "id": 6,
                "parentCommentId": 0,
                "author": {"displayName": "Alice Example"},
                "content": "Thanks!",
                "publishedDate": "2024-05-01T12:01:00.000Z",
                "lastUpdatedDate": "2024-05-01T12:01:00.000Z",
                "commentType": "text"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(
            "/acme/platform/_apis/git/repositories/widget/pullrequests",
        ))
        .and(wiremock::matchers::body_partial_json(json!({
            "sourceRefName": "refs/heads/alice/fix",
            "targetRefName": "refs/heads/main"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(AZ_PR)))
        .expect(1)
        .mount(&server)
        .await;

    let client = AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat", 120).unwrap();
    let repo = azure_repo(&server.uri());
    let reply = client
        .reply(&repo, 7, Some("101"), "1", "Fixed.")
        .await
        .unwrap();
    assert_eq!(reply.comment_id, "5");
    assert_eq!(reply.thread_id.as_deref(), Some("101"));
    assert_eq!(reply.in_reply_to.as_deref(), Some("1"));

    let comment = client.create_comment(&repo, 7, "Thanks!").await.unwrap();
    assert_eq!(comment.comment_id, "6");
    assert_eq!(comment.body, "Thanks!");

    let opened = client
        .open_pr(
            &repo,
            &h4rn_protocol::ForgeNewPr {
                title: "Fix the widget".into(),
                body: "Fixes the widget.".into(),
                head: "alice/fix".into(),
                base: "main".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(opened.summary.number, 7);
}

fn bitbucket_account(
    server: &MockServer,
    username: Option<&str>,
    source: h4rn_protocol::ForgeCredentialSource,
) -> h4rn_protocol::ForgeAccount {
    h4rn_protocol::ForgeAccount {
        account_id: "acct_bitbucket".into(),
        forge: ForgeKind::Bitbucket,
        base_url: server.uri(),
        username: username.map(ToOwned::to_owned),
        display_name: None,
        scopes: Vec::new(),
        source,
        created_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn a_stored_bitbucket_account_keeps_its_auth_scheme() {
    // An API token always uses Basic, even after the daemon restarts and the
    // account row carries a login.
    let basic = MockServer::start().await;
    let expected = base64::engine::general_purpose::STANDARD.encode("alice:s3cret");
    Mock::given(method("GET"))
        .and(path("/2.0/user"))
        .and(header("authorization", format!("Basic {expected}")))
        .respond_with(ok(
            r#"{"type":"user","display_name":"Alice Example","nickname":"alice","uuid":"{alice-uuid}"}"#,
        ))
        .mount(&basic)
        .await;
    let account = bitbucket_account(
        &basic,
        Some("alice"),
        h4rn_protocol::ForgeCredentialSource::BitbucketApiToken,
    );
    let client = h4rn_forge::client_for(&account, "s3cret").unwrap();
    assert_eq!(client.viewer().await.unwrap().login, "alice");

    // A workspace token stays Bearer even if the stored row has a username.
    let bearer = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2.0/user"))
        .and(header("authorization", "Bearer ATBB-workspace"))
        .respond_with(ok(
            r#"{"type":"user","display_name":"Alice Example","nickname":"alice","uuid":"{alice-uuid}"}"#,
        ))
        .mount(&bearer)
        .await;
    let account = bitbucket_account(
        &bearer,
        Some("workspace"),
        h4rn_protocol::ForgeCredentialSource::BitbucketAccessToken,
    );
    let client = h4rn_forge::client_for(&account, "ATBB-workspace").unwrap();
    assert_eq!(client.viewer().await.unwrap().login, "alice");
}
