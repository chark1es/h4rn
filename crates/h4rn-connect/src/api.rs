//! REST surface, authentication extractor and process wiring.
//!
//! Every endpoint returns the fixed `{code, message, hint}` envelope on error
//! (see [`crate::error`]). The REST API is only the control plane: prompts,
//! terminal output and file contents never touch it. Everything that needs to
//! reach a machine travels through [`crate::relay`] as opaque bytes.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, ORIGIN};
use axum::http::request::Parts;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use chrono::Utc;
use h4rn_protocol::skills::{
    SkillBundleFile, SkillManifest, SkillSyncAck, SkillSyncConflict, SkillSyncEntry, SkillSyncPull,
    SkillSyncPullResponse, SkillSyncPush, SkillSyncPushResponse, SkillVersionVector,
};
use h4rn_protocol::{
    AuditEntry, AuditIngestRequest, AuditPage, ConnectDevice, ConnectMeResponse,
    ConnectSessionTokens, DeviceGrantPollRequest, DeviceGrantPollResponse, DeviceGrantStartRequest,
    DeviceGrantStartResponse, DeviceGrantStatus, DeviceRegistration, DeviceRenameRequest,
    LoginCompleteRequest, LoginCompleteResponse, LoginStartRequest, LoginStartResponse,
    LogoutRequest, MachineAddressesUpdate, MachineHost, MachineHostsUpdate, MachineSummary,
    PairingClaimRequest, PairingClaimResponse, PairingCreateRequest, PairingCreateResponse,
    PairingInfo, ProviderInfo, RefreshRequest, UsageBucket, UsageGroupBy, UsageReport, UsageScope,
    UsageSyncRequest, UsageSyncResponse,
};
use h4rn_skills::SkillBundle;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::config::Config;
use crate::crypto::{Envelope, Keystore, SealedSecret, b64, b64_decode};
use crate::dpop::ReplayCache;
use crate::error::{AppError, Result};
use crate::jwt::{AccessClaims, issue_access_token};
use crate::oidc::{DevicePoll, OidcLogin, OidcRegistry};
use crate::pairing;
use crate::rate_limit::RateLimiter;
use crate::relay::RelayHub;
use crate::store::Authenticated;
use crate::store::{
    AuthFlowRow, DeviceGrantRow, DeviceRow, NewDevice, NewDeviceGrant, NewSkillEntry, SessionRow,
    Store, UserRow, user_to_wire,
};

/// Shared process state.
#[derive(Clone)]
pub struct AppState {
    /// Deployment configuration.
    pub config: Arc<Config>,
    /// Postgres.
    pub store: Store,
    /// h4rn token signing key.
    pub keystore: Keystore,
    /// Envelope encryption for provider tokens and refresh successors.
    pub envelope: Envelope,
    /// Discovered OIDC providers.
    pub oidc: Arc<OidcRegistry>,
    /// Relay peers and sessions.
    pub relay: Arc<RelayHub>,
    /// Control-plane rate limiter.
    pub rate: Arc<RateLimiter>,
    /// DPoP replay cache.
    pub replay: Arc<ReplayCache>,
    /// Outbound HTTP client for provider revocation.
    pub http: reqwest::Client,
    /// Process start, for `/readyz`.
    pub started: Instant,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Builds state, connecting to Postgres and applying migrations.
pub async fn build_state(config: Config) -> Result<AppState> {
    let store = Store::connect(&config).await?;
    let keystore = Keystore::from_config(&config)?;
    let envelope = Envelope::from_config(&config)?;
    let config = Arc::new(config);
    let oidc = Arc::new(OidcRegistry::new(config.clone())?);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| AppError::internal(format!("http client: {error}")))?;
    Ok(AppState {
        config,
        store,
        keystore,
        envelope,
        oidc,
        relay: Arc::new(RelayHub::new()),
        rate: Arc::new(RateLimiter::new()),
        replay: Arc::new(ReplayCache::default()),
        http,
        started: Instant::now(),
    })
}

/// Builds the router. Kept pure so tests can exercise it over oneshot without a
/// socket.
pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/v1/auth/providers", get(list_providers))
        .route("/v1/auth/{provider}/start", post(auth_start))
        .route("/v1/auth/{provider}/complete", post(auth_complete))
        .route("/v1/auth/{provider}/callback", get(auth_callback))
        .route("/v1/auth/device/start", post(device_start))
        .route("/v1/auth/device/poll", post(device_poll))
        .route("/v1/auth/refresh", post(refresh))
        .route("/v1/auth/logout", post(logout))
        .route("/v1/me", get(me))
        .route("/v1/devices", get(list_devices))
        .route("/v1/devices/{id}", patch(rename_device))
        .route("/v1/devices/{id}", delete(revoke_device))
        .route("/v1/machines", get(list_machines))
        .route("/v1/machines/{id}/hosts", put(update_hosts))
        .route("/v1/machines/{id}/addresses", put(update_addresses))
        .route("/v1/pairings", get(list_pairings).post(create_pairing))
        .route("/v1/pairings/claim", post(claim_pairing))
        .route("/v1/pairings/{id}", delete(revoke_pairing))
        .route("/v1/audit", get(audit).post(ingest_audit))
        .route("/v1/usage", post(ingest_usage))
        .route("/v1/usage/aggregate", get(usage_aggregate))
        .merge(
            Router::new()
                .route("/v1/skills/sync/push", post(skills_push))
                .route("/v1/skills/sync/pull", post(skills_pull))
                // A bundle is documents and scripts, not media. 16 MiB leaves
                // room for a large reference tree while bounding a bad client.
                .layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            api_rate_limit,
        ))
        .layer(DefaultBodyLimit::max(256 * 1024));

    let mut app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/relay", get(relay_upgrade))
        .merge(api)
        .layer(TraceLayer::new_for_http());

    if let Some(dir) = &state.config.web_dir {
        let index = dir.join("index.html");
        app = app.fallback_service(ServeDir::new(dir).not_found_service(ServeFile::new(index)));
    }

    if let Some(cors) = cors_layer(&state.config) {
        app = app.layer(cors);
    }

    app.with_state(state)
}

