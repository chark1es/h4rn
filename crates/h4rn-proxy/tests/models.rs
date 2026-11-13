//! Model catalogue shape selection and alias merging (ADR-015).

use h4rn_protocol::{ProxyModelShape, ProxyModelSource};
use h4rn_proxy::{MemorySecretStore, ProfileStore, list_models};
use serde_json::json;
use std::sync::Arc;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

struct WithoutHeader(&'static str);

impl wiremock::Match for WithoutHeader {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key(self.0)
    }
}

async fn profile(
    server: &MockServer,
    management_key: Option<&str>,
) -> (tempfile::TempDir, h4rn_proxy::ResolvedProfile) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = ProfileStore::open(dir.path(), Arc::new(MemorySecretStore::new())).expect("store");
    let resolved = store
        .add(h4rn_protocol::ProxyProfileDraft {
            name: "mock".into(),
            base_url: server.uri(),
            management_key: management_key.map(str::to_owned),
            api_key: Some("sk-client".into()),
            default: true,
        })
        .expect("add");
    (dir, resolved)
}

#[tokio::test]
async fn all_three_response_shapes_merge_into_one_catalogue() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(WithoutHeader("anthropic-version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "gpt-6.1-sol", "display_name": "GPT 6.1 Sol", "max_context_length": 400000,
                 "max_output_length": 128000, "reasoning_parameters": {"effort": "high"}}
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {"id": "claude-sonnet-5", "type": "model", "display_name": "Sonnet 5",
                 "max_input_tokens": 200000, "max_tokens": 64000}
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [
                {"name": "models/gemini-3-pro", "displayName": "Gemini 3 Pro",
                 "inputTokenLimit": 1000000, "outputTokenLimit": 65536}
            ]
        })))
        .mount(&server)
        .await;

    let (_dir, profile) = profile(&server, None).await;
    let catalog = list_models(&reqwest::Client::new(), &profile)
        .await
        .expect("catalogue");
    assert!(!catalog.aliases_merged);

    let ids: Vec<&str> = catalog
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    assert_eq!(ids, vec!["claude-sonnet-5", "gemini-3-pro", "gpt-6.1-sol"]);

    let gpt = catalog
        .models
        .iter()
        .find(|model| model.id == "gpt-6.1-sol")
        .expect("gpt");
    assert_eq!(gpt.shape, ProxyModelShape::OpenAi);
    assert_eq!(gpt.context_length, Some(400_000));
    assert_eq!(gpt.max_output_tokens, Some(128_000));
    assert!(gpt.fork_enriched);

    let claude = catalog
        .models
        .iter()
        .find(|model| model.id == "claude-sonnet-5")
        .expect("claude");
    assert_eq!(claude.shape, ProxyModelShape::Claude);
    assert_eq!(claude.context_length, Some(200_000));

    let gemini = catalog
        .models
        .iter()
        .find(|model| model.id == "gemini-3-pro")
        .expect("gemini");
    assert_eq!(gemini.shape, ProxyModelShape::Gemini);
    assert_eq!(gemini.source, ProxyModelSource::Gemini);
}

#[tokio::test]
async fn management_aliases_are_merged_without_copying_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(WithoutHeader("anthropic-version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "gpt-6.1-sol"}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v0/management/config"))
        .and(header("authorization", "Bearer mgmt-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "oauth": {
                "model-alias": {
                    "claude": [
                        {"name": "claude-sonnet-5", "alias": "sonnet", "display-name": "Sonnet"}
                    ]
                }
            },
            "openai-compatibility": [
                {"models": [{"name": "gpt-6.1-sol", "alias": "gpt"}]}
            ],
            "api-keys": {
                "gemini": [
                    // The key is never copied into the catalogue.
                    {"api-key": "super-secret-key", "models": [{"name": "gemini-3-pro"}]}
                ]
            }
        })))
        .mount(&server)
        .await;

    let (_dir, profile) = profile(&server, Some("mgmt-secret")).await;
    let catalog = list_models(&reqwest::Client::new(), &profile)
        .await
        .expect("catalogue");
    assert!(catalog.aliases_merged);

    let sonnet = catalog
        .models
        .iter()
        .find(|model| model.id == "sonnet")
        .expect("alias");
    assert_eq!(sonnet.alias_of.as_deref(), Some("claude-sonnet-5"));
    assert_eq!(sonnet.source, ProxyModelSource::Alias);
    assert_eq!(sonnet.shape, ProxyModelShape::Claude);

    let gpt = catalog
        .models
        .iter()
        .find(|model| model.id == "gpt")
        .expect("compatibility alias");
    assert_eq!(gpt.source, ProxyModelSource::Compatibility);

    let gemini = catalog
        .models
        .iter()
        .find(|model| model.id == "gemini-3-pro")
        .expect("credential model");
    assert_eq!(gemini.source, ProxyModelSource::Credential);

    let serialized = serde_json::to_string(&catalog).expect("serialize");
    assert!(!serialized.contains("super-secret-key"));
    assert!(!serialized.contains("mgmt-secret"));
    assert!(!serialized.contains("sk-client"));
}

#[tokio::test]
async fn a_rejected_client_key_is_an_unauthorized_error() {
    let server = MockServer::start().await;
    for route in ["/v1/models", "/v1beta/models"] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
    }
    let (_dir, profile) = profile(&server, None).await;
    let error = list_models(&reqwest::Client::new(), &profile)
        .await
        .expect_err("unauthorized");
    assert_eq!(error.kind, h4rn_proxy::ProxyErrorKind::Unauthorized);
    assert!(!error.to_string().contains("sk-client"));
}

#[tokio::test]
async fn an_empty_proxy_is_an_upstream_error_with_a_hint() {
    let server = MockServer::start().await;
    for route in ["/v1/models", "/v1beta/models"] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": [], "models": []})),
            )
            .mount(&server)
            .await;
    }
    let (_dir, profile) = profile(&server, None).await;
    let error = list_models(&reqwest::Client::new(), &profile)
        .await
        .expect_err("empty");
    assert_eq!(error.kind, h4rn_proxy::ProxyErrorKind::Upstream);
    assert!(error.hint.is_some());
}
