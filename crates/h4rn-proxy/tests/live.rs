//! A live check against a real CLIProxyAPI (ADR-015).
//!
//! Ignored by default. Run it when a proxy is reachable:
//!
//! ```text
//! H4RN_LIVE_CPA_BASE_URL=http://127.0.0.1:8317 \
//! H4RN_LIVE_CPA_API_KEY=... \
//! H4RN_LIVE_CPA_MANAGEMENT_KEY=... \
//!   cargo test -p h4rn-proxy --test live -- --ignored --nocapture
//! ```
//!
//! The test never prints a key. It detects capabilities, lists models through
//! all three shapes, then runs one real Claude Code turn through the proxy and
//! asserts the exact reply.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use h4rn_harness::SessionSpec;
use h4rn_protocol::{HarnessKind, ProxyProfileDraft, RuntimeMode};
use h4rn_proxy::{DEFAULT_CAPABILITY_TTL, MemorySecretStore, ProxyService, anthropic_base};

const DEFAULT_MODEL: &str = "deepseek-v4.1-flash";

#[tokio::test]
#[ignore = "requires a live CLIProxyAPI and a logged-in Claude Code CLI"]
async fn live_cliproxyapi_detects_lists_and_runs_claude_code() {
    let Ok(base_url) = std::env::var("H4RN_LIVE_CPA_BASE_URL") else {
        eprintln!("skipping: H4RN_LIVE_CPA_BASE_URL is not set");
        return;
    };
    let Ok(api_key) = std::env::var("H4RN_LIVE_CPA_API_KEY") else {
        eprintln!("skipping: H4RN_LIVE_CPA_API_KEY is not set");
        return;
    };
    let management_key = std::env::var("H4RN_LIVE_CPA_MANAGEMENT_KEY").ok();
    let requested_model =
        std::env::var("H4RN_LIVE_CPA_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());
    let claude_bin = std::env::var("H4RN_LIVE_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());

    let dir = tempfile::tempdir().expect("tempdir");
    let service = ProxyService::open(
        dir.path(),
        Arc::new(MemorySecretStore::new()),
        DEFAULT_CAPABILITY_TTL,
    )
    .expect("service");
    let profile = service
        .add_profile(ProxyProfileDraft {
            name: "live".into(),
            base_url: base_url.clone(),
            management_key,
            api_key: Some(api_key),
            default: true,
        })
        .expect("profile");
    let id = profile.id.clone();

    let report = service.test(&id).await.expect("probe");
    assert!(
        report.capabilities.reachable,
        "the proxy is not reachable: {report:?}"
    );
    eprintln!(
        "capabilities: management={:?} openai={} claude={} gemini={}",
        report.capabilities.management,
        report.capabilities.openai_models,
        report.capabilities.claude_models,
        report.capabilities.gemini_models
    );

    let catalog = service.models(&id).await.expect("models");
    assert!(
        !catalog.models.is_empty(),
        "the proxy returned no models: {catalog:?}"
    );
    let model = if catalog
        .models
        .iter()
        .any(|entry| entry.id == requested_model)
    {
        requested_model
    } else {
        eprintln!("warning: {requested_model} is not in the catalogue; using the first model");
        catalog.models[0].id.clone()
    };
    eprintln!(
        "models: {} (aliases merged: {})",
        catalog.models.len(),
        catalog.aliases_merged
    );

    service
        .set_harness_override(&id, HarnessKind::ClaudeCode, true, Some(model.clone()))
        .expect("toggle");
    let applied = service
        .override_for(HarnessKind::ClaudeCode, "default")
        .expect("override");
    assert_eq!(applied.model, model);
    let expected_base = anthropic_base(&base_url);
    assert_eq!(
        applied.env.get("ANTHROPIC_BASE_URL").map(String::as_str),
        Some(expected_base.as_str()),
        "Claude Code must receive the root URL without /v1"
    );

    let spec = SessionSpec {
        workspace: PathBuf::from("/tmp"),
        model: applied.model.clone(),
        runtime_mode: RuntimeMode::ApprovalRequired,
        reasoning_effort: None,
        fast: false,
        resume_id: None,
        mcp_endpoint: None,
        env_overrides: applied.env.clone(),
        attachments: Vec::new(),
        agent_id: None,
        rewind: None,
    };
    // The Claude adapter scrubs inherited Anthropic variables and applies the
    // same map the daemon would; using the recipe directly keeps this test
    // independent of adapter process plumbing.
    let mut command = tokio::process::Command::new(&claude_bin);
    command
        .arg("-p")
        .arg("Reply with exactly: pong")
        .envs(&spec.env_overrides)
        .current_dir(dir.path());
    let output = tokio::time::timeout(Duration::from_secs(240), command.output())
        .await
        .expect("Claude Code timed out")
        .expect("Claude Code failed to start");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Claude Code exited with {}:\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
    assert!(
        stdout.trim().eq_ignore_ascii_case("pong"),
        "unexpected reply: {stdout:?} (stderr: {stderr:?})"
    );
    eprintln!("Claude Code replied: {}", stdout.trim());
}
