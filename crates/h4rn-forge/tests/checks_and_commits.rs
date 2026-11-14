//! Required marks, log links, re-run and the commits list, against recorded
//! API responses.
//!
//! The fixtures under `tests/fixtures/` are trimmed API responses with the
//! provider's own field names. The Gitea ones were recorded from the Docker
//! Gitea that `scripts/test-forges.sh` starts (and are re-checked against it
//! by the live test there); the GitHub and GitLab ones follow those
//! providers' published REST schemas.

use h4rn_forge::{Forge, ForgeError, GitHubClient, GitLabClient, GiteaClient};
use h4rn_protocol::{ForgeCheckStatus, ForgeKind, ForgeRepoRef};
use serde_json::Value;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(raw: &str) -> Value {
    serde_json::from_str(raw).expect("fixture is JSON")
}

fn ok(raw: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(fixture(raw))
}

fn repo(forge: ForgeKind, base_url: &str) -> ForgeRepoRef {
    ForgeRepoRef {
        forge,
        base_url: base_url.to_owned(),
        owner: "acme".into(),
        repo: "widget".into(),
    }
}

const GH_PR: &str = include_str!("fixtures/github_pr_42.json");
const GH_RUNS: &str = include_str!("fixtures/github_check_runs.json");
const GH_PROTECTION: &str = include_str!("fixtures/github_required_status_checks.json");
const GH_RULES: &str = include_str!("fixtures/github_branch_rules.json");
const GH_COMMITS: &str = include_str!("fixtures/github_pull_commits.json");
const GL_MR: &str = include_str!("fixtures/gitlab_mr_7.json");
const GL_JOBS: &str = include_str!("fixtures/gitlab_pipeline_jobs.json");
const GL_COMMITS: &str = include_str!("fixtures/gitlab_mr_commits.json");
const GT_PR: &str = include_str!("fixtures/gitea_pr_5.json");
const GT_STATUSES: &str = include_str!("fixtures/gitea_statuses.json");
const GT_PROTECTION: &str = include_str!("fixtures/gitea_branch_protection.json");
const GT_COMMITS: &str = include_str!("fixtures/gitea_pr_commits.json");

async fn github_server(protection: ResponseTemplate, rules: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    let base = "/api/v3/repos/acme/widget";
    Mock::given(method("GET"))
        .and(path(format!("{base}/pulls/42")))
        .respond_with(ok(GH_PR))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{base}/commits/9f2c1d0e8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d/check-runs"
        )))
        .respond_with(ok(GH_RUNS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{base}/commits/9f2c1d0e8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d/status"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"statuses": []})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "{base}/branches/main/protection/required_status_checks"
        )))
        .respond_with(protection)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{base}/rules/branches/main")))
        .respond_with(rules)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn github_marks_required_checks_and_links_the_log() {
    let server = github_server(ok(GH_PROTECTION), ok(GH_RULES)).await;
    let client = GitHubClient::new(&server.uri(), "ghp_test", 120).unwrap();
    let checks = client
        .list_checks(&repo(ForgeKind::GitHub, &server.uri()), 42)
        .await
        .unwrap();

    let by_name = |name: &str| checks.checks.iter().find(|c| c.name == name).unwrap();
    // `build` comes from branch protection, `test` from the ruleset: both
    // sources are read and merged.
    assert_eq!(by_name("build").required, Some(true));
    assert_eq!(by_name("test").required, Some(true));
    assert_eq!(by_name("lint").required, Some(false));
    // View log opens the check run's page, which shows the log.
    assert_eq!(
        by_name("test").details_url.as_deref(),
        Some("https://github.com/acme/widget/runs/1002")
    );
    assert_eq!(by_name("test").status, ForgeCheckStatus::Failure);
    assert!(checks.rerun.supported);
    assert!(checks.rerun.reason.is_none());
}

#[tokio::test]
async fn github_required_is_unknown_when_the_rules_cannot_be_read() {
    let forbidden = || {
        ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "message": "Resource not accessible by personal access token"
        }))
    };
    let server = github_server(forbidden(), forbidden()).await;
    let client = GitHubClient::new(&server.uri(), "ghp_test", 120).unwrap();
    let checks = client
        .list_checks(&repo(ForgeKind::GitHub, &server.uri()), 42)
        .await
        .unwrap();
    // Unknown is `None`, not `false`: the UI must not call a required check
    // optional because the token could not see the rule.
    assert!(checks.checks.iter().all(|check| check.required.is_none()));
}

