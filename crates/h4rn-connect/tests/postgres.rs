//! Postgres-backed integration tests for the relay.
//!
//! These are `#[ignore]`d so `cargo test --workspace` stays hermetic. The
//! `scripts/test-connect.sh` helper starts Postgres in Docker, sets
//! `H4RN_TEST_DATABASE_URL`, and runs them with `--ignored`. They exercise the
//! real schema, real transactions and the real router; nothing is mocked.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::Utc;
use ed25519_dalek::{Signer as _, SigningKey};
use h4rn_protocol::{ConnectDeviceKind, PairingPayload};
use http_body_util::BodyExt;
use tower::ServiceExt as _;
use uuid::Uuid;

use h4rn_connect::api::{AppState, build_state, router};
use h4rn_connect::config::Config;
use h4rn_connect::crypto::{b64, random_b64, random_bytes};
use h4rn_connect::dpop::canonical_proof;

fn database_url() -> String {
    std::env::var("H4RN_TEST_DATABASE_URL").expect(
        "H4RN_TEST_DATABASE_URL must be set; run ./scripts/test-connect.sh instead of cargo test directly",
    )
}

async fn test_state() -> AppState {
    let mut config = Config {
        public_base_url: "http://localhost:8787".to_string(),
        database_url: database_url(),
        kek: random_b64(32),
        signing_key: Some(random_b64(32)),
        allow_insecure: true,
        ..Config::default()
    };
    config.rate_limits.per_ip_per_minute = 10_000;
    config.rate_limits.auth_per_minute = 10_000;
    config.rate_limits.pairing_per_hour = 10_000;
    build_state(config).await.expect("build relay state")
}

struct Actor {
    user: h4rn_connect::store::UserRow,
    device: h4rn_connect::store::DeviceRow,
    seed: [u8; 32],
    access: String,
    refresh: String,
}

async fn actor(state: &AppState, kind: ConnectDeviceKind, machine_name: Option<&str>) -> Actor {
    let subject = Uuid::now_v7().to_string();
    actor_with_subject(state, kind, machine_name, &subject).await
}

async fn actor_with_subject(
    state: &AppState,
    kind: ConnectDeviceKind,
    machine_name: Option<&str>,
    subject: &str,
) -> Actor {
    let seed_bytes = random_bytes(32);
    let seed: [u8; 32] = seed_bytes.as_slice().try_into().unwrap();
    let signing = SigningKey::from_bytes(&seed);
    let proof = signing.verifying_key().as_bytes().to_vec();
    let noise = random_bytes(32);
    let (user, device) = state
        .store
        .complete_login(
            "test",
            "https://idp.test",
            subject,
            Some(&format!("{subject}@example.test")),
            true,
            Some("Integration Tester"),
            None,
            serde_json::json!({"sub": subject}),
            h4rn_connect::store::NewDevice {
                kind,
                name: format!("{} device", kind.as_str()),
                proof_public_key: proof,
                noise_static_public_key: Some(noise),
                platform: Some("linux".to_string()),
                machine_name: machine_name.map(ToOwned::to_owned),
            },
        )
        .await
        .expect("complete login");
    let issued = state
        .store
        .issue_session(
            &state.keystore,
            &state.config,
            user.id,
            device.id,
            None,
            None,
        )
        .await
        .expect("issue session");
    Actor {
        user,
        device,
        seed,
        access: issued.tokens.access_token,
        refresh: issued.tokens.refresh_token,
    }
}

impl Actor {
    fn headers(&self, method: Method, path: &str) -> Vec<(String, String)> {
        let timestamp = Utc::now().timestamp();
        let nonce = random_b64(16);
        let signature = SigningKey::from_bytes(&self.seed)
            .sign(canonical_proof(method.as_str(), path, timestamp, &nonce).as_bytes())
            .to_bytes();
        vec![
            (
                "authorization".to_string(),
                format!("Bearer {}", self.access),
            ),
            ("x-h4rn-proof".to_string(), b64(&signature)),
            ("x-h4rn-timestamp".to_string(), timestamp.to_string()),
            ("x-h4rn-nonce".to_string(), nonce),
        ]
    }
}

