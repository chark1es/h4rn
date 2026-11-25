//! FCM HTTP v1 sender tests against a mock server (ADR-048).
//!
//! The service-account key is generated at runtime and never committed. The
//! token endpoint and FCM endpoint are the configurable URLs, so every test
//! exercises the real signing, caching and retry code without Google.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use h4rn_connect::config::Config;
use h4rn_connect::push::{FcmClient, PushMessage, SendError};
use rsa::RsaPrivateKey;
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn service_account(server: &MockServer, project: &str) -> String {
    let key = RsaPrivateKey::new(&mut rand_core::OsRng, 2048).expect("generate RSA key");
    let pem = key.to_pkcs8_pem(LineEnding::LF).expect("encode PKCS#8 PEM");
    serde_json::json!({
        "project_id": project,
        "client_email": format!("relay@{project}.iam.gserviceaccount.example"),
        "private_key": pem.as_str(),
        "token_uri": format!("{}/token", server.uri()),
    })
    .to_string()
}

fn client(server: &MockServer, account: Option<String>, project: &str) -> FcmClient {
    let config = Config {
        public_base_url: "http://localhost:8787".to_string(),
        database_url: "postgres://localhost/h4rn".to_string(),
        kek: h4rn_connect::crypto::random_b64(32),
        signing_key: Some(h4rn_connect::crypto::random_b64(32)),
        allow_insecure: true,
        fcm_service_account: account.map(|_| service_account(server, project)),
        fcm_token_url: format!("{}/token", server.uri()),
        fcm_url: server.uri(),
        ..Config::default()
    };
    FcmClient::new(&config, reqwest::Client::new())
}

fn data() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("h4rn_alert".to_string(), "agent".to_string()),
        ("state".to_string(), "completed".to_string()),
        ("thread_id".to_string(), "th_one".to_string()),
        ("title".to_string(), "Fix the thing".to_string()),
        ("body".to_string(), "h4rn · Finished".to_string()),
        ("count".to_string(), "1".to_string()),
    ])
}

async fn token_mock(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "test-access-token",
            "expires_in": 3600,
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_correct_message_shape_is_sent_with_a_cached_token() {
    let server = MockServer::start().await;
    token_mock(&server).await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/demo/messages:send"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "x"})))
        .mount(&server)
        .await;
    let client = client(&server, Some(String::new()), "demo");
    assert!(client.configured());
    let message = PushMessage {
        token: "device-token-one",
        package_name: Some("dev.h4rn.app"),
        data: data(),
    };
    client.send(&message).await.expect("first send");
    client.send(&message).await.expect("second send");

    let requests: Vec<Request> = server
        .received_requests()
        .await
        .expect("requests")
        .into_iter()
        .filter(|request| request.url.path().ends_with("messages:send"))
        .collect();
    assert_eq!(requests.len(), 2);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json");
    assert_eq!(body["message"]["token"], "device-token-one");
    assert_eq!(body["message"]["data"]["state"], "completed");
    assert_eq!(
        body["message"]["android"]["restricted_package_name"],
        "dev.h4rn.app"
    );
    assert_eq!(body["message"]["android"]["priority"], "HIGH");

    // One token request for both sends: the token is cached.
    let tokens = server
        .received_requests()
        .await
        .expect("all requests")
        .into_iter()
        .filter(|request| request.url.path() == "/token")
        .count();
    assert_eq!(tokens, 1);
}

#[tokio::test]
async fn unregistered_tokens_are_reported_for_deletion() {
    let server = MockServer::start().await;
    token_mock(&server).await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/demo/messages:send"))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "error": {
                "status": "NOT_FOUND",
                "details": [{
                    "@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError",
                    "errorCode": "UNREGISTERED"
                }]
            }
        })))
        .mount(&server)
        .await;
    let client = client(&server, Some(String::new()), "demo");
    let error = client
        .send(&PushMessage {
            token: "gone",
            package_name: None,
            data: data(),
        })
        .await
        .expect_err("unregistered");
    assert_eq!(error, SendError::Unregistered);
}

struct Flaky {
    calls: AtomicUsize,
}

impl Respond for Flaky {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(429).set_body_string("quota")
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "x"}))
        }
    }
}

#[tokio::test]
async fn a_429_is_retried_with_backoff() {
    let server = MockServer::start().await;
    token_mock(&server).await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/demo/messages:send"))
        .respond_with(Flaky {
            calls: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;
    let client = client(&server, Some(String::new()), "demo");
    client
        .send(&PushMessage {
            token: "device-token",
            package_name: None,
            data: data(),
        })
        .await
        .expect("retry after 429");
    let sends = server
        .received_requests()
        .await
        .expect("requests")
        .into_iter()
        .filter(|request| request.url.path().ends_with("messages:send"))
        .count();
    assert_eq!(sends, 2);
}

#[tokio::test]
async fn without_a_service_account_nothing_is_requested() {
    let server = MockServer::start().await;
    let client = client(&server, None, "demo");
    assert!(!client.configured());
    let error = client
        .send(&PushMessage {
            token: "device-token",
            package_name: None,
            data: data(),
        })
        .await
        .expect_err("push is off");
    assert_eq!(error, SendError::Disabled);
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
}

#[tokio::test]
async fn an_expired_token_is_refreshed() {
    let server = MockServer::start().await;
    let tokens = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&tokens);
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(move |_request: &Request| {
            let number = counter.fetch_add(1, Ordering::SeqCst) + 1;
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": format!("test-access-token-{number}"),
                "expires_in": 30,
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/demo/messages:send"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let client = client(&server, Some(String::new()), "demo");
    let message = PushMessage {
        token: "device-token",
        package_name: None,
        data: data(),
    };
    client.send(&message).await.expect("first");
    // `expires_in: 30` is inside the sixty-second refresh margin, so the next
    // send asks for another token rather than using an almost-expired one.
    client.send(&message).await.expect("second");
    assert_eq!(tokens.load(Ordering::SeqCst), 2);
}
