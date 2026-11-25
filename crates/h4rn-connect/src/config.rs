//! Relay configuration loaded from environment and an optional TOML file.
//!
//! There is no compiled-in relay URL, issuer or secret (ADR-018, ADR-025):
//! everything is deployment configuration. Environment variables win over the
//! TOML file so a container can be configured without rebuilding an image.

use std::collections::BTreeSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

/// Deployment shape (ADR-018).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// One relay, one team, the operator supplies the IdP and the host.
    #[default]
    SelfHosted,
    /// Multi-tenant hosting. Requires HTTPS and a public URL.
    Hosted,
}

impl Mode {
    fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "self_hosted" | "self-hosted" | "single_tenant" | "single-tenant" => {
                Ok(Self::SelfHosted)
            }
            "hosted" | "multi_tenant" | "multi-tenant" => Ok(Self::Hosted),
            other => Err(AppError::config(
                "invalid_mode",
                format!("unknown mode `{other}`"),
                Some("Use `self_hosted` or `hosted`.".to_string()),
            )),
        }
    }
}

/// One configured OIDC provider.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Stable id used in `/v1/auth/:provider/*` routes.
    pub id: String,
    /// Name shown on the login screen.
    pub display_name: String,
    /// Issuer URL, no trailing slash and no path prefix.
    pub issuer: String,
    /// OIDC client id.
    pub client_id: String,
    /// Client secret for confidential clients; omitted for public clients.
    pub client_secret: Option<String>,
    /// Scopes to request. `openid` and `offline_access` are added when absent.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Redirect URIs this deployment accepts for the provider.
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    /// Whether the deployment may use the device grant. Discovery still has
    /// the final say (quirk 15).
    #[serde(default = "default_true")]
    pub device_grant: bool,
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("id", &self.id)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("scopes", &self.scopes)
            .field("redirect_uris", &self.redirect_uris)
            .field("device_grant", &self.device_grant)
            .finish()
    }
}

impl ProviderConfig {
    /// The provider's discovery URL.
    pub fn discovery_url(&self) -> String {
        format!("{}/.well-known/openid-configuration", self.issuer)
    }

    /// Scopes normalised for an authorization request.
    ///
    /// OrbitID treats the application's scopes as an allowlist, so we request
    /// exactly what was configured and always include `openid`; `offline_access`
    /// is required for a refresh token and therefore requested by default
    /// (quirk 6).
    pub fn requested_scopes(&self) -> Vec<String> {
        let mut scopes: BTreeSet<String> = self.scopes.iter().cloned().collect();
        scopes.insert("openid".to_string());
        scopes.insert("offline_access".to_string());
        scopes.into_iter().collect()
    }

    /// The callback this relay serves for the provider.
    pub fn relay_callback(&self, public_base_url: &str) -> String {
        format!("{}/v1/auth/{}/callback", public_base_url, self.id)
    }

    /// Whether `redirect_uri` is acceptable for this provider.
    ///
    /// Exact match, plus the relay callback. OrbitID's TS build matches
    /// exactly (quirk 19), so the deployment must register the concrete
    /// loopback port and the mobile deep link.
    pub fn allows_redirect(&self, redirect_uri: &str, public_base_url: &str) -> bool {
        redirect_uri == self.relay_callback(public_base_url)
            || self.redirect_uris.iter().any(|uri| uri == redirect_uri)
    }
}

fn default_true() -> bool {
    true
}

/// Rate limits. Zero disables a limit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimits {
    /// General API requests per IP per minute.
    pub per_ip_per_minute: u32,
    /// Auth start/complete/poll requests per IP per minute.
    pub auth_per_minute: u32,
    /// Pairing creates and claims per IP per hour.
    pub pairing_per_hour: u32,
    /// Relay WebSocket upgrades per IP per minute.
    pub relay_connects_per_minute: u32,
    /// Failed pairing claims before a pairing is burned.
    pub pairing_attempts: u32,
    /// Short-code claim attempts per user and per IP per hour. The code is 40
    /// bits, so this is what makes guessing it impractical.
    pub pairing_code_per_hour: u32,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            per_ip_per_minute: 240,
            auth_per_minute: 30,
            pairing_per_hour: 60,
            relay_connects_per_minute: 120,
            pairing_attempts: 10,
            pairing_code_per_hour: 10,
        }
    }
}