fn request(
    method: Method,
    path: &str,
    actor: Option<&Actor>,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method.clone()).uri(path);
    if let Some(actor) = actor {
        for (name, value) in actor.headers(method, path.split('?').next().unwrap_or(path)) {
            builder = builder.header(name, value);
        }
    }
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    builder.body(body).unwrap()
}

async fn send(state: &AppState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = router(state.clone())
        .oneshot(request)
        .await
        .expect("oneshot");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    let value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(&bytes).to_string())
        })
    };
    (status, value)
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn health_ready_and_authentication_envelope() {
    let state = test_state().await;
    let (status, body) = send(&state, request(Method::GET, "/healthz", None, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::Value::String("ok".to_string()));

    let (status, body) = send(&state, request(Method::GET, "/readyz", None, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ready");

    let (status, body) = send(&state, request(Method::GET, "/v1/me", None, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "token_required");
    assert!(body["message"].is_string());

    let actor = actor(&state, ConnectDeviceKind::Desktop, None).await;
    let (status, body) = send(&state, request(Method::GET, "/v1/me", Some(&actor), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user"]["id"], actor.user.id.to_string());

    // Replaying the same proof is rejected.
    let headers = actor.headers(Method::GET, "/v1/me");
    let repeated = || {
        let mut builder = Request::builder().method(Method::GET).uri("/v1/me");
        for (name, value) in &headers {
            builder = builder.header(name, value);
        }
        builder.body(Body::empty()).unwrap()
    };
    let (status, _) = send(&state, repeated()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(&state, repeated()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "proof_replayed");
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn refresh_rotation_replays_within_grace_then_revokes_the_family() {
    let state = test_state().await;
    let actor = actor(&state, ConnectDeviceKind::Desktop, None).await;

    let (status, first) = send(
        &state,
        request(
            Method::POST,
            "/v1/auth/refresh",
            None,
            Some(serde_json::json!({"refresh_token": actor.refresh})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let rotated = first["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(rotated, actor.refresh);

    // A retry of the old token inside the grace window returns the same
    // successor instead of revoking the family (ADR-011).
    let (status, replay) = send(
        &state,
        request(
            Method::POST,
            "/v1/auth/refresh",
            None,
            Some(serde_json::json!({"refresh_token": actor.refresh})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["refresh_token"], rotated);

    // Outside the window, reuse is treated as theft and the family dies.
    let old = h4rn_connect::crypto::hash_refresh_token(&actor.refresh);
    sqlx::query(
        "UPDATE sessions SET rotated_at = now() - interval '1 hour' WHERE refresh_hash = $1",
    )
    .bind(&old)
    .execute(state.store.pool())
    .await
    .unwrap();
    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/auth/refresh",
            None,
            Some(serde_json::json!({"refresh_token": actor.refresh})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "refresh_reused");

    let revoked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions WHERE refresh_hash = $1 AND revoked_at IS NOT NULL",
    )
    .bind(h4rn_connect::crypto::hash_refresh_token(&rotated))
    .fetch_one(state.store.pool())
    .await
    .unwrap();
    assert_eq!(revoked, 1, "the successor must be revoked with the family");
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn pairing_is_single_use_and_scoped_to_the_machine_owner() {
    let state = test_state().await;
    let subject = Uuid::now_v7().to_string();
    let daemon = actor_with_subject(
        &state,
        ConnectDeviceKind::Daemon,
        Some("test-machine"),
        &subject,
    )
    .await;
    let client = actor_with_subject(&state, ConnectDeviceKind::Desktop, None, &subject).await;
    let machine = state
        .store
        .machine(daemon.device.machine_id.expect("daemon machine"))
        .await
        .unwrap();

    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings",
            Some(&daemon),
            Some(serde_json::json!({"machine_id": machine.id.to_string()})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let qr = body["qr"].as_str().unwrap();
    let payload = PairingPayload::parse(qr).unwrap();
    assert_eq!(payload.machine_id, machine.id.to_string());
    assert_eq!(payload.connect_url, "http://localhost:8787");

    let claim_body = serde_json::json!({
        "payload": qr,
        "device_public_key": b64(SigningKey::from_bytes(&client.seed).verifying_key().as_bytes()),
        "noise_static_public_key": b64(&random_bytes(32)),
        "device_name": "Claimed desktop",
    });
    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings/claim",
            Some(&client),
            Some(claim_body.clone()),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["pairing"]["state"], "claimed");
    assert_eq!(body["machine"]["id"], machine.id.to_string());

    // The same QR cannot be claimed twice.
    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings/claim",
            Some(&client),
            Some(claim_body),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "pairing_invalid");

    // A different user's device cannot claim a pairing for this machine.
    let stranger = actor(&state, ConnectDeviceKind::Mobile, None).await;
    let (status, created) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings",
            Some(&daemon),
            Some(serde_json::json!({"machine_id": machine.id.to_string()})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let stranger_claim = serde_json::json!({
        "payload": created["qr"],
        "device_public_key": b64(SigningKey::from_bytes(&stranger.seed).verifying_key().as_bytes()),
    });
    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings/claim",
            Some(&stranger),
            Some(stranger_claim),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "machine_forbidden");
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn secrets_audit_hosts_and_device_revocation_work_through_the_store() {
    let state = test_state().await;
    let daemon = actor(&state, ConnectDeviceKind::Daemon, Some("host-machine")).await;
    let machine = state
        .store
        .machine(daemon.device.machine_id.expect("daemon machine"))
        .await
        .unwrap();

    state
        .store
        .put_secret(daemon.user.id, "test", "token", "s3cr3t", &state.envelope)
        .await
        .unwrap();
    let secret = state
        .store
        .get_secret(daemon.user.id, "test", "token", &state.envelope)
        .await
        .unwrap();
    assert_eq!(secret.as_deref(), Some("s3cr3t"));
    let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
        "SELECT ciphertext FROM encrypted_secrets WHERE user_id = $1 AND purpose = 'test'",
    )
    .bind(daemon.user.id)
    .fetch_all(state.store.pool())
    .await
    .unwrap();
    assert_ne!(
        rows[0].0, b"s3cr3t",
        "secrets must not be stored in plaintext"
    );

    let hosts = serde_json::json!({
        "hosts": [
            {"name": "windows", "kind": "host", "distro": null},
            {"name": "Ubuntu-24.04", "kind": "wsl", "distro": "Ubuntu-24.04"}
        ]
    });
    let (status, body) = send(
        &state,
        request(
            Method::PUT,
            &format!("/v1/machines/{}/hosts", machine.id),
            Some(&daemon),
            Some(hosts),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 2);

    let (status, body) = send(
        &state,
        request(Method::GET, "/v1/audit", Some(&daemon), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body["entries"].as_array().unwrap().is_empty());

    let machines = state
        .store
        .machines(daemon.user.id, &std::collections::HashSet::new())
        .await
        .unwrap();
    assert!(machines.iter().any(|row| row.id == machine.id.to_string()));

    // Revoking the daemon kills its sessions.
    let (status, _) = send(
        &state,
        request(
            Method::DELETE,
            &format!("/v1/devices/{}", daemon.device.id),
            Some(&daemon),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let live: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sessions WHERE device_id = $1 AND revoked_at IS NULL",
    )
    .bind(daemon.device.id)
    .fetch_one(state.store.pool())
    .await
    .unwrap();
    assert_eq!(live, 0);
}
