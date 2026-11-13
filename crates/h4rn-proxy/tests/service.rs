//! Daemon-facing service behaviour: overrides, routing and redaction.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use h4rn_harness::SessionSpec;
use h4rn_protocol::{HarnessKind, ProxyProfileDraft, ProxyProfileId, RuntimeMode};
use h4rn_proxy::{
    AppliedOverride, DEFAULT_CAPABILITY_TTL, MemorySecretStore, ProxyService, harness_kinds,
};

const API_KEY: &str = "sk-client-secret";
const MANAGEMENT_KEY: &str = "mgmt-client-secret";
const MODEL: &str = "deepseek-v4.1-flash";

fn service() -> (tempfile::TempDir, ProxyService, ProxyProfileId) {
    let dir = tempfile::tempdir().expect("tempdir");
    let service = ProxyService::open(
        dir.path(),
        Arc::new(MemorySecretStore::new()),
        DEFAULT_CAPABILITY_TTL,
    )
    .expect("service");
    let profile = service
        .add_profile(ProxyProfileDraft {
            name: "local".into(),
            base_url: "https://proxy.example/v1".into(),
            management_key: Some(MANAGEMENT_KEY.into()),
            api_key: Some(API_KEY.into()),
            default: true,
        })
        .expect("profile");
    (dir, service, profile.id)
}

fn enable_all(service: &ProxyService, id: &ProxyProfileId) {
    for kind in harness_kinds() {
        service
            .set_harness_override(id, kind, true, None)
            .expect("toggle");
    }
}

fn spec_for(applied: &AppliedOverride) -> SessionSpec {
    SessionSpec {
        workspace: PathBuf::from("/tmp"),
        model: applied.model.clone(),
        runtime_mode: RuntimeMode::ApprovalRequired,
        resume_id: None,
        mcp_endpoint: None,
        env_overrides: applied.env.clone(),
        attachments: Vec::new(),
    }
}

