//! Per-harness proxy recipes.
//!
//! The asymmetry that causes most misconfigurations is encoded here: Claude's
//! `ANTHROPIC_BASE_URL` **omits** `/v1` because Claude Code appends
//! `/v1/messages`, while Codex and OpenCode's OpenAI-compatible providers
//! **include** `/v1` because they append `/responses` or `/chat/completions`.
//! Gemini-shaped clients expect the listener root and append `/v1beta`.
//!
//! Everything is returned as environment overrides; nothing secret goes into
//! argv. The daemon applies the map to [`h4rn_harness::SessionSpec`].

use std::collections::BTreeMap;

use h4rn_harness::{
    ENV_ANTHROPIC_AUTH_TOKEN, ENV_ANTHROPIC_BASE_URL, ENV_ANTHROPIC_MODEL,
    ENV_ANTHROPIC_SMALL_FAST_MODEL, ENV_API_KEY_ENV, ENV_BASE_URL, ENV_GATEWAY_MODEL_DISCOVERY,
    ENV_GEMINI_API_KEY, ENV_GEMINI_MODEL, ENV_GOOGLE_GEMINI_BASE_URL, ENV_MODEL_PROVIDER,
    ENV_OPENCODE_API_KEY, ENV_OPENCODE_BASE_URL, ENV_OPENCODE_MODELS, ENV_OPENCODE_PACKAGE,
    ENV_OPENCODE_PROVIDER, ENV_WIRE_API,
};
use h4rn_protocol::HarnessKind;

/// The environment variable that carries the Codex provider key.
pub const CODEX_KEY_ENV: &str = "CLIPROXY_API_KEY";

/// Env/config overrides for one harness session.
#[derive(Clone, PartialEq, Eq)]
pub struct HarnessOverride {
    /// Environment variables applied to the child process.
    pub env: BTreeMap<String, String>,
    /// Model id the adapter should use.
    pub model: String,
    /// Whether the recipe is verified by a test or a live run.
    pub verified: bool,
    /// A short note for the UI.
    pub note: Option<String>,
}

impl std::fmt::Debug for HarnessOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Values may be keys; only names are safe to log.
        f.debug_struct("HarnessOverride")
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("model", &self.model)
            .field("verified", &self.verified)
            .field("note", &self.note)
            .finish()
    }
}

/// The proxy values a recipe needs.
#[derive(Clone)]
pub struct ProxyTarget<'a> {
    /// Profile id, used to name the generated provider.
    pub profile_id: &'a str,
    /// Stored root URL.
    pub base_url: &'a str,
    /// Client API key.
    pub api_key: Option<&'a str>,
    /// Model requested by the thread.
    pub model: &'a str,
    /// Model the profile should use when the thread did not pick one.
    pub default_model: Option<&'a str>,
    /// Known model ids, used to fall back to a served model.
    pub models: &'a [String],
}

impl std::fmt::Debug for ProxyTarget<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyTarget")
            .field("profile_id", &self.profile_id)
            .field("base_url", &self.base_url)
            .field("has_api_key", &self.api_key.is_some())
            .field("model", &self.model)
            .field("default_model", &self.default_model)
            .field("models", &self.models)
            .finish()
    }
}

/// A provider id that is stable per profile and safe as a config key.
pub fn provider_id(profile_id: &str) -> String {
    let cleaned: String = profile_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect();
    format!("h4rn-{cleaned}")
}

/// Builds the recipe for `kind`.
pub fn for_harness(kind: HarnessKind, target: &ProxyTarget<'_>) -> HarnessOverride {
    let provider = provider_id(target.profile_id);
    let model = resolve_model(kind, target, &provider);
    match kind {
        HarnessKind::ClaudeCode => claude_recipe(target, &provider, model),
        HarnessKind::Codex => codex_recipe(target, &provider, model),
        HarnessKind::Opencode => opencode_recipe(target, &provider, model),
        HarnessKind::Antigravity => antigravity_recipe(target, model),
    }
}

fn claude_recipe(target: &ProxyTarget<'_>, _provider: &str, model: String) -> HarnessOverride {
    let mut env = BTreeMap::new();
    env.insert(
        ENV_ANTHROPIC_BASE_URL.into(),
        anthropic_base(target.base_url),
    );
    if let Some(key) = target.api_key {
        env.insert(ENV_ANTHROPIC_AUTH_TOKEN.into(), key.to_owned());
    }
    env.insert(ENV_ANTHROPIC_MODEL.into(), model.clone());
    env.insert(ENV_ANTHROPIC_SMALL_FAST_MODEL.into(), model.clone());
    env.insert(ENV_GATEWAY_MODEL_DISCOVERY.into(), "1".into());
    HarnessOverride {
        env,
        model,
        verified: true,
        note: Some("Claude base URL omits /v1; the API key rides ANTHROPIC_AUTH_TOKEN".into()),
    }
}