/// Runtime configuration.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Public base URL clients use to reach this relay, no trailing slash.
    pub public_base_url: String,
    /// Listen address (`H4RN_CONNECT_BIND`).
    pub bind: SocketAddr,
    /// Postgres URL.
    pub database_url: String,
    /// Base64 or hex encoded 32-byte key-encryption key (ADR-016).
    pub kek: String,
    /// Base64 or hex encoded 32-byte Ed25519 seed for h4rn session tokens.
    /// Generated ephemerally in self-hosted mode when omitted.
    pub signing_key: Option<String>,
    /// Deployment shape.
    pub mode: Mode,
    /// Allowed browser origins for the REST API. Empty disables CORS.
    pub cors_origins: Vec<String>,
    /// Directory holding the built web UI. `None` disables static serving.
    pub web_dir: Option<PathBuf>,
    /// OIDC providers.
    pub providers: Vec<ProviderConfig>,
    /// Rate limits.
    pub rate_limits: RateLimits,
    /// h4rn access-token lifetime.
    pub session_ttl_secs: i64,
    /// h4rn refresh-token lifetime.
    pub refresh_ttl_secs: i64,
    /// Pairing lifetime. Five minutes per the product contract.
    pub pairing_ttl_secs: i64,
    /// Login-flow lifetime.
    pub auth_flow_ttl_secs: i64,
    /// Window in which a rotated refresh token may be retried and receive the
    /// same successor instead of revoking the family (ADR-011).
    pub refresh_grace_secs: i64,
    /// DPoP proof timestamp tolerance.
    pub dpop_max_skew_secs: i64,
    /// Allow plain HTTP issuers and public URLs. Only for local development.
    pub allow_insecure: bool,
    /// Trust `X-Forwarded-For` for rate limiting. Enable only behind a proxy
    /// that overwrites the header.
    pub trust_forwarded_for: bool,
    /// Maximum relay frame payload accepted from a peer.
    pub relay_max_frame_bytes: usize,
    /// Per-connection relay queue depth (backpressure).
    pub relay_queue_depth: usize,
    /// Firebase service-account JSON, inline or a path to a file
    /// (`H4RN_CONNECT_FCM_SERVICE_ACCOUNT`). Missing disables push without
    /// pretending to send.
    pub fcm_service_account: Option<String>,
    /// OAuth2 token endpoint used to exchange the service assertion.
    pub fcm_token_url: String,
    /// Base URL for the FCM v1 API. Tests point this at a mock server.
    pub fcm_url: String,
    /// How long to collect activity transitions before grouping them. Tests
    /// shorten this; the default is one second.
    pub push_group_window_ms: u64,
    /// How old a completed/failed transition may be and still alert.
    pub push_completion_max_age_secs: i64,
    /// How long Connect waits for a relayed guest exchange's answer.
    ///
    /// A daemon that takes longer is answered with an unavailable error and
    /// the exchange is forgotten, so no waiter or half-arrived body survives.
    pub guest_answer_timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            public_base_url: String::new(),
            bind: crate::default_addr(),
            database_url: String::new(),
            kek: String::new(),
            signing_key: None,
            mode: Mode::SelfHosted,
            cors_origins: Vec::new(),
            web_dir: None,
            providers: Vec::new(),
            rate_limits: RateLimits::default(),
            session_ttl_secs: 600,
            refresh_ttl_secs: 30 * 24 * 60 * 60,
            pairing_ttl_secs: 300,
            auth_flow_ttl_secs: 600,
            refresh_grace_secs: 30,
            dpop_max_skew_secs: 60,
            allow_insecure: false,
            trust_forwarded_for: false,
            relay_max_frame_bytes: 8 * 1024 * 1024,
            relay_queue_depth: 64,
            fcm_service_account: None,
            fcm_token_url: "https://oauth2.googleapis.com/token".to_string(),
            fcm_url: "https://fcm.googleapis.com".to_string(),
            push_group_window_ms: 1_000,
            push_completion_max_age_secs: 120,
            guest_answer_timeout_secs: 120,
        }
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("public_base_url", &self.public_base_url)
            .field("bind", &self.bind)
            .field("database_url", &"<redacted>")
            .field("kek", &"<redacted>")
            .field(
                "signing_key",
                &self.signing_key.as_ref().map(|_| "<redacted>"),
            )
            .field("mode", &self.mode)
            .field("cors_origins", &self.cors_origins)
            .field("web_dir", &self.web_dir)
            .field("providers", &self.providers)
            .field("rate_limits", &self.rate_limits)
            .field("session_ttl_secs", &self.session_ttl_secs)
            .field("refresh_ttl_secs", &self.refresh_ttl_secs)
            .field("pairing_ttl_secs", &self.pairing_ttl_secs)
            .field("allow_insecure", &self.allow_insecure)
            .field(
                "fcm_service_account",
                &self.fcm_service_account.as_ref().map(|_| "<redacted>"),
            )
            .field("fcm_token_url", &self.fcm_token_url)
            .field("fcm_url", &self.fcm_url)
            .field("guest_answer_timeout_secs", &self.guest_answer_timeout_secs)
            .finish()
    }
}

