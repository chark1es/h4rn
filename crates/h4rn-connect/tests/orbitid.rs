//! Live OrbitID end-to-end checks (docs/research/orbitid.md, ADR-011).
//!
//! Ignored by default. `scripts/test-connect.sh --orbitid` builds/runs a real
//! OrbitID (TS build), creates the user, session and OIDC clients, exports the
//! `H4RN_TEST_*` variables, and executes this file. Nothing is mocked: the
//! relay router, Postgres schema, network calls, PKCE exchange, ID-token
//! verification and provider revocation are all real.
//!
//! Covered here:
//!
//! 1. authorization code + S256 PKCE through `/v1/auth/:provider/start` and
//!    `/v1/auth/:provider/callback`, headless, with a real session JWT;
//! 2. refresh rotation, the replay window, and family revocation on reuse;
//! 3. local logout plus revocation at OrbitID (the TS build has no
//!    `end_session_endpoint`);
//! 4. the device authorization grant for a daemon;
//! 5. rejection of an ID token whose `iss` is not the configured issuer;
//! 6. tolerance for an issuer configured with a trailing slash.

use std::collections::HashMap;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use chrono::Utc;
use ed25519_dalek::{Signer as _, SigningKey};
use h4rn_connect::api::{AppState, build_state, router};
use h4rn_connect::config::{Config, ProviderConfig};
use h4rn_connect::crypto::{b64, hash_refresh_token, random_b64, random_bytes};
use h4rn_connect::dpop::canonical_proof;
use h4rn_protocol::{ConnectDeviceKind, DeviceRegistration};
use http_body_util::BodyExt;
use openidconnect::PkceCodeChallenge;
use serde_json::{Value, json};
use tower::ServiceExt as _;
use url::Url;
use uuid::Uuid;

struct OrbitId {
    issuer: String,
    alt_issuer: String,
    session_jwt: String,
    alt_session_jwt: String,
    public_client_id: String,
    confidential_client_id: String,
    confidential_client_secret: String,
    redirect: String,
}

