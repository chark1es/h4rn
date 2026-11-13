//! OIDC login: authorization code + S256 PKCE, device grant, discovery
//! (ADR-011, ADR-025).
//!
//! The `openidconnect` crate does discovery, the authorize URL and code
//! exchange, including ID-token signature, `iss`, `aud`, `exp` and `nonce`
//! verification. The provider-specific calls that crate does not model
//! (device authorization, revocation, userinfo) are small form posts against
//! the endpoints from the same discovery document.
//!
//! Every OrbitID quirk that has a coding consequence is handled here:
//!
//! - issuer normalised, root-mounted, no trailing slash (quirks 2, 3);
//! - PKCE always sent with `S256` (quirks 4, 5);
//! - no `form_post`, no `prompt=login`, no `max_age` (quirks 8, 9);
//! - `offline_access` registered because scopes are an allowlist (quirk 6);
//! - only `iss`/`aud`/`exp`/`nonce` are required (quirk 7);
//! - logout is local-first and feature-detects `end_session_endpoint` (quirk 10);
//! - `invalid_grant` is surfaced as "authenticate again", never retried (quirk 13);
//! - device grant requires client auth at `/oauth/device_authorization` (quirk 16);
//! - `aud` is parsed as a single string (quirk 23);
//! - opaque access tokens are used only against `userinfo` (quirk 24);
//! - `429` is surfaced with the provider's hint (quirk 25);
//! - `token_type` is normalised to `Bearer` (quirk 30).

use std::collections::HashMap;
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use h4rn_protocol::ProviderInfo;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, Nonce, OAuth2TokenResponse as _,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RequestTokenError, Scope, TokenResponse as _,
    reqwest::async_http_client,
};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::config::{Config, ProviderConfig};
use crate::error::{AppError, Result};

/// The raw discovery fields h4rn needs beyond `openidconnect`'s model.
///
/// `openidconnect` 3.5 deliberately does not model device authorization,
/// revocation or end-session endpoints, so they are read from the same
/// document. The crate remains the source of validated provider metadata and
/// ID-token verification.
#[derive(Clone, Debug, Deserialize)]
pub struct RawDiscovery {
    /// Issuer. Must equal the configured issuer exactly.
    pub issuer: String,
    /// Authorization endpoint.
    pub authorization_endpoint: String,
    /// Token endpoint.
    pub token_endpoint: String,
    /// JWKS endpoint.
    pub jwks_uri: String,
    /// Userinfo endpoint.
    #[serde(default)]
    pub userinfo_endpoint: Option<String>,
    /// Device authorization endpoint, absent unless enabled (quirk 15).
    #[serde(default)]
    pub device_authorization_endpoint: Option<String>,
    /// Revocation endpoint.
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
    /// Introspection endpoint. Present only for confidential clients.
    #[serde(default)]
    pub introspection_endpoint: Option<String>,
    /// End-session endpoint. TS OrbitID omits it (quirk 10).
    #[serde(default)]
    pub end_session_endpoint: Option<String>,
    /// Advertised scopes.
    #[serde(default)]
    pub scopes_supported: Option<Vec<String>>,
}

/// A discovered provider, cached for the process.
pub struct ProviderRuntime {
    /// Configuration.
    pub config: ProviderConfig,
    /// `openidconnect` client.
    pub client: CoreClient,
    /// Extra discovery fields.
    pub discovery: RawDiscovery,
}

/// Profile data from an ID token or userinfo response.
#[derive(Clone, Debug, Default)]
pub struct UserProfile {
    /// Subject.
    pub subject: String,
    /// Email.
    pub email: Option<String>,
    /// Verified email.
    pub email_verified: bool,
    /// Display name.
    pub display_name: Option<String>,
    /// Avatar.
    pub avatar_url: Option<String>,
    /// Raw claims for the identities table (metadata only).
    pub claims: serde_json::Value,
}

/// A completed OIDC login.
#[derive(Clone, Debug)]
pub struct OidcLogin {
    /// Provider id.
    pub provider_id: String,
    /// Issuer.
    pub issuer: String,
    /// Profile.
    pub profile: UserProfile,
    /// Provider refresh token, stored envelope-encrypted for logout revocation.
    pub refresh_token: Option<String>,
}