fn cors_layer(config: &Config) -> Option<CorsLayer> {
    if config.cors_origins.is_empty() {
        return None;
    }
    let allow_origin: tower_http::cors::AllowOrigin =
        if config.cors_origins.iter().any(|origin| origin == "*") {
            tower_http::cors::AllowOrigin::any()
        } else {
            let origins: Vec<HeaderValue> = config
                .cors_origins
                .iter()
                .filter_map(|origin| HeaderValue::from_str(origin).ok())
                .collect();
            tower_http::cors::AllowOrigin::list(origins)
        };
    Some(
        CorsLayer::new()
            .allow_origin(allow_origin)
            .allow_methods(Any)
            .allow_headers([
                AUTHORIZATION,
                CONTENT_TYPE,
                ORIGIN,
                HeaderName::from_static("x-h4rn-proof"),
                HeaderName::from_static("x-h4rn-timestamp"),
                HeaderName::from_static("x-h4rn-nonce"),
            ]),
    )
}

// ---------------------------------------------------------------------------
// Extractors
// ---------------------------------------------------------------------------

/// The authenticated caller. Every protected route uses this; the proof is
/// verified here so no handler can forget it.
#[derive(Clone, Debug)]
pub struct Authed {
    /// Token claims.
    pub claims: AccessClaims,
    /// User row.
    pub user: UserRow,
    /// Device row.
    pub device: DeviceRow,
    /// Session row.
    pub session: SessionRow,
}

impl FromRequestParts<AppState> for Authed {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> std::result::Result<Self, Self::Rejection> {
        let token = bearer(&parts.headers).ok_or_else(|| {
            AppError::unauthorized("token_required", "an access token is required")
        })?;
        let proof = header(&parts.headers, "x-h4rn-proof")
            .ok_or_else(|| AppError::unauthorized("proof_required", "a DPoP proof is required"))?;
        let timestamp: i64 = header(&parts.headers, "x-h4rn-timestamp")
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| {
                AppError::unauthorized("proof_required", "a proof timestamp is required")
            })?;
        let nonce = header(&parts.headers, "x-h4rn-nonce")
            .ok_or_else(|| AppError::unauthorized("proof_required", "a proof nonce is required"))?;

        let claims = crate::jwt::verify_access_token(&state.keystore, &token, Utc::now())?;
        let Authenticated {
            user,
            device,
            session,
        } = state.store.authenticate(&claims).await?;
        let public_key = b64(&device.proof_public_key);
        crate::dpop::verify_proof(
            &crate::dpop::Proof {
                public_key: &public_key,
                method: parts.method.as_str(),
                path: parts.uri.path(),
                timestamp,
                nonce: &nonce,
                signature: &proof,
            },
            Utc::now(),
            state.config.dpop_max_skew_secs,
        )?;
        if !state
            .replay
            .first_use(&proof, timestamp, Utc::now().timestamp())
        {
            return Err(AppError::unauthorized(
                "proof_replayed",
                "this proof was already used",
            ));
        }
        Ok(Authed {
            claims,
            user,
            device,
            session,
        })
    }
}

/// The caller's IP, honouring `X-Forwarded-For` only when configured.
#[derive(Clone, Debug)]
pub struct ClientIp(pub String);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> std::result::Result<Self, Self::Rejection> {
        Ok(ClientIp(ip_from(&parts.headers, &parts.extensions, state)))
    }
}

fn ip_from(headers: &HeaderMap, extensions: &axum::http::Extensions, state: &AppState) -> String {
    if state.config.trust_forwarded_for {
        if let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return forwarded.to_string();
        }
    }
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(ToOwned::to_owned)
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn user_agent(headers: &HeaderMap) -> Option<String> {
    header(headers, "user-agent").map(|value| value.chars().take(300).collect())
}

async fn api_rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response> {
    let ip = ip_from(request.headers(), request.extensions(), &state);
    state.rate.check(
        "api",
        &ip,
        state.config.rate_limits.per_ip_per_minute,
        Duration::from_secs(60),
    )?;
    Ok(next.run(request).await)
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(state): State<AppState>) -> Response {
    if state.store.ping().await {
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "ready",
                "uptime_secs": state.started.elapsed().as_secs(),
            })),
        )
            .into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"status": "unavailable"})),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------------
// OIDC
// ---------------------------------------------------------------------------

async fn list_providers(State(state): State<AppState>) -> Json<Vec<ProviderInfo>> {
    Json(state.oidc.public_providers().await)
}

fn registration_to_device(registration: &DeviceRegistration) -> Result<NewDevice> {
    let name = registration.name.trim();
    if name.is_empty() || name.len() > 120 {
        return Err(AppError::bad_request(
            "invalid_device_name",
            "device name must be 1-120 characters",
        ));
    }
    let proof = b64_decode(&registration.proof_public_key).ok_or_else(|| {
        AppError::bad_request("device_key_invalid", "proof_public_key is not base64")
    })?;
    if proof.len() != 32 {
        return Err(AppError::bad_request(
            "device_key_invalid",
            "proof_public_key must be 32 bytes",
        ));
    }
    let noise = match registration.noise_static_public_key.as_deref() {
        Some(value) => {
            let bytes = b64_decode(value).ok_or_else(|| {
                AppError::bad_request(
                    "device_key_invalid",
                    "noise_static_public_key is not base64",
                )
            })?;
            if bytes.len() != 32 {
                return Err(AppError::bad_request(
                    "device_key_invalid",
                    "noise_static_public_key must be 32 bytes",
                ));
            }
            Some(bytes)
        }
        None => None,
    };
    if registration.kind == h4rn_protocol::ConnectDeviceKind::Daemon && noise.is_none() {
        return Err(AppError::bad_request_hint(
            "noise_key_required",
            "a daemon must register a Noise static public key",
            "Generate an X25519 key on the daemon and include it in the login request.",
        ));
    }
    Ok(NewDevice {
        kind: registration.kind,
        name: name.to_string(),
        proof_public_key: proof,
        noise_static_public_key: noise,
        platform: registration.platform.clone(),
        machine_name: registration
            .machine_name
            .clone()
            .or_else(|| Some(name.to_string())),
    })
}