#[tokio::test]
async fn github_rerun_rerequests_each_failing_suite_once() {
    let server = github_server(ok(GH_PROTECTION), ok(GH_RULES)).await;
    // `test` and `lint` share suite 12: one request, not two. Passing and
    // skipped suites are left alone (their mocks are absent, so a stray call
    // would fail the test with a 404).
    Mock::given(method("POST"))
        .and(path("/api/v3/repos/acme/widget/check-suites/12/rerequest"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;
    let client = GitHubClient::new(&server.uri(), "ghp_test", 120).unwrap();
    let requested = client
        .rerun_failed_checks(&repo(ForgeKind::GitHub, &server.uri()), 42)
        .await
        .unwrap();
    assert_eq!(requested, 1);
}

#[tokio::test]
async fn github_rerun_with_nothing_failed_is_a_conflict() {
    let server = github_server(ok(GH_PROTECTION), ok(GH_RULES)).await;
    let mut runs = fixture(GH_RUNS);
    for run in runs["check_runs"].as_array_mut().unwrap() {
        run["conclusion"] = Value::String("success".into());
    }
    // A later, more specific mock wins over the shared one.
    Mock::given(method("GET"))
        .and(path_regex(r"/check-runs$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(runs))
        .with_priority(1)
        .mount(&server)
        .await;
    let client = GitHubClient::new(&server.uri(), "ghp_test", 120).unwrap();
    let error = client
        .rerun_failed_checks(&repo(ForgeKind::GitHub, &server.uri()), 42)
        .await
        .unwrap_err();
    assert!(matches!(error, ForgeError::Conflict(_)), "{error:?}");
}

#[tokio::test]
async fn github_lists_commits_oldest_first_with_title_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v3/repos/acme/widget/pulls/42/commits"))
        .respond_with(ok(GH_COMMITS))
        .mount(&server)
        .await;
    let client = GitHubClient::new(&server.uri(), "ghp_test", 120).unwrap();
    let commits = client
        .list_commits(&repo(ForgeKind::GitHub, &server.uri()), 42)
        .await
        .unwrap();
    assert_eq!(commits.commits.len(), 2);
    assert!(!commits.truncated);
    let first = &commits.commits[0];
    assert_eq!(first.title, "Add a failing test");
    assert_eq!(
        first.body.as_deref(),
        Some("Reproduces the panic on empty input.")
    );
    assert_eq!(first.author, "Alice Example");
    assert_eq!(first.author_login.as_deref(), Some("alice"));
    assert!(first.authored_at.is_some());
    assert!(
        first
            .web_url
            .as_deref()
            .is_some_and(|url| url.ends_with("1111111111111111111111111111111111111111"))
    );
    assert_eq!(commits.commits[1].title, "Fix the panic");
    assert!(commits.commits[1].body.is_none());
}

async fn gitlab_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v4/projects/[^/]+/merge_requests/7$"))
        .respond_with(ok(GL_MR))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v4/projects/[^/]+/pipelines/77/jobs$"))
        .respond_with(ok(GL_JOBS))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn gitlab_requires_jobs_that_may_not_fail() {
    let server = gitlab_server().await;
    let client = GitLabClient::new(&server.uri(), "glpat-test", 120).unwrap();
    let checks = client
        .list_checks(&repo(ForgeKind::GitLab, &server.uri()), 7)
        .await
        .unwrap();
    let by_name = |name: &str| checks.checks.iter().find(|c| c.name == name).unwrap();
    assert_eq!(by_name("build").required, Some(true));
    assert_eq!(by_name("test").required, Some(true));
    assert_eq!(
        by_name("lint").required,
        Some(false),
        "allow_failure jobs are optional"
    );
    assert_eq!(
        by_name("test").details_url.as_deref(),
        Some("https://gitlab.example.test/acme/widget/-/jobs/902")
    );
    assert!(checks.rerun.supported);
}

#[tokio::test]
async fn gitlab_rerun_retries_the_head_pipeline() {
    let server = gitlab_server().await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/v4/projects/[^/]+/pipelines/77/retry$"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(serde_json::json!({"id": 77, "status": "running"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = GitLabClient::new(&server.uri(), "glpat-test", 120).unwrap();
    let requested = client
        .rerun_failed_checks(&repo(ForgeKind::GitLab, &server.uri()), 7)
        .await
        .unwrap();
    // Two jobs failed (`test` and the allow-failure `lint`); the pipeline retry
    // re-queues both.
    assert_eq!(requested, 2);
}

#[tokio::test]
async fn gitlab_commits_are_reversed_to_oldest_first() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/api/v4/projects/[^/]+/merge_requests/7/commits$",
        ))
        .respond_with(ok(GL_COMMITS))
        .mount(&server)
        .await;
    let client = GitLabClient::new(&server.uri(), "glpat-test", 120).unwrap();
    let commits = client
        .list_commits(&repo(ForgeKind::GitLab, &server.uri()), 7)
        .await
        .unwrap();
    let titles: Vec<&str> = commits.commits.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["Add a failing test", "Fix the panic"]);
    assert_eq!(
        commits.commits[0].body.as_deref(),
        Some("Reproduces the panic on empty input.")
    );
    assert_eq!(commits.commits[0].author, "Alice Example");
}

