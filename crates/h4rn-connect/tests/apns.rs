//! APNs sender tests against a mock server (ADR-052).
//!
//! The signing key is generated at runtime and never committed. The endpoint
//! is configuration, so every test exercises the real ES256 signing, caching
//! and retry code without Apple.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use h4rn_connect::apns::ApnsClient;
use h4rn_connect::config::Config;
use h4rn_connect::push::PushMessage;
use h4rn_connect::push::PushPlatform;
use h4rn_connect::push::SendError;
use p256::ecdsa::SigningKey;
use p256::pkcs8::{EncodePrivateKey, LineEnding};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn key_pem() -> String {
    let key = SigningKey::random(&mut rand_core::OsRng);
    key.to_pkcs8_pem(LineEnding::LF)
        .expect("encode PKCS#8 PEM")
        .to_string()
}

fn client(server: &MockServer, configured: bool) -> ApnsClient {
    let key = if configured { Some(key_pem()) } else { None };
    let config = Config {
        public_base_url: "http://localhost:8787".to_string(),
        database_url: "postgres://localhost/h4rn".to_string(),
        kek: h4rn_connect::crypto::random_b64(32),
        signing_key: Some(h4rn_connect::crypto::random_b64(32)),
        allow_insecure: true,
        apns_key: key,
        apns_key_id: configured.then(|| "KEYID12345".to_string()),
        apns_team_id: configured.then(|| "TEAM123456".to_string()),
        apns_url: server.uri(),
        apns_topic: "dev.h4rn.app".to_string(),
        ..Config::default()
    };
    ApnsClient::new(&config, reqwest::Client::new())
}

fn message() -> PushMessage<'static> {
    PushMessage {
        token: "apns-device-token",
        platform: PushPlatform::Ios,
        package_name: None,
        data: std::collections::BTreeMap::from([
            ("h4rn_alert".to_string(), "agent".to_string()),
            ("state".to_string(), "completed".to_string()),
            ("thread_id".to_string(), "th_one".to_string()),
            ("title".to_string(), "Fix the thing".to_string()),
            ("body".to_string(), "h4rn · Finished".to_string()),
            ("count".to_string(), "1".to_string()),
        ]),
    }
}

#[tokio::test]
async fn an_alert_is_sent_as_an_apns_alert_with_a_cached_provider_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/3/device/apns-device-token"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let client = client(&server, true);
    assert!(client.configured());
    let message = message();

    client
        .send(message.token, &message.apns_payload())
        .await
        .expect("first send");
    client
        .send(message.token, &message.apns_payload())
        .await
        .expect("second send");

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 2);
    let first = &requests[0];
    assert_eq!(
        first
            .headers
            .get("apns-topic")
            .and_then(|value| value.to_str().ok()),
        Some("dev.h4rn.app")
    );
    assert_eq!(
        first
            .headers
            .get("apns-push-type")
            .and_then(|value| value.to_str().ok()),
        Some("alert")
    );
    assert_eq!(
        first
            .headers
            .get("apns-priority")
            .and_then(|value| value.to_str().ok()),
        Some("10")
    );
    let authorization = first
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .expect("authorization");
    assert!(authorization.starts_with("bearer "));
    assert_eq!(
        authorization.split('.').count(),
        3,
        "the provider token must be a JWT"
    );

    let body: serde_json::Value = serde_json::from_slice(&first.body).expect("json");
    assert_eq!(body["aps"]["alert"]["title"], "Fix the thing");
    assert_eq!(body["aps"]["alert"]["body"], "h4rn · Finished");
    assert_eq!(body["aps"]["mutable-content"], 1);
    assert_eq!(body["thread_id"], "th_one");

    // Both sends reuse one provider token: Apple's token was signed once.
    let tokens = requests
        .iter()
        .filter_map(|request| {
            request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
        })
        .collect::<Vec<_>>();
    assert_eq!(tokens[0], tokens[1], "the provider token was not cached");
}

#[tokio::test]
async fn a_gone_device_token_is_reported_for_deletion() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/3/device/gone"))
        .respond_with(ResponseTemplate::new(410).set_body_json(serde_json::json!({
            "reason": "Unregistered",
            "timestamp": 1_700_000_000_000_i64,
        })))
        .mount(&server)
        .await;
    let client = client(&server, true);
    let error = client
        .send("gone", &message().apns_payload())
        .await
        .expect_err("unregistered");
    assert_eq!(error, SendError::Unregistered);
}

#[tokio::test]
async fn a_bad_device_token_is_not_enough_to_delete_the_row() {
    // A development build pointed at the production host answers this for a
    // token that is still valid elsewhere; keeping the row is safer.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/3/device/mismatch"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "reason": "BadDeviceToken",
        })))
        .mount(&server)
        .await;
    let client = client(&server, true);
    let error = client
        .send("mismatch", &message().apns_payload())
        .await
        .expect_err("bad device token");
    assert!(
        matches!(error, SendError::Failed(ref reason) if reason.contains("BadDeviceToken")),
        "{error:?}"
    );
}

struct Flaky {
    calls: AtomicUsize,
}

impl Respond for Flaky {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "reason": "TooManyRequests",
            }))
        } else {
            ResponseTemplate::new(200)
        }
    }
}

#[tokio::test]
async fn a_429_is_retried_with_backoff() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/3/device/flaky"))
        .respond_with(Flaky {
            calls: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;
    let client = client(&server, true);
    client
        .send("flaky", &message().apns_payload())
        .await
        .expect("retry after 429");
    let sends = server
        .received_requests()
        .await
        .expect("requests")
        .iter()
        .filter(|request| request.url.path() == "/3/device/flaky")
        .count();
    assert_eq!(sends, 2);
}

#[tokio::test]
async fn without_a_key_nothing_is_requested() {
    let server = MockServer::start().await;
    let client = client(&server, false);
    assert!(!client.configured());
    let error = client
        .send("apns-device-token", &message().apns_payload())
        .await
        .expect_err("iOS push is off");
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
async fn a_payload_over_apples_limit_is_refused_before_the_request() {
    let server = MockServer::start().await;
    let client = client(&server, true);
    let mut payload = message().apns_payload();
    payload["thread_id"] = "x".repeat(5000).into();
    let error = client
        .send("apns-device-token", &payload)
        .await
        .expect_err("too large");
    assert!(matches!(error, SendError::Failed(ref reason) if reason.contains("4096")));
    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
}

#[tokio::test]
async fn an_expired_provider_token_is_re_signed_once() {
    let server = MockServer::start().await;
    let expired = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&expired);
    Mock::given(method("POST"))
        .and(path("/3/device/renew"))
        .respond_with(move |_request: &Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(403).set_body_json(serde_json::json!({
                    "reason": "ExpiredProviderToken",
                }))
            } else {
                ResponseTemplate::new(200)
            }
        })
        .mount(&server)
        .await;
    let client = client(&server, true);
    client
        .send("renew", &message().apns_payload())
        .await
        .expect("re-sign and retry");
    assert_eq!(expired.load(Ordering::SeqCst), 2);
}
