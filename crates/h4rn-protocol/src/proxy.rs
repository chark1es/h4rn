//! CLIProxyAPI wire types (ADR-015).
//!
//! Feature detection is expressed as data, never inferred from a version
//! string: [`ProxyCapabilities`] records what the probes actually observed and
//! [`ProxyTestReport`] carries the human-readable diagnostics that explain a
//! failed probe. Secret values only ever travel **into** the daemon (in
//! [`ProxyProfileDraft`] and [`ProxyProfilePatch`]); every type that leaves the
//! daemon reports only `has_management_key` / `has_api_key` booleans.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::common::HarnessKind;

/// Identifies one stored CLIProxyAPI profile.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
pub struct ProxyProfileId(String);

impl ProxyProfileId {
    /// Wraps an existing identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrows the identifier as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProxyProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ProxyProfileId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for ProxyProfileId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl AsRef<str> for ProxyProfileId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// How the keyless management probe resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProxyManagementAccess {
    /// `/v0/management/*` answered 404: the routes were never registered.
    Disabled,
    /// The routes exist and answered 401/403: a management key is required.
    NeedsKey,
    /// A management key was supplied and accepted.
    Available,
    /// A keyless request succeeded: the deployment is open (misconfigured).
    Open,
    /// The probe did not establish the state.
    Unknown,
}

/// The response family a model came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProxyModelShape {
    /// OpenAI `/v1/models` shape.
    OpenAi,
    /// Anthropic `/v1/models` shape selected by `Anthropic-Version`.
    Claude,
    /// Gemini `/v1beta/models` shape.
    Gemini,
}

/// Where one model entry came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProxyModelSource {
    /// Served by `/v1/models` in the OpenAI shape.
    OpenAi,
    /// Served by `/v1/models` in the Claude shape.
    Claude,
    /// Served by `/v1beta/models`.
    Gemini,
    /// Added from `oauth.model-alias`.
    Alias,
    /// Added from `openai-compatibility[].models`.
    Compatibility,
    /// Added from `api-keys.<provider>[].models`.
    Credential,
    /// Added from the static model-definitions catalogue.
    Static,
}

/// One model the proxy can serve, with provenance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProxyModel {
    /// Model id a client sends upstream.
    pub id: String,
    /// Friendly name, when the proxy reports one.
    pub display_name: Option<String>,
    /// Which response family this entry was read from.
    pub shape: ProxyModelShape,
    /// Why this entry exists.
    pub source: ProxyModelSource,
    /// The upstream id when this entry is an alias.
    pub alias_of: Option<String>,
    /// Owner reported by the proxy, when present.
    pub owned_by: Option<String>,
    /// Context window in tokens, when reported.
    pub context_length: Option<u64>,
    /// Maximum output tokens, when reported.
    pub max_output_tokens: Option<u64>,
    /// Whether the fork's enrichment fields (`max_output_length`,
    /// `reasoning_parameters`) were present.
    pub fork_enriched: bool,
}

/// What feature probes observed on one proxy.
///
/// Booleans mean "the probe succeeded", not "the version should have it".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProxyCapabilities {
    /// The server answered `HEAD /healthz` with 2xx.
    pub reachable: bool,
    /// Keyless management probe outcome.
    pub management: ProxyManagementAccess,
    /// `X-CPA-VERSION`, informational only.
    pub version: Option<String>,
    /// `X-CPA-COMMIT`, informational only.
    pub commit: Option<String>,
    /// `X-CPA-BUILD-DATE`, informational only.
    pub build_date: Option<String>,
    /// `X-CPA-SUPPORT-PLUGIN` was `1`.
    pub plugin_support: bool,
    /// `/v0/management/usage-queue` answered 200.
    pub usage_queue: bool,
    /// `/v0/management/usage` answered 200 (CLIProxyAPI ≤ 6.9.x).
    pub legacy_usage: bool,
    /// `/v8/management/config` answered 200.
    pub v8_config: bool,
    /// The v8 config response carried an `ETag`.
    pub v8_etag: Option<String>,
    /// `/v8/management/performance` answered 200 (fork-only).
    pub performance: bool,
    /// `/v0/management/api-key-usage` answered 200.
    pub api_key_usage: bool,
    /// `/v8/management/credentials/verify` exists (upstream-only route).
    pub credentials_verify: bool,
    /// `/v0/management/auth-files` exposed credential cooldowns.
    pub cooldowns: bool,
    /// `/v0/management/quota/providers` returned a non-empty list.
    pub quota_providers: bool,
    /// `/v1/models` answered in the OpenAI shape.
    pub openai_models: bool,
    /// `/v1/models` answered in the Claude shape when asked.
    pub claude_models: bool,
    /// `/v1beta/models` answered in the Gemini shape.
    pub gemini_models: bool,
    /// At least one OpenAI-shaped entry carried the fork's enrichment fields.
    pub fork_model_enrichment: bool,
    /// When the probe ran.
    pub detected_at: DateTime<Utc>,
}