/// Device authorization start response.
#[derive(Clone, Debug)]
pub struct DeviceAuthorization {
    /// Provider device code.
    pub device_code: String,
    /// User code.
    pub user_code: String,
    /// Verification URI.
    pub verification_uri: String,
    /// Verification URI with the code.
    pub verification_uri_complete: Option<String>,
    /// Lifetime seconds.
    pub expires_in: i64,
    /// Poll interval.
    pub interval: i64,
}

/// One device-grant poll result.
#[derive(Clone, Debug)]
pub enum DevicePoll {
    /// Still waiting.
    Pending,
    /// The provider asked us to slow down.
    SlowDown {
        /// Seconds to wait.
        retry_after: i64,
    },
    /// Approved.
    Approved {
        /// Login data.
        login: Box<OidcLogin>,
    },
    /// Denied by the user.
    Denied,
    /// Code expired.
    Expired,
    /// Rate limited or another retryable provider failure.
    Retry {
        /// Seconds to wait.
        retry_after: i64,
        /// Short reason for logs.
        reason: String,
    },
}

#[derive(Debug, Deserialize)]
struct DeviceAuthJson {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default = "default_expires")]
    expires_in: i64,
    #[serde(default = "default_interval")]
    interval: i64,
}

fn default_expires() -> i64 {
    600
}

fn default_interval() -> i64 {
    5
}

#[derive(Debug, Deserialize)]
struct TokenJson {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorJson {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserInfoJson {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<bool>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    picture: Option<String>,
}

/// The configured provider set.
#[derive(Clone)]
pub struct OidcRegistry {
    config: Arc<Config>,
    http: reqwest::Client,
    cache: Arc<RwLock<HashMap<String, Arc<ProviderRuntime>>>>,
}

impl std::fmt::Debug for OidcRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcRegistry")
            .field("providers", &self.config.providers.len())
            .finish()
    }
}