async fn auth_start(
    State(state): State<AppState>,
    client: ClientIp,
    Path(provider): Path<String>,
    Json(request): Json<LoginStartRequest>,
) -> Result<Json<LoginStartResponse>> {
    state.rate.check(
        "auth",
        &client.0,
        state.config.rate_limits.auth_per_minute,
        Duration::from_secs(60),
    )?;
    if state.config.provider(&provider).is_none() {
        return Err(AppError::not_found(
            "provider_not_found",
            format!("no OIDC provider `{provider}` is configured"),
        ));
    }
    let device = registration_to_device(&request.device)?;
    let flow_state = crate::crypto::random_b64(32);
    let nonce = crate::crypto::random_b64(32);
    let (authorize_url, verifier) = state
        .oidc
        .start_login(&provider, &request.redirect_uri, &flow_state, &nonce)
        .await?;
    state
        .store
        .create_auth_flow(
            &provider,
            &flow_state,
            &nonce,
            &verifier,
            &request.redirect_uri,
            &device,
            state.config.auth_flow_ttl(),
        )
        .await?;
    Ok(Json(LoginStartResponse {
        provider,
        authorize_url,
        state: flow_state,
        expires_in: state.config.auth_flow_ttl_secs,
    }))
}

async fn auth_complete(
    State(state): State<AppState>,
    client: ClientIp,
    Path(provider): Path<String>,
    Json(request): Json<LoginCompleteRequest>,
) -> Result<Json<LoginCompleteResponse>> {
    state.rate.check(
        "auth",
        &client.0,
        state.config.rate_limits.auth_per_minute,
        Duration::from_secs(60),
    )?;
    let flow = state
        .store
        .consume_auth_flow(&provider, &request.state)
        .await?;
    if flow.redirect_uri != request.redirect_uri {
        return Err(AppError::bad_request(
            "redirect_uri_mismatch",
            "redirect_uri does not match the one used to start the flow",
        ));
    }
    let login = state
        .oidc
        .complete_login(&provider, &request.code, &flow.pkce_verifier, &flow.nonce)
        .await?;
    Ok(Json(finish_login(&state, &flow, login).await?))
}

async fn finish_login(
    state: &AppState,
    flow: &AuthFlowRow,
    login: OidcLogin,
) -> Result<LoginCompleteResponse> {
    let kind = h4rn_protocol::ConnectDeviceKind::parse(&flow.device_kind).ok_or_else(|| {
        AppError::internal(format!(
            "stored device kind `{}` is invalid",
            flow.device_kind
        ))
    })?;
    let new_device = NewDevice {
        kind,
        name: flow.device_name.clone(),
        proof_public_key: flow.proof_public_key.clone(),
        noise_static_public_key: flow.noise_static_public_key.clone(),
        platform: flow.platform.clone(),
        machine_name: flow.machine_name.clone(),
    };
    let profile = &login.profile;
    let (user, device) = state
        .store
        .complete_login(
            &login.provider_id,
            &login.issuer,
            &profile.subject,
            profile.email.as_deref(),
            profile.email_verified,
            profile.display_name.as_deref(),
            profile.avatar_url.as_deref(),
            profile.claims.clone(),
            new_device,
        )
        .await?;

    if let Some(refresh) = login.refresh_token.as_deref() {
        state
            .store
            .put_secret(
                user.id,
                "oidc_refresh",
                &login.provider_id,
                refresh,
                &state.envelope,
            )
            .await
            .ok();
    }

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
        .await?;
    state
        .store
        .audit(
            Some(user.id),
            Some(device.id),
            "auth.login",
            Some("device"),
            Some(&device.id.to_string()),
            serde_json::json!({"provider": login.provider_id}),
        )
        .await
        .ok();
    Ok(LoginCompleteResponse {
        tokens: issued.tokens,
        user: user_to_wire(&user),
        device: device.to_wire(),
    })
}

#[derive(Debug, serde::Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn auth_callback(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(query): Query<CallbackQuery>,
) -> Result<Redirect> {
    let flow_state = query.state.as_deref().ok_or_else(|| {
        AppError::bad_request("state_missing", "the provider did not return a state value")
    })?;
    let flow = state.store.consume_auth_flow(&provider, flow_state).await?;

    if let Some(error) = query.error.as_deref() {
        let mut url = url::Url::parse(&flow.redirect_uri).map_err(|_| {
            AppError::bad_request("redirect_invalid", "stored redirect_uri is invalid")
        })?;
        let mut fragment = url::form_urlencoded::Serializer::new(String::new());
        fragment.append_pair("error", error);
        if let Some(description) = query.error_description.as_deref() {
            fragment.append_pair("error_description", description);
        }
        url.set_fragment(Some(&fragment.finish()));
        return Ok(Redirect::to(url.as_str()));
    }

    let code = query.code.as_deref().ok_or_else(|| {
        AppError::bad_request("code_missing", "the provider did not return a code")
    })?;
    let login = state
        .oidc
        .complete_login(&provider, code, &flow.pkce_verifier, &flow.nonce)
        .await?;
    let complete = finish_login(&state, &flow, login).await?;

    let mut url = url::Url::parse(&flow.redirect_uri)
        .map_err(|_| AppError::bad_request("redirect_invalid", "stored redirect_uri is invalid"))?;
    let mut fragment = url::form_urlencoded::Serializer::new(String::new());
    fragment.append_pair("access_token", &complete.tokens.access_token);
    fragment.append_pair("refresh_token", &complete.tokens.refresh_token);
    fragment.append_pair("expires_in", &complete.tokens.expires_in.to_string());
    fragment.append_pair("token_type", &complete.tokens.token_type);
    fragment.append_pair("user_id", &complete.user.id);
    fragment.append_pair("device_id", &complete.device.id);
    url.set_fragment(Some(&fragment.finish()));
    Ok(Redirect::to(url.as_str()))
}

// ---------------------------------------------------------------------------
// Device grant
// ---------------------------------------------------------------------------