impl ProxyCapabilities {
    /// A capabilities value for a proxy that could not be reached at all.
    pub fn unreachable(at: DateTime<Utc>) -> Self {
        Self {
            reachable: false,
            management: ProxyManagementAccess::Unknown,
            version: None,
            commit: None,
            build_date: None,
            plugin_support: false,
            usage_queue: false,
            legacy_usage: false,
            v8_config: false,
            v8_etag: None,
            performance: false,
            api_key_usage: false,
            credentials_verify: false,
            cooldowns: false,
            quota_providers: false,
            openai_models: false,
            claude_models: false,
            gemini_models: false,
            fork_model_enrichment: false,
            detected_at: at,
        }
    }
}

/// Severity of one diagnostics entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProxyDiagnosticLevel {
    /// Something worked and is worth reporting.
    Info,
    /// Something optional is unavailable.
    Warning,
    /// The primary operation failed.
    Error,
}

/// One "what happened / what to do" line from a probe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProxyDiagnostic {
    /// Severity.
    pub level: ProxyDiagnosticLevel,
    /// Stable short code such as `management_disabled`.
    pub code: String,
    /// What happened, without secrets.
    pub message: String,
    /// What the user should do next, when there is an action.
    pub hint: Option<String>,
}

impl ProxyDiagnostic {
    /// Builds a diagnostic.
    pub fn new(
        level: ProxyDiagnosticLevel,
        code: impl Into<String>,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self {
            level,
            code: code.into(),
            message: message.into(),
            hint,
        }
    }
}

/// The result of testing one profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProxyTestReport {
    /// What the probes observed.
    pub capabilities: ProxyCapabilities,
    /// Round-trip time of the health probe.
    pub latency_ms: u64,
    /// Ordered diagnostics, most important first.
    pub diagnostics: Vec<ProxyDiagnostic>,
}

/// Fields accepted when creating a profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProxyProfileDraft {
    /// Human label.
    pub name: String,
    /// Proxy root URL, for example `http://127.0.0.1:8317`.
    pub base_url: String,
    /// Management key. Stored in the daemon secret store, never returned.
    pub management_key: Option<String>,
    /// Client API key. Stored in the daemon secret store, never returned.
    pub api_key: Option<String>,
    /// Make this the default profile.
    #[serde(default)]
    pub default: bool,
}

/// A field-wise profile update. `None` leaves a field unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProxyProfilePatch {
    /// New label.
    pub name: Option<String>,
    /// New root URL.
    pub base_url: Option<String>,
    /// Replacement management key.
    pub management_key: Option<String>,
    /// Remove the stored management key.
    #[serde(default)]
    pub clear_management_key: bool,
    /// Replacement client API key.
    pub api_key: Option<String>,
    /// Remove the stored client API key.
    #[serde(default)]
    pub clear_api_key: bool,
    /// Change the default profile.
    pub default: Option<bool>,
}

/// One harness toggle on one profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProxyHarnessOverride {
    /// Harness this toggle applies to.
    pub harness: HarnessKind,
    /// Whether sessions of this harness route through the proxy.
    pub enabled: bool,
    /// Model used when the thread did not pick a proxy model.
    pub default_model: Option<String>,
    /// Whether the recipe has been verified in this repository.
    pub verified: bool,
    /// A short note, for example that Antigravity is unverified.
    pub note: Option<String>,
}

/// A profile as exposed to clients. Never contains a secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProxyProfile {
    /// Stable profile id.
    pub id: ProxyProfileId,
    /// Human label.
    pub name: String,
    /// Root URL.
    pub base_url: String,
    /// Whether this is the default profile.
    pub default: bool,
    /// A management key is stored.
    pub has_management_key: bool,
    /// A client API key is stored.
    pub has_api_key: bool,
    /// Per-harness routing toggles.
    pub harnesses: Vec<ProxyHarnessOverride>,
    /// Last cached capabilities, when a probe has run.
    pub capabilities: Option<ProxyCapabilities>,
}