#[test]
fn claude_recipe_matches_the_golden_environment() {
    let (_dir, service, id) = service();
    enable_all(&service, &id);
    let applied = service
        .override_for(HarnessKind::ClaudeCode, MODEL)
        .expect("override");

    let expected: BTreeMap<String, String> = [
        ("ANTHROPIC_BASE_URL", "https://proxy.example"),
        ("ANTHROPIC_AUTH_TOKEN", API_KEY),
        ("ANTHROPIC_MODEL", MODEL),
        ("ANTHROPIC_SMALL_FAST_MODEL", MODEL),
        ("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    assert_eq!(applied.env, expected);
    assert!(applied.verified);
}

#[test]
fn codex_recipe_matches_the_golden_environment() {
    let (_dir, service, id) = service();
    enable_all(&service, &id);
    let applied = service
        .override_for(HarnessKind::Codex, MODEL)
        .expect("override");

    let expected: BTreeMap<String, String> = [
        ("H4RN_CODEX_MODEL_PROVIDER", "h4rn-proxy_0"),
        ("H4RN_CODEX_BASE_URL", "https://proxy.example/v1"),
        ("H4RN_CODEX_WIRE_API", "responses"),
        ("H4RN_CODEX_API_KEY_ENV", "CLIPROXY_API_KEY"),
        ("CLIPROXY_API_KEY", API_KEY),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect();
    assert_eq!(applied.env, expected);
}

#[test]
fn opencode_recipe_generates_a_provider_config() {
    let (_dir, service, id) = service();
    enable_all(&service, &id);
    let applied = service
        .override_for(HarnessKind::Opencode, MODEL)
        .expect("override");
    assert_eq!(applied.model, format!("h4rn-proxy_0/{MODEL}"));
    assert_eq!(
        applied
            .env
            .get("H4RN_OPENCODE_BASE_URL")
            .map(String::as_str),
        Some("https://proxy.example/v1")
    );

    let config: serde_json::Value =
        serde_json::from_str(&h4rn_harness::config_content_for(&spec_for(&applied))).expect("json");
    let provider = &config["providers"]["h4rn-proxy_0"];
    assert_eq!(provider["settings"]["baseURL"], "https://proxy.example/v1");
    assert_eq!(provider["settings"]["apiKey"], API_KEY);
    assert_eq!(provider["package"], "@ai-sdk/openai-compatible");
    assert!(provider["models"].get(MODEL).is_some());
}

#[test]
fn antigravity_recipe_is_present_but_unverified() {
    let (_dir, service, id) = service();
    enable_all(&service, &id);
    let applied = service
        .override_for(HarnessKind::Antigravity, MODEL)
        .expect("override");
    assert_eq!(
        applied
            .env
            .get("GOOGLE_GEMINI_BASE_URL")
            .map(String::as_str),
        Some("https://proxy.example")
    );
    assert_eq!(
        applied.env.get("GEMINI_API_KEY").map(String::as_str),
        Some(API_KEY)
    );
    assert_eq!(
        applied.env.get("GEMINI_MODEL").map(String::as_str),
        Some(MODEL)
    );
    assert!(!applied.verified);
}

#[test]
fn a_profile_without_the_toggle_is_not_applied() {
    let (_dir, service, id) = service();
    assert!(
        service
            .override_for(HarnessKind::ClaudeCode, MODEL)
            .is_none()
    );
    service
        .set_harness_override(&id, HarnessKind::ClaudeCode, true, None)
        .expect("toggle");
    assert!(
        service
            .override_for(HarnessKind::ClaudeCode, MODEL)
            .is_some()
    );
    service
        .set_harness_override(&id, HarnessKind::ClaudeCode, false, None)
        .expect("untoggle");
    assert!(
        service
            .override_for(HarnessKind::ClaudeCode, MODEL)
            .is_none()
    );
}

#[test]
fn the_toggle_default_model_wins_over_an_unset_thread_model() {
    let (_dir, service, id) = service();
    service
        .set_harness_override(
            &id,
            HarnessKind::ClaudeCode,
            true,
            Some("claude-sonnet-5".into()),
        )
        .expect("toggle");
    let applied = service
        .override_for(HarnessKind::ClaudeCode, "default")
        .expect("override");
    assert_eq!(applied.model, "claude-sonnet-5");
    assert_eq!(
        applied.env.get("ANTHROPIC_MODEL").map(String::as_str),
        Some("claude-sonnet-5")
    );
}

#[test]
fn profiles_and_overrides_never_render_a_secret() {
    let (dir, service, id) = service();
    enable_all(&service, &id);

    let profile = service.profile(&id).expect("profile");
    let serialized = serde_json::to_string(&profile).expect("serialize");
    assert!(!serialized.contains(API_KEY));
    assert!(!serialized.contains(MANAGEMENT_KEY));
    assert!(profile.has_api_key);
    assert!(profile.has_management_key);

    let applied = service
        .override_for(HarnessKind::ClaudeCode, MODEL)
        .expect("override");
    let debug = format!("{applied:?}");
    assert!(!debug.contains(API_KEY));
    assert!(!debug.contains(MANAGEMENT_KEY));
    assert!(debug.contains("ANTHROPIC_AUTH_TOKEN"));

    // The metadata file on disk is also secret-free.
    let metadata =
        std::fs::read_to_string(dir.path().join("proxy-profiles.json")).expect("metadata");
    assert!(!metadata.contains(API_KEY));
    assert!(!metadata.contains(MANAGEMENT_KEY));
}

#[test]
fn the_default_profile_is_first_in_the_list() {
    let (_dir, service, first) = service();
    let second = service
        .add_profile(ProxyProfileDraft {
            name: "second".into(),
            base_url: "https://second.example".into(),
            management_key: None,
            api_key: None,
            default: true,
        })
        .expect("second");
    let profiles = service.profiles();
    assert_eq!(profiles[0].id, second.id);
    assert!(profiles[0].default);
    assert!(profiles.iter().any(|profile| profile.id == first));
}

#[test]
fn caches_are_invalidated_by_profile_changes() {
    let (_dir, service, id) = service();
    // No catalogue has been read yet.
    assert!(service.cached_models(&id).is_none());
    assert!(service.model_ids(&id).is_empty());
    service.invalidate(&id);
    assert!(
        service
            .profile(&id)
            .expect("profile")
            .capabilities
            .is_none()
    );
}
