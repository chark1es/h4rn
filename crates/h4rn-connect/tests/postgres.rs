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

    let (status, all) = send(
        &state,
        request(Method::GET, "/v1/machines", Some(&one), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{all}");
    assert_eq!(all.as_array().unwrap().len(), 2);

    // Each daemon runs exactly one of the user's machines.
    let (status, linked) = send(
        &state,
        request(Method::GET, "/v1/machines?linked=true", Some(&one), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{linked}");
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
