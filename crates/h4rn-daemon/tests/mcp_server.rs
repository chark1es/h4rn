//! End-to-end coverage for h4rn as an MCP server (ADR-044).
//!
//! The daemon is real: a FakeHarness session receives the per-thread token,
//! OAuth runs over the daemon's HTTP listener, and every tool reads the same
//! state the UI reads. Nothing here reaches a network beyond loopback.

use std::sync::Arc;
use std::time::{Duration, Instant};

use h4rn_core::LifecycleSettings;
use h4rn_daemon::config::Config;
use h4rn_daemon::daemon::{BoundDaemon, DaemonBuilder};
use h4rn_daemon::mcp_server::OutsideAgents;
use h4rn_daemon::threads::{NewThread, ThreadManager, ThreadManagerOptions};
use h4rn_harness::{FakeHarness, SessionMcp};
use h4rn_protocol::{
    ApprovalDecision, HarnessKind, HostId, McpRequest, OutsideAgentLevel, ResponseBody,
    RuntimeMode, TurnStatus, Usage,
};
use reqwest::StatusCode;
use reqwest::redirect::Policy;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use url::Url;

struct TestDaemon {
    addr: std::net::SocketAddr,
    addrs: Vec<std::net::SocketAddr>,
    dir: tempfile::TempDir,
    manager: Arc<ThreadManager>,
    outside: Arc<OutsideAgents>,
    mcp_service: Arc<h4rn_daemon::mcp::McpService>,
    host: HostId,
    mcp: SessionMcp,
    settings: h4rn_harness::SessionSettings,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl TestDaemon {
    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn mcp_url(&self) -> String {
        format!("{}/mcp", self.base())
    }

    fn outside_url(&self) -> String {
        format!("{}/mcp", self.base())
    }

    fn data_dir(&self) -> &std::path::Path {
        self.dir.path()
    }

    fn outside_agents(&self) -> Arc<h4rn_daemon::mcp::McpService> {
        Arc::clone(&self.mcp_service)
    }

    async fn stop(mut self) {
        self.manager.shutdown().await;
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.task.await;
    }
}

async fn boot(rate_limit: Option<usize>) -> TestDaemon {
    boot_with(FakeHarness::new(), rate_limit, None).await
}

async fn boot_with(
    fake: FakeHarness,
    rate_limit: Option<usize>,
    extra_bind: Option<std::net::SocketAddr>,
) -> TestDaemon {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        bind: "127.0.0.1:0".parse().expect("bind"),
        extra_bind,
        log_level: "off".to_owned(),
        tailscale: h4rn_daemon::remote::TailscaleMode::Auto,
        network_access: None,
        network_port: 0,
    };
    let options = ThreadManagerOptions {
        settings: LifecycleSettings::default()
            .with_working_exit_grace(chrono::TimeDelta::milliseconds(80))
            .with_stale_bypass(chrono::TimeDelta::seconds(60)),
        tick_interval: Duration::from_millis(20),
        clock: None,
    };
    let (fake, mcp) = fake.with_mcp_injection();
    let (fake, settings) = fake.with_session_settings();
    let daemon = DaemonBuilder::new(config)
        .without_title_agents()
        .with_harness(Arc::new(fake))
        .with_lifecycle(options)
        .build()
        .await
        .expect("build daemon");
    let manager = daemon.manager();
    let mcp_service = daemon.mcp();
    let bound: BoundDaemon = daemon.bind().await.expect("bind daemon");
    let addr = bound.local_addr();
    let addrs = bound.addrs().to_vec();
    let outside = bound.outside_agents();
    if let Some(limit) = rate_limit {
        outside.set_limits(h4rn_daemon::mcp_server::rate::RateLimiter::new(
            limit,
            Duration::from_secs(60),
        ));
    }
    let hosts = bound.host_inventory();
    let host = hosts.local_id();
    let (shutdown, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        bound
            .serve(async move {
                let _ = rx.await;
            })
            .await
    });
    TestDaemon {
        addr,
        addrs,
        dir,
        manager,
        outside,
        mcp_service,
        host,
        mcp,
        settings,
        shutdown: Some(shutdown),
        task,
    }
}

fn git_repo_with_remote() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@h4rn.invalid"]);
    run(&["config", "user.name", "h4rn test"]);
    std::fs::write(dir.path().join("README.md"), "# test\n").expect("write");
    run(&["add", "README.md"]);
    run(&["commit", "-q", "-m", "init"]);
    run(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/widgets.git",
    ]);
    dir
}