impl Config {
    /// Loads configuration from an explicit TOML path, `H4RN_CONNECT_CONFIG`,
    /// `./h4rn-connect.toml` when present, and environment overrides.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = explicit
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os("H4RN_CONNECT_CONFIG").map(PathBuf::from))
            .or_else(|| {
                let candidate = PathBuf::from("h4rn-connect.toml");
                candidate.is_file().then_some(candidate)
            });

        let mut config = match path {
            Some(path) => {
                let text = std::fs::read_to_string(&path).map_err(|err| {
                    AppError::config(
                        "config_unreadable",
                        format!("could not read {}: {err}", path.display()),
                        Some("Set H4RN_CONNECT_CONFIG to a readable TOML file.".to_string()),
                    )
                })?;
                toml::from_str(&text).map_err(|err| {
                    AppError::config(
                        "config_invalid",
                        format!("{} is not valid configuration: {err}", path.display()),
                        None,
                    )
                })?
            }
            None => Self::default(),
        };

        config.apply_env()?;
        config.normalise()?;
        Ok(config)
    }

    fn apply_env(&mut self) -> Result<()> {
        if let Some(value) = env("H4RN_CONNECT_PUBLIC_BASE_URL") {
            self.public_base_url = value;
        }
        if let Some(value) = env("H4RN_CONNECT_BIND") {
            self.bind = value.parse().map_err(|_| {
                AppError::config("invalid_bind", format!("`{value}` is not an address"), None)
            })?;
        }
        if let Some(value) = env("H4RN_CONNECT_DATABASE_URL").or_else(|| env("DATABASE_URL")) {
            self.database_url = value;
        }
        if let Some(value) = env("H4RN_CONNECT_KEK") {
            self.kek = value;
        }
        if let Some(value) = env("H4RN_CONNECT_SIGNING_KEY") {
            self.signing_key = Some(value);
        }
        if let Some(value) = env("H4RN_CONNECT_MODE") {
            self.mode = Mode::parse(&value)?;
        }
        if let Some(value) = env("H4RN_CONNECT_CORS_ORIGINS") {
            self.cors_origins = split_list(&value);
        }
        if let Some(value) = env("H4RN_CONNECT_WEB_DIR") {
            self.web_dir = (!value.trim().is_empty()).then(|| PathBuf::from(value));
        }
        if let Some(value) = env("H4RN_CONNECT_ALLOW_INSECURE") {
            self.allow_insecure = parse_bool(&value)?;
        }
        if let Some(value) = env("H4RN_CONNECT_TRUST_FORWARDED_FOR") {
            self.trust_forwarded_for = parse_bool(&value)?;
        }
        if let Some(value) = env("H4RN_CONNECT_SESSION_TTL_SECS") {
            self.session_ttl_secs = parse_i64("H4RN_CONNECT_SESSION_TTL_SECS", &value)?;
        }
        if let Some(value) = env("H4RN_CONNECT_REFRESH_TTL_SECS") {
            self.refresh_ttl_secs = parse_i64("H4RN_CONNECT_REFRESH_TTL_SECS", &value)?;
        }
        if let Some(value) = env("H4RN_CONNECT_PAIRING_TTL_SECS") {
            self.pairing_ttl_secs = parse_i64("H4RN_CONNECT_PAIRING_TTL_SECS", &value)?;
        }
        if let Some(value) = env("H4RN_CONNECT_REFRESH_GRACE_SECS") {
            self.refresh_grace_secs = parse_i64("H4RN_CONNECT_REFRESH_GRACE_SECS", &value)?;
        }
        if let Some(value) = env("H4RN_CONNECT_OIDC_PROVIDERS") {
            self.providers = serde_json::from_str(&value).map_err(|err| {
                AppError::config(
                    "invalid_providers",
                    format!("H4RN_CONNECT_OIDC_PROVIDERS is not valid JSON: {err}"),
                    Some(
                        "Provide a JSON array of {id, display_name, issuer, client_id, ...}."
                            .to_string(),
                    ),
                )
            })?;
        }
        if let Some(value) = env("H4RN_CONNECT_RATE_PER_IP_PER_MINUTE") {
            self.rate_limits.per_ip_per_minute =
                parse_u32("H4RN_CONNECT_RATE_PER_IP_PER_MINUTE", &value)?;
        }
        if let Some(value) = env("H4RN_CONNECT_RATE_AUTH_PER_MINUTE") {
            self.rate_limits.auth_per_minute =
                parse_u32("H4RN_CONNECT_RATE_AUTH_PER_MINUTE", &value)?;
        }
        if let Some(value) = env("H4RN_CONNECT_RATE_PAIRING_PER_HOUR") {
            self.rate_limits.pairing_per_hour =
                parse_u32("H4RN_CONNECT_RATE_PAIRING_PER_HOUR", &value)?;
        }
        if let Some(value) = env("H4RN_CONNECT_RATE_RELAY_CONNECTS_PER_MINUTE") {
            self.rate_limits.relay_connects_per_minute =
                parse_u32("H4RN_CONNECT_RATE_RELAY_CONNECTS_PER_MINUTE", &value)?;
        }
        if let Some(value) = env("H4RN_CONNECT_RATE_PAIRING_CODE_PER_HOUR") {
            self.rate_limits.pairing_code_per_hour =
                parse_u32("H4RN_CONNECT_RATE_PAIRING_CODE_PER_HOUR", &value)?;
        }
        if let Some(value) = env("H4RN_CONNECT_PAIRING_ATTEMPTS") {
            self.rate_limits.pairing_attempts = parse_u32("H4RN_CONNECT_PAIRING_ATTEMPTS", &value)?;
        }
        if let Some(value) = env("H4RN_CONNECT_FCM_SERVICE_ACCOUNT") {
            self.fcm_service_account = Some(value);
        }
        if let Some(value) = env("H4RN_CONNECT_FCM_TOKEN_URL") {
            self.fcm_token_url = value;
        }
        if let Some(value) = env("H4RN_CONNECT_FCM_URL") {
            self.fcm_url = value;
        }
        if let Some(value) = env("H4RN_CONNECT_PUSH_GROUP_WINDOW_MS") {
            self.push_group_window_ms = value.parse().map_err(|_| {
                AppError::config(
                    "invalid_number",
                    "H4RN_CONNECT_PUSH_GROUP_WINDOW_MS is not a number",
                    None,
                )
            })?;
        }
        if let Some(value) = env("H4RN_CONNECT_GUEST_ANSWER_TIMEOUT_SECS") {
            self.guest_answer_timeout_secs =
                parse_u64("H4RN_CONNECT_GUEST_ANSWER_TIMEOUT_SECS", &value)?;
        }
        Ok(())
    }

    /// Validates and normalises the configuration. Called by [`Config::load`];
    /// public so tests and embedders can validate a programmatic config.
    pub fn normalise(&mut self) -> Result<()> {
        self.public_base_url = self
            .public_base_url
            .trim()
            .trim_end_matches('/')
            .to_string();
        if self.public_base_url.is_empty() {
            return Err(AppError::config(
                "missing_public_base_url",
                "public_base_url is required",
                Some("Set H4RN_CONNECT_PUBLIC_BASE_URL to the URL clients use.".to_string()),
            ));
        }
        let base = url::Url::parse(&self.public_base_url).map_err(|_| {
            AppError::config(
                "invalid_public_base_url",
                "public_base_url is not a URL",
                None,
            )
        })?;
        if self.mode == Mode::Hosted && !self.allow_insecure && base.scheme() != "https" {
            return Err(AppError::config(
                "https_required",
                "hosted mode requires an https public_base_url",
                Some(
                    "Terminate TLS in front of h4rn-connect, or set allow_insecure for local dev."
                        .to_string(),
                ),
            ));
        }
        if self.database_url.trim().is_empty() {
            return Err(AppError::config(
                "missing_database_url",
                "database_url is required",
                Some("Set H4RN_CONNECT_DATABASE_URL or DATABASE_URL.".to_string()),
            ));
        }
        if self.kek.trim().is_empty() {
            return Err(AppError::config(
                "missing_kek",
                "kek is required",
                Some("Set H4RN_CONNECT_KEK to a 32-byte key, base64 or hex.".to_string()),
            ));
        }
        if self.signing_key.is_none() {
            if self.mode == Mode::Hosted {
                return Err(AppError::config(
                    "missing_signing_key",
                    "signing_key is required in hosted mode",
                    Some("Set H4RN_CONNECT_SIGNING_KEY so sessions survive a restart.".to_string()),
                ));
            }
            tracing::warn!(
                "H4RN_CONNECT_SIGNING_KEY is unset; generating an ephemeral key (sessions end on restart)"
            );
            self.signing_key = Some(crate::crypto::random_b64(32));
        }
        if self.pairing_ttl_secs <= 0 || self.pairing_ttl_secs > 3600 {
            return Err(AppError::config(
                "invalid_pairing_ttl",
                "pairing_ttl_secs must be between 1 and 3600",
                Some("The product contract is 300 seconds.".to_string()),
            ));
        }

        let mut seen = BTreeSet::new();
        for provider in &mut self.providers {
            provider.id = provider.id.trim().to_ascii_lowercase();
            if provider.id.is_empty()
                || !provider
                    .id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                return Err(AppError::config(
                    "invalid_provider_id",
                    "provider id must be lowercase letters, digits or `-`",
                    None,
                ));
            }
            if !seen.insert(provider.id.clone()) {
                return Err(AppError::config(
                    "duplicate_provider",
                    format!("provider `{}` is configured twice", provider.id),
                    None,
                ));
            }
            provider.issuer = provider.issuer.trim().trim_end_matches('/').to_string();
            let issuer = url::Url::parse(&provider.issuer).map_err(|_| {
                AppError::config(
                    "invalid_issuer",
                    format!("provider `{}` has an invalid issuer", provider.id),
                    None,
                )
            })?;
            // Quirk 2 and 3: OrbitID's issuer is root-mounted with no trailing
            // slash, and its TS discovery only answers at the root path.
            if issuer.path() != "/" && !issuer.path().is_empty() {
                return Err(AppError::config(
                    "issuer_path_prefix",
                    format!(
                        "provider `{}` issuer has a path prefix ({})",
                        provider.id,
                        issuer.path()
                    ),
                    Some(
                        "Mount the provider at the root; OrbitID discovery does not resolve a prefixed issuer."
                            .to_string(),
                    ),
                ));
            }
            if !self.allow_insecure && issuer.scheme() != "https" {
                return Err(AppError::config(
                    "insecure_issuer",
                    format!("provider `{}` issuer must use https", provider.id),
                    Some("Set allow_insecure for local development only.".to_string()),
                ));
            }
            if provider.client_id.trim().is_empty() {
                return Err(AppError::config(
                    "missing_client_id",
                    format!("provider `{}` has no client_id", provider.id),
                    None,
                ));
            }
            if provider.client_secret.as_deref() == Some("") {
                provider.client_secret = None;
            }
            provider.scopes = provider.requested_scopes();
        }

        Ok(())
    }

    /// Access-token lifetime.
    pub fn session_ttl(&self) -> Duration {
        Duration::from_secs(self.session_ttl_secs.max(1) as u64)
    }

    /// Refresh-token lifetime.
    pub fn refresh_ttl(&self) -> Duration {
        Duration::from_secs(self.refresh_ttl_secs.max(1) as u64)
    }

    /// Pairing lifetime.
    pub fn pairing_ttl(&self) -> Duration {
        Duration::from_secs(self.pairing_ttl_secs.max(1) as u64)
    }

    /// Login-flow lifetime.
    pub fn auth_flow_ttl(&self) -> Duration {
        Duration::from_secs(self.auth_flow_ttl_secs.max(1) as u64)
    }

    /// Finds a provider by id.
    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|provider| provider.id == id)
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(AppError::config(
            "invalid_boolean",
            format!("`{value}` is not a boolean"),
            None,
        )),
    }
}