impl OrbitId {
    fn load() -> Self {
        Self {
            issuer: env("H4RN_TEST_ORBITID_URL")
                .trim_end_matches('/')
                .to_string(),
            alt_issuer: env("H4RN_TEST_ORBITID_ALT_URL")
                .trim_end_matches('/')
                .to_string(),
            session_jwt: env("H4RN_TEST_ORBITID_SESSION_JWT"),
            alt_session_jwt: env("H4RN_TEST_ORBITID_ALT_SESSION_JWT"),
            public_client_id: env("H4RN_TEST_OIDC_CLIENT_ID"),
            confidential_client_id: env("H4RN_TEST_OIDC_CONFIDENTIAL_CLIENT_ID"),
            confidential_client_secret: env("H4RN_TEST_OIDC_CONFIDENTIAL_CLIENT_SECRET"),
            redirect: std::env::var("H4RN_TEST_OIDC_REDIRECT_URI")
                .unwrap_or_else(|_| "h4rn://callback".to_string()),
        }
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for the OrbitID e2e test"))
}

fn provider(
    id: &str,
    issuer: &str,
    client_id: &str,
    client_secret: Option<&str>,
    redirect: &str,
) -> ProviderConfig {
    ProviderConfig {
        id: id.to_string(),
        display_name: id.to_string(),
        issuer: issuer.to_string(),
        client_id: client_id.to_string(),
        client_secret: client_secret.map(ToOwned::to_owned),
        scopes: vec![
            "openid".to_string(),
            "profile".to_string(),
            "email".to_string(),
            "offline_access".to_string(),
        ],
        redirect_uris: vec![redirect.to_string()],
        device_grant: true,
    }
}

fn test_config(settings: &OrbitId) -> Config {
    let mut config = Config {
        public_base_url: "http://127.0.0.1:8787".to_string(),
        database_url: env("H4RN_TEST_DATABASE_URL"),
        kek: random_b64(32),
        signing_key: Some(random_b64(32)),
        allow_insecure: true,
        providers: vec![
            provider(
                "orbitid",
                &settings.issuer,
                &settings.public_client_id,
                None,
                &settings.redirect,
            ),
            provider(
                "orbitid-confidential",
                &settings.issuer,
                &settings.confidential_client_id,
                Some(&settings.confidential_client_secret),
                &settings.redirect,
            ),
            // Quirk 2/32: a deployment may spell the issuer with a trailing
            // slash. `normalise` must trim it before discovery.
            provider(
                "orbitid-slash",
                &format!("{}/", settings.issuer),
                &settings.public_client_id,
                None,
                &settings.redirect,
            ),
        ],
        ..Config::default()
    };
    config.rate_limits.per_ip_per_minute = 10_000;
    config.rate_limits.auth_per_minute = 10_000;
    config.rate_limits.pairing_per_hour = 10_000;
    config.normalise().expect("valid provider configuration");
    config
}

async fn test_state(settings: &OrbitId) -> AppState {
    build_state(test_config(settings))
        .await
        .expect("build relay state")
}

fn random_seed() -> [u8; 32] {
    random_bytes(32).as_slice().try_into().expect("32 bytes")
}

fn device_registration(
    seed: [u8; 32],
    kind: ConnectDeviceKind,
    machine_name: Option<&str>,
) -> DeviceRegistration {
    let signing = SigningKey::from_bytes(&seed);
    DeviceRegistration {
        name: format!("e2e {} device", kind.as_str()),
        kind,
        proof_public_key: b64(signing.verifying_key().as_bytes()),
        noise_static_public_key: Some(b64(&random_bytes(32))),
        platform: Some("linux".to_string()),
        machine_name: machine_name.map(ToOwned::to_owned),
    }
}

fn dpop_headers(
    seed: [u8; 32],
    access: &str,
    method: &Method,
    path: &str,
) -> Vec<(String, String)> {
    let timestamp = Utc::now().timestamp();
    let nonce = random_b64(16);
    let signature = SigningKey::from_bytes(&seed)
        .sign(canonical_proof(method.as_str(), path, timestamp, &nonce).as_bytes())
        .to_bytes();
    vec![
        ("authorization".to_string(), format!("Bearer {access}")),
        ("x-h4rn-proof".to_string(), b64(&signature)),
        ("x-h4rn-timestamp".to_string(), timestamp.to_string()),
        ("x-h4rn-nonce".to_string(), nonce),
    ]
}

fn json_request(method: Method, path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn empty_request(method: Method, path: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

fn authed_request(
    method: Method,
    path: &str,
    seed: [u8; 32],
    access: &str,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method.clone()).uri(path);
    for (name, value) in dpop_headers(seed, access, &method, path) {
        builder = builder.header(name, value);
    }
    let body = match body {
        Some(body) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&body).unwrap())
        }
        None => Body::empty(),
    };
    builder.body(body).unwrap()
}

async fn send_full(state: &AppState, request: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let response = router(state.clone())
        .oneshot(request)
        .await
        .expect("oneshot");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()))
    };
    (status, headers, value)
}

async fn send(state: &AppState, request: Request<Body>) -> (StatusCode, Value) {
    let (status, _, body) = send_full(state, request).await;
    (status, body)
}

struct Login {
    access: String,
    refresh: String,
    user_id: String,
    device_id: String,
    seed: [u8; 32],
}

