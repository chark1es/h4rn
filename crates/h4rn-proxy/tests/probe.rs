//! Probe outcomes against a mock CLIProxyAPI (ADR-015).
//!
//! Every capability is decided by what the endpoints answer, so these tests
//! drive each path: an upstream build without management routes, a fork with
//! headers and v8 routes, disabled management, an open (misconfigured) plane
//! and rejected keys.

use h4rn_proxy::{ProbeTarget, probe};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Matches when the request does *not* carry `name`.
struct WithoutHeader(&'static str);

impl wiremock::Match for WithoutHeader {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key(self.0)
    }
}

async fn probe_at(target: ProbeTarget) -> h4rn_protocol::ProxyTestReport {
    probe(&reqwest::Client::new(), &target).await
}

fn target(server: &MockServer) -> ProbeTarget {
    ProbeTarget::new(
        server.uri(),
        Some("mgmt-secret".into()),
        Some("sk-client".into()),
    )
}

fn openai_models() -> serde_json::Value {
    json!({"object": "list", "data": [
        {"id": "gpt-6.1-sol", "object": "model", "owned_by": "openai"}
    ]})
}

fn claude_models() -> serde_json::Value {
    json!({"data": [
        {"id": "claude-sonnet-5", "type": "model", "max_input_tokens": 200000}
    ]})
}

#[tokio::test]
async fn an_upstream_build_without_management_routes_is_usable() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v0/management/debug"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(WithoutHeader("anthropic-version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_models()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let report = probe_at(target(&server)).await;
    let caps = &report.capabilities;
    assert!(caps.reachable);
    assert_eq!(
        caps.management,
        h4rn_protocol::ProxyManagementAccess::Disabled
    );
    assert!(caps.openai_models);
    assert!(!caps.claude_models);
    assert!(!caps.gemini_models);
    assert!(!caps.v8_config);
    assert!(caps.version.is_none());
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "management_disabled")
    );
}

#[tokio::test]
async fn a_fork_reports_headers_v8_routes_and_header_selected_shapes() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v0/management/debug"))
        .respond_with(
            ResponseTemplate::new(401)
                .insert_header("x-cpa-version", "private-6ae18171")
                .insert_header("x-cpa-commit", "6ae18171")
                .insert_header("x-cpa-support-plugin", "1"),
        )
        .mount(&server)
        .await;
    for route in [
        "/v0/management/usage-queue",
        "/v0/management/api-key-usage",
        "/v0/management/auth-files",
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .and(header("authorization", "Bearer mgmt-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"files": []})))
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/v0/management/quota/providers"))
        .and(header("authorization", "Bearer mgmt-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"providers": ["claude"]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v8/management/config"))
        .and(header("authorization", "Bearer mgmt-secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"v1\"")
                .set_body_json(json!({})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v8/management/performance"))
        .and(header("authorization", "Bearer mgmt-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"rows": []})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v8/management/credentials/verify"))
        .and(header("authorization", "Bearer mgmt-secret"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    // `/v1/models` returns the Claude shape only when `Anthropic-Version` is
    // present, and the OpenAI shape with fork enrichment otherwise.
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(claude_models()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(WithoutHeader("anthropic-version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": "deepseek-v4.1-flash",
                "max_output_length": 8192,
                "reasoning_parameters": {"effort": "high"}
            }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{"name": "models/gemini-3-pro"}]
        })))
        .mount(&server)
        .await;

    let report = probe_at(target(&server)).await;
    let caps = &report.capabilities;
    assert!(caps.reachable);
    assert_eq!(
        caps.management,
        h4rn_protocol::ProxyManagementAccess::Available
    );
    assert_eq!(caps.version.as_deref(), Some("private-6ae18171"));
    assert_eq!(caps.commit.as_deref(), Some("6ae18171"));
    assert!(caps.plugin_support);
    assert!(caps.usage_queue);
    assert!(!caps.legacy_usage);
    assert!(caps.api_key_usage);
    assert!(caps.v8_config);
    assert_eq!(caps.v8_etag.as_deref(), Some("\"v1\""));
    assert!(caps.performance);
    assert!(!caps.credentials_verify);
    assert!(caps.cooldowns);
    assert!(caps.quota_providers);
    assert!(caps.openai_models);
    assert!(caps.claude_models);
    assert!(caps.gemini_models);
    assert!(caps.fork_model_enrichment);
}

#[tokio::test]
async fn a_missing_management_key_is_a_warning_not_a_failure() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v0/management/debug"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_models()))
        .mount(&server)
        .await;

    let mut target = target(&server);
    target.management_key = None;
    let report = probe_at(target).await;
    assert_eq!(
        report.capabilities.management,
        h4rn_protocol::ProxyManagementAccess::NeedsKey
    );
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "management_key_missing")
    );
}

#[tokio::test]
async fn an_open_management_plane_is_reported_as_a_misconfiguration() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v0/management/debug"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_models()))
        .mount(&server)
        .await;

    let report = probe_at(target(&server)).await;
    assert_eq!(
        report.capabilities.management,
        h4rn_protocol::ProxyManagementAccess::Open
    );
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "management_open")
    );
}

#[tokio::test]
async fn a_rejected_management_key_is_an_error_with_a_hint() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v0/management/debug"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v0/management/usage-queue"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_models()))
        .mount(&server)
        .await;

    let report = probe_at(target(&server)).await;
    let diagnostic = report
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "management_key_rejected")
        .expect("rejected diagnostic");
    assert_eq!(diagnostic.level, h4rn_protocol::ProxyDiagnosticLevel::Error);
    assert!(
        diagnostic
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("remote-management"))
    );
    assert_eq!(
        report.capabilities.management,
        h4rn_protocol::ProxyManagementAccess::NeedsKey
    );
}

#[tokio::test]
async fn a_rejected_client_key_does_not_echo_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/healthz"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v0/management/debug"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
        .mount(&server)
        .await;

    let report = probe_at(target(&server)).await;
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "client_key_rejected")
    );
    let serialized = serde_json::to_string(&report).expect("serialize");
    assert!(!serialized.contains("sk-client"));
    assert!(!serialized.contains("mgmt-secret"));
}

#[tokio::test]
async fn an_unreachable_proxy_reports_the_health_failure() {
    // Port 9 is the discard protocol: nothing listens for HTTP there.
    let target = ProbeTarget::new("http://127.0.0.1:9", None, None);
    let report = probe_at(target).await;
    assert!(!report.capabilities.reachable);
    assert_eq!(
        report
            .diagnostics
            .first()
            .map(|diagnostic| diagnostic.code.as_str()),
        Some("unreachable")
    );
}
