//! Bitbucket Cloud and Azure DevOps against recorded API responses.
//!
//! Every fixture is a trimmed response with the provider's own field names;
//! no test reaches the network beyond the local wiremock server.

use base64::Engine as _;
use h4rn_forge::{AzureDevOpsClient, BitbucketClient, Forge, ForgeError};
use h4rn_protocol::{
    ForgeChecksState, ForgeDiffSide, ForgeFileStatus, ForgeKind, ForgeMergeable, ForgePrFilter,
    ForgePrState, ForgeRepoRef, ForgeReviewState,
};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BB_PR: &str = include_str!("fixtures/bitbucket_pr_42.json");
const BB_LIST: &str = include_str!("fixtures/bitbucket_pr_list.json");
const BB_COMMITS: &str = include_str!("fixtures/bitbucket_commits.json");
const BB_COMMENTS: &str = include_str!("fixtures/bitbucket_comments.json");
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
const AZ_PROJECT: &str = include_str!("fixtures/azure_project.json");
const AZ_CONNECTION: &str = include_str!("fixtures/azure_connection.json");

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
        .respond_with(ok(r#"{"username":"alice","display_name":"Alice Example"}"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{api}/pullrequests")))
        .respond_with(ok(BB_LIST))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{api}/pullrequests/42")))
        .respond_with(ok(BB_PR))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{api}/pullrequests/42/diffstat")))
        .respond_with(ok(BB_DIFFSTAT))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{api}/pullrequests/42/commits")))
        .respond_with(ok(BB_COMMITS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{api}/pullrequests/42.diff")))
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

    let page = client
        .list_prs(&repo, &ForgePrFilter::Open, 1)
        .await
        .unwrap();
    assert_eq!(page.prs.len(), 3);
    assert_eq!(page.prs[0].state, ForgePrState::Open);
    assert_eq!(page.prs[1].state, ForgePrState::Merged);
    assert_eq!(page.prs[2].state, ForgePrState::Closed);
    assert_eq!(page.prs[0].head_branch, "alice/fix");
    assert!(!page.has_more);

    let pr = client.get_pr(&repo, 42).await.unwrap();
    assert_eq!(pr.body, "Fixes the widget and adds a regression test.");
    assert_eq!(pr.additions, 5);
    assert_eq!(pr.deletions, 1);
    assert_eq!(pr.changed_files, 2);
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
    let expected = base64::engine::general_purpose::STANDARD.encode("alice:s3cret");
    Mock::given(method("GET"))
        .and(path("/2.0/user"))
        .and(header("authorization", format!("Basic {expected}")))
        .respond_with(ok(r#"{"username":"alice"}"#))
        .expect(1)
        .mount(&basic)
        .await;
    let client = BitbucketClient::new(&basic.uri(), "s3cret", Some("alice"), 120).unwrap();
    assert_eq!(client.viewer().await.unwrap().login, "alice");

    let bearer = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2.0/user"))
        .and(header("authorization", "Bearer ATBB-workspace"))
        .respond_with(ok(r#"{"username":"workspace"}"#))
        .expect(1)
        .mount(&bearer)
        .await;
    let client = BitbucketClient::new(&bearer.uri(), "ATBB-workspace", None, 120).unwrap();
    assert_eq!(client.viewer().await.unwrap().login, "workspace");
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
        .and(path("/acme/_apis/projects/platform"))
        .respond_with(ok(AZ_PROJECT))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/pullrequests")))
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
    Mock::given(method("GET"))
        .and(path(format!("{pr}/commits")))
        .respond_with(ok(AZ_COMMITS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/items")))
        .and(query_param("path", "/src/lib.rs"))
        .and(query_param("versionDescriptor.version", AZ_BASE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": "one\ntwo\nthree\n"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/items")))
        .and(query_param("path", "/src/lib.rs"))
        .and(query_param("versionDescriptor.version", AZ_HEAD))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": "one\ntwo\nTHREE\nfour\n"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/items")))
        .and(query_param("path", "/src/new.rs"))
        .and(query_param("versionDescriptor.version", AZ_HEAD))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": "hello\nnew\n"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/items")))
        .and(query_param("path", "/src/old.rs"))
        .and(query_param("versionDescriptor.version", AZ_BASE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": "bye\n"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/items")))
        .and(query_param("path", "/src/old_name.rs"))
        .and(query_param("versionDescriptor.version", AZ_BASE))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": "same\n"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{repo}/items")))
        .and(query_param("path", "/src/new_name.rs"))
        .and(query_param("versionDescriptor.version", AZ_HEAD))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": "same\nchanged\n"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/acme/platform/_apis/policy/evaluations"))
        .respond_with(ok(AZ_POLICIES))
        .mount(&server)
        .await;
    server
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
    assert_eq!(page.prs.len(), 1);
    assert_eq!(page.prs[0].state, ForgePrState::Open);
    assert_eq!(page.prs[0].head_branch, "alice/fix");
    assert_eq!(page.prs[0].base_branch, "main");
    assert_eq!(page.prs[0].author, "Alice Example");

    let pr = client.get_pr(&repo, 7).await.unwrap();
    assert_eq!(pr.body, "Fixes the widget and adds a regression test.");
    assert_eq!(pr.labels.len(), 1);
    assert_eq!(pr.mergeable, ForgeMergeable::Mergeable);
    assert_eq!(pr.additions, 5);
    assert_eq!(pr.deletions, 2);
    assert_eq!(pr.changed_files, 4);
    assert_eq!(pr.commits, 2);
    assert_eq!(pr.reviewers.len(), 3);
    assert_eq!(pr.summary.review, ForgeReviewState::ChangesRequested);
    assert_eq!(pr.merge_commit_sha, None);

    let diff = client.get_diff(&repo, 7, 1, 10).await.unwrap();
    assert_eq!(diff.files.len(), 4);
    let lib = diff.files.iter().find(|f| f.path == "src/lib.rs").unwrap();
    assert_eq!(lib.additions, 2);
    assert_eq!(lib.deletions, 1);
    assert!(lib.patch.as_deref().unwrap().contains("+THREE\n"));
    let added = diff.files.iter().find(|f| f.path == "src/new.rs").unwrap();
    assert_eq!(added.status, ForgeFileStatus::Added);
    assert!(added.patch.as_deref().unwrap().contains("--- /dev/null"));
    let deleted = diff.files.iter().find(|f| f.path == "src/old.rs").unwrap();
    assert_eq!(deleted.status, ForgeFileStatus::Deleted);
    assert!(deleted.patch.as_deref().unwrap().contains("+++ /dev/null"));
    let renamed = diff
        .files
        .iter()
        .find(|f| f.path == "src/new_name.rs")
        .unwrap();
    assert_eq!(renamed.status, ForgeFileStatus::Renamed);
    assert_eq!(renamed.previous_path.as_deref(), Some("src/old_name.rs"));

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
async fn bitbucket_writes_replies_comments_and_pull_requests() {
    let server = bitbucket_server().await;
    let api = "/2.0/repositories/acme/widget";
    Mock::given(method("POST"))
        .and(path(format!("{api}/pullrequests/42/comments")))
        .and(wiremock::matchers::body_partial_json(
            json!({"parent": {"id": 9100}}),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 9200,
            "content": {"raw": "Fixed."},
            "user": {"display_name": "Alice", "nickname": "alice"},
            "created_on": "2024-05-01T12:00:00Z",
            "inline": {"path": "src/lib.rs", "from": null, "to": 12}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{api}/pullrequests/42/comments")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 9201,
            "content": {"raw": "Thanks!"},
            "user": {"display_name": "Alice", "nickname": "alice"},
            "created_on": "2024-05-01T12:01:00Z"
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
            "publishedDate": "2024-05-01T12:00:00Z",
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
                "publishedDate": "2024-05-01T12:01:00Z",
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
    // An app password always uses Basic, even after the daemon restarts and
    // the account row carries a login.
    let basic = MockServer::start().await;
    let expected = base64::engine::general_purpose::STANDARD.encode("alice:s3cret");
    Mock::given(method("GET"))
        .and(path("/2.0/user"))
        .and(header("authorization", format!("Basic {expected}")))
        .respond_with(ok(r#"{"username":"alice"}"#))
        .mount(&basic)
        .await;
    let account = bitbucket_account(
        &basic,
        Some("alice"),
        h4rn_protocol::ForgeCredentialSource::BitbucketAppPassword,
    );
    let client = h4rn_forge::client_for(&account, "s3cret").unwrap();
    assert_eq!(client.viewer().await.unwrap().login, "alice");

    // A workspace token stays Bearer even if the stored row has a username.
    let bearer = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2.0/user"))
        .and(header("authorization", "Bearer ATBB-workspace"))
        .respond_with(ok(r#"{"username":"workspace"}"#))
        .mount(&bearer)
        .await;
    let account = bitbucket_account(
        &bearer,
        Some("workspace"),
        h4rn_protocol::ForgeCredentialSource::BitbucketAccessToken,
    );
    let client = h4rn_forge::client_for(&account, "ATBB-workspace").unwrap();
    assert_eq!(client.viewer().await.unwrap().login, "workspace");
}