async fn login(
    state: &AppState,
    settings: &OrbitId,
    provider_id: &str,
    kind: ConnectDeviceKind,
    machine_name: Option<&str>,
) -> Login {
    let seed = random_seed();
    let device = device_registration(seed, kind, machine_name);
    let start_path = format!("/v1/auth/{provider_id}/start");
    let start_body = json!({"redirect_uri": settings.redirect, "device": device});
    let (status, _, start) =
        send_full(state, json_request(Method::POST, &start_path, start_body)).await;
    assert_eq!(status, StatusCode::OK, "start failed: {start}");

    let authorize_url = start["authorize_url"]
        .as_str()
        .expect("authorize_url")
        .to_string();
    let flow_state = start["state"].as_str().expect("state").to_string();
    let parsed = Url::parse(&authorize_url).expect("authorize URL");
    let params: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
    assert_eq!(
        params.get("code_challenge_method").map(String::as_str),
        Some("S256"),
        "{authorize_url}"
    );
    assert!(params.contains_key("code_challenge"));
    assert!(
        params
            .get("scope")
            .is_some_and(|scope| scope.split(' ').any(|item| item == "offline_access")),
        "offline_access must be requested: {authorize_url}"
    );
    assert_eq!(params.get("state"), Some(&flow_state));

    let code = authorize_headless(
        &settings.issuer,
        &authorize_url,
        &settings.session_jwt,
        &flow_state,
    )
    .await;

    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("code", &code)
        .append_pair("state", &flow_state)
        .finish();
    let callback_path = format!("/v1/auth/{provider_id}/callback?{query}");
    let (status, headers, body) =
        send_full(state, empty_request(Method::GET, &callback_path)).await;
    assert!(status.is_redirection(), "callback failed: {status} {body}");
    let location = headers
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("callback Location")
        .to_string();
    assert!(
        location.starts_with(&settings.redirect),
        "callback did not return to the app: {location}"
    );
    let parsed = Url::parse(&location).expect("callback URL");
    let fragment: HashMap<String, String> =
        url::form_urlencoded::parse(parsed.fragment().unwrap_or_default().as_bytes())
            .into_owned()
            .collect();
    assert_eq!(
        fragment.get("token_type").map(String::as_str),
        Some("Bearer")
    );
    Login {
        access: fragment.get("access_token").expect("access_token").clone(),
        refresh: fragment
            .get("refresh_token")
            .expect("refresh_token")
            .clone(),
        user_id: fragment.get("user_id").expect("user_id").clone(),
        device_id: fragment.get("device_id").expect("device_id").clone(),
        seed,
    }
}

/// Follows `/oauth/authorize` headlessly with a session-JWT bearer.
///
/// OrbitID resolves the caller from `Authorization: Bearer` before looking at
/// the sign-in/consent screens. Consent is disabled for the test client, but
/// the consent branch is still handled because a misconfigured instance
/// redirects there instead of to the application (research quirk 27).
async fn authorize_headless(
    issuer: &str,
    authorize_url: &str,
    session_jwt: &str,
    expected_state: &str,
) -> String {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("authorize HTTP client");
    let issuer_url = Url::parse(issuer).expect("issuer URL");
    let mut next = authorize_url.to_string();
    for _ in 0..4 {
        let response = http
            .get(&next)
            .bearer_auth(session_jwt)
            .send()
            .await
            .expect("authorize request");
        let status = response.status();
        assert!(
            status.is_redirection(),
            "authorize returned {status} (expected a redirect)"
        );
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .expect("authorize Location")
            .to_string();
        let target = Url::parse(&location).expect("authorize redirect URL");
        let same_origin = target.scheme() == issuer_url.scheme()
            && target.host_str() == issuer_url.host_str()
            && target.port_or_known_default() == issuer_url.port_or_known_default();
        if same_origin && target.path().ends_with("/sign-in") {
            panic!(
                "OrbitID asked for an interactive sign-in even though a session JWT bearer was supplied: {location}"
            );
        }
        if same_origin && target.path().ends_with("/oauth-consent") {
            let query: HashMap<String, String> = target.query_pairs().into_owned().collect();
            let scopes: Vec<&str> = query
                .get("scope")
                .map(|scope| scope.split(' ').collect())
                .unwrap_or_default();
            let body = json!({
                "client_id": query.get("client_id"),
                "approve": true,
                "scope": query.get("scope"),
                "scopes": scopes,
                "redirect_uri": query.get("redirect_uri"),
                "state": query.get("state"),
                "response_type": query.get("response_type"),
                "code_challenge": query.get("code_challenge"),
                "code_challenge_method": query.get("code_challenge_method"),
            });
            let consent = http
                .post(format!("{issuer}/oauth/consent"))
                .bearer_auth(session_jwt)
                .json(&body)
                .send()
                .await
                .expect("consent request");
            assert!(
                consent.status().is_success(),
                "consent returned {}",
                consent.status()
            );
            let json: Value = consent.json().await.expect("consent JSON");
            next = json["redirect_url"]
                .as_str()
                .expect("consent redirect_url")
                .to_string();
            continue;
        }
        let query: HashMap<String, String> = target.query_pairs().into_owned().collect();
        if let Some(error) = query.get("error") {
            panic!(
                "OrbitID returned `{error}` ({}): {location}",
                query
                    .get("error_description")
                    .map(String::as_str)
                    .unwrap_or("no description")
            );
        }
        assert_eq!(
            query.get("state").map(String::as_str),
            Some(expected_state),
            "state mismatch at {location}"
        );
        return query.get("code").expect("authorization code").clone();
    }
    panic!("authorize never reached the application redirect: {authorize_url}");
}