impl OidcRegistry {
    /// Builds the registry. Discovery is lazy: a relay can boot while an IdP is
    /// briefly down, and `/readyz` reports the database only.
    pub fn new(config: Arc<Config>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| AppError::config("http_client", format!("{error}"), None))?;
        Ok(Self {
            config,
            http,
            cache: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Returns a discovered provider, caching success.
    pub async fn provider(&self, id: &str) -> Result<Arc<ProviderRuntime>> {
        let config = self
            .config
            .provider(id)
            .ok_or_else(|| {
                AppError::not_found(
                    "provider_not_found",
                    format!("no OIDC provider `{id}` is configured"),
                )
            })?
            .clone();
        if let Some(provider) = self.cache.read().await.get(id).cloned() {
            return Ok(provider);
        }
        let provider = Arc::new(self.discover(config).await?);
        self.cache
            .write()
            .await
            .insert(id.to_string(), provider.clone());
        Ok(provider)
    }

    async fn discover(&self, config: ProviderConfig) -> Result<ProviderRuntime> {
        let issuer = config.issuer.clone();
        let issuer_url = openidconnect::IssuerUrl::new(issuer.clone()).map_err(|_| {
            AppError::config("invalid_issuer", format!("invalid issuer `{issuer}`"), None)
        })?;
        let metadata = CoreProviderMetadata::discover_async(issuer_url, async_http_client)
            .await
            .map_err(|error| {
                AppError::upstream(
                    "provider_discovery_failed",
                    format!(
                        "could not discover `{}`: {error}. Check that the issuer is root-mounted with no trailing slash.",
                        config.id
                    ),
                )
            })?;

        let raw: RawDiscovery = self
            .http
            .get(config.discovery_url())
            .send()
            .await
            .map_err(|error| {
                AppError::upstream(
                    "provider_discovery_failed",
                    format!("could not fetch discovery for `{}`: {error}", config.id),
                )
            })?
            .error_for_status()
            .map_err(|error| {
                AppError::upstream(
                    "provider_discovery_failed",
                    format!("discovery for `{}` failed: {error}", config.id),
                )
            })?
            .json()
            .await
            .map_err(|error| {
                AppError::upstream(
                    "provider_discovery_invalid",
                    format!("discovery for `{}` is not JSON: {error}", config.id),
                )
            })?;

        // Quirk 2: a trailing slash or a different spelling is an issuer
        // mismatch, and `openidconnect` would fail later with an opaque error.
        if raw.issuer.trim_end_matches('/') != issuer {
            return Err(AppError::upstream(
                "issuer_mismatch",
                format!(
                    "provider `{}` advertises issuer `{}` but `{}` is configured",
                    config.id, raw.issuer, issuer
                ),
            ));
        }

        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(config.client_id.clone()),
            config.client_secret.clone().map(ClientSecret::new),
        )
        .set_auth_type(openidconnect::AuthType::RequestBody);

        Ok(ProviderRuntime {
            config,
            client,
            discovery: raw,
        })
    }

    /// Public provider list for the login screen.
    pub async fn public_providers(&self) -> Vec<ProviderInfo> {
        let mut out = Vec::new();
        for provider in &self.config.providers {
            let (device_grant, scopes) = match self.provider(&provider.id).await {
                Ok(runtime) => (
                    provider.device_grant
                        && runtime.discovery.device_authorization_endpoint.is_some(),
                    runtime.discovery.scopes_supported.clone(),
                ),
                Err(_) => (false, None),
            };
            let _ = scopes;
            out.push(ProviderInfo {
                id: provider.id.clone(),
                display_name: provider.display_name.clone(),
                issuer: provider.issuer.clone(),
                device_grant,
                redirect_uris: provider.redirect_uris.clone(),
            });
        }
        out
    }

    /// Starts an authorization-code flow.
    pub async fn start_login(
        &self,
        provider_id: &str,
        redirect_uri: &str,
        state: &str,
        nonce: &str,
    ) -> Result<(String, String)> {
        let provider = self.provider(provider_id).await?;
        if !provider
            .config
            .allows_redirect(redirect_uri, &self.config.public_base_url)
        {
            return Err(AppError::bad_request_hint(
                "redirect_uri_not_allowed",
                "redirect_uri is not registered for this provider",
                "Use one of the provider's configured redirect URIs.",
            ));
        }
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let redirect = RedirectUrl::new(redirect_uri.to_string()).map_err(|_| {
            AppError::bad_request("redirect_uri_invalid", "redirect_uri is not a URL")
        })?;
        let state_value = state.to_string();
        let nonce_value = nonce.to_string();
        let (url, _, _) = provider
            .client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                move || CsrfToken::new(state_value),
                move || Nonce::new(nonce_value),
            )
            .set_redirect_uri(std::borrow::Cow::Owned(redirect))
            .set_pkce_challenge(challenge)
            .add_scopes(
                provider
                    .config
                    .requested_scopes()
                    .into_iter()
                    .map(Scope::new),
            )
            .url();
        Ok((url.to_string(), verifier.secret().to_string()))
    }

    /// Exchanges an authorization code and verifies the ID token.
    pub async fn complete_login(
        &self,
        provider_id: &str,
        code: &str,
        verifier: &str,
        nonce: &str,
    ) -> Result<OidcLogin> {
        let provider = self.provider(provider_id).await?;
        let response = provider
            .client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .set_pkce_verifier(PkceCodeVerifier::new(verifier.to_string()))
            .request_async(async_http_client)
            .await
            .map_err(map_token_error)?;

        let id_token = response.id_token().ok_or_else(|| {
            AppError::upstream("id_token_missing", "provider did not return an ID token")
        })?;
        let nonce = Nonce::new(nonce.to_string());
        let claims = id_token
            .claims(&provider.client.id_token_verifier(), &nonce)
            .map_err(|error| {
                AppError::unauthorized(
                    "id_token_invalid",
                    format!("ID token verification failed: {error}"),
                )
            })?;

        let mut profile = profile_from_id_token(claims);
        if profile.email.is_none() || profile.display_name.is_none() {
            if let Some(endpoint) = &provider.discovery.userinfo_endpoint {
                let userinfo =
                    fetch_userinfo(&self.http, endpoint, response.access_token().secret()).await?;
                if userinfo.sub != profile.subject {
                    return Err(AppError::unauthorized(
                        "userinfo_subject_mismatch",
                        "userinfo subject does not match the ID token",
                    ));
                }
                merge_userinfo(&mut profile, userinfo);
            }
        }

        Ok(OidcLogin {
            provider_id: provider.config.id.clone(),
            issuer: provider.config.issuer.clone(),
            profile,
            refresh_token: response
                .refresh_token()
                .map(|token| token.secret().to_string()),
        })
    }

