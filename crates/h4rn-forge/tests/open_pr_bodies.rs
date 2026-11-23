//! What each forge receives when a pull request is opened, draft or not.
//!
//! GitHub, Bitbucket and Azure carry a draft boolean; GitLab and Gitea mark a
//! draft with a title prefix (`Draft:` and `WIP:` respectively) and have no
//! such field. A Gitea create request that sends `draft` is ignored by the
//! server, so the title is the only thing that makes the request a draft.

use h4rn_forge::{
    AzureDevOpsClient, BitbucketClient, Forge, GitHubClient, GitLabClient, GiteaClient,
};
use h4rn_protocol::{ForgeKind, ForgeNewPr, ForgeRepoRef};
use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn repo(forge: ForgeKind, base_url: &str) -> ForgeRepoRef {
    ForgeRepoRef {
        forge,
        base_url: base_url.to_owned(),
        owner: "acme".into(),
        repo: "widget".into(),
    }
}

fn request(draft: bool) -> ForgeNewPr {
    ForgeNewPr {
        title: "Fix the widget".into(),
        body: "Fixes the widget.".into(),
        head: "feature/widget".into(),
        base: "main".into(),
        draft,
    }
}

/// GitHub: a `draft` boolean, true or false.
#[tokio::test]
async fn github_sends_a_draft_boolean() {
    for draft in [true, false] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v3/repos/acme/widget/pulls"))
            .and(body_json(json!({
                "title": "Fix the widget",
                "body": "Fixes the widget.",
                "head": "feature/widget",
                "base": "main",
                "draft": draft,
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "number": 42,
                "title": "Fix the widget",
                "state": "open",
                "draft": draft,
                "user": {"login": "alice"},
                "head": {"ref": "feature/widget", "sha": "abc123"},
                "base": {"ref": "main", "sha": "def456"},
                "updated_at": "2024-05-01T12:00:00Z",
                "html_url": "https://github.com/acme/widget/pull/42",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = GitHubClient::new(&server.uri(), "token", 120).unwrap();
        let repo = repo(ForgeKind::GitHub, &server.uri());
        let pr = client.open_pr(&repo, &request(draft)).await.unwrap();
        assert_eq!(pr.summary.number, 42);
    }
}

/// GitLab: no draft field; `Draft:` on the title, and never twice.
#[tokio::test]
async fn gitlab_prefixes_the_title_once_for_a_draft() {
    for draft in [true, false] {
        let server = MockServer::start().await;
        let expected = if draft {
            "Draft: Fix the widget"
        } else {
            "Fix the widget"
        };
        Mock::given(method("POST"))
            .and(path("/api/v4/projects/acme%2Fwidget/merge_requests"))
            .and(body_json(json!({
                "source_branch": "feature/widget",
                "target_branch": "main",
                "title": expected,
                "description": "Fixes the widget.",
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "iid": 42,
                "title": expected,
                "state": "opened",
                "draft": draft,
                "author": {"username": "alice"},
                "source_branch": "feature/widget",
                "target_branch": "main",
                "sha": "abc123",
                "updated_at": "2024-05-01T12:00:00Z",
                "web_url": "https://gitlab.com/acme/widget/-/merge_requests/42",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = GitLabClient::new(&server.uri(), "token", 120).unwrap();
        let repo = repo(ForgeKind::GitLab, &server.uri());
        let pr = client.open_pr(&repo, &request(draft)).await.unwrap();
        assert_eq!(pr.summary.number, 42);
    }
}

#[tokio::test]
async fn gitlab_does_not_prefix_a_title_that_already_says_draft() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v4/projects/acme%2Fwidget/merge_requests"))
        .and(body_json(json!({
            "source_branch": "feature/widget",
            "target_branch": "main",
            "title": "Draft: Fix the widget",
            "description": "Fixes the widget.",
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "iid": 42,
            "title": "Draft: Fix the widget",
            "state": "opened",
            "draft": true,
            "author": {"username": "alice"},
            "source_branch": "feature/widget",
            "target_branch": "main",
            "sha": "abc123",
            "updated_at": "2024-05-01T12:00:00Z",
            "web_url": "https://gitlab.com/acme/widget/-/merge_requests/42",
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = GitLabClient::new(&server.uri(), "token", 120).unwrap();
    let repo = repo(ForgeKind::GitLab, &server.uri());
    let mut draft_request = request(true);
    draft_request.title = "Draft: Fix the widget".into();
    client.open_pr(&repo, &draft_request).await.unwrap();
}

/// Gitea: no draft field; `WIP:` on the title, and never twice.
#[tokio::test]
async fn gitea_prefixes_the_title_once_for_a_draft_and_sends_no_draft_field() {
    for draft in [true, false] {
        let server = MockServer::start().await;
        let expected = if draft {
            "WIP: Fix the widget"
        } else {
            "Fix the widget"
        };
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/acme/widget/pulls"))
            .and(body_json(json!({
                "title": expected,
                "body": "Fixes the widget.",
                "head": "feature/widget",
                "base": "main",
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "number": 42,
                "title": expected,
                "state": "open",
                "draft": draft,
                "user": {"login": "alice"},
                "head": {"ref": "feature/widget", "sha": "abc123"},
                "base": {"ref": "main", "sha": "def456"},
                "updated_at": "2024-05-01T12:00:00Z",
                "html_url": "https://gitea.example.test/acme/widget/pulls/42",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = GiteaClient::new(ForgeKind::Gitea, &server.uri(), "token", 120).unwrap();
        let repo = repo(ForgeKind::Gitea, &server.uri());
        let pr = client.open_pr(&repo, &request(draft)).await.unwrap();
        assert_eq!(pr.summary.number, 42);
    }
}

#[tokio::test]
async fn gitea_keeps_an_existing_wip_prefix() {
    for title in [
        "WIP: Fix the widget",
        "[WIP] Fix the widget",
        "wip: Fix the widget",
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/repos/acme/widget/pulls"))
            .and(body_json(json!({
                "title": title,
                "body": "Fixes the widget.",
                "head": "feature/widget",
                "base": "main",
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "number": 42,
                "title": title,
                "state": "open",
                "draft": true,
                "user": {"login": "alice"},
                "head": {"ref": "feature/widget", "sha": "abc123"},
                "base": {"ref": "main", "sha": "def456"},
                "updated_at": "2024-05-01T12:00:00Z",
                "html_url": "https://gitea.example.test/acme/widget/pulls/42",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = GiteaClient::new(ForgeKind::Gitea, &server.uri(), "token", 120).unwrap();
        let repo = repo(ForgeKind::Gitea, &server.uri());
        let mut draft_request = request(true);
        draft_request.title = title.into();
        client.open_pr(&repo, &draft_request).await.unwrap();
    }
}

/// Bitbucket Cloud: a `draft` boolean.
#[tokio::test]
async fn bitbucket_sends_a_draft_boolean() {
    for draft in [true, false] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/2.0/repositories/acme/widget/pullrequests"))
            .and(body_json(json!({
                "title": "Fix the widget",
                "description": "Fixes the widget.",
                "source": {"branch": {"name": "feature/widget"}},
                "destination": {"branch": {"name": "main"}},
                "close_source_branch": false,
                "draft": draft,
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": 42,
                "title": "Fix the widget",
                "state": "OPEN",
                "draft": draft,
                "author": {"display_name": "Alice Example", "nickname": "alice"},
                "source": {"branch": {"name": "feature/widget"}, "commit": {"hash": "aaaa1111"}},
                "destination": {"branch": {"name": "main"}, "commit": {"hash": "bbbb1111"}},
                "created_on": "2024-05-01T09:00:00+00:00",
                "updated_on": "2024-05-01T12:00:00+00:00",
                "links": {"html": {"href": "https://bitbucket.org/acme/widget/pull-requests/42"}},
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = BitbucketClient::new(&server.uri(), "token", None, 120).unwrap();
        let repo = repo(ForgeKind::Bitbucket, &server.uri());
        let pr = client.open_pr(&repo, &request(draft)).await.unwrap();
        assert_eq!(pr.summary.number, 42);
    }
}

/// Azure DevOps: `isDraft`, true or false.
#[tokio::test]
async fn azure_sends_an_isdraft_boolean() {
    for draft in [true, false] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(
                "/acme/platform/_apis/git/repositories/widget/pullrequests",
            ))
            .and(body_json(json!({
                "sourceRefName": "refs/heads/feature/widget",
                "targetRefName": "refs/heads/main",
                "title": "Fix the widget",
                "description": "Fixes the widget.",
                "isDraft": draft,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "pullRequestId": 42,
                "title": "Fix the widget",
                "status": "active",
                "isDraft": draft,
                "createdBy": {"displayName": "Alice Example", "uniqueName": "alice@example.test"},
                "sourceRefName": "refs/heads/feature/widget",
                "targetRefName": "refs/heads/main",
                "creationDate": "2024-05-01T09:00:00Z",
                "lastMergeSourceCommit": {"commitId": "aaaa1111"},
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = AzureDevOpsClient::new(&format!("{}/acme", server.uri()), "pat", 120).unwrap();
        let repo = ForgeRepoRef {
            forge: ForgeKind::AzureDevOps,
            base_url: format!("{}/acme", server.uri()),
            owner: "platform".into(),
            repo: "widget".into(),
        };
        let pr = client.open_pr(&repo, &request(draft)).await.unwrap();
        assert_eq!(pr.summary.number, 42);
    }
}