async fn device_start(
    State(state): State<AppState>,
    client: ClientIp,
    Json(request): Json<DeviceGrantStartRequest>,
) -> Result<Json<DeviceGrantStartResponse>> {
    state.rate.check(
        "auth",
        &client.0,
        state.config.rate_limits.auth_per_minute,
        Duration::from_secs(60),
    )?;
    let device = registration_to_device(&request.device)?;
    let authorization = state.oidc.start_device_grant(&request.provider).await?;
    let grant = state
        .store
        .create_device_grant(NewDeviceGrant {
            provider_id: &request.provider,
            device_code: &authorization.device_code,
            user_code: &authorization.user_code,
            verification_uri: &authorization.verification_uri,
            verification_uri_complete: authorization.verification_uri_complete.as_deref(),
            interval_secs: authorization.interval as i32,
            expires_in: authorization.expires_in,
            device: &device,
        })
        .await?;
    Ok(Json(DeviceGrantStartResponse {
        grant_id: grant.id.to_string(),
        user_code: authorization.user_code,
        verification_uri: authorization.verification_uri,
        verification_uri_complete: authorization.verification_uri_complete,
        expires_in: authorization.expires_in,
        interval: authorization.interval,
    }))
}

async fn device_poll(
    State(state): State<AppState>,
    client: ClientIp,
    Json(request): Json<DeviceGrantPollRequest>,
) -> Result<Json<DeviceGrantPollResponse>> {
    state.rate.check(
        "auth",
        &client.0,
        state.config.rate_limits.auth_per_minute,
        Duration::from_secs(60),
    )?;
    let grant_id = Uuid::parse_str(&request.grant_id)
        .map_err(|_| AppError::bad_request("grant_invalid", "grant_id is not a UUID"))?;
    let grant = state.store.device_grant(grant_id).await?;

    if grant.status == "approved" {
        return approved_poll(&state, &grant).await;
    }
    if grant.status == "denied" {
        return Ok(Json(poll_response(DeviceGrantStatus::Denied, None)));
    }
    if grant.status == "expired" || grant.expires_at <= Utc::now() {
        state
            .store
            .set_device_grant_status(grant.id, "expired", None, None)
            .await
            .ok();
        return Ok(Json(poll_response(DeviceGrantStatus::Expired, None)));
    }

    // Respect the provider's poll interval without burning the grant.
    if let Some(last) = grant.last_polled_at {
        if Utc::now() - last < chrono::Duration::seconds(grant.interval_secs as i64) {
            return Ok(Json(poll_response(
                DeviceGrantStatus::Pending,
                Some(grant.interval_secs as i64),
            )));
        }
    }

    match state
        .oidc
        .poll_device_grant(&grant.provider_id, &grant.device_code)
        .await?
    {
        DevicePoll::Pending => {
            state.store.note_device_poll(grant.id).await?;
            Ok(Json(poll_response(DeviceGrantStatus::Pending, None)))
        }
        DevicePoll::SlowDown { retry_after } => {
            state.store.note_device_poll(grant.id).await?;
            Ok(Json(poll_response(
                DeviceGrantStatus::SlowDown,
                Some(retry_after),
            )))
        }
        DevicePoll::Denied => {
            state
                .store
                .set_device_grant_status(grant.id, "denied", None, None)
                .await?;
            Ok(Json(poll_response(DeviceGrantStatus::Denied, None)))
        }
        DevicePoll::Expired => {
            state
                .store
                .set_device_grant_status(grant.id, "expired", None, None)
                .await?;
            Ok(Json(poll_response(DeviceGrantStatus::Expired, None)))
        }
        DevicePoll::Retry {
            retry_after,
            reason,
        } => {
            tracing::warn!(grant = %grant.id, %reason, "provider asked us to retry the device poll");
            state.store.note_device_poll(grant.id).await?;
            Ok(Json(poll_response(
                DeviceGrantStatus::SlowDown,
                Some(retry_after),
            )))
        }
        DevicePoll::Approved { login } => {
            let complete = finish_grant(&state, &grant, *login).await?;
            Ok(Json(DeviceGrantPollResponse {
                status: DeviceGrantStatus::Approved,
                tokens: Some(complete.tokens),
                user: Some(complete.user),
                device: Some(complete.device),
                retry_after: None,
            }))
        }
    }
}

async fn finish_grant(
    state: &AppState,
    grant: &DeviceGrantRow,
    login: OidcLogin,
) -> Result<LoginCompleteResponse> {
    let kind = h4rn_protocol::ConnectDeviceKind::parse(&grant.device_kind)
        .ok_or_else(|| AppError::internal("stored device kind is invalid"))?;
    let new_device = NewDevice {
        kind,
        name: grant.device_name.clone(),
        proof_public_key: grant.proof_public_key.clone(),
        noise_static_public_key: grant.noise_static_public_key.clone(),
        platform: grant.platform.clone(),
        machine_name: grant.machine_name.clone(),
    };
    let profile = &login.profile;
    let (user, device) = state
        .store
        .complete_login(
            &login.provider_id,
            &login.issuer,
            &profile.subject,
            profile.email.as_deref(),
            profile.email_verified,
            profile.display_name.as_deref(),
            profile.avatar_url.as_deref(),
            profile.claims.clone(),
            new_device,
        )
        .await?;
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
        .await?;

    let aad = format!("grant:{}", grant.id);
    let sealed = state
        .envelope
        .seal_str(&issued.tokens.refresh_token, aad.as_bytes())?;
    let ciphertext =
        serde_json::to_vec(&sealed).map_err(|_| AppError::internal("seal grant refresh"))?;
    state
        .store
        .approve_device_grant(grant.id, user.id, device.id, issued.session.id, &ciphertext)
        .await?;
    if let Some(refresh) = login.refresh_token.as_deref() {
        state
            .store
            .put_secret(
                user.id,
                "oidc_refresh",
                &login.provider_id,
                refresh,
                &state.envelope,
            )
            .await
            .ok();
    }
    state
        .store
        .audit(
            Some(user.id),
            Some(device.id),
            "auth.login",
            Some("device"),
            Some(&device.id.to_string()),
            serde_json::json!({"provider": login.provider_id, "grant": grant.id.to_string()}),
        )
        .await
        .ok();
    Ok(LoginCompleteResponse {
        tokens: issued.tokens,
        user: user_to_wire(&user),
        device: device.to_wire(),
    })
}