/// Runs one authorization-code exchange directly against OrbitID, without the
/// relay, so a token from the alternate issuer can be inspected.
async fn direct_code_token(
    settings: &OrbitId,
    issuer: &str,
    client_id: &str,
    client_secret: Option<&str>,
    session_jwt: &str,
    nonce: &str,
) -> Value {
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut url = Url::parse(&format!("{issuer}/oauth/authorize")).expect("authorize endpoint");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &settings.redirect)
        .append_pair("scope", "openid profile email offline_access")
        .append_pair("state", "direct-state")
        .append_pair("nonce", nonce)
        .append_pair("code_challenge", challenge.as_str())
        .append_pair("code_challenge_method", "S256");
    let code = authorize_headless(issuer, url.as_str(), session_jwt, "direct-state").await;

    let mut form = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code),
        ("redirect_uri".to_string(), settings.redirect.clone()),
        ("client_id".to_string(), client_id.to_string()),
        ("code_verifier".to_string(), verifier.secret().to_string()),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret".to_string(), secret.to_string()));
    }
    let response = reqwest::Client::new()
        .post(format!("{issuer}/oauth/token"))
        .form(&form)
        .send()
        .await
        .expect("token request");
    let status = response.status();
    let body: Value = response.json().await.expect("token JSON");
    assert!(
        status.is_success(),
        "token endpoint returned {status}: {body}"
    );
    body
}

async fn me(state: &AppState, seed: [u8; 32], access: &str) -> (StatusCode, Value) {
    let request = authed_request(Method::GET, "/v1/me", seed, access, None);
    send(state, request).await
}