fn new_thread(host: &HostId, workspace: &std::path::Path, prompt: &str) -> NewThread {
    NewThread {
        thread_id: None,
        parent_thread_id: None,
        subagent_id: None,
        host: host.clone(),
        harness: HarnessKind::ClaudeCode,
        agent_id: None,
        workspace_path: workspace.display().to_string(),
        model: "default".into(),
        prompt: prompt.into(),
        attachments: Vec::new(),
        runtime_mode: RuntimeMode::ApprovalRequired,
        reasoning_effort: None,
        fast: false,
        worktree: None,
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client")
}

async fn post_rpc(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = client
        .post(url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("send");
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

async fn initialize(client: &reqwest::Client, url: &str, token: &str) -> Value {
    let (status, body) = post_rpc(
        client,
        url,
        token,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "initialize body: {body}");
    body["result"].clone()
}

async fn list_tools(client: &reqwest::Client, url: &str, token: &str) -> Vec<String> {
    let (status, body) = post_rpc(
        client,
        url,
        token,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tools/list body: {body}");
    body["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect()
}

async fn call_tool(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    name: &str,
    arguments: Value,
) -> Value {
    let (status, body) = post_rpc(
        client,
        url,
        token,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "tools/call body: {body}");
    body
}

async fn wait_for_injection(mcp: &SessionMcp, count: usize) -> Vec<h4rn_harness::McpInjection> {
    let started = Instant::now();
    loop {
        let injections = mcp.get();
        if injections.len() >= count {
            return injections;
        }
        if started.elapsed() > Duration::from_secs(10) {
            panic!("the fake harness never received an MCP injection: {injections:?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_fake_session_receives_a_token_bound_to_its_thread() {
    let daemon = boot(None).await;
    let repo = git_repo_with_remote();
    let first = daemon
        .manager
        .create_thread(new_thread(&daemon.host, repo.path(), "first"))
        .await
        .expect("create first");
    let second = daemon
        .manager
        .create_thread(new_thread(&daemon.host, repo.path(), "second"))
        .await
        .expect("create second");
    let injections = wait_for_injection(&daemon.mcp, 2).await;
    let injection = injections
        .iter()
        .find(|injection| injection.endpoint.as_deref() == Some(daemon.mcp_url().as_str()))
        .expect("the daemon injected its own hub endpoint");
    let token = injection.token.as_deref().expect("a session token");
    assert!(token.starts_with("h4rn_mcp_"));
    assert!(!token.is_empty(), "the token must not be blank");

    let client = http();
    let init = initialize(&client, &daemon.mcp_url(), token).await;
    assert_eq!(init["serverInfo"]["name"], "h4rn");
    let tools = list_tools(&client, &daemon.mcp_url(), token).await;
    assert!(
        tools.iter().any(|tool| tool == "h4rn__thread_status"),
        "tools: {tools:?}"
    );
    let status = call_tool(
        &client,
        &daemon.mcp_url(),
        token,
        "h4rn__thread_status",
        json!({}),
    )
    .await;
    let thread_id = status["result"]["structuredContent"]["thread_id"]
        .as_str()
        .expect("structured thread id");
    assert!(
        [first.thread_id.as_str(), second.thread_id.as_str()].contains(&thread_id),
        "token answered for a thread it does not own: {thread_id}"
    );

    // The two sessions got different tokens, and each answers for its own
    // thread; there is no argument that names another thread.
    let tokens: Vec<_> = injections
        .iter()
        .filter_map(|injection| injection.token.as_deref())
        .collect();
    assert_eq!(tokens.len(), 2);
    assert_ne!(tokens[0], tokens[1]);
    let other = tokens
        .iter()
        .find(|candidate| **candidate != token)
        .expect("the other token");
    let other_status = call_tool(
        &client,
        &daemon.mcp_url(),
        other,
        "h4rn__thread_status",
        json!({"thread_id": "th_other"}),
    )
    .await;
    let other_thread = other_status["result"]["structuredContent"]["thread_id"]
        .as_str()
        .expect("thread id");
    assert_ne!(other_thread, thread_id);
    daemon.stop().await;
}

#[tokio::test]
async fn the_hub_lists_h4rn_tools_and_reports_unknown_tools() {
    let daemon = boot(None).await;
    let repo = git_repo_with_remote();
    let thread = daemon
        .manager
        .create_thread(new_thread(&daemon.host, repo.path(), "hello"))
        .await
        .expect("create thread");
    let injections = wait_for_injection(&daemon.mcp, 1).await;
    let token = injections[0].token.as_deref().expect("token");
    let client = http();
    let tools = list_tools(&client, &daemon.mcp_url(), token).await;
    for expected in [
        "h4rn__link_pull_request",
        "h4rn__unlink_pull_request",
        "h4rn__list_thread_pull_requests",
        "h4rn__watch_pull_request",
        "h4rn__unwatch_pull_request",
        "h4rn__thread_status",
        "h4rn__list_forge_pull_requests",
    ] {
        assert!(
            tools.iter().any(|tool| tool == expected),
            "{expected} missing"
        );
    }
    let unknown = call_tool(
        &client,
        &daemon.mcp_url(),
        token,
        "h4rn__does_not_exist",
        json!({}),
    )
    .await;
    assert!(
        unknown.get("error").is_some(),
        "an unknown tool must be a JSON-RPC error: {unknown}"
    );
    let _ = thread;
    daemon.stop().await;
}

async fn register(client: &reqwest::Client, base: &str, redirect: &str) -> (StatusCode, Value) {
    let response = client
        .post(format!("{base}/oauth/register"))
        .json(&json!({"client_name": "Claude Code", "redirect_uris": [redirect]}))
        .send()
        .await
        .expect("register");
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

async fn begin_and_approve(
    daemon: &TestDaemon,
    client: &reqwest::Client,
    base: &str,
    client_id: &str,
    redirect: &str,
    level: &str,
) -> String {
    let (verifier, challenge) = pkce();
    let request = open_authorize(client, base, client_id, redirect, &challenge).await;
    // Approval runs over the authenticated h4rn session, never over the page.
    daemon
        .outside
        .respond_sign_in(
            &request,
            h4rn_protocol::ApprovalDecision::Approve,
            h4rn_protocol::OutsideAgentLevel::parse(level),
        )
        .expect("the owner approves the sign-in");
    let code = follow_pending(client, base, &request).await;
    token_exchange(client, base, client_id, &code, &verifier).await
}

/// Opens the client's authorize page and returns the request id it names.
async fn open_authorize(
    client: &reqwest::Client,
    base: &str,
    client_id: &str,
    redirect: &str,
    challenge: &str,
) -> String {
    let authorize = client
        .get(format!("{base}/oauth/authorize"))
        .query(&[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect),
            ("state", "state-1"),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .expect("authorize");
    assert_eq!(authorize.status(), StatusCode::OK);
    let page = authorize.text().await.expect("page");
    assert!(page.contains("Claude Code"));
    assert!(page.contains("unverified"));
    assert!(page.contains("http://localhost:1455"));
    assert!(
        !page.contains("<form"),
        "the page must not be able to grant anything"
    );
    page.split("name=\"request\" value=\"")
        .nth(1)
        .or_else(|| {
            page.split("request=")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
        })
        .expect("approval request id")
        .to_owned()
}

/// Follows the page's own navigation to the callback and returns the code.
async fn follow_pending(client: &reqwest::Client, base: &str, request: &str) -> String {
    let response = client
        .get(format!("{base}/oauth/authorize/pending"))
        .query(&[("request", request)])
        .send()
        .await
        .expect("pending");
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "an answered sign-in navigates to the callback"
    );
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .expect("location")
        .to_str()
        .expect("location text")
        .to_owned();
    let url = Url::parse(&location).expect("location url");
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned());
    assert!(
        matches!(state.as_deref(), None | Some("state-1")),
        "unexpected state {state:?}"
    );
    url.query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .expect("code")
}

async fn token_exchange(
    client: &reqwest::Client,
    base: &str,
    client_id: &str,
    code: &str,
    verifier: &str,
) -> String {
    // OAuth 2.1: the token request needs no redirect_uri; PKCE binds it.
    let token_response = client
        .post(format!("{base}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .expect("token");
    assert_eq!(token_response.status(), StatusCode::OK);
    let body: Value = token_response.json().await.expect("token json");
    body["access_token"]
        .as_str()
        .expect("access token")
        .to_owned()
}

fn pkce() -> (String, String) {
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_owned();
    let challenge = h4rn_daemon::mcp_server::url::s256(&verifier);
    (verifier, challenge)
}

#[tokio::test]
async fn oauth_pkce_grants_the_approved_level_and_stores_hashes() {
    let daemon = boot(None).await;
    let client = http();
    let redirect = "http://localhost:1455/callback";
    let (status, registered) = register(&client, &daemon.base(), redirect).await;
    assert_eq!(status, StatusCode::CREATED, "{registered}");
    let client_id = registered["client_id"].as_str().expect("client id");

    let read_token = begin_and_approve(
        &daemon,
        &client,
        &daemon.base(),
        client_id,
        redirect,
        "read_only",
    )
    .await;
    assert!(read_token.starts_with("h4rn_at_"));
    let tools = list_tools(&client, &daemon.outside_url(), &read_token).await;
    assert!(tools.contains(&"list_projects".to_owned()));
    assert!(!tools.contains(&"start_thread".to_owned()));
    let refused = call_tool(
        &client,
        &daemon.outside_url(),
        &read_token,
        "start_thread",
        json!({"harness": "claude_code", "model": "default", "prompt": "hi", "workspace_path": "/tmp"}),
    )
    .await;
    assert!(
        refused.get("error").is_some(),
        "read-only start_thread must be refused: {refused}"
    );

    // The store keeps digests, never the tokens themselves.
    let raw = std::fs::read_to_string(daemon.data_dir().join("outside_agents.json"))
        .expect("outside-agent store");
    assert!(
        !raw.contains(&read_token),
        "the access token leaked to disk"
    );
    assert!(
        raw.contains(&h4rn_daemon::mcp_server::oauth::digest_for_test(
            &read_token
        ))
    );

    // A supervised grant can start a thread, but it always runs at
    // approval-required even when the caller asks for full access.
    let supervised = begin_and_approve(
        &daemon,
        &client,
        &daemon.base(),
        client_id,
        redirect,
        "supervised",
    )
    .await;
    let repo = git_repo_with_remote();
    let started = call_tool(
        &client,
        &daemon.outside_url(),
        &supervised,
        "start_thread",
        json!({
            "harness": "claude_code",
            "model": "default",
            "prompt": "do the thing",
            "workspace_path": repo.path().display().to_string(),
            "runtime_mode": "full_access"
        }),
    )
    .await;
    assert!(
        started.get("error").is_none(),
        "supervised start_thread failed: {started}"
    );
    assert_eq!(
        started["result"]["structuredContent"]["runtime_mode"],
        "approval_required"
    );

    // Revocation cuts the token off immediately.
    let revoked = client
        .post(format!("{}/oauth/revoke", daemon.base()))
        .form(&[("token", read_token.as_str()), ("client_id", client_id)])
        .send()
        .await
        .expect("revoke");
    assert_eq!(revoked.status(), StatusCode::OK);
    let (status, body) = post_rpc(
        &client,
        &daemon.outside_url(),
        &read_token,
        json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    daemon.stop().await;
}

#[tokio::test]
async fn oauth_refuses_redirects_outside_https_and_localhost() {
    let daemon = boot(None).await;
    let client = http();
    let (status, body) = register(&client, &daemon.base(), "http://example.com/callback").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_request");
    let (status, registered) =
        register(&client, &daemon.base(), "https://chatgpt.com/callback").await;
    assert_eq!(status, StatusCode::CREATED);
    let client_id = registered["client_id"].as_str().expect("client id");
    let (verifier, challenge) = pkce();
    let _ = verifier;
    let response = client
        .get(format!("{}/oauth/authorize", daemon.base()))
        .query(&[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", "http://localhost:1455/other"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .expect("authorize");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    daemon.stop().await;
}

#[tokio::test]
async fn denying_the_page_returns_access_denied_and_issues_no_code() {
    let daemon = boot(None).await;
    let client = http();
    let redirect = "http://localhost:1455/callback";
    let (_, registered) = register(&client, &daemon.base(), redirect).await;
    let client_id = registered["client_id"].as_str().expect("client id");
    let (_, challenge) = pkce();
    let page = client
        .get(format!("{}/oauth/authorize", daemon.base()))
        .query(&[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect),
            ("state", "s"),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .expect("authorize")
        .text()
        .await
        .expect("page");
    let request = page
        .split("request=")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("request id")
        .to_owned();
    // The person denies it inside h4rn.
    daemon
        .outside
        .respond_sign_in(&request, h4rn_protocol::ApprovalDecision::Deny, None)
        .expect("deny");
    let response = client
        .get(format!("{}/oauth/authorize/pending", daemon.base()))
        .query(&[("request", request.as_str())])
        .send()
        .await
        .expect("pending");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .expect("location")
        .to_str()
        .expect("location");
    assert!(location.contains("error=access_denied"), "{location}");
    assert!(!location.contains("code="), "{location}");
    daemon.stop().await;
}

#[tokio::test]
async fn oauth_mutations_are_rate_limited_in_aggregate_before_parsing() {
    let daemon = boot(None).await;
    daemon
        .outside
        .set_aggregate_limits(h4rn_daemon::mcp_server::rate::RateLimiter::new(
            1,
            Duration::from_secs(60),
        ));
    let client = http();
    let redirect = "http://localhost:1455/callback";
    let (first, _) = register(&client, &daemon.base(), redirect).await;
    assert_eq!(first, StatusCode::CREATED);
    // A body that would not parse is still refused before it is read: the
    // refusal comes from the aggregate limiter, not from parsing.
    let response = client
        .post(format!("{}/oauth/register", daemon.base()))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body("{ not json")
        .send()
        .await
        .expect("register");
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        response
            .headers()
            .contains_key(reqwest::header::RETRY_AFTER)
    );
    daemon.stop().await;
}

#[tokio::test]
async fn settings_report_connected_agents_and_revoke_them() {
    let daemon = boot(None).await;
    let client = http();
    let redirect = "http://localhost:1455/callback";
    let (_, registered) = register(&client, &daemon.base(), redirect).await;
    let client_id = registered["client_id"].as_str().expect("client id");
    let token = begin_and_approve(
        &daemon,
        &client,
        &daemon.base(),
        client_id,
        redirect,
        "read_only",
    )
    .await;
    let agents = daemon.outside.store().connected_agents();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].name, "Claude Code");
    assert_eq!(agents[0].level, h4rn_protocol::OutsideAgentLevel::ReadOnly);

    // The protocol surface the Settings page uses.
    match daemon
        .outside
        .store()
        .connected_agents()
        .first()
        .map(|agent| agent.client_id.clone())
    {
        Some(id) => {
            assert!(daemon.outside.store().revoke_client(&id).is_ok());
        }
        None => panic!("no connected agent"),
    }
    let (status, _) = post_rpc(
        &client,
        &daemon.outside_url(),
        &token,
        json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(daemon.outside.store().connected_agents().is_empty());
    daemon.stop().await;
}

#[tokio::test]
async fn the_protocol_lists_and_revokes_outside_agents() {
    let daemon = boot(None).await;
    let client = http();
    let redirect = "http://localhost:1455/callback";
    let (_, registered) = register(&client, &daemon.base(), redirect).await;
    let client_id = registered["client_id"].as_str().expect("client id");
    let _token = begin_and_approve(
        &daemon,
        &client,
        &daemon.base(),
        client_id,
        redirect,
        "read_only",
    )
    .await;

    let listed = daemon
        .outside_agents()
        .handle(McpRequest::ListOutsideAgents)
        .await
        .expect("list outside agents");
    let ResponseBody::OutsideAgents { agents, mcp_url } = listed else {
        panic!("unexpected response {listed:?}");
    };
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].client_id, client_id);
    assert!(mcp_url.ends_with("/mcp"));

    let revoked = daemon
        .outside_agents()
        .handle(McpRequest::RevokeOutsideAgent {
            client_id: client_id.to_owned(),
        })
        .await
        .expect("revoke");
    assert!(matches!(revoked, ResponseBody::Ack));
    assert!(daemon.outside.store().connected_agents().is_empty());

    // h4rn's own tool switches are readable and writable through the same
    // envelope the MCP page uses.
    let settings = daemon
        .outside_agents()
        .handle(McpRequest::AgentTools)
        .await
        .expect("agent tools");
    assert!(matches!(settings, ResponseBody::AgentTools { .. }));
    let updated = daemon
        .outside_agents()
        .handle(McpRequest::SetAgentToolsEnabled {
            harness: HarnessKind::Codex,
            enabled: false,
        })
        .await
        .expect("set agent tools");
    let ResponseBody::AgentTools { settings } = updated else {
        panic!("unexpected response");
    };
    assert!(
        !settings
            .enabled
            .is_enabled(HarnessKind::Codex, &daemon.host)
    );
    assert!(
        settings
            .enabled
            .is_enabled(HarnessKind::ClaudeCode, &daemon.host)
    );
    daemon.stop().await;
}

// ---------------------------------------------------------------------------
// PR-link tools against a scripted forge (the same seam the PR watch tests use)
// ---------------------------------------------------------------------------

struct MockSource;

#[async_trait::async_trait]
impl h4rn_daemon::pr_links::PrSource for MockSource {
    async fn resolve_account(
        &self,
        _account_id: Option<&str>,
        _repo: &h4rn_protocol::ForgeRepoRef,
    ) -> Result<String, h4rn_daemon::pr_links::PrSourceError> {
        Ok("account-1".into())
    }

    fn account_login(&self, _account_id: &str) -> Option<String> {
        Some("me".into())
    }

    async fn get_pr(
        &self,
        _account_id: &str,
        repo: &h4rn_protocol::ForgeRepoRef,
        number: u32,
    ) -> Result<h4rn_protocol::ForgePullRequest, h4rn_daemon::pr_links::PrSourceError> {
        Ok(h4rn_protocol::ForgePullRequest {
            summary: h4rn_protocol::ForgePrSummary {
                repo: repo.clone(),
                number,
                title: "Add the widget".into(),
                state: h4rn_protocol::ForgePrState::Open,
                author: "octo".into(),
                head_branch: "feature".into(),
                base_branch: "main".into(),
                head_sha: None,
                updated_at: chrono::Utc::now(),
                web_url: format!("{}/pull/{number}", repo.web_url()),
                checks: h4rn_protocol::ForgeChecksState::Passing,
                review: h4rn_protocol::ForgeReviewState::Pending,
                draft: false,
            },
            body: String::new(),
            labels: Vec::new(),
            mergeable: h4rn_protocol::ForgeMergeable::Mergeable,
            base_sha: None,
            additions: 1,
            deletions: 0,
            changed_files: 1,
            changed_files_known: true,
            line_counts_known: true,
            commits: 1,
            created_at: chrono::Utc::now(),
            reviewers: Vec::new(),
            merge_commit_sha: None,
        })
    }

    async fn list_comments(
        &self,
        _account_id: &str,
        _repo: &h4rn_protocol::ForgeRepoRef,
        _number: u32,
    ) -> Result<h4rn_protocol::ForgeCommentPage, h4rn_daemon::pr_links::PrSourceError> {
        Err(h4rn_daemon::pr_links::PrSourceError::Other(
            "the scripted forge has no comments".into(),
        ))
    }

    async fn list_checks(
        &self,
        _account_id: &str,
        _repo: &h4rn_protocol::ForgeRepoRef,
        _number: u32,
    ) -> Result<h4rn_protocol::ForgeChecks, h4rn_daemon::pr_links::PrSourceError> {
        Err(h4rn_daemon::pr_links::PrSourceError::Other(
            "the scripted forge has no checks".into(),
        ))
    }
}

struct MockSink;

#[async_trait::async_trait]
impl h4rn_daemon::pr_links::PrWakeSink for MockSink {
    async fn thread_title(&self, _thread_id: &h4rn_protocol::ThreadId) -> Option<String> {
        Some("A thread".into())
    }

    async fn is_active(&self, _thread_id: &h4rn_protocol::ThreadId) -> bool {
        true
    }

    async fn wake(
        &self,
        _thread_id: &h4rn_protocol::ThreadId,
        _message: String,
    ) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn pr_link_tools_use_the_thread_and_its_forge_remote() {
    use h4rn_daemon::mcp_server::{AgentToolsProvider, settings::AgentToolSettingsStore};
    use h4rn_mcp::provider::ToolProvider;
    use h4rn_mcp::session::McpScope;
    use h4rn_protocol::ForgeRepoRef;

    let daemon = boot(None).await;
    let repo = git_repo_with_remote();
    let thread = daemon
        .manager
        .create_thread(new_thread(&daemon.host, repo.path(), "link a PR"))
        .await
        .expect("create thread");
    let journal = Arc::new(
        h4rn_daemon::journal::SqliteJournal::in_memory()
            .await
            .expect("journal"),
    );
    let links = h4rn_daemon::pr_links::PrLinkService::new(
        journal,
        Arc::new(MockSource),
        Arc::new(MockSink),
    );
    let forge = h4rn_daemon::forge::ForgeService::open(daemon.data_dir()).expect("forge");
    let settings = Arc::new(AgentToolSettingsStore::open(
        daemon.data_dir(),
        daemon.host.clone(),
    ));
    let provider = AgentToolsProvider::new(
        Arc::clone(&daemon.manager),
        links,
        forge,
        settings,
        daemon.host.clone(),
    );
    let scope = McpScope::harness(
        HarnessKind::ClaudeCode,
        daemon.host.clone(),
        Some(thread.thread_id.clone()),
    );
    let context = h4rn_mcp::provider::ToolCallContext::new(scope, "h4rn", Duration::from_secs(5));
    let tools = provider.list_tools(&context.scope).await;
    assert!(tools.iter().any(|tool| tool.name == "link_pull_request"));

    let linked = provider
        .call_tool("link_pull_request", json!({"number": 7}), &context)
        .await
        .expect("link");
    assert!(!linked.is_error);
    assert!(
        linked.text_output().contains("acme/widgets#7"),
        "{}",
        linked.text_output()
    );
    assert_eq!(
        linked.structured_content.as_ref().expect("json")["pull_request"]["number"],
        7
    );

    let listed = provider
        .call_tool("list_thread_pull_requests", json!({}), &context)
        .await
        .expect("list");
    assert_eq!(
        listed.structured_content.as_ref().expect("json")["pull_requests"]
            .as_array()
            .expect("array")
            .len(),
        1
    );

    let watched = provider
        .call_tool("watch_pull_request", json!({"number": 7}), &context)
        .await
        .expect("watch");
    assert!(!watched.is_error);

    provider
        .call_tool("unwatch_pull_request", json!({"number": 7}), &context)
        .await
        .expect("unwatch");

    provider
        .call_tool("unlink_pull_request", json!({"number": 7}), &context)
        .await
        .expect("unlink");
    let empty = provider
        .call_tool("list_thread_pull_requests", json!({}), &context)
        .await
        .expect("list after unlink");
    assert_eq!(
        empty.structured_content.as_ref().expect("json")["pull_requests"]
            .as_array()
            .expect("array")
            .len(),
        0
    );

    // A repository can be named explicitly, and a wrong one is refused with
    // the remotes that exist instead of guessing.
    provider
        .call_tool(
            "link_pull_request",
            json!({"number": 8, "repository": "acme/widgets"}),
            &context,
        )
        .await
        .expect("link by repository");
    let wrong = provider
        .call_tool(
            "link_pull_request",
            json!({"number": 9, "repository": "other/repo"}),
            &context,
        )
        .await
        .expect_err("a wrong repository is a tool error");
    assert!(wrong.message.contains("acme/widgets"));
    let _ = ForgeRepoRef {
        forge: h4rn_protocol::ForgeKind::GitHub,
        base_url: "https://github.com".into(),
        owner: "acme".into(),
        repo: "widgets".into(),
    };
    daemon.stop().await;
}

#[tokio::test]
async fn agent_tools_are_hidden_when_their_harness_switch_is_off() {
    use h4rn_daemon::mcp_server::{AgentToolsProvider, settings::AgentToolSettingsStore};
    use h4rn_mcp::provider::ToolProvider;
    use h4rn_mcp::session::McpScope;

    let daemon = boot(None).await;
    let repo = git_repo_with_remote();
    let thread = daemon
        .manager
        .create_thread(new_thread(&daemon.host, repo.path(), "quiet"))
        .await
        .expect("create thread");
    let journal = Arc::new(
        h4rn_daemon::journal::SqliteJournal::in_memory()
            .await
            .expect("journal"),
    );
    let links = h4rn_daemon::pr_links::PrLinkService::new(
        journal,
        Arc::new(MockSource),
        Arc::new(MockSink),
    );
    let forge = h4rn_daemon::forge::ForgeService::open(daemon.data_dir()).expect("forge");
    let settings = Arc::new(AgentToolSettingsStore::open(
        daemon.data_dir(),
        daemon.host.clone(),
    ));
    settings
        .set_harness(HarnessKind::ClaudeCode, false)
        .expect("write the switch");
    let provider = AgentToolsProvider::new(
        Arc::clone(&daemon.manager),
        links,
        forge,
        Arc::clone(&settings),
        daemon.host.clone(),
    );
    let context = h4rn_mcp::provider::ToolCallContext::new(
        McpScope::harness(
            HarnessKind::ClaudeCode,
            daemon.host.clone(),
            Some(thread.thread_id.clone()),
        ),
        "h4rn",
        Duration::from_secs(5),
    );
    assert!(provider.list_tools(&context.scope).await.is_empty());
    let refused = provider
        .call_tool("thread_status", json!({}), &context)
        .await
        .expect_err("a direct call is refused too");
    assert!(refused.message.contains("turned off"));
    daemon.stop().await;
}

// ---------------------------------------------------------------------------
// The security model: only the person grants, a grant never exceeds its level,
// a revoked grant stays revoked, and nothing secret leaves the daemon.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn approval_needs_the_owner_session_not_the_public_page() {
    let daemon = boot(None).await;
    let client = http();
    let redirect = "http://localhost:1455/callback";
    let (_, registered) = register(&client, &daemon.base(), redirect).await;
    let client_id = registered["client_id"].as_str().expect("client id");
    let (_, challenge) = pkce();
    let request = open_authorize(&client, &daemon.base(), client_id, redirect, &challenge).await;

    // The page that anything on the machine can reach cannot grant anything.
    let response = client
        .post(format!("{}/oauth/approve", daemon.base()))
        .form(&[
            ("request", request.as_str()),
            ("decision", "approve"),
            ("level", "full_access"),
        ])
        .send()
        .await
        .expect("approve");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Nothing was granted: the browser still waits, and no client is connected.
    let waiting = client
        .get(format!("{}/oauth/authorize/pending", daemon.base()))
        .query(&[("request", request.as_str())])
        .send()
        .await
        .expect("pending");
    assert_eq!(waiting.status(), StatusCode::OK);
    assert!(
        waiting
            .text()
            .await
            .expect("page")
            .contains("Waiting for h4rn")
    );
    assert!(daemon.outside.store().connected_agents().is_empty());
    daemon.stop().await;
}

#[tokio::test]
async fn the_owner_answers_a_sign_in_over_the_protocol() {
    let daemon = boot(None).await;
    let client = http();
    let redirect = "http://localhost:1455/callback";
    let (_, registered) = register(&client, &daemon.base(), redirect).await;
    let client_id = registered["client_id"].as_str().expect("client id");
    let (_, challenge) = pkce();
    let request = open_authorize(&client, &daemon.base(), client_id, redirect, &challenge).await;

    let listed = daemon
        .outside_agents()
        .handle(McpRequest::ListOutsideAgentSignIns)
        .await
        .expect("list sign-ins");
    let ResponseBody::OutsideAgentSignIns { sign_ins } = listed else {
        panic!("unexpected response {listed:?}");
    };
    assert_eq!(sign_ins.len(), 1);
    assert_eq!(sign_ins[0].request_id, request);
    assert_eq!(sign_ins[0].client_name, "Claude Code");
    assert_eq!(sign_ins[0].redirect_origin, "http://localhost:1455");

    // Approving without a level is refused: the person's choice is required.
    let refused = daemon
        .outside_agents()
        .handle(McpRequest::RespondOutsideAgentSignIn {
            request_id: request.clone(),
            decision: ApprovalDecision::Approve,
            level: None,
        })
        .await;
    assert!(refused.is_err());

    let approved = daemon
        .outside_agents()
        .handle(McpRequest::RespondOutsideAgentSignIn {
            request_id: request.clone(),
            decision: ApprovalDecision::Approve,
            level: Some(OutsideAgentLevel::ReadOnly),
        })
        .await
        .expect("approve");
    assert!(matches!(approved, ResponseBody::Ack));
    assert_eq!(daemon.outside.store().connected_agents().len(), 0);

    // The browser's next navigation carries the code to the callback.
    let code = follow_pending(&client, &daemon.base(), &request).await;
    assert!(!code.is_empty());
    daemon.stop().await;
}

#[tokio::test]
async fn a_cross_site_origin_cannot_register_or_spend_a_code() {
    let daemon = boot(None).await;
    let client = http();
    let redirect = "http://localhost:1455/callback";
    let response = client
        .post(format!("{}/oauth/register", daemon.base()))
        .header(reqwest::header::ORIGIN, "https://evil.example")
        .json(&json!({"client_name": "Evil", "redirect_uris": [redirect]}))
        .send()
        .await
        .expect("register");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = client
        .post(format!("{}/oauth/token", daemon.base()))
        .header(reqwest::header::ORIGIN, "https://evil.example")
        .form(&[("grant_type", "authorization_code"), ("code", "x")])
        .send()
        .await
        .expect("token");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // A same-site origin, which is what the daemon's own pages send, works.
    let (status, _) = register(&client, &daemon.base(), redirect).await;
    assert_eq!(status, StatusCode::CREATED);
    daemon.stop().await;
}

#[tokio::test]
async fn a_rebinding_host_is_refused() {
    let daemon = boot(None).await;
    let client = http();
    let response = client
        .get(format!(
            "{}/.well-known/oauth-authorization-server",
            daemon.base()
        ))
        .header(reqwest::header::HOST, "evil.example")
        .send()
        .await
        .expect("discovery");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = client
        .post(format!("{}/mcp", daemon.base()))
        .header(reqwest::header::HOST, "evil.example")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
        .send()
        .await
        .expect("mcp");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    daemon.stop().await;
}

#[tokio::test]
async fn oauth_and_mcp_answer_only_on_the_loopback_listener() {
    // A second listener bound to every interface is classified as non-loopback
    // even though this test reaches it through 127.0.0.1.
    let daemon = boot_with(FakeHarness::new(), None, Some("0.0.0.0:0".parse().unwrap())).await;
    assert_eq!(daemon.addrs.len(), 2, "the extra listener must be bound");
    let lan = daemon.addrs[1];
    let lan_base = format!("http://127.0.0.1:{}", lan.port());

    // Liveness is served there.
    let response = reqwest::get(format!("{lan_base}/healthz"))
        .await
        .expect("healthz");
    assert_eq!(response.status(), StatusCode::OK);

    let client = http();
    for path in [
        "/.well-known/oauth-authorization-server",
        "/oauth/authorize",
    ] {
        let response = client
            .get(format!("{lan_base}{path}"))
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path} leaked");
    }
    let response = client
        .post(format!("{lan_base}/mcp"))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
        .send()
        .await
        .expect("mcp");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // The loopback listener still serves them.
    let (status, _) = register(&client, &daemon.base(), "http://localhost:1455/cb").await;
    assert_eq!(status, StatusCode::CREATED);
    daemon.stop().await;
}

#[tokio::test]
async fn a_plain_pkce_challenge_is_rejected() {
    let daemon = boot(None).await;
    let client = http();
    let redirect = "http://localhost:1455/callback";
    let (_, registered) = register(&client, &daemon.base(), redirect).await;
    let client_id = registered["client_id"].as_str().expect("client id");
    let (_, challenge) = pkce();
    let response = client
        .get(format!("{}/oauth/authorize", daemon.base()))
        .query(&[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "plain"),
        ])
        .send()
        .await
        .expect("authorize");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        response
            .text()
            .await
            .expect("body")
            .contains("code_challenge_method must be S256")
    );
    daemon.stop().await;
}

/// Registers and approves a supervised client, starting the PKCE flow.
async fn supervised_token(daemon: &TestDaemon, client: &reqwest::Client) -> String {
    let redirect = "http://localhost:1455/callback";
    let (_, registered) = register(client, &daemon.base(), redirect).await;
    let client_id = registered["client_id"].as_str().expect("client id");
    begin_and_approve(
        daemon,
        client,
        &daemon.base(),
        client_id,
        redirect,
        "supervised",
    )
    .await
}

async fn wait_for_settings(
    settings: &h4rn_harness::SessionSettings,
    count: usize,
) -> Vec<h4rn_harness::SettingChange> {
    let started = Instant::now();
    loop {
        let changes = settings.get();
        if changes.len() >= count {
            return changes;
        }
        if started.elapsed() > Duration::from_secs(10) {
            panic!("the fake harness never started a session: {changes:?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_supervised_message_runs_at_approval_required_and_leaves_the_mode_alone() {
    let daemon = boot(None).await;
    let client = http();
    let token = supervised_token(&daemon, &client).await;
    let repo = git_repo_with_remote();
    let mut request = new_thread(&daemon.host, repo.path(), "already running");
    request.runtime_mode = RuntimeMode::FullAccess;
    let thread = daemon
        .manager
        .create_thread(request)
        .await
        .expect("create thread");
    let sent = call_tool(
        &client,
        &daemon.outside_url(),
        &token,
        "send_thread_message",
        json!({"thread_id": thread.thread_id, "text": "one more thing"}),
    )
    .await;
    assert!(sent.get("error").is_none(), "{sent}");
    assert_eq!(
        sent["result"]["structuredContent"]["runtime_mode_ceiling"],
        "approval_required"
    );
    let summary = daemon
        .manager
        .thread_summary(&thread.thread_id)
        .await
        .expect("summary");
    // The person's own mode is untouched: only the turn the agent queued is
    // clamped.
    assert_eq!(summary.runtime_mode, RuntimeMode::FullAccess);
    daemon.stop().await;
}

#[tokio::test]
async fn raising_the_thread_mode_does_not_lift_a_queued_supervised_ceiling() {
    use h4rn_harness::SettingChange;

    // The session startup is held, so the first turn waits with no session:
    // the agent's message queues behind it, and the person can still raise the
    // thread's mode before anything is dispatched.
    let (fake, startup) = FakeHarness::new().with_start_gate();
    let daemon = boot_with(fake, None, None).await;
    let client = http();
    let token = supervised_token(&daemon, &client).await;
    let repo = git_repo_with_remote();

    let thread = daemon
        .manager
        .create_thread(new_thread(&daemon.host, repo.path(), "held open"))
        .await
        .expect("create thread");

    let sent = call_tool(
        &client,
        &daemon.outside_url(),
        &token,
        "send_thread_message",
        json!({"thread_id": thread.thread_id, "text": "raise it afterwards"}),
    )
    .await;
    assert!(sent.get("error").is_none(), "{sent}");

    // The person raises the thread to full access while the message waits.
    daemon
        .manager
        .set_thread_runtime_mode(&thread.thread_id, RuntimeMode::FullAccess)
        .await
        .expect("raise the mode");

    // Let both turns run, then read what the fake was actually started with.
    startup.release();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let summary = daemon
            .manager
            .thread_summary(&thread.thread_id)
            .await
            .expect("summary");
        let busy = daemon.manager.active_turn_count().await > 0;
        if (!busy && summary.agent_state == h4rn_protocol::AgentState::Done)
            || Instant::now() > deadline
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let changes = daemon.settings.get();
    let started: Vec<_> = changes
        .iter()
        .filter_map(|change| match change {
            SettingChange::Started { runtime_mode, .. } => Some(*runtime_mode),
            _ => None,
        })
        .collect();
    assert!(
        !started.is_empty(),
        "the fake harness never started a session: {changes:?}"
    );
    assert_eq!(
        started.last().copied(),
        Some(RuntimeMode::ApprovalRequired),
        "the queued message ran on a session it should not have: {changes:?}"
    );
    // And the person's mode is still what they chose.
    let summary = daemon
        .manager
        .thread_summary(&thread.thread_id)
        .await
        .expect("summary");
    assert_eq!(summary.runtime_mode, RuntimeMode::FullAccess);
    daemon.stop().await;
}

#[tokio::test]
async fn refresh_rotation_works_over_http_and_a_replay_kills_the_family() {
    let daemon = boot(None).await;
    let client = http();
    let redirect = "http://localhost:1455/callback";
    let (_, registered) = register(&client, &daemon.base(), redirect).await;
    let client_id = registered["client_id"].as_str().expect("client id");
    let (verifier, challenge) = pkce();
    let request = open_authorize(&client, &daemon.base(), client_id, redirect, &challenge).await;
    daemon
        .outside
        .respond_sign_in(
            &request,
            ApprovalDecision::Approve,
            Some(OutsideAgentLevel::ReadOnly),
        )
        .expect("approve");
    let code = follow_pending(&client, &daemon.base(), &request).await;
    let token_body: Value = client
        .post(format!("{}/oauth/token", daemon.base()))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("client_id", client_id),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .expect("token")
        .json()
        .await
        .expect("token json");
    let first_refresh = token_body["refresh_token"]
        .as_str()
        .expect("refresh")
        .to_owned();

    let rotated: Value = client
        .post(format!("{}/oauth/token", daemon.base()))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", first_refresh.as_str()),
            ("client_id", client_id),
        ])
        .send()
        .await
        .expect("refresh")
        .json()
        .await
        .expect("refresh json");
    let access = rotated["access_token"].as_str().expect("access").to_owned();
    let second_refresh = rotated["refresh_token"]
        .as_str()
        .expect("second refresh")
        .to_owned();
    assert_ne!(first_refresh, second_refresh);

    // Replaying the rotated refresh token revokes the whole family.
    let replay = client
        .post(format!("{}/oauth/token", daemon.base()))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", first_refresh.as_str()),
            ("client_id", client_id),
        ])
        .send()
        .await
        .expect("replay");
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    let (status, _) = post_rpc(
        &client,
        &daemon.outside_url(),
        &access,
        json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    daemon.stop().await;
}

#[tokio::test]
async fn a_token_issued_for_another_resource_is_refused_at_mcp() {
    let daemon = boot(None).await;
    let client = http();
    let redirect = "http://localhost:1455/callback";
    let (_, registered) = register(&client, &daemon.base(), redirect).await;
    let client_id = registered["client_id"].as_str().expect("client id");
    let (verifier, challenge) = pkce();

    // A resource this daemon does not serve is refused at authorization.
    let response = client
        .get(format!("{}/oauth/authorize", daemon.base()))
        .query(&[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("resource", "https://elsewhere.example/mcp"),
        ])
        .send()
        .await
        .expect("authorize");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"], "invalid_target");

    // The address this daemon serves is accepted and the token works.
    let mcp = daemon.outside.mcp_url();
    let response = client
        .get(format!("{}/oauth/authorize", daemon.base()))
        .query(&[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("resource", mcp.as_str()),
        ])
        .send()
        .await
        .expect("authorize");
    assert_eq!(response.status(), StatusCode::OK);
    let page = response.text().await.expect("page");
    let request = page
        .split("request=")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("request id")
        .to_owned();
    daemon
        .outside
        .respond_sign_in(
            &request,
            ApprovalDecision::Approve,
            Some(OutsideAgentLevel::ReadOnly),
        )
        .expect("approve");
    let code = follow_pending(&client, &daemon.base(), &request).await;
    let token = token_exchange(&client, &daemon.base(), client_id, &code, &verifier).await;
    let (status, _) = post_rpc(
        &client,
        &daemon.outside_url(),
        &token,
        json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    daemon.stop().await;
}

#[tokio::test]
async fn the_mcp_endpoint_refuses_another_site_and_json_rpc_batches() {
    let daemon = boot(None).await;
    let client = http();
    let token = supervised_token(&daemon, &client).await;

    let cross_site = client
        .post(daemon.outside_url())
        .bearer_auth(&token)
        .header(reqwest::header::ORIGIN, "https://evil.example")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {}}))
        .send()
        .await
        .expect("mcp");
    assert_eq!(cross_site.status(), StatusCode::FORBIDDEN);

    // A batch would be one rate-limit hit and one scope check for many calls.
    let batch = client
        .post(daemon.outside_url())
        .bearer_auth(&token)
        .json(&json!([
            {"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {}},
            {"jsonrpc": "2.0", "id": 2, "method": "ping", "params": {}}
        ]))
        .send()
        .await
        .expect("batch");
    assert_eq!(batch.status(), StatusCode::BAD_REQUEST);
    let body: Value = batch.json().await.expect("json");
    assert_eq!(body["error"]["code"], -32600);

    // One message still works.
    let (status, _) = post_rpc(
        &client,
        &daemon.outside_url(),
        &token,
        json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    daemon.stop().await;
}

#[tokio::test]
async fn get_thread_redacts_h4rn_and_forge_credentials() {
    let access_placeholder = format!("{}{}", "h4rn_at_", "TESTVALUE0123456789");
    let forge_placeholder = format!("{}{}", "ghp_", "TESTVALUE0123456789");
    let hub_placeholder = format!("{}{}", "h4rn_mcp_", "TESTVALUE0123456789");
    let text = format!("here they are: {access_placeholder} {forge_placeholder} {hub_placeholder}");
    let daemon = boot_with(
        FakeHarness::with_events(vec![
            h4rn_harness::HarnessEvent::TextDelta { text },
            h4rn_harness::HarnessEvent::TurnCompleted {
                status: TurnStatus::Completed,
                usage: Usage::default(),
            },
        ]),
        None,
        None,
    )
    .await;
    let client = http();
    let token = supervised_token(&daemon, &client).await;
    let repo = git_repo_with_remote();
    let thread = daemon
        .manager
        .create_thread(new_thread(&daemon.host, repo.path(), "tokens galore"))
        .await
        .expect("create thread");
    // Wait until the scripted reply really landed in the journal.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = daemon
            .manager
            .get_thread(&thread.thread_id, 0)
            .await
            .expect("snapshot");
        let answered = snapshot.events.iter().any(|wire| {
            matches!(
                &wire.event,
                h4rn_protocol::Event::ItemCompleted {
                    item: h4rn_protocol::Item::Message {
                        role: h4rn_protocol::MessageRole::Assistant,
                        ..
                    },
                    ..
                }
            )
        });
        if answered || Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let read = call_tool(
        &client,
        &daemon.outside_url(),
        &token,
        "get_thread",
        json!({"thread_id": thread.thread_id}),
    )
    .await;
    assert!(read.get("error").is_none(), "{read}");
    let rendered = read.to_string();
    for secret in [&access_placeholder, &forge_placeholder, &hub_placeholder] {
        assert!(
            !rendered.contains(secret.as_str()),
            "a credential came back out of get_thread: {rendered}"
        );
    }
    assert!(rendered.contains("[redacted]"));
    daemon.stop().await;
}

#[tokio::test]
async fn no_token_appears_in_a_response_body_or_the_store() {
    let daemon = boot(None).await;
    let client = http();
    let redirect = "http://localhost:1455/callback";
    let (_, registered) = register(&client, &daemon.base(), redirect).await;
    let client_id = registered["client_id"].as_str().expect("client id");
    let (verifier, challenge) = pkce();
    let request = open_authorize(&client, &daemon.base(), client_id, redirect, &challenge).await;
    daemon
        .outside
        .respond_sign_in(
            &request,
            ApprovalDecision::Approve,
            Some(OutsideAgentLevel::ReadOnly),
        )
        .expect("approve");
    let code = follow_pending(&client, &daemon.base(), &request).await;

    // A wrong verifier is refused without echoing the code or the verifier.
    let wrong = "A".repeat(43);
    let response = client
        .post(format!("{}/oauth/token", daemon.base()))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("client_id", client_id),
            ("code_verifier", wrong.as_str()),
        ])
        .send()
        .await
        .expect("token");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response.text().await.expect("body");
    assert!(!body.contains(&code), "{body}");
    assert!(!body.contains(&wrong), "{body}");

    let token = token_exchange(&client, &daemon.base(), client_id, &code, &verifier).await;
    // An unauthorized call never repeats a token.
    let response = client
        .post(daemon.outside_url())
        .bearer_auth("not-a-real-token")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
        .send()
        .await
        .expect("mcp");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let headers = response.headers().clone();
    let body = response.text().await.expect("body");
    assert!(!body.contains(&token));
    assert!(!format!("{headers:?}").contains(&token));

    let raw =
        std::fs::read_to_string(daemon.data_dir().join("outside_agents.json")).expect("store file");
    assert!(!raw.contains(&token), "the access token leaked to disk");
    daemon.stop().await;
}

#[tokio::test]
async fn the_harness_mcp_endpoint_refuses_another_site_and_batches_too() {
    let daemon = boot(None).await;
    let repo = git_repo_with_remote();
    daemon
        .manager
        .create_thread(new_thread(&daemon.host, repo.path(), "hub transport"))
        .await
        .expect("create thread");
    let injections = wait_for_injection(&daemon.mcp, 1).await;
    let token = injections[0].token.as_deref().expect("session token");
    let client = http();

    let cross_site = client
        .post(daemon.mcp_url())
        .bearer_auth(token)
        .header(reqwest::header::ORIGIN, "https://evil.example")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {}}))
        .send()
        .await
        .expect("mcp");
    assert_eq!(cross_site.status(), StatusCode::FORBIDDEN);

    let batch = client
        .post(daemon.mcp_url())
        .bearer_auth(token)
        .json(&json!([
            {"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {}},
            {"jsonrpc": "2.0", "id": 2, "method": "ping", "params": {}}
        ]))
        .send()
        .await
        .expect("batch");
    assert_eq!(batch.status(), StatusCode::BAD_REQUEST);

    let (status, body) = post_rpc(
        &client,
        &daemon.mcp_url(),
        token,
        json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    daemon.stop().await;
}