async fn approved_poll(
    state: &AppState,
    grant: &DeviceGrantRow,
) -> Result<Json<DeviceGrantPollResponse>> {
    let session_id = grant.session_id.ok_or_else(|| {
        AppError::internal("approved grant has no session; cannot reissue tokens")
    })?;
    let session = state.store.session(session_id).await?;
    if session.revoked_at.is_some() {
        return Ok(Json(poll_response(DeviceGrantStatus::Denied, None)));
    }
    let sealed = grant
        .refresh_ciphertext
        .as_deref()
        .and_then(|bytes| serde_json::from_slice(bytes).ok())
        .ok_or_else(|| AppError::internal("approved grant has no stored refresh token"))?;
    let aad = format!("grant:{}", grant.id);
    let refresh = state.envelope.open_str(&sealed, aad.as_bytes())?;
    let user_id = grant
        .user_id
        .ok_or_else(|| AppError::internal("approved grant has no user"))?;
    let device_id = grant
        .device_id
        .ok_or_else(|| AppError::internal("approved grant has no device"))?;
    let user = state.store.user(user_id).await?;
    let device = state.store.device(device_id).await?;
    let access = issue_access_token(
        &state.keystore,
        &user_id.to_string(),
        &device_id.to_string(),
        &session.id.to_string(),
        &session.family_id.to_string(),
        &session.access_jti.to_string(),
        state.config.session_ttl_secs,
    )?;
    Ok(Json(DeviceGrantPollResponse {
        status: DeviceGrantStatus::Approved,
        tokens: Some(ConnectSessionTokens {
            access_token: access,
            refresh_token: refresh,
            expires_in: state.config.session_ttl_secs,
            token_type: "Bearer".to_string(),
        }),
        user: Some(user_to_wire(&user)),
        device: Some(device.to_wire()),
        retry_after: None,
    }))
}

fn poll_response(status: DeviceGrantStatus, retry_after: Option<i64>) -> DeviceGrantPollResponse {
    DeviceGrantPollResponse {
        status,
        tokens: None,
        user: None,
        device: None,
        retry_after,
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

async fn refresh(
    State(state): State<AppState>,
    client: ClientIp,
    headers: HeaderMap,
    Json(request): Json<RefreshRequest>,
) -> Result<Json<ConnectSessionTokens>> {
    state.rate.check(
        "auth",
        &client.0,
        state.config.rate_limits.auth_per_minute,
        Duration::from_secs(60),
    )?;
    let user_agent = user_agent(&headers);
    let issued = state
        .store
        .rotate_session(
            &state.keystore,
            &state.config,
            &state.envelope,
            &request.refresh_token,
            user_agent.as_deref(),
            Some(&client.0),
        )
        .await?;
    state
        .store
        .audit(
            Some(issued.session.user_id),
            Some(issued.session.device_id),
            "auth.refreshed",
            Some("session"),
            Some(&issued.session.id.to_string()),
            serde_json::json!({}),
        )
        .await
        .ok();
    Ok(Json(issued.tokens))
}

async fn logout(
    State(state): State<AppState>,
    authed: Authed,
    Json(_request): Json<LogoutRequest>,
) -> Result<StatusCode> {
    state
        .store
        .revoke_family(authed.session.family_id, "logout")
        .await?;
    for provider in &state.config.providers {
        if let Ok(Some(token)) = state
            .store
            .get_secret(
                authed.user.id,
                "oidc_refresh",
                &provider.id,
                &state.envelope,
            )
            .await
        {
            state.oidc.revoke_provider_token(&provider.id, &token).await;
            let _ = state
                .store
                .delete_secret(authed.user.id, "oidc_refresh", &provider.id)
                .await;
        }
    }
    state
        .store
        .audit(
            Some(authed.user.id),
            Some(authed.device.id),
            "auth.logout",
            Some("session"),
            Some(&authed.session.id.to_string()),
            serde_json::json!({}),
        )
        .await
        .ok();
    Ok(StatusCode::NO_CONTENT)
}

async fn me(State(state): State<AppState>, authed: Authed) -> Result<Json<ConnectMeResponse>> {
    let devices = state.store.devices(authed.user.id).await?;
    Ok(Json(ConnectMeResponse {
        user: user_to_wire(&authed.user),
        devices: devices.iter().map(DeviceRow::to_wire).collect(),
    }))
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

async fn list_devices(
    State(state): State<AppState>,
    authed: Authed,
) -> Result<Json<Vec<ConnectDevice>>> {
    let devices = state.store.devices(authed.user.id).await?;
    Ok(Json(devices.iter().map(DeviceRow::to_wire).collect()))
}

async fn rename_device(
    State(state): State<AppState>,
    authed: Authed,
    Path(id): Path<String>,
    Json(request): Json<DeviceRenameRequest>,
) -> Result<Json<ConnectDevice>> {
    let device_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::bad_request("device_invalid", "device id is not a UUID"))?;
    let device = state
        .store
        .rename_device(authed.user.id, device_id, &request.name)
        .await?;
    state
        .store
        .audit(
            Some(authed.user.id),
            Some(authed.device.id),
            "device.renamed",
            Some("device"),
            Some(&device.id.to_string()),
            serde_json::json!({"name": device.name}),
        )
        .await
        .ok();
    Ok(Json(device.to_wire()))
}

async fn revoke_device(
    State(state): State<AppState>,
    authed: Authed,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    let device_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::bad_request("device_invalid", "device id is not a UUID"))?;
    let machines = state.store.revoke_device(authed.user.id, device_id).await?;
    for machine_id in machines {
        state.relay.notify_revoked(machine_id, device_id).await;
    }
    state
        .store
        .audit(
            Some(authed.user.id),
            Some(authed.device.id),
            "device.revoked",
            Some("device"),
            Some(&device_id.to_string()),
            serde_json::json!({}),
        )
        .await
        .ok();
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Machines
// ---------------------------------------------------------------------------

async fn list_machines(
    State(state): State<AppState>,
    authed: Authed,
) -> Result<Json<Vec<MachineSummary>>> {
    let online: HashSet<Uuid> = state.relay.online_machines(authed.user.id).await;
    let machines = state.store.machines(authed.user.id, &online).await?;
    Ok(Json(machines))
}

async fn update_hosts(
    State(state): State<AppState>,
    authed: Authed,
    Path(id): Path<String>,
    Json(request): Json<MachineHostsUpdate>,
) -> Result<Json<Vec<MachineHost>>> {
    let machine_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::bad_request("machine_invalid", "machine id is not a UUID"))?;
    if authed.device.machine_id != Some(machine_id) {
        return Err(AppError::forbidden(
            "hosts_forbidden",
            "only the machine's daemon may report its hosts",
        ));
    }
    let hosts: Vec<(String, h4rn_protocol::MachineHostKind, Option<String>)> = request
        .hosts
        .into_iter()
        .map(|host| {
            let name = host.name.trim();
            (
                name.to_string(),
                host.kind,
                host.distro.filter(|distro| !distro.trim().is_empty()),
            )
        })
        .filter(|(name, _, _)| !name.is_empty())
        .collect();
    let rows = state.store.replace_hosts(machine_id, &hosts).await?;
    state
        .store
        .audit(
            Some(authed.user.id),
            Some(authed.device.id),
            "machine.hosts_updated",
            Some("machine"),
            Some(&machine_id.to_string()),
            serde_json::json!({"hosts": rows.len()}),
        )
        .await
        .ok();
    Ok(Json(rows.iter().map(|row| row.to_wire()).collect()))
}

async fn update_addresses(
    State(state): State<AppState>,
    authed: Authed,
    Path(id): Path<String>,
    Json(request): Json<MachineAddressesUpdate>,
) -> Result<Json<Vec<String>>> {
    let machine_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::bad_request("machine_invalid", "machine id is not a UUID"))?;
    if authed.device.machine_id != Some(machine_id) {
        return Err(AppError::forbidden(
            "addresses_forbidden",
            "only the machine's daemon may report its addresses",
        ));
    }
    let addresses: Vec<String> = request
        .addresses
        .into_iter()
        .map(|address| address.trim().to_string())
        .filter(|address| !address.is_empty())
        .take(32)
        .collect();
    let stored = state
        .store
        .replace_addresses(machine_id, &addresses)
        .await?;
    state
        .store
        .audit(
            Some(authed.user.id),
            Some(authed.device.id),
            "machine.addresses_updated",
            Some("machine"),
            Some(&machine_id.to_string()),
            serde_json::json!({"addresses": stored.len()}),
        )
        .await
        .ok();
    Ok(Json(stored))
}