    /// Starts a device authorization grant.
    pub async fn start_device_grant(&self, provider_id: &str) -> Result<DeviceAuthorization> {
        let provider = self.provider(provider_id).await?;
        if !provider.config.device_grant {
            return Err(AppError::bad_request_hint(
                "device_grant_disabled",
                "this provider's device grant is disabled by configuration",
                "Enable it on the provider and in the provider config.",
            ));
        }
        let endpoint = provider
            .discovery
            .device_authorization_endpoint
            .as_ref()
            .ok_or_else(|| {
                AppError::bad_request_hint(
                    "device_grant_unsupported",
                    "the provider does not advertise a device authorization endpoint",
                    "Enable deviceAuthorizationEnabled on OrbitID, or pair with a QR code.",
                )
            })?;
        let mut form: Vec<(String, String)> = vec![
            ("client_id".to_string(), provider.config.client_id.clone()),
            (
                "scope".to_string(),
                provider.config.requested_scopes().join(" "),
            ),
        ];
        if let Some(secret) = &provider.config.client_secret {
            // Quirk 16: a confidential client must authenticate here too.
            form.push(("client_secret".to_string(), secret.clone()));
        }
        let response = self
            .http
            .post(endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|error| {
                AppError::upstream(
                    "device_grant_failed",
                    format!("device request failed: {error}"),
                )
            })?;
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        if !status.is_success() {
            return Err(provider_error(status.as_u16(), body));
        }
        let parsed: DeviceAuthJson = serde_json::from_value(body).map_err(|error| {
            AppError::upstream(
                "device_grant_invalid",
                format!("device authorization response is invalid: {error}"),
            )
        })?;
        Ok(DeviceAuthorization {
            device_code: parsed.device_code,
            user_code: parsed.user_code,
            verification_uri: parsed.verification_uri,
            verification_uri_complete: parsed.verification_uri_complete,
            expires_in: parsed.expires_in,
            interval: parsed.interval,
        })
    }