async fn gitea_server(protection: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    let base = "/api/v1/repos/acme/widget";
    Mock::given(method("GET"))
        .and(path(format!("{base}/pulls/5")))
        .respond_with(ok(GT_PR))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{base}/commits/abc123/statuses")))
        .respond_with(ok(GT_STATUSES))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{base}/branch_protections/main")))
        .respond_with(protection)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn gitea_marks_required_statuses_from_branch_protection() {
    for kind in [ForgeKind::Gitea, ForgeKind::Forgejo] {
        let server = gitea_server(ok(GT_PROTECTION)).await;
        let client = GiteaClient::new(kind, &server.uri(), "token", 120).unwrap();
        let checks = client
            .list_checks(&repo(kind, &server.uri()), 5)
            .await
            .unwrap();
        let by_name = |name: &str| checks.checks.iter().find(|c| c.name == name).unwrap();
        assert_eq!(by_name("ci/test").required, Some(true));
        assert_eq!(by_name("ci/lint").required, Some(false));
        assert_eq!(
            by_name("ci/test").details_url.as_deref(),
            Some("https://gitea.example.test/acme/widget/actions/runs/9/jobs/1")
        );
        // Commit statuses have no re-run API: the action is hidden, with the
        // reason the tooltip shows.
        assert!(!checks.rerun.supported);
        assert!(
            checks
                .rerun
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("commit statuses")),
            "{:?}",
            checks.rerun
        );
        let error = client
            .rerun_failed_checks(&repo(kind, &server.uri()), 5)
            .await
            .unwrap_err();
        assert!(matches!(error, ForgeError::Unsupported(_)), "{error:?}");
    }
}

#[tokio::test]
async fn gitea_required_is_unknown_without_admin_access_to_the_rule() {
    let server = gitea_server(ResponseTemplate::new(403).set_body_json(serde_json::json!({
        "message": "token does not have at least one of required scope(s)"
    })))
    .await;
    let client = GiteaClient::new(ForgeKind::Gitea, &server.uri(), "token", 120).unwrap();
    let checks = client
        .list_checks(&repo(ForgeKind::Gitea, &server.uri()), 5)
        .await
        .unwrap();
    assert!(checks.checks.iter().all(|check| check.required.is_none()));
}

#[tokio::test]
async fn gitea_commits_are_reversed_to_oldest_first() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/repos/acme/widget/pulls/5/commits"))
        .respond_with(ok(GT_COMMITS))
        .mount(&server)
        .await;
    let client = GiteaClient::new(ForgeKind::Gitea, &server.uri(), "token", 120).unwrap();
    let commits = client
        .list_commits(&repo(ForgeKind::Gitea, &server.uri()), 5)
        .await
        .unwrap();
    // Recorded from Gitea 1.22, which answers newest first.
    let titles: Vec<&str> = commits.commits.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["add the feature file", "add the second file"]);
    assert_eq!(
        commits.commits[1].body.as_deref(),
        Some("It has a body line.")
    );
    assert_eq!(
        commits.commits[1].author_login.as_deref(),
        Some("h4rn-admin")
    );
    assert_eq!(commits.commits[1].sha.len(), 40);
}