fn parse_i64(name: &str, value: &str) -> Result<i64> {
    value
        .parse()
        .map_err(|_| AppError::config("invalid_number", format!("{name} is not a number"), None))
}

fn parse_u64(name: &str, value: &str) -> Result<u64> {
    value.parse().map_err(|_| {
        AppError::config(
            "invalid_number",
            format!("{name} is not a non-negative integer"),
            None,
        )
    })
}

fn parse_u32(name: &str, value: &str) -> Result<u32> {
    value.parse().map_err(|_| {
        AppError::config(
            "invalid_number",
            format!("{name} is not a non-negative integer"),
            None,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Config {
        Config {
            public_base_url: "http://localhost:8787".to_string(),
            database_url: "postgres://localhost/h4rn".to_string(),
            kek: crate::crypto::random_b64(32),
            signing_key: Some(crate::crypto::random_b64(32)),
            allow_insecure: true,
            ..Config::default()
        }
    }

    #[test]
    fn normalises_public_url_and_issuer() {
        let mut config = base();
        config.public_base_url = "http://localhost:8787/".to_string();
        config.providers = vec![ProviderConfig {
            id: "OrbitID".to_string(),
            display_name: "OrbitID".to_string(),
            issuer: "http://localhost:4000/".to_string(),
            client_id: "oa_x".to_string(),
            client_secret: None,
            scopes: vec!["profile".to_string()],
            redirect_uris: vec!["h4rn://callback".to_string()],
            device_grant: true,
        }];
        config.normalise().unwrap();
        assert_eq!(config.public_base_url, "http://localhost:8787");
        assert_eq!(config.providers[0].id, "orbitid");
        assert_eq!(config.providers[0].issuer, "http://localhost:4000");
        assert!(config.providers[0].scopes.contains(&"openid".to_string()));
        assert!(
            config.providers[0]
                .scopes
                .contains(&"offline_access".to_string())
        );
    }

    #[test]
    fn rejects_prefixed_issuer() {
        let mut config = base();
        config.providers = vec![ProviderConfig {
            id: "orbitid".to_string(),
            display_name: "OrbitID".to_string(),
            issuer: "http://localhost:4000/auth".to_string(),
            client_id: "oa_x".to_string(),
            client_secret: None,
            scopes: Vec::new(),
            redirect_uris: Vec::new(),
            device_grant: true,
        }];
        let error = config.normalise().unwrap_err();
        assert_eq!(error.body().code, "issuer_path_prefix");
    }

    #[test]
    fn hosted_mode_requires_https() {
        let mut config = base();
        config.mode = Mode::Hosted;
        config.allow_insecure = false;
        let error = config.normalise().unwrap_err();
        assert_eq!(error.body().code, "https_required");
    }

    #[test]
    fn redirect_allowlist_includes_relay_callback() {
        let provider = ProviderConfig {
            id: "orbitid".to_string(),
            display_name: "OrbitID".to_string(),
            issuer: "https://auth.example.com".to_string(),
            client_id: "oa_x".to_string(),
            client_secret: None,
            scopes: Vec::new(),
            redirect_uris: vec!["h4rn://callback".to_string()],
            device_grant: true,
        };
        assert!(provider.allows_redirect("h4rn://callback", "https://connect.example.com"));
        assert!(provider.allows_redirect(
            "https://connect.example.com/v1/auth/orbitid/callback",
            "https://connect.example.com"
        ));
        assert!(!provider.allows_redirect("https://evil.test/cb", "https://connect.example.com"));
    }
}
