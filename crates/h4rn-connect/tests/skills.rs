//! Postgres-backed skill sync tests.
//!
//! Two independent daemon registries share one real h4rn-connect account. The
//! test drives the same push/pull code the daemon uses, through the real axum
//! router and real Postgres schema, and proves that a version created on one
//! "machine" reaches the other and that concurrent edits surface a conflict.
//!
//! Ignored by default. Run it with `./scripts/test-connect.sh`, which starts
//! Postgres and sets `H4RN_TEST_DATABASE_URL` (ADR-029).

use std::path::Path;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use chrono::Utc;
use ed25519_dalek::{Signer as _, SigningKey};
use h4rn_protocol::ConnectDeviceKind;
use h4rn_protocol::skills::{
    SkillManifest, SkillSyncPull, SkillSyncPullResponse, SkillSyncPush, SkillSyncPushResponse,
    SkillVersionVector,
};
use h4rn_skills::model::SKILL_FILE;
use h4rn_skills::store::Registry;
use h4rn_skills::{SkillBundle, SkillSyncApi};
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
    build_state(config).await.expect("build relay state")
}

struct Actor {
    seed: [u8; 32],
    access: String,
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

async fn actor(state: &AppState, subject: &str) -> Actor {
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
            Some("Skill Tester"),
            None,
            serde_json::json!({"sub": subject}),
            h4rn_connect::store::NewDevice {
                kind: ConnectDeviceKind::Daemon,
                name: "skill daemon".to_string(),
                proof_public_key: proof,
                noise_static_public_key: Some(noise),
                platform: Some("linux".to_string()),
                machine_name: Some("skill-host".to_string()),
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
        seed,
        access: issued.tokens.access_token,
    }
}

fn request(
    method: Method,
    path: &str,
    actor: &Actor,
    body: Option<serde_json::Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method.clone()).uri(path);
    for (name, value) in actor.headers(method, path) {
        builder = builder.header(name, value);
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

/// A daemon-side sync API backed by the real connect router.
struct TestApi {
    state: AppState,
    actor: Actor,
}

impl TestApi {
    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl serde::Serialize,
    ) -> T {
        let value = serde_json::to_value(body).expect("serialise request");
        let (status, body) = send(
            &self.state,
            request(Method::POST, path, &self.actor, Some(value)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "unexpected connect body: {body}");
        serde_json::from_value(body).expect("decode connect response")
    }
}

#[async_trait::async_trait]
impl SkillSyncApi for TestApi {
    async fn push(
        &self,
        request: SkillSyncPush,
    ) -> Result<SkillSyncPushResponse, h4rn_skills::SkillError> {
        Ok(self.post("/v1/skills/sync/push", &request).await)
    }

    async fn pull(
        &self,
        request: SkillSyncPull,
    ) -> Result<SkillSyncPullResponse, h4rn_skills::SkillError> {
        Ok(self.post("/v1/skills/sync/pull", &request).await)
    }
}

fn make_skill(dir: &Path, name: &str, note: &str) -> SkillBundle {
    let root = dir.join(format!("{name}-{note}"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join(SKILL_FILE),
        format!("---\nname: {name}\ndescription: Demo {note}\n---\n\n{note}\n"),
    )
    .unwrap();
    std::fs::write(root.join("reference.md"), note).unwrap();
    h4rn_skills::read_bundle(&root).unwrap()
}

async fn registry() -> (tempfile::TempDir, Registry) {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::open(dir.path()).await.unwrap();
    (dir, registry)
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn two_registries_sync_a_skill_and_surface_a_concurrent_edit() {
    let state = test_state().await;
    let subject = format!("skills-{}", Uuid::now_v7());
    let api_a = TestApi {
        state: state.clone(),
        actor: actor(&state, &subject).await,
    };
    let api_b = TestApi {
        state: state.clone(),
        actor: actor(&state, &subject).await,
    };

    let (_dir_a, registry_a) = registry().await;
    let (_dir_b, registry_b) = registry().await;
    let machine_a = registry_a.machine_id().await.unwrap();
    let machine_b = registry_b.machine_id().await.unwrap();

    let source = tempfile::tempdir().unwrap();
    let one = make_skill(source.path(), "demo", "one");
    let record = registry_a
        .insert(
            &one,
            &h4rn_protocol::SkillSource::LocalFolder {
                path: "/skills/demo".into(),
            },
            Utc::now(),
        )
        .await
        .unwrap();

    // Machine A publishes its version.
    let first = h4rn_skills::sync::sync_now(&registry_a, &api_a, &machine_a, None)
        .await
        .unwrap();
    assert_eq!(first.status.pushed, 1);
    let cursor_a = first.status.etag.clone();

    // Machine B starts empty, pulls A's version and stores the same hash.
    let second = h4rn_skills::sync::sync_now(&registry_b, &api_b, &machine_b, None)
        .await
        .unwrap();
    assert_eq!(second.status.pulled, 1);
    let remote = registry_b.get(&record.id).await.unwrap();
    assert_eq!(remote.version, one.version());

    // B edits the skill; its revision makes the outgoing vector concurrent
    // with A's, so last-writer-wins keeps B's newer timestamp.
    let two = make_skill(source.path(), "demo", "two");
    registry_b
        .update(
            &record.id,
            &two,
            &h4rn_protocol::SkillSource::LocalFolder {
                path: "/skills/demo".into(),
            },
            Utc::now() + chrono::Duration::seconds(1),
        )
        .await
        .unwrap();
    let b_push =
        h4rn_skills::sync::sync_now(&registry_b, &api_b, &machine_b, second.status.etag.clone())
            .await
            .unwrap();
    assert_eq!(b_push.status.pushed, 1);

    // A still has the old version. Its push is stale and is rejected, then the
    // pull applies B's newer version and records the conflict.
    let a_sync = h4rn_skills::sync::sync_now(&registry_a, &api_a, &machine_a, cursor_a)
        .await
        .unwrap();
    assert_eq!(a_sync.status.pulled, 1);
    assert!(a_sync.status.conflicts >= 1);
    assert_eq!(
        registry_a.get(&record.id).await.unwrap().version,
        two.version()
    );

    // A removal is a tombstone, not a missing row: B must see it and drop the
    // skill instead of resurrecting it on the next push.
    registry_a
        .remove(&record.id, Utc::now() + chrono::Duration::seconds(2))
        .await
        .unwrap();
    let a_delete =
        h4rn_skills::sync::sync_now(&registry_a, &api_a, &machine_a, a_sync.status.etag.clone())
            .await
            .unwrap();
    assert_eq!(a_delete.status.pushed, 1);

    let b_delete =
        h4rn_skills::sync::sync_now(&registry_b, &api_b, &machine_b, b_push.status.etag.clone())
            .await
            .unwrap();
    assert!(b_delete.status.pulled >= 1);
    assert!(registry_b.get(&record.id).await.is_err());
}

#[tokio::test]
#[ignore = "requires H4RN_TEST_DATABASE_URL (use ./scripts/test-connect.sh)"]
async fn a_bundle_that_does_not_match_its_version_is_rejected() {
    let state = test_state().await;
    let api = TestApi {
        state: state.clone(),
        actor: actor(&state, &format!("skills-bad-{}", Uuid::now_v7())).await,
    };
    let source = tempfile::tempdir().unwrap();
    let bundle = make_skill(source.path(), "demo", "one");
    let mut vector = SkillVersionVector::default();
    vector.clocks.insert("machine-a".into(), 1);
    let request = SkillSyncPush {
        entries: vec![h4rn_protocol::SkillSyncEntry {
            skill_id: "sk_bad".into(),
            name: "demo".into(),
            description: "Demo".into(),
            version: "0".repeat(64),
            manifest: SkillManifest {
                name: "demo".into(),
                description: "Demo".into(),
                total_bytes: bundle.manifest.total_bytes,
                files: bundle.manifest.files.clone(),
            },
            files: bundle.wire_files(),
            vector,
            updated_at: Utc::now(),
            deleted: false,
            expected_etag: None,
        }],
    };
    let value = serde_json::to_value(&request).unwrap();
    let (status, body) = send(
        &state,
        request_with(Method::POST, "/v1/skills/sync/push", &api.actor, value),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "skill_version_mismatch");
}

fn request_with(
    method: Method,
    path: &str,
    actor: &Actor,
    body: serde_json::Value,
) -> Request<Body> {
    request(method, path, actor, Some(body))
}