#[tokio::test]
#[ignore = "requires a running OrbitID (use ./scripts/test-connect.sh --orbitid)"]
async fn authorization_code_pkce_creates_user_identity_session_and_refresh_token() {
    let settings = OrbitId::load();
    let state = test_state(&settings).await;

    let public = login(
        &state,
        &settings,
        "orbitid",
        ConnectDeviceKind::Desktop,
        None,
    )
    .await;
    let user_id = Uuid::parse_str(&public.user_id).expect("user id is a UUID");
    let (status, me_body) = me(&state, public.seed, &public.access).await;
    assert_eq!(status, StatusCode::OK, "{me_body}");
    assert_eq!(me_body["user"]["id"], public.user_id);
    assert_eq!(me_body["user"]["email"], "ada@h4rn.test");
    assert_eq!(me_body["devices"][0]["id"], public.device_id);

    let identity: (String, String, String) =
        sqlx::query_as("SELECT provider_id, issuer, subject FROM identities WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(state.store.pool())
            .await
            .expect("identity row");
    assert_eq!(identity.0, "orbitid");
    assert_eq!(identity.1, settings.issuer);
    assert!(identity.2.starts_with("user_"), "{}", identity.2);

    let live: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions WHERE refresh_hash = $1 AND revoked_at IS NULL",
    )
    .bind(hash_refresh_token(&public.refresh))
    .fetch_one(state.store.pool())
    .await
    .expect("session count");
    assert_eq!(live, 1, "a session row must back the refresh token");

    // Quirk 6/13: the provider refresh token is stored encrypted, never in
    // plaintext, so logout can revoke it later.
    let stored = state
        .store
        .get_secret(user_id, "oidc_refresh", "orbitid", &state.envelope)
        .await
        .expect("read stored provider refresh token");
    assert!(stored.is_some());

    // The confidential client works through the same start/callback surface
    // and reuses the identity by `iss` + `sub` instead of creating a user.
    let confidential = login(
        &state,
        &settings,
        "orbitid-confidential",
        ConnectDeviceKind::Mobile,
        None,
    )
    .await;
    assert_eq!(
        confidential.user_id, public.user_id,
        "the same OrbitID subject must map to one h4rn user"
    );
    let (status, me_body) = me(&state, confidential.seed, &confidential.access).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me_body["devices"].as_array().unwrap().len(), 2);
}