    /// Polls the provider for one device grant.
    pub async fn poll_device_grant(
        &self,
        provider_id: &str,
        device_code: &str,
    ) -> Result<DevicePoll> {
        let provider = self.provider(provider_id).await?;
        let mut form: Vec<(String, String)> = vec![
            (
                "grant_type".to_string(),
                "urn:ietf:params:oauth:grant-type:device_code".to_string(),
            ),
            ("device_code".to_string(), device_code.to_string()),
            ("client_id".to_string(), provider.config.client_id.clone()),
        ];
        if let Some(secret) = &provider.config.client_secret {
            form.push(("client_secret".to_string(), secret.clone()));
        }
        let response = self
            .http
            .post(&provider.discovery.token_endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|error| {
                AppError::upstream(
                    "device_poll_failed",
                    format!("token request failed: {error}"),
                )
            })?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = retry_after(&response);
            let body: serde_json::Value = response.json().await.unwrap_or_default();
            let error: OAuthErrorJson = serde_json::from_value(body).unwrap_or(OAuthErrorJson {
                error: "unknown_error".to_string(),
                error_description: None,
            });
            return Ok(match error.error.as_str() {
                "authorization_pending" => DevicePoll::Pending,
                "slow_down" => DevicePoll::SlowDown {
                    retry_after: retry_after.unwrap_or(5),
                },
                "access_denied" => DevicePoll::Denied,
                "expired_token" => DevicePoll::Expired,
                other => DevicePoll::Retry {
                    retry_after: retry_after.unwrap_or(5),
                    reason: error.error_description.unwrap_or_else(|| other.to_string()),
                },
            });
        }
        let body: serde_json::Value = response.json().await.map_err(|error| {
            AppError::upstream(
                "device_poll_invalid",
                format!("token response is invalid: {error}"),
            )
        })?;
        let token: TokenJson = serde_json::from_value(body.clone()).map_err(|error| {
            AppError::upstream(
                "device_poll_invalid",
                format!("token response is missing fields: {error}"),
            )
        })?;
        let id_token = token.id_token.as_deref().ok_or_else(|| {
            AppError::upstream("id_token_missing", "provider did not return an ID token")
        })?;
        let id_token = openidconnect::core::CoreIdToken::from_str(id_token).map_err(|error| {
            AppError::upstream(
                "id_token_invalid",
                format!("ID token is malformed: {error}"),
            )
        })?;
        let claims = id_token
            .claims(&provider.client.id_token_verifier(), accept_any_nonce)
            .map_err(|error| {
                AppError::unauthorized(
                    "id_token_invalid",
                    format!("ID token verification failed: {error}"),
                )
            })?;
        let mut profile = profile_from_id_token(claims);
        if profile.email.is_none() || profile.display_name.is_none() {
            if let Some(endpoint) = &provider.discovery.userinfo_endpoint {
                let userinfo = fetch_userinfo(&self.http, endpoint, &token.access_token).await?;
                if userinfo.sub != profile.subject {
                    return Err(AppError::unauthorized(
                        "userinfo_subject_mismatch",
                        "userinfo subject does not match the ID token",
                    ));
                }
                merge_userinfo(&mut profile, userinfo);
            }
        }
        Ok(DevicePoll::Approved {
            login: Box::new(OidcLogin {
                provider_id: provider.config.id.clone(),
                issuer: provider.config.issuer.clone(),
                profile,
                refresh_token: token.refresh_token,
            }),
        })
    }

    /// Revokes a provider token. Local logout never depends on this succeeding.
    pub async fn revoke_provider_token(&self, provider_id: &str, token: &str) {
        let Ok(provider) = self.provider(provider_id).await else {
            return;
        };
        let Some(endpoint) = provider.discovery.revocation_endpoint.as_ref() else {
            return;
        };
        let mut form: Vec<(String, String)> = vec![
            ("token".to_string(), token.to_string()),
            ("client_id".to_string(), provider.config.client_id.clone()),
        ];
        if let Some(secret) = &provider.config.client_secret {
            form.push(("client_secret".to_string(), secret.clone()));
        }
        match self.http.post(endpoint).form(&form).send().await {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                tracing::warn!(provider = %provider_id, status = %response.status(), "provider token revocation failed");
            }
            Err(error) => {
                tracing::warn!(provider = %provider_id, error = %error, "provider token revocation failed");
            }
        }
    }

    /// Whether the provider advertises RP-initiated logout (quirk 10).
    pub async fn end_session_endpoint(&self, provider_id: &str) -> Option<String> {
        self.provider(provider_id)
            .await
            .ok()
            .and_then(|provider| provider.discovery.end_session_endpoint.clone())
    }
}

fn accept_any_nonce(_nonce: Option<&Nonce>) -> std::result::Result<(), String> {
    // The device grant has no browser session and therefore no nonce to bind.
    // Discovery, signature, issuer, audience and expiry are still verified.
    Ok(())
}

fn retry_after(response: &reqwest::Response) -> Option<i64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

fn map_token_error(
    error: RequestTokenError<
        openidconnect::reqwest::AsyncHttpClientError,
        openidconnect::StandardErrorResponse<openidconnect::core::CoreErrorResponseType>,
    >,
) -> AppError {
    match error {
        RequestTokenError::ServerResponse(response) => {
            let code = response.error().to_string();
            if code == "invalid_grant" {
                // Quirk 13: re-authenticate, never retry.
                AppError::unauthorized(
                    "invalid_grant",
                    "the authorization code is invalid or was already used",
                )
            } else {
                AppError::upstream(
                    "provider_token_error",
                    format!("provider rejected the token request: {code}"),
                )
            }
        }
        RequestTokenError::Request(error) => AppError::upstream(
            "provider_unreachable",
            format!("token request failed: {error}"),
        ),
        RequestTokenError::Parse(error, _) => AppError::upstream(
            "provider_token_invalid",
            format!("token response is invalid: {error}"),
        ),
        RequestTokenError::Other(error) => AppError::upstream(
            "provider_token_error",
            format!("token request failed: {error}"),
        ),
    }
}

