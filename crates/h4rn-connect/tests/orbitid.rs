//! Live OrbitID end-to-end checks for the quirks that only a real provider can
//! expose (docs/research/orbitid.md, ADR-011).
//!
//! Ignored by default; `scripts/test-connect.sh --orbitid` runs it when
//! `H4RN_TEST_ORBITID_URL` and `H4RN_TEST_OIDC_CLIENT_ID` point at a running
//! instance. It deliberately stops before the browser consent screen: the
//! authorization URL, PKCE parameters, discovery and the device authorization
//! response are what the relay is responsible for. A full approval needs a
//! human or a headless session and belongs in the deployment's own smoke test.

use std::sync::Arc;

use h4rn_connect::config::{Config, ProviderConfig};
use h4rn_connect::oidc::OidcRegistry;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the OrbitID e2e test"))
}

#[tokio::test]
#[ignore = "requires a running OrbitID (set H4RN_TEST_ORBITID_URL)"]
async fn orbitid_discovery_authorize_url_and_device_grant() {
    let issuer = env("H4RN_TEST_ORBITID_URL")
        .trim_end_matches('/')
        .to_string();
    let client_id = env("H4RN_TEST_OIDC_CLIENT_ID");
    let client_secret = std::env::var("H4RN_TEST_OIDC_CLIENT_SECRET").ok();
    let redirect = std::env::var("H4RN_TEST_OIDC_REDIRECT_URI")
        .unwrap_or_else(|_| "http://127.0.0.1:53682/callback".to_string());

    let mut config = Config {
        public_base_url: "http://127.0.0.1:8787".to_string(),
        database_url: "postgres://unused/unused".to_string(),
        kek: h4rn_connect::crypto::random_b64(32),
        signing_key: Some(h4rn_connect::crypto::random_b64(32)),
        allow_insecure: issuer.starts_with("http://"),
        providers: vec![ProviderConfig {
            id: "orbitid".to_string(),
            display_name: "OrbitID".to_string(),
            issuer,
            client_id,
            client_secret,
            scopes: vec![
                "openid".to_string(),
                "profile".to_string(),
                "email".to_string(),
                "offline_access".to_string(),
            ],
            redirect_uris: vec![redirect.clone()],
            device_grant: true,
        }],
        ..Config::default()
    };
    config.normalise().expect("valid provider configuration");

    let registry = OidcRegistry::new(Arc::new(config)).expect("registry");

    // Quirk 15: `device_grant` follows discovery, not just configuration.
    let providers = registry.public_providers().await;
    assert_eq!(providers.len(), 1);
    assert!(
        providers[0].device_grant,
        "OrbitID must advertise device_authorization_endpoint and client auth"
    );

    // Quirks 4 and 6: PKCE is always S256, and offline_access is requested so a
    // refresh token is issued.
    let (url, verifier) = registry
        .start_login("orbitid", &redirect, "test-state", "test-nonce")
        .await
        .expect("authorize URL");
    assert!(!verifier.is_empty());
    let parsed = url::Url::parse(&url).unwrap();
    let params: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
    assert_eq!(
        params.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert!(params.contains_key("code_challenge"));
    assert_eq!(params.get("state").map(String::as_str), Some("test-state"));
    assert_eq!(params.get("nonce").map(String::as_str), Some("test-nonce"));
    assert!(
        params
            .get("scope")
            .is_some_and(|scope| scope.split(' ').any(|item| item == "offline_access"))
    );

    // Quirk 16: the device authorization endpoint must accept client auth.
    let authorization = registry
        .start_device_grant("orbitid")
        .await
        .expect("device authorization");
    assert!(!authorization.user_code.is_empty());
    assert!(!authorization.device_code.is_empty());
    assert!(authorization.expires_in > 0);
    assert!(authorization.verification_uri.starts_with("http"));

    // Quirk 10: the TS build does not advertise an end-session endpoint. This
    // is not fatal; local logout is authoritative.
    let _ = registry.end_session_endpoint("orbitid").await;
}