async fn list_pairings(
    State(state): State<AppState>,
    authed: Authed,
) -> Result<Json<Vec<PairingInfo>>> {
    let now = Utc::now();
    let rows = state.store.pairings(authed.user.id).await?;
    Ok(Json(rows.iter().map(|row| row.to_wire(now)).collect()))
}

async fn create_pairing(
    State(state): State<AppState>,
    client: ClientIp,
    authed: Authed,
    Json(request): Json<PairingCreateRequest>,
) -> Result<Json<PairingCreateResponse>> {
    state.rate.check(
        "pairing",
        &client.0,
        state.config.rate_limits.pairing_per_hour,
        Duration::from_secs(60 * 60),
    )?;
    let machine_id = Uuid::parse_str(&request.machine_id)
        .map_err(|_| AppError::bad_request("machine_invalid", "machine_id is not a UUID"))?;
    let response = pairing::create(&state, &authed, machine_id).await?;
    Ok(Json(response))
}

async fn claim_pairing(
    State(state): State<AppState>,
    client: ClientIp,
    authed: Authed,
    Json(request): Json<PairingClaimRequest>,
) -> Result<Json<PairingClaimResponse>> {
    state.rate.check(
        "pairing",
        &client.0,
        state.config.rate_limits.pairing_per_hour,
        Duration::from_secs(60 * 60),
    )?;
    let response = pairing::claim(&state, &authed, request).await?;
    Ok(Json(response))
}

async fn revoke_pairing(
    State(state): State<AppState>,
    authed: Authed,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    let pairing_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::bad_request("pairing_invalid", "pairing id is not a UUID"))?;
    state
        .store
        .revoke_pairing(authed.user.id, pairing_id)
        .await?;
    state
        .store
        .audit(
            Some(authed.user.id),
            Some(authed.device.id),
            "pairing.revoked",
            Some("pairing"),
            Some(&pairing_id.to_string()),
            serde_json::json!({}),
        )
        .await
        .ok();
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct AuditQuery {
    before_id: Option<i64>,
    limit: Option<i64>,
}

async fn audit(
    State(state): State<AppState>,
    authed: Authed,
    Query(query): Query<AuditQuery>,
) -> Result<Json<AuditPage>> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let rows = state
        .store
        .audit_page(authed.user.id, query.before_id, limit)
        .await?;
    let next_before_id = rows.last().map(|row| row.id);
    Ok(Json(AuditPage {
        entries: rows
            .into_iter()
            .map(|row| AuditEntry {
                id: row.id,
                actor_device_id: row.actor_device_id.map(|id| id.to_string()),
                action: row.action,
                target_type: row.target_type,
                target_id: row.target_id,
                metadata: row.metadata,
                at: row.at,
            })
            .collect(),
        next_before_id,
    }))
}