/// A model catalogue read from one profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProxyModelCatalog {
    /// Profile the catalogue came from.
    pub profile_id: ProxyProfileId,
    /// Every model, sorted by id, aliases merged when management was readable.
    pub models: Vec<ProxyModel>,
    /// Whether management-config aliases were merged.
    pub aliases_merged: bool,
    /// When the catalogue was read.
    pub fetched_at: DateTime<Utc>,
}

/// One credential cooldown, copied from CLIProxyAPI's `CooldownView`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProxyCooldown {
    /// `model` or another proxy-defined scope.
    pub scope: Option<String>,
    /// Model the cooldown applies to, when scoped.
    pub model_key: Option<String>,
    /// Human-readable upstream reason.
    pub reason: Option<String>,
    /// When the credential becomes usable again.
    pub retry_at: Option<DateTime<Utc>>,
    /// Seconds remaining, as the proxy reported them.
    pub remaining_seconds: Option<u64>,
    /// Consecutive failure level; higher means a longer backoff.
    pub backoff_level: Option<u32>,
    /// Upstream HTTP status that caused the cooldown.
    pub http_status: Option<u16>,
}

/// One credential and its health.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProxyCredential {
    /// Stable proxy auth index.
    pub auth_index: Option<String>,
    /// Credential file name.
    pub name: Option<String>,
    /// Provider name.
    pub provider: Option<String>,
    /// Proxy-reported status.
    pub status: Option<String>,
    /// Whether the credential is disabled.
    pub disabled: bool,
    /// Successful requests in the current process.
    pub success: Option<u64>,
    /// Failed requests in the current process.
    pub failed: Option<u64>,
    /// Whether quota probing is supported.
    pub supports_quota: bool,
    /// Active cooldowns.
    pub cooldowns: Vec<ProxyCooldown>,
}

/// Credential health and limits for one profile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProxyLimits {
    /// Profile the limits came from.
    pub profile_id: ProxyProfileId,
    /// Whether the management API could be read.
    pub available: bool,
    /// Credentials and their cooldowns.
    pub credentials: Vec<ProxyCredential>,
    /// When the read happened.
    pub observed_at: DateTime<Utc>,
    /// Why limits are unavailable, when they are.
    pub note: Option<String>,
}

/// How [`crate::ProxyFailoverChoice`] is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProxyFailoverStrategy {
    /// Prefer a model from the same vendor family as the current one.
    SameFamilyFirst,
    /// Prefer a credential that is not cooling down, then any model.
    AccountFirst,
    /// Take any model the proxy can serve.
    AnyModel,
}

/// Policy for picking a failover model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProxyFailoverPolicy {
    /// Selection strategy.
    pub strategy: ProxyFailoverStrategy,
    /// Ignore candidates whose cooldown backoff is above this level.
    pub max_backoff_level: Option<u32>,
    /// Model ids that must not be chosen.
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl Default for ProxyFailoverPolicy {
    fn default() -> Self {
        Self {
            strategy: ProxyFailoverStrategy::SameFamilyFirst,
            max_backoff_level: Some(3),
            exclude: Vec::new(),
        }
    }
}

/// A model (and optionally an account) the proxy can serve next.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProxyFailoverChoice {
    /// Replacement model id.
    pub model: String,
    /// Credential name to prefer, when the choice is account-specific.
    pub account: Option<String>,
    /// Why this candidate won.
    pub reason: String,
}

/// One proxy change broadcast to subscribed clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProxyEvent {
    /// The stored profile list changed.
    ProfilesChanged {
        /// The new list, without secrets.
        profiles: Vec<ProxyProfile>,
    },
    /// A profile test finished.
    ProfileTested {
        /// Profile that was tested.
        profile_id: ProxyProfileId,
        /// The report.
        report: ProxyTestReport,
    },
    /// A profile's model catalogue changed.
    ModelsChanged {
        /// Profile whose catalogue changed.
        profile_id: ProxyProfileId,
        /// The new catalogue.
        catalog: ProxyModelCatalog,
    },
    /// A profile's credential health changed.
    LimitsChanged {
        /// Profile whose limits changed.
        profile_id: ProxyProfileId,
        /// The new health snapshot.
        limits: ProxyLimits,
    },
}