fn codex_recipe(target: &ProxyTarget<'_>, provider: &str, model: String) -> HarnessOverride {
    let mut env = BTreeMap::new();
    env.insert(ENV_MODEL_PROVIDER.into(), provider.to_owned());
    env.insert(ENV_BASE_URL.into(), openai_base(target.base_url));
    env.insert(ENV_WIRE_API.into(), "responses".into());
    if let Some(key) = target.api_key {
        env.insert(ENV_API_KEY_ENV.into(), CODEX_KEY_ENV.into());
        env.insert(CODEX_KEY_ENV.into(), key.to_owned());
    }
    HarnessOverride {
        env,
        model,
        verified: true,
        note: Some(
            "Codex base URL includes /v1; a model_providers entry names CLIPROXY_API_KEY".into(),
        ),
    }
}

fn opencode_recipe(target: &ProxyTarget<'_>, provider: &str, model: String) -> HarnessOverride {
    let mut env = BTreeMap::new();
    env.insert(ENV_OPENCODE_BASE_URL.into(), openai_base(target.base_url));
    if let Some(key) = target.api_key {
        env.insert(ENV_OPENCODE_API_KEY.into(), key.to_owned());
    }
    env.insert(ENV_OPENCODE_PROVIDER.into(), provider.to_owned());
    env.insert(
        ENV_OPENCODE_PACKAGE.into(),
        "@ai-sdk/openai-compatible".into(),
    );
    let bare = strip_provider(&model, provider);
    env.insert(ENV_OPENCODE_MODELS.into(), bare.clone());
    HarnessOverride {
        env,
        model: format!("{provider}/{bare}"),
        verified: true,
        note: Some(
            "OpenCode receives a generated provider config with baseURL, apiKey and models".into(),
        ),
    }
}

fn antigravity_recipe(target: &ProxyTarget<'_>, model: String) -> HarnessOverride {
    let mut env = BTreeMap::new();
    env.insert(
        ENV_GOOGLE_GEMINI_BASE_URL.into(),
        gemini_base(target.base_url),
    );
    if let Some(key) = target.api_key {
        env.insert(ENV_GEMINI_API_KEY.into(), key.to_owned());
    }
    env.insert(ENV_GEMINI_MODEL.into(), model.clone());
    HarnessOverride {
        env,
        model,
        verified: false,
        note: Some(
            "Antigravity reads GEMINI_API_KEY and GOOGLE_GEMINI_BASE_URL, but the path agy \
             appends is unverified"
                .into(),
        ),
    }
}

/// Resolves the model id a harness should request.
pub fn resolve_model(kind: HarnessKind, target: &ProxyTarget<'_>, provider: &str) -> String {
    let requested = target.model.trim();
    if !requested.is_empty() && !requested.eq_ignore_ascii_case("default") {
        return if kind == HarnessKind::Opencode {
            requested.to_owned()
        } else {
            strip_provider(requested, provider)
        };
    }
    if let Some(default) = target
        .default_model
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        return if kind == HarnessKind::Opencode {
            default.to_owned()
        } else {
            strip_provider(default, provider)
        };
    }
    let fallback = target.models.first().cloned().unwrap_or_default();
    if kind == HarnessKind::Opencode {
        fallback
    } else {
        strip_provider(&fallback, provider)
    }
}

fn strip_provider(model: &str, provider: &str) -> String {
    model
        .strip_prefix(provider)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(model)
        .to_owned()
}

/// Claude Code appends `/v1/messages`, so the base URL must not end in `/v1`.
pub fn anthropic_base(base_url: &str) -> String {
    strip_suffix(base_url, "/v1")
}

/// Codex and OpenAI-compatible clients append `/responses` or
/// `/chat/completions`, so the base URL must end in `/v1`.
pub fn openai_base(base_url: &str) -> String {
    let base = strip_suffix(base_url, "/v1");
    format!("{base}/v1")
}

/// Gemini clients append `/v1beta`.
pub fn gemini_base(base_url: &str) -> String {
    strip_suffix(&strip_suffix(base_url, "/v1beta"), "/v1")
}

/// The listener root, with any API-version suffix removed.
///
/// Profiles may be entered as `https://proxy.example`, `.../v1` or
/// `.../v1beta`; probes and management calls always target the root.
pub fn root_base(base_url: &str) -> String {
    gemini_base(base_url)
}