async fn ingest_audit(
    State(state): State<AppState>,
    authed: Authed,
    Json(request): Json<AuditIngestRequest>,
) -> Result<StatusCode> {
    let action = request.action.trim();
    if action.is_empty() || action.len() > 120 {
        return Err(AppError::bad_request(
            "audit_action_invalid",
            "audit action must be 1-120 characters",
        ));
    }
    let target_type = request
        .target_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(60).collect::<String>());
    let target_id = request
        .target_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(200).collect::<String>());
    let metadata = if request.metadata.is_null() {
        serde_json::json!({})
    } else {
        request.metadata
    };
    state
        .store
        .audit(
            Some(authed.user.id),
            Some(authed.device.id),
            action,
            target_type.as_deref(),
            target_id.as_deref(),
            metadata,
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

/// The plaintext half of a sealed bundle row.
///
/// The manifest travels with the files so a pull can rebuild a complete
/// [`SkillSyncEntry`] without a second table. Tombstones seal a manifest with
/// an empty file list, which keeps the shape uniform.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SkillPayload {
    manifest: SkillManifest,
    files: Vec<SkillBundleFile>,
}

/// Publishes local skill versions and reads back the server's decisions.
async fn skills_push(
    State(state): State<AppState>,
    authed: Authed,
    Json(request): Json<SkillSyncPush>,
) -> Result<Json<SkillSyncPushResponse>> {
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();

    for entry in request.entries {
        validate_skill_entry(&entry)?;
        let payload = if entry.deleted {
            SkillPayload {
                manifest: entry.manifest.clone(),
                files: Vec::new(),
            }
        } else {
            let bundle = SkillBundle::from_wire(entry.manifest.clone(), entry.files.clone())
                .map_err(|error| {
                    AppError::bad_request("skill_bundle_invalid", error.to_string())
                })?;
            if bundle.version() != entry.version {
                return Err(AppError::bad_request(
                    "skill_version_mismatch",
                    format!(
                        "the bundle hashes to {}, not {}",
                        bundle.version(),
                        entry.version
                    ),
                ));
            }
            SkillPayload {
                manifest: bundle.manifest.clone(),
                files: bundle.wire_files(),
            }
        };

        if entry.name != payload.manifest.name || entry.description != payload.manifest.description
        {
            return Err(AppError::bad_request(
                "skill_metadata_mismatch",
                "the entry metadata does not match the signed manifest",
            ));
        }

        let vector_json = serde_json::to_string(&entry.vector)
            .map_err(|_| AppError::internal("serialise skill vector"))?;
        let etag = skill_etag(
            &entry.skill_id,
            &entry.version,
            &vector_json,
            entry.updated_at,
        );
        let existing = state
            .store
            .skill_entry(authed.user.id, &entry.skill_id)
            .await?;

        if let Some(row) = &existing {
            let existing_vector: SkillVersionVector =
                serde_json::from_str(&row.vector_json).unwrap_or_default();
            let same_content = row.version == entry.version && row.deleted == entry.deleted;
            let etag_mismatch = entry
                .expected_etag
                .as_deref()
                .is_some_and(|expected| expected != row.etag);
            let stale = existing_vector.dominates(&entry.vector);
            let concurrent = existing_vector.concurrent(&entry.vector);

            if etag_mismatch {
                state
                    .store
                    .record_skill_conflict(
                        authed.user.id,
                        &entry.skill_id,
                        Some(&row.version),
                        &entry.version,
                        "the client updated a version the server has already replaced",
                    )
                    .await?;
                rejected.push(SkillSyncAck {
                    skill_id: entry.skill_id.clone(),
                    version: entry.version.clone(),
                    etag: Some(row.etag.clone()),
                    accepted: false,
                    reason: Some("etag mismatch".into()),
                });
                continue;
            }
            if same_content && !entry.vector.dominates(&existing_vector) {
                accepted.push(SkillSyncAck {
                    skill_id: entry.skill_id.clone(),
                    version: row.version.clone(),
                    etag: Some(row.etag.clone()),
                    accepted: true,
                    reason: None,
                });
                continue;
            }
            if stale {
                state
                    .store
                    .record_skill_conflict(
                        authed.user.id,
                        &entry.skill_id,
                        Some(&row.version),
                        &entry.version,
                        "the server already has a newer version",
                    )
                    .await?;
                rejected.push(SkillSyncAck {
                    skill_id: entry.skill_id.clone(),
                    version: entry.version.clone(),
                    etag: Some(row.etag.clone()),
                    accepted: false,
                    reason: Some("the remote vector dominates this version".into()),
                });
                continue;
            }
            if concurrent {
                state
                    .store
                    .record_skill_conflict(
                        authed.user.id,
                        &entry.skill_id,
                        Some(&row.version),
                        &entry.version,
                        "both machines changed the skill at the same time",
                    )
                    .await?;
                if entry.updated_at < row.updated_at {
                    rejected.push(SkillSyncAck {
                        skill_id: entry.skill_id.clone(),
                        version: entry.version.clone(),
                        etag: Some(row.etag.clone()),
                        accepted: false,
                        reason: Some("a concurrent version has a newer timestamp".into()),
                    });
                    continue;
                }
            }
        }

        let plaintext = serde_json::to_vec(&payload)
            .map_err(|error| AppError::internal(format!("serialise skill payload: {error}")))?;
        let aad = skill_aad(authed.user.id, &entry.skill_id, &entry.version);
        let sealed = state.envelope.seal(&plaintext, &aad)?;
        state
            .store
            .upsert_skill_entry(
                authed.user.id,
                Some(authed.device.id),
                &NewSkillEntry {
                    skill_id: &entry.skill_id,
                    name: &entry.name,
                    description: &entry.description,
                    version: &entry.version,
                    vector_json: &vector_json,
                    etag: &etag,
                    deleted: entry.deleted,
                    sealed,
                    updated_at: entry.updated_at,
                },
            )
            .await?;
        accepted.push(SkillSyncAck {
            skill_id: entry.skill_id,
            version: entry.version,
            etag: Some(etag),
            accepted: true,
            reason: None,
        });
    }

    let response = SkillSyncPushResponse {
        accepted,
        rejected,
        conflicts: user_conflicts(&state, authed.user.id).await?,
        etag: cursor(&state, authed.user.id).await?,
        server_time: Utc::now(),
    };
    Ok(Json(response))
}

