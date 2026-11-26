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
use rsa::RsaPrivateKey;
use rsa::pkcs8::{EncodePrivateKey, LineEnding};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt as _;
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use h4rn_connect::alerts::AlertService;
use h4rn_connect::api::{AppState, build_state, router};
use h4rn_connect::config::Config;
use h4rn_connect::crypto::{b64, random_b64, random_bytes};
use h4rn_connect::dpop::canonical_proof;
use h4rn_connect::push::FcmClient;

fn database_url() -> String {
    std::env::var("H4RN_TEST_DATABASE_URL").expect(
        "H4RN_TEST_DATABASE_URL must be set; run ./scripts/test-connect.sh instead of cargo test directly",
    )
}

async fn test_state() -> AppState {
    test_state_with(|_| {}).await
}

async fn test_state_with(tweak: impl FnOnce(&mut Config)) -> AppState {
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
    tweak(&mut config);
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
    let (status, _, value) = send_with_headers(state, request).await;
    (status, value)
}

/// Like [`send`], but also hands back the response headers.
async fn send_with_headers(
    state: &AppState,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
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
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(&bytes).to_string())
        })
    };
    (status, headers, value)
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

fn usage_row(row_id: &str, started: &str, output: u64, deduped: bool) -> serde_json::Value {
    serde_json::json!({
        "row_id": row_id,
        "thread_id": "th_meter",
        "run_id": format!("run_{row_id}"),
        "workspace": "/repo",
        "machine": "host",
        "account": null,
        "harness": "claude-code",
        "model": "claude-sonnet-5",
        "provider": "anthropic",
        "credential": null,
        "source": if deduped { "proxy_reported" } else { "self_metered" },
        "correlation_id": format!("req_{row_id}"),
        "tokens_in": 100,
        "tokens_out": output,
        "tokens_cached_read": 50,
        "tokens_cached_creation": 10,
        "cost_micros": 1234,
        "cost_source": "price_table",
        "price_table": "2026-11",
        "ttft_ms": 100,
        "tps": 20.0,
        "generation_ms": 10_000,
        "wall_ms": 12_000,
        "started_at": started,
        "completed_at": started,
        "ts": started,
        "raw": {"source": "test"},
        "deduped": deduped
    })
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn usage_sync_is_idempotent_and_aggregates_by_bucket() {
    let state = test_state().await;
    let daemon = actor(&state, ConnectDeviceKind::Daemon, Some("meter-machine")).await;

    let first = usage_row("usage_1", "2026-11-13T12:05:00Z", 200, false);
    let second = usage_row("usage_2", "2026-11-13T12:35:00Z", 6, false);
    let deduped = usage_row("usage_3", "2026-11-13T12:40:00Z", 9_999, true);
    let batch = serde_json::json!({"rows": [first, second]});

    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/usage",
            Some(&daemon),
            Some(batch.clone()),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["accepted"], 2);
    assert_eq!(body["duplicates"], 0);

    // A retried batch must not double count.
    let (status, body) = send(
        &state,
        request(Method::POST, "/v1/usage", Some(&daemon), Some(batch)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["accepted"], 0);
    assert_eq!(body["duplicates"], 2);

    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/usage",
            Some(&daemon),
            Some(serde_json::json!({"rows": [deduped]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["accepted"], 1);

    let (status, report) = send(
        &state,
        request(
            Method::GET,
            "/v1/usage/aggregate?scope=thread&scope_id=th_meter&group_by=model&bucket=hour&since=2026-11-13T00:00:00Z&until=2026-11-14T00:00:00Z",
            Some(&daemon),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["totals"]["turns"], 2);
    assert_eq!(report["totals"]["input_tokens"], 200);
    assert_eq!(report["totals"]["output_tokens"], 206);
    assert_eq!(report["totals"]["cached_input_tokens"], 100);
    assert_eq!(report["totals"]["cost_micros"], 2_468);
    assert_eq!(report["groups"].as_array().unwrap().len(), 1);
    assert_eq!(report["groups"][0]["key"], "claude-sonnet-5");
    let buckets = report["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 1, "both turns are in the same hour");
    assert_eq!(buckets[0]["totals"]["turns"], 2);

    // Another account cannot see the first account's rows.
    let stranger = actor(&state, ConnectDeviceKind::Desktop, None).await;
    let (status, report) = send(
        &state,
        request(
            Method::GET,
            "/v1/usage/aggregate?scope=global",
            Some(&stranger),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{report}");
    assert_eq!(report["totals"]["turns"], 0);

    let stored: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM usage_events WHERE user_id = $1 AND row_id LIKE 'usage_%'",
    )
    .bind(daemon.user.id)
    .fetch_one(state.store.pool())
    .await
    .unwrap();
    assert_eq!(stored, 3, "idempotent retries store one row per row_id");
}

fn claim_code_body(code: &str, actor: &Actor) -> serde_json::Value {
    serde_json::json!({
        "code": code,
        "device_public_key": b64(SigningKey::from_bytes(&actor.seed).verifying_key().as_bytes()),
        "noise_static_public_key": b64(&random_bytes(32)),
        "device_name": "Typed-code phone",
    })
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn short_code_pairing_grants_exactly_the_chosen_scopes() {
    let state = test_state().await;
    let subject = Uuid::now_v7().to_string();
    let daemon =
        actor_with_subject(&state, ConnectDeviceKind::Daemon, Some("scoped"), &subject).await;
    let phone = actor_with_subject(&state, ConnectDeviceKind::Mobile, None, &subject).await;
    let machine_id = daemon.device.machine_id.expect("daemon machine");

    // An unpaired phone holds `view` alone: it can claim, not mint pairings.
    assert_eq!(
        state.store.token_scopes(phone.device.id).await.unwrap(),
        vec!["view".to_string()]
    );
    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings",
            Some(&phone),
            Some(serde_json::json!({"machine_id": machine_id.to_string()})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "scope_required");

    // The daemon mints a pairing that grants `approve` (and the implied view).
    let (status, created) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings",
            Some(&daemon),
            Some(serde_json::json!({
                "machine_id": machine_id.to_string(),
                "scopes": ["approve"],
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(
        created["pairing"]["scopes"],
        serde_json::json!(["view", "approve"])
    );
    let code = created["short_code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 8);
    assert!(
        code.bytes()
            .all(|b| h4rn_protocol::SHORT_CODE_ALPHABET.contains(&b))
    );

    // A typed code is forgiving about case and separators.
    let typed = format!("{}-{}", code[..4].to_lowercase(), code[4..].to_lowercase());
    let (status, claimed) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings/claim-code",
            Some(&phone),
            Some(claim_code_body(&typed, &phone)),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{claimed}");
    assert_eq!(claimed["pairing"]["state"], "claimed");
    assert_eq!(claimed["machine"]["id"], machine_id.to_string());
    assert!(
        claimed["daemon_noise_public_key"]
            .as_str()
            .is_some_and(|k| !k.is_empty())
    );

    // Single use.
    let (status, again) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings/claim-code",
            Some(&phone),
            Some(claim_code_body(&code, &phone)),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{again}");
    assert_eq!(again["code"], "pairing_invalid");

    // The link, the token and the daemon's snapshot all carry exactly the
    // granted scopes.
    use h4rn_protocol::PairScope::{Approve, View};
    assert_eq!(
        state
            .store
            .link_scopes(machine_id, phone.device.id)
            .await
            .unwrap(),
        vec![View, Approve]
    );
    let issued = state
        .store
        .issue_session(
            &state.keystore,
            &state.config,
            phone.user.id,
            phone.device.id,
            None,
            None,
        )
        .await
        .unwrap();
    let claims = h4rn_connect::jwt::verify_access_token(
        &state.keystore,
        &issued.tokens.access_token,
        Utc::now(),
    )
    .unwrap();
    assert_eq!(claims.scp, vec!["view".to_string(), "approve".to_string()]);
    let wires = state.store.paired_device_wires(machine_id).await.unwrap();
    let wire = wires
        .iter()
        .find(|device| device.id == phone.device.id.to_string())
        .expect("phone in the snapshot");
    assert_eq!(wire.scopes, vec![View, Approve]);

    // Having claimed without `admin`, the phone can no longer mint pairings.
    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings",
            Some(&phone),
            Some(serde_json::json!({"machine_id": machine_id.to_string()})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "scope_required");

    // Nor can it revoke another device, though it may revoke itself.
    let other = format!("/v1/devices/{}", daemon.device.id);
    let (status, body) = send(&state, request(Method::DELETE, &other, Some(&phone), None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "scope_required");

    // The Devices list reports the scopes each device holds.
    let (status, me) = send(&state, request(Method::GET, "/v1/me", Some(&daemon), None)).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    let listed = me["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|device| device["id"] == phone.device.id.to_string())
        .unwrap();
    assert_eq!(listed["scopes"], serde_json::json!(["view", "approve"]));
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn short_codes_expire_hide_across_users_and_are_rate_limited() {
    let state = test_state_with(|config| config.rate_limits.pairing_code_per_hour = 4).await;
    let subject = Uuid::now_v7().to_string();
    let daemon = actor_with_subject(&state, ConnectDeviceKind::Daemon, Some("m"), &subject).await;
    let phone = actor_with_subject(&state, ConnectDeviceKind::Mobile, None, &subject).await;
    let machine_id = daemon.device.machine_id.unwrap();

    let create = |scopes: serde_json::Value| {
        request(
            Method::POST,
            "/v1/pairings",
            Some(&daemon),
            Some(serde_json::json!({"machine_id": machine_id.to_string(), "scopes": scopes})),
        )
    };

    // An expired pairing's code no longer works.
    let (_, expired) = send(&state, create(serde_json::json!(["view"]))).await;
    let pool = sqlx::PgPool::connect(&database_url()).await.unwrap();
    sqlx::query("UPDATE pairings SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(Uuid::parse_str(expired["pairing"]["id"].as_str().unwrap()).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings/claim-code",
            Some(&phone),
            Some(claim_code_body(
                expired["short_code"].as_str().unwrap(),
                &phone,
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "pairing_invalid");

    // Another account's code is indistinguishable from an unknown one.
    let (_, live) = send(&state, create(serde_json::json!(["view"]))).await;
    let stranger = actor(&state, ConnectDeviceKind::Mobile, None).await;
    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings/claim-code",
            Some(&stranger),
            Some(claim_code_body(
                live["short_code"].as_str().unwrap(),
                &stranger,
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "pairing_invalid");

    // Malformed codes are refused before touching the database.
    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings/claim-code",
            Some(&phone),
            Some(claim_code_body("nope", &phone)),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "pairing_code_invalid");

    // Guessing is throttled: after the hourly budget even the live code is
    // refused, so 40 bits cannot be ground through.
    let mut refused = None;
    for attempt in 0..12 {
        let (status, _) = send(
            &state,
            request(
                Method::POST,
                "/v1/pairings/claim-code",
                Some(&phone),
                Some(claim_code_body("0000-0000", &phone)),
            ),
        )
        .await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            refused = Some(attempt);
            break;
        }
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    assert!(
        refused.is_some_and(|n| n < 4),
        "limit never tripped: {refused:?}"
    );
    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings/claim-code",
            Some(&phone),
            Some(claim_code_body(
                live["short_code"].as_str().unwrap(),
                &phone,
            )),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
}

/// Admin on one machine must not be admin on another: scopes are per link, not
/// the union across the account.
#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn admin_on_one_machine_does_not_mint_pairings_for_another() {
    let state = test_state().await;
    let subject = Uuid::now_v7().to_string();
    let alpha =
        actor_with_subject(&state, ConnectDeviceKind::Daemon, Some("alpha"), &subject).await;
    let beta = actor_with_subject(&state, ConnectDeviceKind::Daemon, Some("beta"), &subject).await;
    let phone = actor_with_subject(&state, ConnectDeviceKind::Mobile, None, &subject).await;
    let alpha_id = alpha.device.machine_id.expect("alpha machine");
    let beta_id = beta.device.machine_id.expect("beta machine");
    assert_ne!(alpha_id, beta_id);

    let pair = |daemon: &Actor, machine: Uuid, scopes: serde_json::Value| {
        request(
            Method::POST,
            "/v1/pairings",
            Some(daemon),
            Some(serde_json::json!({"machine_id": machine.to_string(), "scopes": scopes})),
        )
    };
    // The phone is admin on alpha and view-only on beta.
    for (daemon, machine, scopes) in [
        (&alpha, alpha_id, serde_json::json!(["admin"])),
        (&beta, beta_id, serde_json::json!(["view"])),
    ] {
        let (status, created) = send(&state, pair(daemon, machine, scopes)).await;
        assert_eq!(status, StatusCode::OK, "{created}");
        let code = created["short_code"].as_str().unwrap().to_string();
        let (status, claimed) = send(
            &state,
            request(
                Method::POST,
                "/v1/pairings/claim-code",
                Some(&phone),
                Some(claim_code_body(&code, &phone)),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{claimed}");
    }
    use h4rn_protocol::PairScope::{Admin, View};
    assert!(
        state
            .store
            .link_scopes(alpha_id, phone.device.id)
            .await
            .unwrap()
            .contains(&Admin)
    );
    assert_eq!(
        state
            .store
            .link_scopes(beta_id, phone.device.id)
            .await
            .unwrap(),
        vec![View]
    );

    // Admin on alpha still mints pairings for alpha...
    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings",
            Some(&phone),
            Some(serde_json::json!({"machine_id": alpha_id.to_string(), "scopes": ["admin"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let alpha_pairing = body["pairing"]["id"].as_str().unwrap().to_string();

    // ...but not for beta, where it holds `view` only.
    let (status, body) = send(
        &state,
        request(
            Method::POST,
            "/v1/pairings",
            Some(&phone),
            Some(serde_json::json!({"machine_id": beta_id.to_string(), "scopes": ["admin"]})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "scope_required");

    // Nor may it revoke beta's pending pairing, or beta's daemon.
    let (status, created) = send(&state, pair(&beta, beta_id, serde_json::json!(["view"]))).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let beta_pairing = created["pairing"]["id"].as_str().unwrap().to_string();
    let (status, body) = send(
        &state,
        request(
            Method::DELETE,
            &format!("/v1/pairings/{beta_pairing}"),
            Some(&phone),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = send(
        &state,
        request(
            Method::DELETE,
            &format!("/v1/devices/{}", beta.device.id),
            Some(&phone),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "scope_required");

    // Its admin on alpha does cover alpha's pairing.
    let (status, body) = send(
        &state,
        request(
            Method::DELETE,
            &format!("/v1/pairings/{alpha_pairing}"),
            Some(&phone),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn machine_list_can_be_limited_to_this_devices_links() {
    let state = test_state().await;
    let subject = Uuid::now_v7().to_string();
    let one = actor_with_subject(&state, ConnectDeviceKind::Daemon, Some("one"), &subject).await;
    let two = actor_with_subject(&state, ConnectDeviceKind::Daemon, Some("two"), &subject).await;
    let desktop = actor_with_subject(&state, ConnectDeviceKind::Desktop, None, &subject).await;

    let (status, headers, all) = send_with_headers(
        &state,
        request(Method::GET, "/v1/machines", Some(&one), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert_eq!(all.as_array().unwrap().len(), 2);
    // An unfiltered answer must not claim the linked scope.
    assert!(headers.get("x-h4rn-machine-filter").is_none());

    // Each daemon runs exactly one of the user's machines.
    let (status, headers, linked) = send_with_headers(
        &state,
        request(Method::GET, "/v1/machines?linked=true", Some(&one), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{linked}");
    // The marker is how a client tells a filtered answer from an older
    // relay that ignored the query and returned the whole account.
    assert_eq!(
        headers
            .get("x-h4rn-machine-filter")
            .and_then(|value| value.to_str().ok()),
        Some("linked")
    );
    let linked = linked.as_array().unwrap();
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0]["id"], one.device.machine_id.unwrap().to_string());
    assert_ne!(linked[0]["id"], two.device.machine_id.unwrap().to_string());

    // A desktop device linked to no machine has no addressable machines; the
    // unflagged list still shows the account's machines for the picker.
    let (status, linked) = send(
        &state,
        request(
            Method::GET,
            "/v1/machines?linked=true",
            Some(&desktop),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{linked}");
    assert!(linked.as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Guest links over the real relay
// ---------------------------------------------------------------------------

/// Answers one non-chunked guest request as a daemon would.
fn fake_guest_answer(
    request: h4rn_protocol::GuestHttpRequest,
    token: &str,
    binding: &str,
    page_state: &str,
    link_bound: bool,
) -> h4rn_protocol::GuestHttpResponse {
    let request_id = request.request_id.clone();
    let cookie = request
        .headers
        .iter()
        .find(|(name, _)| name == "cookie")
        .map(|(_, value)| value.clone());
    let bound = cookie.as_deref() == Some(format!("h4rn_guest={binding}").as_str());
    // A relayed daemon scopes its cookie to the public prefix it was told
    // about, so Connect can check the path exactly.
    let prefix = request
        .public_prefix
        .clone()
        .unwrap_or_else(|| format!("/guest/{token}"));
    let set_cookie =
        format!("h4rn_guest={binding}; Path={prefix}; HttpOnly; Secure; SameSite=Strict");
    let page = |state: &str| {
        format!("<html><body data-state=\"{state}\">daemon page</body></html>").into_bytes()
    };
    let (status, headers, body) = if request.path == format!("/guest/{token}") {
        // The daemon's page is asked for its page-load cookie and rendered
        // state; Connect must never return this HTML. A bound link renders
        // "unbound" for a browser that presents no cookie, and a stopped token
        // answers 410 with a plain sentence rather than the page.
        if page_state == "revoked" {
            (
                410,
                vec![("content-type".to_string(), "text/plain".to_string())],
                b"This link was stopped or replaced by the owner.".to_vec(),
            )
        } else if link_bound && !bound {
            (
                200,
                vec![("content-type".to_string(), "text/html".to_string())],
                page("unbound"),
            )
        } else {
            (
                200,
                vec![
                    ("content-type".to_string(), "text/html".to_string()),
                    ("set-cookie".to_string(), set_cookie.clone()),
                ],
                page(page_state),
            )
        }
    } else if request.path.ends_with("/state") {
        if bound {
            (
                200,
                vec![("content-type".to_string(), "application/json".to_string())],
                br#"{"state":"active","events":[],"next_seq":0}"#.to_vec(),
            )
        } else {
            (
                200,
                vec![
                    ("content-type".to_string(), "application/json".to_string()),
                    ("set-cookie".to_string(), set_cookie.clone()),
                    (
                        "content-security-policy".to_string(),
                        "default-src *".to_string(),
                    ),
                ],
                br#"{"state":"unbound","events":[],"next_seq":0}"#.to_vec(),
            )
        }
    } else {
        (
            404,
            vec![("content-type".to_string(), "text/plain".to_string())],
            b"not found".to_vec(),
        )
    };
    h4rn_protocol::GuestHttpResponse {
        request_id,
        status,
        chunked: false,
        headers,
        body,
        set_cookie: Some(set_cookie),
    }
}

#[tokio::test]
#[ignore]
async fn a_relay_guest_link_keeps_its_cookie_and_carries_a_large_upload() {
    let state = test_state().await;
    let daemon = actor(&state, ConnectDeviceKind::Daemon, Some("guest-machine")).await;
    let machine = state
        .store
        .machines(daemon.user.id, &std::collections::HashSet::new())
        .await
        .expect("machines")
        .into_iter()
        .find(|machine| machine.name == "guest-machine")
        .expect("the daemon's machine");
    let machine_id = Uuid::parse_str(&machine.id).expect("machine id");
    let token = "guest_token_for_the_relay_test";
    let binding = "guest_binding_value";

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let connection = Uuid::now_v7();
    state
        .relay
        .register(h4rn_connect::relay::Peer {
            device_id: daemon.device.id,
            connection_id: connection,
            announces_hello: false,
            user_id: daemon.user.id,
            machine_id: Some(machine_id),
            kind: "daemon".to_string(),
            tx,
        })
        .await;
    let link = h4rn_connect::relay::DaemonLink {
        device_id: daemon.device.id,
        connection_id: connection,
    };

    let pages = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let page_state = std::sync::Arc::new(std::sync::Mutex::new("active".to_string()));
    let link_bound = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task_state = state.clone();
    let task_link = link;
    let task_pages = pages.clone();
    let task_token = token.to_string();
    let task_binding = binding.to_string();
    let task_page_state = page_state.clone();
    let task_link_bound = link_bound.clone();
    let fake = tokio::spawn(async move {
        let mut bodies: std::collections::HashMap<String, Vec<u8>> =
            std::collections::HashMap::new();
        while let Some(event) = rx.recv().await {
            let h4rn_protocol::RelayServerEvent::Frame { frame } = event else {
                continue;
            };
            match frame.kind {
                h4rn_protocol::RelayFrameKind::GuestHttp => {
                    let Ok(request) =
                        serde_json::from_slice::<h4rn_protocol::GuestHttpRequest>(&frame.payload)
                    else {
                        continue;
                    };
                    if request.path == format!("/guest/{task_token}") {
                        task_pages.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if request.chunked {
                        bodies.insert(request.request_id.clone(), Vec::new());
                        continue;
                    }
                    let state = task_page_state.lock().expect("page state").clone();
                    let bound = task_link_bound.load(std::sync::atomic::Ordering::Relaxed);
                    let response =
                        fake_guest_answer(request, &task_token, &task_binding, &state, bound);
                    task_state.relay.complete_guest(task_link, response).await;
                }
                h4rn_protocol::RelayFrameKind::GuestHttpChunk => {
                    let Ok(chunk) =
                        serde_json::from_slice::<h4rn_protocol::GuestHttpChunk>(&frame.payload)
                    else {
                        continue;
                    };
                    let Some(body) = bodies.get_mut(&chunk.request_id) else {
                        continue;
                    };
                    body.extend_from_slice(&chunk.body);
                    if chunk.last {
                        let body = bodies.remove(&chunk.request_id).expect("body");
                        task_state
                            .relay
                            .complete_guest(
                                task_link,
                                h4rn_protocol::GuestHttpResponse {
                                    request_id: chunk.request_id,
                                    status: 202,
                                    chunked: false,
                                    headers: vec![(
                                        "content-type".to_string(),
                                        "application/json".to_string(),
                                    )],
                                    body: format!("{{\"received\":{}}}", body.len()).into_bytes(),
                                    set_cookie: None,
                                },
                            )
                            .await;
                    }
                }
                _ => {}
            }
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .expect("serve");
    });

    let client = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .expect("client with a cookie jar");
    let base = format!("http://{addr}");

    // The page is Connect's own, not the daemon's HTML.
    let page = client
        .get(format!("{base}/guest/{machine_id}/{token}"))
        .send()
        .await
        .expect("page");
    assert_eq!(page.status(), StatusCode::OK);
    let csp = page
        .headers()
        .get("content-security-policy")
        .and_then(|value| value.to_str().ok())
        .expect("CSP")
        .to_string();
    assert!(csp.contains("sha256"), "{csp}");
    assert!(!csp.contains("default-src *"), "{csp}");
    let html = page.text().await.expect("html");
    assert!(html.contains("data-state"));
    assert!(
        !html.contains("daemon page"),
        "Connect returned the daemon's HTML"
    );
    assert_eq!(
        pages.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the daemon was asked for its page-load cookie exactly once"
    );

    // A dead link's page opens on its notice, because Connect passes the state
    // the daemon rendered instead of always serving the active page.
    *page_state.lock().expect("page state") = "expired".to_string();
    let dead = client
        .get(format!("{base}/guest/{machine_id}/{token}"))
        .send()
        .await
        .expect("expired page");
    assert_eq!(dead.status(), StatusCode::OK);
    let dead_html = dead.text().await.expect("expired html");
    assert!(
        dead_html.contains(r#"data-state="expired""#),
        "Connect served the active state for an expired link"
    );
    assert!(
        !dead_html.contains("daemon page"),
        "Connect returned the daemon's HTML"
    );
    // The rest of the live flow still works: the page's state only changes
    // what the browser starts on, not the relayed routes.
    *page_state.lock().expect("page state") = "active".to_string();

    // The page-load cookie must reach the browser on the public path and come
    // back on both state reads. Before the cookie-path fix the jar dropped it,
    // so the daemon answered "unbound" every time.
    let first: serde_json::Value = client
        .get(format!("{base}/guest/{machine_id}/{token}/state"))
        .send()
        .await
        .expect("first state")
        .json()
        .await
        .expect("first body");
    assert_eq!(
        first["state"], "active",
        "the page-load cookie did not survive the relay path"
    );
    let second: serde_json::Value = client
        .get(format!("{base}/guest/{machine_id}/{token}/state"))
        .send()
        .await
        .expect("second state")
        .json()
        .await
        .expect("second body");
    assert_eq!(
        second["state"], "active",
        "the browser did not present its binding cookie"
    );

    // The link is bound to this browser now.
    link_bound.store(true, std::sync::atomic::Ordering::Relaxed);

    // A reload forwards the browser's binding cookie to the daemon's page, so
    // the bound browser starts on its own state instead of "unbound".
    let reload = client
        .get(format!("{base}/guest/{machine_id}/{token}"))
        .send()
        .await
        .expect("reload page");
    assert_eq!(reload.status(), StatusCode::OK);
    let reload_html = reload.text().await.expect("reload html");
    assert!(
        reload_html.contains(r#"data-state="active""#),
        "a bound browser reloaded to: {reload_html}"
    );

    // A fresh browser on a bound link is not the browser it was claimed by.
    let fresh = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .expect("fresh browser");
    let fresh_page = fresh
        .get(format!("{base}/guest/{machine_id}/{token}"))
        .send()
        .await
        .expect("fresh page");
    assert_eq!(fresh_page.status(), StatusCode::OK);
    let fresh_html = fresh_page.text().await.expect("fresh html");
    assert!(
        fresh_html.contains(r#"data-state="unbound""#),
        "a fresh browser on a bound link saw: {fresh_html}"
    );

    // A stopped or replaced link answers 410 and opens on its revoked notice.
    *page_state.lock().expect("page state") = "revoked".to_string();
    let stopped = client
        .get(format!("{base}/guest/{machine_id}/{token}"))
        .send()
        .await
        .expect("stopped page");
    assert_eq!(stopped.status(), StatusCode::OK);
    let stopped_html = stopped.text().await.expect("stopped html");
    assert!(
        stopped_html.contains(r#"data-state="revoked""#),
        "a stopped link did not open revoked: {stopped_html}"
    );
    *page_state.lock().expect("page state") = "active".to_string();

    // A full 10 MB upload crosses in bounded frames and arrives whole.
    let big = vec![b'a'; 10 * 1024 * 1024];
    let upload = client
        .post(format!("{base}/guest/{machine_id}/{token}/messages"))
        .header("content-type", "application/json")
        .body(big)
        .send()
        .await
        .expect("upload");
    assert_eq!(upload.status(), StatusCode::ACCEPTED);
    let received: serde_json::Value = upload.json().await.expect("upload body");
    assert_eq!(received["received"], 10 * 1024 * 1024);

    // An exchange over the limit is refused before it reaches the socket, and
    // the daemon's session still answers the next request.
    let oversized = vec![b'a'; h4rn_protocol::GUEST_MAX_EXCHANGE_BYTES + 1024];
    let refused = client
        .post(format!("{base}/guest/{machine_id}/{token}/messages"))
        .header("content-type", "application/json")
        .body(oversized)
        .send()
        .await
        .expect("oversized upload");
    assert_eq!(refused.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let after = client
        .get(format!("{base}/guest/{machine_id}/{token}/state"))
        .send()
        .await
        .expect("state after refusal");
    assert_eq!(after.status(), StatusCode::OK);

    server.abort();
    fake.abort();
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn push_devices_register_rotate_and_leave_with_the_session() {
    // FCM configured but no APNs key: `configured` (any platform) and
    // `platform_configured` (this phone's platform) must be able to disagree.
    let state = test_state_with(|config| {
        config.fcm_service_account = Some(
            serde_json::json!({
                "project_id": "demo",
                "client_email": "relay@demo.iam.gserviceaccount.example",
                "private_key": "test-key-material",
            })
            .to_string(),
        );
    })
    .await;
    let phone = actor(&state, ConnectDeviceKind::Mobile, None).await;
    let other = actor(&state, ConnectDeviceKind::Mobile, None).await;
    let token_one = random_b64(32);
    let token_two = random_b64(32);

    let (status, body) = send(
        &state,
        request(Method::GET, "/v1/push/status", Some(&phone), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["devices"], 0);
    assert_eq!(
        body["registered"], false,
        "an unregistered phone must not claim a registration"
    );
    assert_eq!(
        body["configured"], true,
        "this relay has an FCM account, so Android alerts are on"
    );

    let (status, body) = send(
        &state,
        request(
            Method::PUT,
            "/v1/push/devices",
            Some(&phone),
            Some(serde_json::json!({
                "device_id": phone.device.id.to_string(),
                "token": token_one,
                "package_name": "dev.h4rn.app",
                "app_version": "1.0.1",
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = send(
        &state,
        request(Method::GET, "/v1/push/status", Some(&phone), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["configured"], true);
    assert_eq!(
        body["platform_configured"], true,
        "an Android phone is served by the configured FCM sender"
    );
    assert_eq!(body["devices"], 1);
    assert_eq!(
        body["registered"], true,
        "the phone's own registration must be reported"
    );

    // Rotation replaces the old token instead of adding a row.
    let (status, body) = send(
        &state,
        request(
            Method::PUT,
            "/v1/push/devices",
            Some(&phone),
            Some(serde_json::json!({
                "device_id": phone.device.id.to_string(),
                "token": token_two,
                "package_name": "dev.h4rn.app",
                "app_version": "1.0.2",
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT token, app_version FROM push_devices WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(phone.user.id)
    .fetch_all(state.store.pool())
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, token_two);
    assert_eq!(rows[0].1, "1.0.2");

    // The row records the platform so Connect can choose the right sender.
    // An iOS token has to look like an APNs one, so a stray FCM token is
    // refused instead of stored and retried forever.
    let token_three = apns_device_token();
    let (status, body) = send(
        &state,
        request(
            Method::PUT,
            "/v1/push/devices",
            Some(&phone),
            Some(serde_json::json!({
                "device_id": phone.device.id.to_string(),
                "token": "not-an-apns-token",
                "platform": "ios",
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "push_token_invalid");
    let (status, body) = send(
        &state,
        request(
            Method::PUT,
            "/v1/push/devices",
            Some(&phone),
            Some(serde_json::json!({
                "device_id": phone.device.id.to_string(),
                "token": token_three,
                "platform": "ios",
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let platform: String =
        sqlx::query_scalar("SELECT platform FROM push_devices WHERE user_id = $1")
            .bind(phone.user.id)
            .fetch_one(state.store.pool())
            .await
            .unwrap();
    assert_eq!(platform, "ios");
    let (status, body) = send(
        &state,
        request(Method::GET, "/v1/push/status", Some(&phone), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["configured"], true,
        "the relay still has an FCM account for other phones"
    );
    assert_eq!(
        body["platform_configured"], false,
        "this relay has no APNs key, so an iPhone must not be told alerts will arrive"
    );

    // A platform Connect does not know is refused rather than stored.
    let (status, body) = send(
        &state,
        request(
            Method::PUT,
            "/v1/push/devices",
            Some(&phone),
            Some(serde_json::json!({
                "device_id": phone.device.id.to_string(),
                "token": token_three,
                "platform": "watchos",
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "push_platform_invalid");
    let platform: String =
        sqlx::query_scalar("SELECT platform FROM push_devices WHERE user_id = $1")
            .bind(phone.user.id)
            .fetch_one(state.store.pool())
            .await
            .unwrap();
    assert_eq!(platform, "ios", "a refused registration changed the row");

    // A device can only register itself, and only remove itself.
    let (status, body) = send(
        &state,
        request(
            Method::PUT,
            "/v1/push/devices",
            Some(&other),
            Some(serde_json::json!({
                "device_id": phone.device.id.to_string(),
                "token": random_b64(32),
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "push_device_forbidden");
    let (status, body) = send(
        &state,
        request(
            Method::DELETE,
            &format!("/v1/push/devices/{}", phone.device.id),
            Some(&other),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "push_device_forbidden");

    // The phone removes its own registration.
    let (status, body) = send(
        &state,
        request(
            Method::DELETE,
            &format!("/v1/push/devices/{}", phone.device.id),
            Some(&phone),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM push_devices WHERE user_id = $1")
        .bind(phone.user.id)
        .fetch_one(state.store.pool())
        .await
        .unwrap();
    assert_eq!(remaining, 0, "the phone's own delete left its token behind");

    // Register again so sign-out has a row to clear.
    let (status, body) = send(
        &state,
        request(
            Method::PUT,
            "/v1/push/devices",
            Some(&phone),
            Some(serde_json::json!({
                "device_id": phone.device.id.to_string(),
                "token": token_two,
                "package_name": "dev.h4rn.app",
                "app_version": "1.0.2",
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // Signing out removes the registration as well as the session.
    let (status, _) = send(
        &state,
        request(
            Method::POST,
            "/v1/auth/logout",
            Some(&phone),
            Some(serde_json::json!({"refresh_token": phone.refresh})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM push_devices WHERE user_id = $1")
        .bind(phone.user.id)
        .fetch_one(state.store.pool())
        .await
        .unwrap();
    assert_eq!(remaining, 0, "sign-out left a deliverable token");

    // The delivered baseline is keyed by the transition, not only the state:
    // a reply of one frame is dropped, while a later completion or approval in
    // the same state alerts again.
    let thread = h4rn_protocol::ThreadId::new_uuid_v7();
    assert!(
        state
            .store
            .claim_push_thread_state(phone.user.id, thread.as_str(), "completed", "turn-1")
            .await
            .unwrap()
    );
    assert!(
        !state
            .store
            .claim_push_thread_state(phone.user.id, thread.as_str(), "completed", "turn-1")
            .await
            .unwrap(),
        "a replay of the same frame was claimed twice"
    );
    assert!(
        state
            .store
            .claim_push_thread_state(phone.user.id, thread.as_str(), "completed", "turn-2")
            .await
            .unwrap(),
        "a later completion in the same state never alerted"
    );
    assert!(
        state
            .store
            .claim_push_thread_state(phone.user.id, thread.as_str(), "approval", "approval-1")
            .await
            .unwrap()
    );
    assert!(
        !state
            .store
            .claim_push_thread_state(phone.user.id, thread.as_str(), "approval", "approval-1")
            .await
            .unwrap(),
        "a replay of the same approval was claimed twice"
    );
    assert!(
        state
            .store
            .claim_push_thread_state(phone.user.id, thread.as_str(), "approval", "approval-2")
            .await
            .unwrap(),
        "a later approval in the same state never alerted"
    );

    // Account deletion cascades to every device row.
    send(
        &state,
        request(
            Method::PUT,
            "/v1/push/devices",
            Some(&other),
            Some(serde_json::json!({
                "device_id": other.device.id.to_string(),
                "token": random_b64(32),
            })),
        ),
    )
    .await;
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(other.user.id)
        .execute(state.store.pool())
        .await
        .unwrap();
    let leftover: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM push_devices WHERE user_id = $1")
        .bind(other.user.id)
        .fetch_one(state.store.pool())
        .await
        .unwrap();
    assert_eq!(leftover, 0, "deleting the account left device rows");
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn a_revoked_phone_loses_its_push_rows() {
    let state = test_state().await;
    let phone = actor(&state, ConnectDeviceKind::Mobile, None).await;
    state
        .store
        .register_push_device(
            phone.user.id,
            &phone.device.id.to_string(),
            "revoked-phone-token",
            "android",
            Some("dev.h4rn.app"),
            Some("1.0.1"),
        )
        .await
        .expect("register");
    assert_eq!(
        state.store.push_devices(phone.user.id).await.unwrap().len(),
        1
    );

    // Defence in depth: even a row that survives revocation is not deliverable.
    sqlx::query("UPDATE devices SET revoked_at = now() WHERE id = $1")
        .bind(phone.device.id)
        .execute(state.store.pool())
        .await
        .unwrap();
    assert!(
        state
            .store
            .push_devices(phone.user.id)
            .await
            .unwrap()
            .is_empty(),
        "a revoked device was still returned as a push target"
    );
    sqlx::query("UPDATE devices SET revoked_at = NULL WHERE id = $1")
        .bind(phone.device.id)
        .execute(state.store.pool())
        .await
        .unwrap();

    // Revoking through the API deletes the registration in the same
    // transaction, so the lost phone stops receiving titles at once.
    let (status, body) = send(
        &state,
        request(
            Method::DELETE,
            &format!("/v1/devices/{}", phone.device.id),
            Some(&phone),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM push_devices WHERE user_id = $1")
        .bind(phone.user.id)
        .fetch_one(state.store.pool())
        .await
        .unwrap();
    assert_eq!(rows, 0, "revoking a device left its push row behind");
}

/// A service account generated for one test; it is never written to disk or
/// committed.
/// A 64-character hex APNs device token, generated for each test.
fn apns_device_token() -> String {
    random_bytes(32)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

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

fn alert_activity(
    thread: &h4rn_protocol::ThreadId,
    state: h4rn_protocol::AgentAlertState,
    at: chrono::DateTime<Utc>,
    transition: &str,
) -> h4rn_protocol::AgentActivity {
    h4rn_protocol::AgentActivity {
        thread_id: thread.clone(),
        thread_title: format!("Thread {}", transition),
        project_name: "h4rn".to_string(),
        state,
        at,
        transition: transition.to_string(),
    }
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn alert_service_drops_stale_completions_groups_and_deletes_a_dead_token() {
    use h4rn_protocol::{AgentAlertState, ThreadId};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "test-access-token",
            "expires_in": 3600,
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/demo/messages:send"))
        .and(body_string_contains("kept-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "x"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/demo/messages:send"))
        .and(body_string_contains("gone-token"))
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

    let state = test_state().await;
    let subject = Uuid::now_v7().to_string();
    let kept = actor_with_subject(&state, ConnectDeviceKind::Mobile, None, &subject).await;
    let gone = actor_with_subject(&state, ConnectDeviceKind::Mobile, None, &subject).await;
    assert_eq!(kept.user.id, gone.user.id, "one account, two phones");
    state
        .store
        .register_push_device(
            kept.user.id,
            &kept.device.id.to_string(),
            "kept-token",
            "android",
            None,
            None,
        )
        .await
        .expect("register kept phone");
    state
        .store
        .register_push_device(
            kept.user.id,
            &gone.device.id.to_string(),
            "gone-token",
            "android",
            None,
            None,
        )
        .await
        .expect("register gone phone");

    let config = Config {
        fcm_service_account: Some(service_account(&server, "demo")),
        fcm_token_url: format!("{}/token", server.uri()),
        fcm_url: server.uri(),
        ..Config::default()
    };
    let fcm = Arc::new(FcmClient::new(&config, reqwest::Client::new()));
    let apns = Arc::new(h4rn_connect::apns::ApnsClient::disabled(
        reqwest::Client::new(),
    ));
    let service = Arc::new(AlertService::new(
        state.store.clone(),
        fcm,
        apns,
        Duration::from_millis(100),
        Duration::from_secs(120),
    ));

    // A completion older than the cutoff is recorded but never sent.
    let stale = ThreadId::new_uuid_v7();
    service
        .ingest(
            kept.user.id,
            alert_activity(
                &stale,
                AgentAlertState::Completed,
                Utc::now() - chrono::Duration::minutes(10),
                "stale",
            ),
        )
        .await;
    service.flush(kept.user.id).await;

    // Two completions inside the window become one grouped alert.
    let first = ThreadId::new_uuid_v7();
    let second = ThreadId::new_uuid_v7();
    service
        .ingest(
            kept.user.id,
            alert_activity(&first, AgentAlertState::Completed, Utc::now(), "first"),
        )
        .await;
    service
        .ingest(
            kept.user.id,
            alert_activity(&second, AgentAlertState::Completed, Utc::now(), "second"),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    let sends: Vec<wiremock::Request> = server
        .received_requests()
        .await
        .expect("requests")
        .into_iter()
        .filter(|request| request.url.path().ends_with("messages:send"))
        .collect();
    assert_eq!(
        sends.len(),
        2,
        "one grouped alert per phone, and nothing for the stale completion"
    );
    for request in &sends {
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json");
        assert_eq!(body["message"]["data"]["count"], "2");
        assert_eq!(body["message"]["data"]["state"], "completed");
    }

    // The token FCM reported as UNREGISTERED is removed; the live one stays.
    assert!(
        state
            .store
            .push_device(kept.user.id, &gone.device.id.to_string())
            .await
            .unwrap()
            .is_none(),
        "an unregistered token was kept"
    );
    assert!(
        state
            .store
            .push_device(kept.user.id, &kept.device.id.to_string())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn a_working_update_is_deduped_and_goes_to_android_only() {
    use h4rn_protocol::{AgentAlertState, ThreadId};
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::{EncodePrivateKey, LineEnding};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "test-access-token",
            "expires_in": 3600,
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/demo/messages:send"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "x"})))
        .mount(&server)
        .await;

    let key = SigningKey::random(&mut rand_core::OsRng);
    let pem = key
        .to_pkcs8_pem(LineEnding::LF)
        .expect("encode PKCS#8 PEM")
        .to_string();
    let config = Config {
        fcm_service_account: Some(service_account(&server, "demo")),
        fcm_token_url: format!("{}/token", server.uri()),
        fcm_url: server.uri(),
        apns_key: Some(pem),
        apns_key_id: Some("KEYID12345".to_string()),
        apns_team_id: Some("TEAM123456".to_string()),
        apns_url: server.uri(),
        apns_topic: "dev.h4rn.app".to_string(),
        ..Config::default()
    };
    let state = test_state().await;
    let subject = Uuid::now_v7().to_string();
    let android = actor_with_subject(&state, ConnectDeviceKind::Mobile, None, &subject).await;
    let iphone = actor_with_subject(&state, ConnectDeviceKind::Mobile, None, &subject).await;
    let old_android = actor_with_subject(&state, ConnectDeviceKind::Mobile, None, &subject).await;
    assert_eq!(android.user.id, iphone.user.id, "one account, two phones");
    assert_eq!(
        android.user.id, old_android.user.id,
        "one account, three phones"
    );
    state
        .store
        .register_push_device(
            android.user.id,
            &android.device.id.to_string(),
            "android-fcm-token",
            "android",
            Some("dev.h4rn.app"),
            Some("1.0.2"),
        )
        .await
        .expect("register the Android phone");
    // A build from before Live Updates would show "Working" as an alert.
    state
        .store
        .register_push_device(
            old_android.user.id,
            &old_android.device.id.to_string(),
            "old-android-fcm-token",
            "android",
            Some("dev.h4rn.app"),
            Some("1.0.1"),
        )
        .await
        .expect("register the older Android phone");
    state
        .store
        .register_push_device(
            iphone.user.id,
            &iphone.device.id.to_string(),
            &apns_device_token(),
            "ios",
            None,
            None,
        )
        .await
        .expect("register the iPhone");

    let service = Arc::new(AlertService::new(
        state.store.clone(),
        Arc::new(FcmClient::new(&config, reqwest::Client::new())),
        Arc::new(h4rn_connect::apns::ApnsClient::new(
            &config,
            reqwest::Client::new(),
        )),
        Duration::from_millis(100),
        Duration::from_secs(120),
    ));
    let thread = ThreadId::new_uuid_v7();
    let working = alert_activity(&thread, AgentAlertState::Working, Utc::now(), "turn-1");
    // The same transition delivered twice is one update, not two.
    service.ingest(android.user.id, working.clone()).await;
    service.ingest(android.user.id, working).await;
    // A later turn start is its own transition and updates the phone again.
    service
        .ingest(
            android.user.id,
            alert_activity(&thread, AgentAlertState::Working, Utc::now(), "turn-2"),
        )
        .await;

    let requests = server.received_requests().await.expect("requests");
    let fcm: Vec<&wiremock::Request> = requests
        .iter()
        .filter(|request| request.url.path().ends_with("messages:send"))
        .collect();
    let apns: Vec<&wiremock::Request> = requests
        .iter()
        .filter(|request| request.url.path().starts_with("/3/device/"))
        .collect();
    assert_eq!(
        fcm.len(),
        2,
        "the replayed working frame was delivered twice"
    );
    assert!(
        apns.is_empty(),
        "a working frame became an iPhone alert for a turn that did not end"
    );
    for request in &fcm {
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("FCM payload");
        assert_eq!(
            body["message"]["token"], "android-fcm-token",
            "an app without Live Updates was sent a working frame"
        );
    }

    let body: serde_json::Value = serde_json::from_slice(&fcm[0].body).expect("FCM payload");
    let data = body["message"]["data"].as_object().expect("data object");
    let mut keys: Vec<&str> = data.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "body",
            "count",
            "h4rn_alert",
            "project",
            "state",
            "thread_id",
            "title"
        ],
        "an undocumented field reached the phone"
    );
    assert_eq!(data["state"], "working");
    assert_eq!(data["project"], "h4rn");
    assert_eq!(data["thread_id"], thread.to_string());
    assert_eq!(data["count"], "1");
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn a_queued_turn_start_arrives_after_the_previous_turns_finish() {
    use h4rn_protocol::{AgentAlertState, ThreadId};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "test-access-token",
            "expires_in": 3600,
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/demo/messages:send"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "x"})))
        .mount(&server)
        .await;
    let config = Config {
        fcm_service_account: Some(service_account(&server, "demo")),
        fcm_token_url: format!("{}/token", server.uri()),
        fcm_url: server.uri(),
        ..Config::default()
    };
    let state = test_state().await;
    let phone = actor(&state, ConnectDeviceKind::Mobile, None).await;
    state
        .store
        .register_push_device(
            phone.user.id,
            &phone.device.id.to_string(),
            "android-fcm-token",
            "android",
            Some("dev.h4rn.app"),
            Some("1.0.2"),
        )
        .await
        .expect("register the Android phone");
    let service = Arc::new(AlertService::new(
        state.store.clone(),
        Arc::new(FcmClient::new(&config, reqwest::Client::new())),
        Arc::new(h4rn_connect::apns::ApnsClient::new(
            &config,
            reqwest::Client::new(),
        )),
        Duration::from_millis(200),
        Duration::from_secs(120),
    ));
    let thread = ThreadId::new_uuid_v7();
    // A queued message is promoted: the first turn's finish and the second
    // turn's start reach Connect back to back.
    service
        .ingest(
            phone.user.id,
            alert_activity(
                &thread,
                AgentAlertState::Completed,
                Utc::now(),
                "turn-1-done",
            ),
        )
        .await;
    service
        .ingest(
            phone.user.id,
            alert_activity(
                &thread,
                AgentAlertState::Working,
                Utc::now(),
                "turn-2-start",
            ),
        )
        .await;
    // Let the group window pass so a deferred finish would have gone out.
    tokio::time::sleep(Duration::from_millis(600)).await;

    let requests = server.received_requests().await.expect("requests");
    let states: Vec<String> = requests
        .iter()
        .filter(|request| request.url.path().ends_with("messages:send"))
        .map(|request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("FCM payload");
            assert_eq!(body["message"]["data"]["thread_id"], thread.to_string());
            body["message"]["data"]["state"]
                .as_str()
                .expect("state")
                .to_string()
        })
        .collect();
    assert_eq!(
        states,
        vec!["completed".to_string(), "working".to_string()],
        "the earlier finish replaced the running turn's Live Update"
    );
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn an_alert_goes_to_each_phones_own_transport() {
    use h4rn_protocol::{AgentAlertState, ThreadId};
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::{EncodePrivateKey, LineEnding};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "test-access-token",
            "expires_in": 3600,
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/projects/demo/messages:send"))
        .and(body_string_contains("android-fcm-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"name": "x"})))
        .mount(&server)
        .await;
    let ios_token = apns_device_token();
    Mock::given(method("POST"))
        .and(path(format!("/3/device/{ios_token}")))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let key = SigningKey::random(&mut rand_core::OsRng);
    let pem = key
        .to_pkcs8_pem(LineEnding::LF)
        .expect("encode PKCS#8 PEM")
        .to_string();
    let config = Config {
        fcm_service_account: Some(service_account(&server, "demo")),
        fcm_token_url: format!("{}/token", server.uri()),
        fcm_url: server.uri(),
        apns_key: Some(pem),
        apns_key_id: Some("KEYID12345".to_string()),
        apns_team_id: Some("TEAM123456".to_string()),
        apns_url: server.uri(),
        apns_topic: "dev.h4rn.app".to_string(),
        ..Config::default()
    };
    let state = test_state().await;
    let subject = Uuid::now_v7().to_string();
    let android = actor_with_subject(&state, ConnectDeviceKind::Mobile, None, &subject).await;
    let iphone = actor_with_subject(&state, ConnectDeviceKind::Mobile, None, &subject).await;
    assert_eq!(android.user.id, iphone.user.id, "one account, two phones");
    state
        .store
        .register_push_device(
            android.user.id,
            &android.device.id.to_string(),
            "android-fcm-token",
            "android",
            Some("dev.h4rn.app"),
            None,
        )
        .await
        .expect("register the Android phone");
    state
        .store
        .register_push_device(
            iphone.user.id,
            &iphone.device.id.to_string(),
            &ios_token,
            "ios",
            None,
            None,
        )
        .await
        .expect("register the iPhone");

    let service = Arc::new(AlertService::new(
        state.store.clone(),
        Arc::new(FcmClient::new(&config, reqwest::Client::new())),
        Arc::new(h4rn_connect::apns::ApnsClient::new(
            &config,
            reqwest::Client::new(),
        )),
        Duration::from_millis(100),
        Duration::from_secs(120),
    ));
    let thread = ThreadId::new_uuid_v7();
    service
        .ingest(
            android.user.id,
            alert_activity(&thread, AgentAlertState::Approval, Utc::now(), "one"),
        )
        .await;
    service.flush(android.user.id).await;

    let requests = server.received_requests().await.expect("requests");
    let fcm: Vec<&wiremock::Request> = requests
        .iter()
        .filter(|request| request.url.path().ends_with("messages:send"))
        .collect();
    let apns: Vec<&wiremock::Request> = requests
        .iter()
        .filter(|request| request.url.path().starts_with("/3/device/"))
        .collect();
    assert_eq!(fcm.len(), 1, "the Android row must go to FCM once");
    assert_eq!(apns.len(), 1, "the iOS row must go to APNs once");
    assert_eq!(
        apns[0].url.path(),
        format!("/3/device/{ios_token}"),
        "the APNs request must address the iOS row's token"
    );

    // Each token crossed only its own transport: the APNs token never reached
    // Google, and the FCM token never reached Apple.
    let fcm_body = String::from_utf8_lossy(&fcm[0].body).to_string();
    assert!(fcm_body.contains("android-fcm-token"));
    assert!(
        !fcm_body.contains(&ios_token),
        "the iPhone's APNs token was sent to FCM"
    );
    assert!(
        !String::from_utf8_lossy(&apns[0].body).contains("android-fcm-token"),
        "the Android token was sent to APNs"
    );

    // Both shapes come from the same record.
    let apns_body: serde_json::Value = serde_json::from_slice(&apns[0].body).expect("APNs payload");
    assert_eq!(apns_body["thread_id"], thread.to_string());
    assert_eq!(apns_body["state"], "approval");
    assert_eq!(apns_body["aps"]["alert"]["title"], "Thread one");
    let fcm_body: serde_json::Value = serde_json::from_str(&fcm_body).expect("FCM payload");
    assert_eq!(fcm_body["message"]["data"]["thread_id"], thread.to_string());
    assert_eq!(fcm_body["message"]["data"]["state"], "approval");
}

#[tokio::test]
#[ignore]
async fn a_failed_guest_proxy_leaves_no_waiter_and_no_body() {
    use h4rn_protocol::RelayServerEvent;
    use h4rn_protocol::{GuestHttpChunk, GuestHttpRequest, GuestHttpResponse, RelayFrameKind};

    let token = "guest_token_for_the_failure_test";

    // A relay that cannot deliver the frame: the peer's queue is already gone.
    let state = test_state().await;
    let daemon = actor(&state, ConnectDeviceKind::Daemon, Some("guest-failure-a")).await;
    let machine = state
        .store
        .machines(daemon.user.id, &std::collections::HashSet::new())
        .await
        .expect("machines")
        .into_iter()
        .find(|machine| machine.name == "guest-failure-a")
        .expect("the daemon's machine");
    let machine_id = Uuid::parse_str(&machine.id).expect("machine id");
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    drop(rx);
    let connection = Uuid::now_v7();
    state
        .relay
        .register(h4rn_connect::relay::Peer {
            device_id: daemon.device.id,
            connection_id: connection,
            announces_hello: false,
            user_id: daemon.user.id,
            machine_id: Some(machine_id),
            kind: "daemon".to_string(),
            tx,
        })
        .await;
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/guest/{machine_id}/{token}/state"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("delivery failure response");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        state.relay.pending_guests().await,
        0,
        "a failed delivery kept its waiter"
    );

    // A daemon that starts a chunked answer and then never finishes: the
    // answer timeout must take the waiter and the half-arrived body with it.
    let state = test_state_with(|config| config.guest_answer_timeout_secs = 1).await;
    let daemon = actor(&state, ConnectDeviceKind::Daemon, Some("guest-failure-b")).await;
    let machine = state
        .store
        .machines(daemon.user.id, &std::collections::HashSet::new())
        .await
        .expect("machines")
        .into_iter()
        .find(|machine| machine.name == "guest-failure-b")
        .expect("the daemon's machine");
    let machine_id = Uuid::parse_str(&machine.id).expect("machine id");
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let connection = Uuid::now_v7();
    state
        .relay
        .register(h4rn_connect::relay::Peer {
            device_id: daemon.device.id,
            connection_id: connection,
            announces_hello: false,
            user_id: daemon.user.id,
            machine_id: Some(machine_id),
            kind: "daemon".to_string(),
            tx,
        })
        .await;
    let link = h4rn_connect::relay::DaemonLink {
        device_id: daemon.device.id,
        connection_id: connection,
    };
    let fake = {
        let relay = state.relay.clone();
        let device = link;
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let RelayServerEvent::Frame { frame } = event else {
                    continue;
                };
                if frame.kind != RelayFrameKind::GuestHttp {
                    continue;
                }
                let request: GuestHttpRequest =
                    serde_json::from_slice(&frame.payload).expect("request json");
                // A chunked answer that has started is never completed.
                let started = relay
                    .complete_guest(
                        device,
                        GuestHttpResponse {
                            request_id: request.request_id.clone(),
                            status: 200,
                            chunked: true,
                            headers: vec![(
                                "content-type".to_string(),
                                "application/json".to_string(),
                            )],
                            body: Vec::new(),
                            set_cookie: None,
                        },
                    )
                    .await;
                assert!(started);
                let chunk = relay
                    .complete_guest_chunk(
                        device,
                        GuestHttpChunk {
                            request_id: request.request_id,
                            seq: 0,
                            last: false,
                            body: b"{\"state\":\"active\"".to_vec(),
                        },
                    )
                    .await;
                assert!(chunk);
                break;
            }
        })
    };
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/guest/{machine_id}/{token}/state"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("timeout response");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        state.relay.pending_guests().await,
        0,
        "a timed-out exchange kept its waiter"
    );
    assert_eq!(
        state.relay.pending_guest_bodies().await,
        0,
        "a timed-out exchange kept its half-arrived body"
    );
    fake.await.expect("fake daemon task");
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn a_router_upload_waits_for_the_announced_admission() {
    use h4rn_protocol::{
        GuestHttpChunk, GuestHttpRequest, GuestHttpResponse, RelayFrameKind, RelayServerEvent,
    };

    let state = test_state().await;
    let daemon = actor(&state, ConnectDeviceKind::Daemon, Some("guest-admission")).await;
    let machine_id = daemon.device.machine_id.expect("the daemon owns a machine");
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let connection = Uuid::now_v7();
    state
        .relay
        .register(h4rn_connect::relay::Peer {
            device_id: daemon.device.id,
            connection_id: connection,
            announces_hello: true,
            user_id: daemon.user.id,
            machine_id: Some(machine_id),
            kind: "daemon".to_owned(),
            tx,
        })
        .await;

    let relay = state.relay.clone();
    let device = h4rn_connect::relay::DaemonLink {
        device_id: daemon.device.id,
        connection_id: connection,
    };
    let (report_tx, report_rx) = tokio::sync::oneshot::channel();
    let fake = tokio::spawn(async move {
        // Request one arrives before the hello: Connect must time it out and
        // cancel it. Request two is admitted after a short pause, which proves
        // no chunk crosses before the admission. Request three arrives after a
        // greeted hello but is never admitted: that must time out and cancel
        // too, which a hard-coded legacy branch behind the announced one cannot
        // do.
        let mut first = String::new();
        let mut second = String::new();
        let mut second_admitted = false;
        let mut chunk_before_admission = false;
        let mut cancelled_first = false;
        let mut body = Vec::new();
        let mut answered_second = false;
        let mut third_chunks = 0usize;
        let mut cancelled_third = false;
        while let Some(event) = rx.recv().await {
            let RelayServerEvent::Frame { frame } = event else {
                continue;
            };
            match frame.kind {
                RelayFrameKind::GuestHttp => {
                    let request: GuestHttpRequest =
                        serde_json::from_slice(&frame.payload).expect("guest request");
                    assert!(request.chunked, "a router upload was not chunked");
                    if first.is_empty() {
                        first = request.request_id;
                        continue;
                    }
                    if second.is_empty() {
                        second = request.request_id;
                        // Show that Connect waits for the admission; if it did
                        // not, a chunk would appear in this window.
                        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                        while let Ok(event) = rx.try_recv() {
                            if let RelayServerEvent::Frame { frame } = event
                                && frame.kind == RelayFrameKind::GuestHttpChunk
                            {
                                chunk_before_admission = true;
                            }
                        }
                        assert!(
                            relay.admit_guest(device, &second).await,
                            "the admission failed"
                        );
                        second_admitted = true;
                        continue;
                    }
                    // Request three: greeted, but this daemon never admits it.
                }
                RelayFrameKind::GuestHttpChunk => {
                    let chunk: GuestHttpChunk =
                        serde_json::from_slice(&frame.payload).expect("chunk");
                    if chunk.request_id == first {
                        // A relay that guessed the timeout streamed this body
                        // for an exchange it never admitted. Answer it so the
                        // test fails on the status instead of hanging.
                        cancelled_first = true;
                        relay
                            .complete_guest(
                                device,
                                GuestHttpResponse {
                                    request_id: chunk.request_id,
                                    status: 202,
                                    chunked: false,
                                    headers: Vec::new(),
                                    body: b"{}".to_vec(),
                                    set_cookie: None,
                                },
                            )
                            .await;
                        continue;
                    }
                    if chunk.request_id != second {
                        third_chunks += 1;
                        continue;
                    }
                    if !second_admitted {
                        chunk_before_admission = true;
                    }
                    body.extend_from_slice(&chunk.body);
                    if chunk.last && !answered_second {
                        answered_second = true;
                        relay
                            .complete_guest(
                                device,
                                GuestHttpResponse {
                                    request_id: chunk.request_id,
                                    status: 202,
                                    chunked: false,
                                    headers: vec![(
                                        "content-type".to_owned(),
                                        "application/json".to_owned(),
                                    )],
                                    body: format!("{{\"received\":{}}}", body.len()).into_bytes(),
                                    set_cookie: None,
                                },
                            )
                            .await;
                    }
                }
                RelayFrameKind::GuestHttpCancel => {
                    if let Ok(cancel) =
                        serde_json::from_slice::<h4rn_protocol::GuestHttpAdmission>(&frame.payload)
                    {
                        if cancel.request_id == first {
                            cancelled_first = true;
                        }
                        if !second.is_empty() && cancel.request_id != second {
                            cancelled_third = true;
                        }
                    }
                }
                _ => {}
            }
            if cancelled_third {
                // Give a wrong implementation's chunks a moment to arrive, so
                // "zero chunks" is a real observation, not a race.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                while let Ok(event) = rx.try_recv() {
                    if let RelayServerEvent::Frame { frame } = event
                        && frame.kind == RelayFrameKind::GuestHttpChunk
                    {
                        let chunk: GuestHttpChunk =
                            serde_json::from_slice(&frame.payload).expect("chunk");
                        if !second.is_empty() && chunk.request_id != second {
                            third_chunks += 1;
                        }
                    }
                }
                let _ = report_tx.send((
                    chunk_before_admission,
                    cancelled_first,
                    body.len(),
                    third_chunks,
                    cancelled_third,
                ));
                return;
            }
        }
    });

    // A real router request, so `guest_proxy_exchange` negotiates the hello
    // itself instead of a test passing its result in.
    let post = |body: Vec<u8>| {
        Request::builder()
            .method(Method::POST)
            .uri(format!(
                "/guest/{machine_id}/guest_token_for_router_test/messages"
            ))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };

    // A connection that promised a hello but has not sent one is unknown:
    // the handler fails closed and sends no body, rather than treating the
    // silence as an older daemon.
    let (status, body) = send(&state, post(vec![b'a'; 64 * 1024])).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "guest_negotiating");

    // The hello arrives after the grace. The next upload is admitted and
    // streams to the daemon; the earlier request left nothing to stream to.
    state
        .relay
        .announce(
            daemon.device.id,
            connection,
            h4rn_protocol::RelayHello {
                features: vec![h4rn_protocol::RELAY_FEATURE_GUEST_ADMISSION.to_owned()],
            },
        )
        .await;
    let upload = vec![b'a'; 128 * 1024];
    let (status, body) = send(&state, post(upload.clone())).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["received"], upload.len());

    // A greeted connection that stays unadmitted past the deadline is an
    // error too. A branch that hard-coded the announced path to the legacy
    // one would stream this body and answer 202 (or hang on the daemon).
    let (status, body) = send(&state, post(vec![b'b'; 64 * 1024])).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "guest_admission_timeout");

    let (early, cancelled, received, third_chunks, cancelled_third) =
        report_rx.await.expect("the daemon never answered");
    assert!(!early, "a chunk crossed before the admission");
    assert!(cancelled, "Connect never cancelled the timed-out admission");
    assert_eq!(received, upload.len());
    assert_eq!(
        third_chunks, 0,
        "a greeted, unadmitted upload streamed its body"
    );
    assert!(
        cancelled_third,
        "Connect never cancelled the greeted upload it timed out"
    );
    fake.abort();
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn a_cancelled_page_fetch_leaves_no_waiter() {
    use h4rn_protocol::{RelayFrameKind, RelayServerEvent};

    let state = test_state().await;
    let daemon = actor(&state, ConnectDeviceKind::Daemon, Some("guest-page-cancel")).await;
    let machine = state
        .store
        .machines(daemon.user.id, &std::collections::HashSet::new())
        .await
        .expect("machines")
        .into_iter()
        .find(|machine| machine.name == "guest-page-cancel")
        .expect("the daemon's machine");
    let machine_id = Uuid::parse_str(&machine.id).expect("machine id");
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    state
        .relay
        .register(h4rn_connect::relay::Peer {
            device_id: daemon.device.id,
            connection_id: Uuid::now_v7(),
            announces_hello: false,
            user_id: daemon.user.id,
            machine_id: Some(machine_id),
            kind: "daemon".to_owned(),
            tx,
        })
        .await;

    let app = router(state.clone());
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("/guest/{machine_id}/guest_page_cancel_token"))
        .body(Body::empty())
        .unwrap();
    let handler = tokio::spawn(async move {
        let _ = app.oneshot(request).await;
    });

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("the daemon never received the page request")
        .expect("relay queue closed");
    let RelayServerEvent::Frame { frame } = event else {
        panic!("unexpected relay event");
    };
    assert_eq!(frame.kind, RelayFrameKind::GuestHttp);
    assert_eq!(state.relay.pending_guests().await, 1);

    // A browser that closes the tab during the fetch drops the handler future;
    // the waiter must go with it instead of waiting for a 10-second timeout
    // that nothing will run.
    handler.abort();
    let _ = handler.await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while state.relay.pending_guests().await > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a cancelled page fetch kept its waiter");
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn a_cancelled_guest_request_leaves_no_waiter_or_body() {
    use h4rn_protocol::{
        GuestHttpChunk, GuestHttpRequest, GuestHttpResponse, RelayFrameKind, RelayServerEvent,
    };

    let state = test_state().await;
    let daemon = actor(&state, ConnectDeviceKind::Daemon, Some("guest-cancel")).await;
    let machine_id = daemon.device.machine_id.expect("the daemon owns a machine");
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let connection = Uuid::now_v7();
    state
        .relay
        .register(h4rn_connect::relay::Peer {
            device_id: daemon.device.id,
            connection_id: connection,
            announces_hello: false,
            user_id: daemon.user.id,
            machine_id: Some(machine_id),
            kind: "daemon".to_owned(),
            tx,
        })
        .await;
    let link = h4rn_connect::relay::DaemonLink {
        device_id: daemon.device.id,
        connection_id: connection,
    };

    // Start the real handler future and cancel it, as a browser disconnect
    // does mid-answer.
    let app = router(state.clone());
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("/guest/{machine_id}/guest_cancel_token/state"))
        .body(Body::empty())
        .unwrap();
    let handler = tokio::spawn(async move {
        let _ = app.oneshot(request).await;
    });

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("the daemon never received the exchange")
        .expect("relay queue closed");
    let RelayServerEvent::Frame { frame } = event else {
        panic!("unexpected relay event");
    };
    assert_eq!(frame.kind, RelayFrameKind::GuestHttp);
    let request: GuestHttpRequest = serde_json::from_slice(&frame.payload).expect("guest request");
    assert_eq!(state.relay.pending_guests().await, 1);

    // A chunked answer that has started but not finished is the partial body
    // that used to outlive its browser.
    assert!(
        state
            .relay
            .complete_guest(
                link,
                GuestHttpResponse {
                    request_id: request.request_id.clone(),
                    status: 200,
                    chunked: true,
                    headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                    body: Vec::new(),
                    set_cookie: None,
                },
            )
            .await
    );
    assert!(
        state
            .relay
            .complete_guest_chunk(
                link,
                GuestHttpChunk {
                    request_id: request.request_id,
                    seq: 0,
                    last: false,
                    body: b"{\"state\":".to_vec(),
                },
            )
            .await
    );
    assert_eq!(state.relay.pending_guest_bodies().await, 1);

    handler.abort();
    let _ = handler.await;

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if state.relay.pending_guests().await == 0
                && state.relay.pending_guest_bodies().await == 0
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a cancelled request kept its waiter or partial body");
}