fn provider_error(status: u16, body: serde_json::Value) -> AppError {
    let error: OAuthErrorJson = serde_json::from_value(body).unwrap_or(OAuthErrorJson {
        error: "unknown_error".to_string(),
        error_description: None,
    });
    if status == 429 {
        return AppError::rate_limited(5);
    }
    AppError::upstream(
        "provider_error",
        format!(
            "provider returned {}: {}{}",
            status,
            error.error,
            error
                .error_description
                .map(|description| format!(" ({description})"))
                .unwrap_or_default()
        ),
    )
}

fn profile_from_id_token(
    claims: &openidconnect::IdTokenClaims<
        openidconnect::EmptyAdditionalClaims,
        openidconnect::core::CoreGenderClaim,
    >,
) -> UserProfile {
    let email = claims.email().map(|email| email.as_str().to_string());
    let display_name = claims
        .name()
        .and_then(|name| name.get(None))
        .map(|name| name.as_str().to_string())
        .or_else(|| {
            claims
                .preferred_username()
                .map(|username| username.as_str().to_string())
        });
    let avatar_url = claims
        .picture()
        .and_then(|picture| picture.get(None))
        .map(|picture| picture.as_str().to_string());
    UserProfile {
        subject: claims.subject().as_str().to_string(),
        email,
        email_verified: claims.email_verified().unwrap_or(false),
        display_name,
        avatar_url,
        claims: serde_json::json!({
            "sub": claims.subject().as_str(),
            "iss": claims.issuer().as_str(),
            "email_verified": claims.email_verified(),
        }),
    }
}

fn merge_userinfo(profile: &mut UserProfile, userinfo: UserInfoJson) {
    if profile.email.is_none() {
        profile.email = userinfo.email;
    }
    if profile.display_name.is_none() {
        profile.display_name = userinfo.name.or(userinfo.preferred_username);
    }
    if profile.avatar_url.is_none() {
        profile.avatar_url = userinfo.picture;
    }
    if let Some(verified) = userinfo.email_verified {
        profile.email_verified = verified;
    }
}

async fn fetch_userinfo(
    http: &reqwest::Client,
    endpoint: &str,
    access_token: &str,
) -> Result<UserInfoJson> {
    let response = http
        .get(endpoint)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|error| {
            AppError::upstream(
                "userinfo_failed",
                format!("userinfo request failed: {error}"),
            )
        })?;
    if !response.status().is_success() {
        return Err(AppError::upstream(
            "userinfo_failed",
            format!("userinfo returned {}", response.status()),
        ));
    }
    response.json().await.map_err(|error| {
        AppError::upstream(
            "userinfo_invalid",
            format!("userinfo response is invalid: {error}"),
        )
    })
}

/// Builds the `end_session` URL when a provider advertises one.
pub fn end_session_url(
    provider: &ProviderRuntime,
    id_token_hint: Option<&str>,
    post_logout: &str,
) -> Option<String> {
    let endpoint = provider.discovery.end_session_endpoint.as_ref()?;
    let mut url = url::Url::parse(endpoint).ok()?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("client_id", &provider.config.client_id);
        query.append_pair("post_logout_redirect_uri", post_logout);
        if let Some(hint) = id_token_hint {
            query.append_pair("id_token_hint", hint);
        }
    }
    Some(url.to_string())
}

/// Returns the current unix time in seconds (kept here to make the OIDC
/// module's time-dependent behaviour injectable in tests later).
pub fn now_unix() -> i64 {
    Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_discovery_captures_optional_endpoints() {
        let json = serde_json::json!({
            "issuer": "http://localhost:4000",
            "authorization_endpoint": "http://localhost:4000/oauth/authorize",
            "token_endpoint": "http://localhost:4000/oauth/token",
            "jwks_uri": "http://localhost:4000/oauth/jwks",
            "userinfo_endpoint": "http://localhost:4000/oauth/userinfo",
            "device_authorization_endpoint": "http://localhost:4000/oauth/device_authorization",
            "revocation_endpoint": "http://localhost:4000/oauth/revoke"
        });
        let discovery: RawDiscovery = serde_json::from_value(json).unwrap();
        assert!(discovery.device_authorization_endpoint.is_some());
        assert!(discovery.end_session_endpoint.is_none());
    }

    #[test]
    fn endpoint_errors_are_classified() {
        let error = provider_error(429, serde_json::json!({"error": "slow_down"}));
        assert_eq!(error.status().as_u16(), 429);
    }
}