#[tokio::test]
#[ignore = "requires a running OrbitID (use ./scripts/test-connect.sh --orbitid)"]
async fn refresh_rotation_replays_within_grace_then_revokes_the_family() {
    let settings = OrbitId::load();
    let state = test_state(&settings).await;
    let session = login(
        &state,
        &settings,
        "orbitid-confidential",
        ConnectDeviceKind::Desktop,
        None,
    )
    .await;

    let (status, first) = send(
        &state,
        json_request(
            Method::POST,
            "/v1/auth/refresh",
            json!({"refresh_token": session.refresh}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let rotated = first["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(rotated, session.refresh);
    assert_eq!(first["token_type"], "Bearer");

    // A replay inside the grace window returns the same successor.
    let (status, replay) = send(
        &state,
        json_request(
            Method::POST,
            "/v1/auth/refresh",
            json!({"refresh_token": session.refresh}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["refresh_token"], rotated);

    // Outside the window, reuse is theft and the whole family dies.
    sqlx::query(
        "UPDATE sessions SET rotated_at = now() - interval '1 hour' WHERE refresh_hash = $1",
    )
    .bind(hash_refresh_token(&session.refresh))
    .execute(state.store.pool())
    .await
    .expect("age the rotation");
    let (status, body) = send(
        &state,
        json_request(
            Method::POST,
            "/v1/auth/refresh",
            json!({"refresh_token": session.refresh}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "refresh_reused");
    let revoked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions WHERE refresh_hash = $1 AND revoked_at IS NOT NULL",
    )
    .bind(hash_refresh_token(&rotated))
    .fetch_one(state.store.pool())
    .await
    .expect("successor row");
    assert_eq!(revoked, 1, "the successor must be revoked with the family");
}

#[tokio::test]
#[ignore = "requires a running OrbitID (use ./scripts/test-connect.sh --orbitid)"]
async fn logout_revokes_the_local_family_and_the_provider_refresh_token() {
    let settings = OrbitId::load();
    let state = test_state(&settings).await;
    let session = login(
        &state,
        &settings,
        "orbitid-confidential",
        ConnectDeviceKind::Desktop,
        None,
    )
    .await;
    let user_id = Uuid::parse_str(&session.user_id).expect("user id");

    let provider_refresh = state
        .store
        .get_secret(
            user_id,
            "oidc_refresh",
            "orbitid-confidential",
            &state.envelope,
        )
        .await
        .expect("read provider refresh token")
        .expect("offline_access must have produced a provider refresh token");

    // Quirk 10: the TS build has no end-session endpoint. Logout is local-first
    // and the provider refresh token is revoked through `/oauth/revoke`.
    assert!(
        state
            .oidc
            .end_session_endpoint("orbitid-confidential")
            .await
            .is_none(),
        "this suite targets the TS OrbitID build, which has no end_session_endpoint"
    );

    let (status, body) = send(
        &state,
        authed_request(
            Method::POST,
            "/v1/auth/logout",
            session.seed,
            &session.access,
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, body) = me(&state, session.seed, &session.access).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "session_revoked");

    let remaining = state
        .store
        .get_secret(
            user_id,
            "oidc_refresh",
            "orbitid-confidential",
            &state.envelope,
        )
        .await
        .expect("read provider refresh token after logout");
    assert!(remaining.is_none(), "the provider token must be dropped");

    // The provider must now reject the revoked refresh token.
    let response = reqwest::Client::new()
        .post(format!("{}/oauth/token", settings.issuer))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", provider_refresh.as_str()),
            ("client_id", settings.confidential_client_id.as_str()),
            (
                "client_secret",
                settings.confidential_client_secret.as_str(),
            ),
        ])
        .send()
        .await
        .expect("provider refresh request");
    assert!(
        !response.status().is_success(),
        "OrbitID still accepted a revoked refresh token"
    );
    let body: Value = response.json().await.expect("OAuth error JSON");
    assert_eq!(body["error"], "invalid_grant", "{body}");
}

#[tokio::test]
#[ignore = "requires a running OrbitID (use ./scripts/test-connect.sh --orbitid)"]
async fn device_authorization_grant_logs_a_daemon_in() {
    let settings = OrbitId::load();
    let state = test_state(&settings).await;
    let seed = random_seed();
    let device = device_registration(seed, ConnectDeviceKind::Daemon, Some("e2e-machine"));

    let (status, start) = send(
        &state,
        json_request(
            Method::POST,
            "/v1/auth/device/start",
            json!({"provider": "orbitid-confidential", "device": device}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{start}");
    let grant_id = start["grant_id"].as_str().expect("grant_id").to_string();
    let user_code = start["user_code"].as_str().expect("user_code").to_string();
    let interval = start["interval"].as_i64().unwrap_or(1);
    assert!(!user_code.is_empty());
    assert!(
        start["verification_uri"]
            .as_str()
            .is_some_and(|uri| uri.starts_with(&settings.issuer)),
        "{start}"
    );

    // Approve the user code with the same headless session JWT.
    let approval = reqwest::Client::new()
        .post(format!(
            "{}/v1/client/oauth/device/approve",
            settings.issuer
        ))
        .bearer_auth(&settings.session_jwt)
        .json(&json!({"user_code": user_code, "action": "approve"}))
        .send()
        .await
        .expect("device approval request");
    let status = approval.status();
    let approval_body: Value = approval.json().await.expect("approval JSON");
    assert!(status.is_success(), "approval failed: {approval_body}");
    assert_eq!(approval_body["status"], "approved");

    let mut poll = Value::Null;
    let mut approved = false;
    for _ in 0..3 {
        let (status, body) = send(
            &state,
            json_request(
                Method::POST,
                "/v1/auth/device/poll",
                json!({"grant_id": grant_id}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        poll = body;
        match poll["status"].as_str() {
            Some("approved") => {
                approved = true;
                break;
            }
            Some("pending") | Some("slow_down") => {
                tokio::time::sleep(Duration::from_secs((interval + 1).max(2) as u64)).await;
            }
            other => panic!("unexpected device poll status: {other:?} in {poll}"),
        }
    }
    assert!(approved, "device grant was not approved: {poll}");
    let tokens = &poll["tokens"];
    let access = tokens["access_token"].as_str().expect("access token");
    let refresh = tokens["refresh_token"].as_str().expect("refresh token");
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(poll["device"]["kind"], "daemon");
    assert!(poll["device"]["machine_id"].is_string());

    let (status, me_body) = me(&state, seed, access).await;
    assert_eq!(status, StatusCode::OK, "{me_body}");
    assert_eq!(me_body["user"]["id"], poll["user"]["id"]);
    assert!(
        me_body["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|device| device["kind"] == "daemon" && device["id"] == poll["device"]["id"])
    );
    assert!(!refresh.is_empty());
}

#[tokio::test]
#[ignore = "requires a running OrbitID (use ./scripts/test-connect.sh --orbitid)"]
async fn id_token_from_a_non_configured_issuer_is_rejected() {
    let settings = OrbitId::load();
    let state = test_state(&settings).await;

    // The alternate instance shares the database and signing keys but has a
    // different `ORBITID_BASE_URL`, so its ID tokens claim an issuer that the
    // relay was not configured with.
    let alt_nonce = random_b64(16);
    let alt = direct_code_token(
        &settings,
        &settings.alt_issuer,
        &settings.confidential_client_id,
        Some(&settings.confidential_client_secret),
        &settings.alt_session_jwt,
        &alt_nonce,
    )
    .await;
    let alt_id_token = alt["id_token"].as_str().expect("alt id_token");
    let payload = alt_id_token.split('.').nth(1).expect("JWT payload");
    let payload: Value = serde_json::from_slice(
        &h4rn_connect::crypto::b64_decode(payload).expect("base64url payload"),
    )
    .expect("JWT payload JSON");
    assert_eq!(payload["iss"], settings.alt_issuer);

    let error = state
        .oidc
        .verify_id_token(
            "orbitid-confidential",
            alt_id_token,
            Some(alt_nonce.as_str()),
        )
        .await
        .expect_err("a token from another issuer must be rejected");
    assert!(
        error.to_string().contains("issuer_mismatch"),
        "unexpected error: {error}"
    );

    // Sanity: a token from the configured issuer still verifies through the
    // same code path, so the negative case is not rejecting everything.
    let main_nonce = random_b64(16);
    let main = direct_code_token(
        &settings,
        &settings.issuer,
        &settings.confidential_client_id,
        Some(&settings.confidential_client_secret),
        &settings.session_jwt,
        &main_nonce,
    )
    .await;
    let profile = state
        .oidc
        .verify_id_token(
            "orbitid-confidential",
            main["id_token"].as_str().expect("main id_token"),
            Some(main_nonce.as_str()),
        )
        .await
        .expect("a token from the configured issuer must verify");
    assert!(profile.subject.starts_with("user_"));
    assert_eq!(profile.email.as_deref(), Some("ada@h4rn.test"));
}

#[tokio::test]
#[ignore = "requires a running OrbitID (use ./scripts/test-connect.sh --orbitid)"]
async fn issuer_with_a_trailing_slash_is_normalised() {
    let settings = OrbitId::load();
    let state = test_state(&settings).await;

    let configured = state
        .config
        .provider("orbitid-slash")
        .expect("slash provider")
        .issuer
        .clone();
    assert_eq!(
        configured, settings.issuer,
        "the trailing slash must be trimmed before discovery"
    );

    let providers = state.oidc.public_providers().await;
    let slash = providers
        .iter()
        .find(|provider| provider.id == "orbitid-slash")
        .expect("slash provider is advertised");
    assert_eq!(slash.issuer, settings.issuer);
    assert!(slash.device_grant, "device grant must follow discovery");

    let (url, verifier) = state
        .oidc
        .start_login(
            "orbitid-slash",
            &settings.redirect,
            "slash-state",
            "slash-nonce",
        )
        .await
        .expect("authorize URL from a slash-configured issuer");
    assert!(!verifier.is_empty());
    let parsed = Url::parse(&url).expect("authorize URL");
    assert!(parsed.path().ends_with("/oauth/authorize"));
    let params: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
    assert_eq!(
        params.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
}