fn strip_suffix(value: &str, suffix: &str) -> String {
    value
        .trim_end_matches('/')
        .trim_end_matches(suffix)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target<'a>(models: &'a [String]) -> ProxyTarget<'a> {
        ProxyTarget {
            profile_id: "proxy_0",
            base_url: "http://127.0.0.1:8317/v1/",
            api_key: Some("sk-test"),
            model: "gpt-6.1-sol",
            default_model: None,
            models,
        }
    }

    #[test]
    fn claude_omits_v1_and_uses_the_bearer_token() {
        let models = vec!["gpt-6.1-sol".into()];
        let recipe = for_harness(HarnessKind::ClaudeCode, &target(&models));
        assert_eq!(recipe.env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8317");
        assert_eq!(recipe.env["ANTHROPIC_AUTH_TOKEN"], "sk-test");
        assert_eq!(recipe.env["ANTHROPIC_MODEL"], "gpt-6.1-sol");
        assert_eq!(recipe.env["ANTHROPIC_SMALL_FAST_MODEL"], "gpt-6.1-sol");
        assert_eq!(
            recipe.env["CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"],
            "1"
        );
        assert!(recipe.verified);
    }

    #[test]
    fn codex_includes_v1_and_names_the_key_env() {
        let models = vec!["gpt-6.1-sol".into()];
        let recipe = for_harness(HarnessKind::Codex, &target(&models));
        assert_eq!(
            recipe.env["H4RN_CODEX_BASE_URL"],
            "http://127.0.0.1:8317/v1"
        );
        assert_eq!(recipe.env["H4RN_CODEX_MODEL_PROVIDER"], "h4rn-proxy_0");
        assert_eq!(recipe.env["H4RN_CODEX_API_KEY_ENV"], CODEX_KEY_ENV);
        assert_eq!(recipe.env[CODEX_KEY_ENV], "sk-test");
        assert_eq!(recipe.env["H4RN_CODEX_WIRE_API"], "responses");
    }

    #[test]
    fn opencode_prefixes_the_model_with_its_provider() {
        let models = vec!["gpt-6.1-sol".into()];
        let recipe = for_harness(HarnessKind::Opencode, &target(&models));
        assert_eq!(
            recipe.env["H4RN_OPENCODE_BASE_URL"],
            "http://127.0.0.1:8317/v1"
        );
        assert_eq!(recipe.env["H4RN_OPENCODE_PROVIDER"], "h4rn-proxy_0");
        assert_eq!(recipe.env["H4RN_OPENCODE_MODELS"], "gpt-6.1-sol");
        assert_eq!(recipe.model, "h4rn-proxy_0/gpt-6.1-sol");
    }

    #[test]
    fn antigravity_is_marked_unverified() {
        let models = vec!["gemini-3-pro".into()];
        let recipe = for_harness(HarnessKind::Antigravity, &target(&models));
        assert_eq!(
            recipe.env["GOOGLE_GEMINI_BASE_URL"],
            "http://127.0.0.1:8317"
        );
        assert_eq!(recipe.env["GEMINI_API_KEY"], "sk-test");
        assert_eq!(recipe.env["GEMINI_MODEL"], "gpt-6.1-sol");
        assert!(!recipe.verified);
        assert!(recipe.note.expect("note").contains("unverified"));
    }

    #[test]
    fn a_default_model_is_used_when_the_thread_did_not_pick_one() {
        let models = vec!["first".into()];
        let mut target = target(&models);
        target.model = "default";
        target.default_model = Some("chosen");
        assert_eq!(
            for_harness(HarnessKind::ClaudeCode, &target).model,
            "chosen"
        );
    }

    #[test]
    fn a_model_only_the_proxy_serves_survives_every_recipe() {
        let models = vec!["deepseek-v4.1-flash".into()];
        let mut target = target(&models);
        target.model = "deepseek-v4.1-flash";
        for kind in [
            HarnessKind::ClaudeCode,
            HarnessKind::Codex,
            HarnessKind::Opencode,
            HarnessKind::Antigravity,
        ] {
            let recipe = for_harness(kind, &target);
            assert!(recipe.model.contains("deepseek-v4.1-flash"), "{kind:?}");
        }
    }

    #[test]
    fn base_url_math_is_idempotent() {
        assert_eq!(anthropic_base("http://x/v1/"), "http://x");
        assert_eq!(anthropic_base("http://x"), "http://x");
        assert_eq!(openai_base("http://x"), "http://x/v1");
        assert_eq!(openai_base("http://x/v1"), "http://x/v1");
        assert_eq!(gemini_base("http://x/v1beta"), "http://x");
        assert_eq!(gemini_base("http://x"), "http://x");
    }
}