/// Reads entries changed since a cursor.
async fn skills_pull(
    State(state): State<AppState>,
    authed: Authed,
    Json(request): Json<SkillSyncPull>,
) -> Result<Json<SkillSyncPullResponse>> {
    let since = match request.since.as_deref() {
        None | Some("") => None,
        Some(value) => Some(
            chrono::DateTime::parse_from_rfc3339(value)
                .map_err(|_| {
                    AppError::bad_request("skill_cursor_invalid", "the sync cursor is not RFC 3339")
                })?
                .with_timezone(&Utc),
        ),
    };
    let rows = state
        .store
        .skill_entries_since(authed.user.id, since)
        .await?;
    let mut entries = Vec::with_capacity(rows.len());
    for row in rows {
        let aad = skill_aad(row.user_id, &row.skill_id, &row.version);
        let sealed = SealedSecret {
            dek_nonce: row.dek_nonce,
            wrapped_dek: row.wrapped_dek,
            nonce: row.nonce,
            ciphertext: row.ciphertext,
        };
        let plaintext = state.envelope.open(&sealed, &aad)?;
        let payload: SkillPayload = serde_json::from_slice(&plaintext)
            .map_err(|error| AppError::internal(format!("decode skill payload: {error}")))?;
        let vector: SkillVersionVector = serde_json::from_str(&row.vector_json).unwrap_or_default();
        entries.push(SkillSyncEntry {
            skill_id: row.skill_id,
            name: row.name,
            description: row.description,
            version: row.version,
            manifest: payload.manifest,
            files: if row.deleted {
                Vec::new()
            } else {
                payload.files
            },
            vector,
            updated_at: row.updated_at,
            deleted: row.deleted,
            expected_etag: Some(row.etag),
        });
    }

    let response = SkillSyncPullResponse {
        entries,
        conflicts: user_conflicts(&state, authed.user.id).await?,
        etag: cursor(&state, authed.user.id).await?,
        server_time: Utc::now(),
    };
    Ok(Json(response))
}

/// Rejects metadata that is obviously malformed before doing any work.
fn validate_skill_entry(entry: &SkillSyncEntry) -> Result<()> {
    let id = entry.skill_id.trim();
    if id.is_empty() || id.len() > 128 {
        return Err(AppError::bad_request(
            "skill_id_invalid",
            "a skill id must be 1-128 characters",
        ));
    }
    if entry.name.trim().is_empty() || entry.name.len() > 200 {
        return Err(AppError::bad_request(
            "skill_name_invalid",
            "a skill name must be 1-200 characters",
        ));
    }
    if entry.version.len() != 64 || !entry.version.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::bad_request(
            "skill_version_invalid",
            "a skill version must be a BLAKE3 hex digest",
        ));
    }
    Ok(())
}

fn skill_aad(user_id: Uuid, skill_id: &str, version: &str) -> Vec<u8> {
    format!("h4rn-connect/skills/{user_id}/{skill_id}/{version}").into_bytes()
}

fn skill_etag(
    skill_id: &str,
    version: &str,
    vector_json: &str,
    updated_at: chrono::DateTime<Utc>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(skill_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(version.as_bytes());
    hasher.update(b"\0");
    hasher.update(vector_json.as_bytes());
    hasher.update(b"\0");
    hasher.update(updated_at.to_rfc3339().as_bytes());
    hasher.finalize().to_hex().to_string()
}

async fn user_conflicts(state: &AppState, user_id: Uuid) -> Result<Vec<SkillSyncConflict>> {
    Ok(state
        .store
        .skill_conflicts(user_id)
        .await?
        .into_iter()
        .map(|row| SkillSyncConflict {
            skill_id: row.skill_id,
            local_version: row.local_version,
            remote_version: row.remote_version,
            reason: row.reason,
            at: row.created_at,
        })
        .collect())
}

async fn cursor(state: &AppState, user_id: Uuid) -> Result<Option<String>> {
    Ok(state
        .store
        .skill_cursor(user_id)
        .await?
        .map(|time| time.to_rfc3339()))
}

// ---------------------------------------------------------------------------
// Usage ledger sync
// ---------------------------------------------------------------------------

async fn ingest_usage(
    State(state): State<AppState>,
    authed: Authed,
    Json(request): Json<UsageSyncRequest>,
) -> Result<Json<UsageSyncResponse>> {
    if request.rows.len() > 500 {
        return Err(AppError::bad_request(
            "usage_batch_too_large",
            "a usage batch may contain at most 500 rows",
        ));
    }
    let response = state
        .store
        .sync_usage(authed.user.id, authed.device.machine_id, &request.rows)
        .await?;
    Ok(Json(response))
}

#[derive(Debug, serde::Deserialize)]
struct UsageAggregateQuery {
    scope: Option<UsageScope>,
    scope_id: Option<String>,
    since: Option<chrono::DateTime<Utc>>,
    until: Option<chrono::DateTime<Utc>>,
    group_by: Option<UsageGroupBy>,
    bucket: Option<UsageBucket>,
}

async fn usage_aggregate(
    State(state): State<AppState>,
    authed: Authed,
    Query(query): Query<UsageAggregateQuery>,
) -> Result<Json<UsageReport>> {
    let scope = query.scope.unwrap_or(UsageScope::Global);
    if matches!(scope, UsageScope::Thread | UsageScope::Workspace)
        && query.scope_id.as_deref().is_none_or(str::is_empty)
    {
        return Err(AppError::bad_request(
            "usage_scope_id_missing",
            "a thread or workspace usage query needs scope_id",
        ));
    }
    let report = state
        .store
        .aggregate_usage(
            authed.user.id,
            scope,
            query.scope_id.as_deref(),
            query.since,
            query.until,
            query.group_by.unwrap_or(UsageGroupBy::None),
            query.bucket,
        )
        .await?;
    Ok(Json(report))
}

// ---------------------------------------------------------------------------
// Relay
// ---------------------------------------------------------------------------

async fn relay_upgrade(
    State(state): State<AppState>,
    client: ClientIp,
    query: Query<crate::relay::RelayAuthQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    crate::relay::upgrade(state, client, query, headers, ws).await
}
