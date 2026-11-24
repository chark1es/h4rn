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
    GuestHttpRequest, LoginCompleteRequest, LoginCompleteResponse, LoginStartRequest,
    LoginStartResponse, LogoutRequest, MachineAddressesUpdate, MachineHost, MachineHostsUpdate,
    MachineSummary, PairScope, PairingClaimCodeRequest, PairingClaimRequest, PairingClaimResponse,
    PairingCreateRequest, PairingCreateResponse, PairingInfo, ProviderInfo, RefreshRequest,
    RelayFrame, RelayFrameKind, RelayServerEvent, UsageBucket, UsageGroupBy, UsageReport,
    UsageScope, UsageSyncRequest, UsageSyncResponse,
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
        .route("/v1/pairings/claim-code", post(claim_pairing_code))
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
        .merge(
            Router::new()
                // Connect serves the fixed guest page itself; only JSON and
                // media ever come back from a daemon, so daemon HTML never
                // runs on Connect's origin.
                .route("/guest/{machine_id}/{token}", get(guest_page))
                .route(
                    "/guest/{machine_id}/{token}/state",
                    get(guest_state_proxy),
                )
                .route(
                    "/guest/{machine_id}/{token}/state/{since}",
                    get(guest_state_since_proxy),
                )
                .route(
                    "/guest/{machine_id}/{token}/messages",
                    post(guest_message_proxy),
                )
                .route(
                    "/guest/{machine_id}/{token}/blob/{id}",
                    get(guest_blob_proxy),
                )
                .layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .merge(api)
        // A guest path carries a bearer token, so the span names the route
        // template instead of the raw URI.
        .layer(TraceLayer::new_for_http().make_span_with(|request: &Request| {
            tracing::info_span!(
                "request",
                method = %request.method(),
                path = %route_template(request.uri().path()),
            )
        }));

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
            ])
            // The machine list's filter marker tells the client whether an
            // older relay ignored `?linked=true`; a browser cannot read it
            // without being allowed to see it cross-origin.
            .expose_headers([HeaderName::from_static(MACHINES_FILTER_HEADER)]),
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
    /// Scopes the device holds right now, read from the database rather than
    /// the token so a re-pairing or revocation takes effect immediately.
    pub scopes: Vec<PairScope>,
}

impl Authed {
    /// Fails with `scope_required` unless the device holds `scope`.
    pub fn require(&self, scope: PairScope) -> Result<()> {
        if self.scopes.contains(&scope) {
            Ok(())
        } else {
            Err(AppError::forbidden_hint(
                "scope_required",
                format!(
                    "this device was not paired with the `{}` scope",
                    scope.as_str()
                ),
                "Pair the device again and enable that permission.",
            ))
        }
    }
}

impl Authed {
    /// Fails with `scope_required` unless the device holds `scope` *on this
    /// machine*. Machine-scoped actions use this instead of [`Self::require`],
    /// which checks the union across every machine.
    pub async fn require_on(
        &self,
        state: &AppState,
        machine_id: Uuid,
        scope: PairScope,
    ) -> Result<()> {
        let scopes = state
            .store
            .machine_scopes(machine_id, self.device.id)
            .await?;
        if scopes.contains(&scope) {
            Ok(())
        } else {
            Err(AppError::forbidden_hint(
                "scope_required",
                format!(
                    "this device was not paired with the `{}` scope on this machine",
                    scope.as_str()
                ),
                "Pair the device again and enable that permission for this machine.",
            ))
        }
    }

    /// Authorises an action on another device: admin on every machine that
    /// device belongs to, or the account-wide default when it belongs to none.
    pub async fn require_admin_over_device(&self, state: &AppState, target: Uuid) -> Result<()> {
        let machines = state.store.device_machines(target).await?;
        if machines.is_empty() {
            return self.require(PairScope::Admin);
        }
        for machine_id in machines {
            self.require_on(state, machine_id, PairScope::Admin).await?;
        }
        Ok(())
    }
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
        let scopes = state.store.device_scopes(device.id).await?;
        Ok(Authed {
            claims,
            user,
            device,
            session,
            scopes,
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
        .complete_login(
            &provider,
            &request.code,
            &flow.pkce_verifier,
            &flow.nonce,
            &flow.redirect_uri,
        )
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
        .complete_login(
            &provider,
            code,
            &flow.pkce_verifier,
            &flow.nonce,
            &flow.redirect_uri,
        )
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
        &state.store.token_scopes(device_id).await?,
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
    let devices = device_wires(&state, authed.user.id).await?;
    Ok(Json(ConnectMeResponse {
        user: user_to_wire(&authed.user),
        devices,
    }))
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

/// The account's devices, each carrying the scopes it holds right now.
async fn device_wires(state: &AppState, user_id: Uuid) -> Result<Vec<ConnectDevice>> {
    let rows = state.store.devices(user_id).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut wire = row.to_wire();
        wire.scopes = state.store.device_scopes(row.id).await?;
        out.push(wire);
    }
    Ok(out)
}

async fn list_devices(
    State(state): State<AppState>,
    authed: Authed,
) -> Result<Json<Vec<ConnectDevice>>> {
    Ok(Json(device_wires(&state, authed.user.id).await?))
}

async fn rename_device(
    State(state): State<AppState>,
    authed: Authed,
    Path(id): Path<String>,
    Json(request): Json<DeviceRenameRequest>,
) -> Result<Json<ConnectDevice>> {
    let device_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::bad_request("device_invalid", "device id is not a UUID"))?;
    if device_id != authed.device.id {
        authed.require_admin_over_device(&state, device_id).await?;
    }
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
    if device_id != authed.device.id {
        authed.require_admin_over_device(&state, device_id).await?;
    }
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

#[derive(Debug, Default, serde::Deserialize)]
struct MachineListQuery {
    /// Only machines this device is linked to, or runs.
    #[serde(default)]
    linked: bool,
}

/// Response header naming the filter a machine list was served with.
///
/// Older relays ignore `?linked=true` and answer with every machine on the
/// account. A client cannot tell that answer from a filtered one by the body
/// alone, so a relay that honours the filter says so; its absence means the
/// list is the whole account, not this device's links.
const MACHINES_FILTER_HEADER: &str = "x-h4rn-machine-filter";

fn machine_list_headers(linked: bool) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if linked {
        headers.insert(
            HeaderName::from_static(MACHINES_FILTER_HEADER),
            HeaderValue::from_static("linked"),
        );
    }
    headers
}

async fn list_machines(
    State(state): State<AppState>,
    authed: Authed,
    Query(query): Query<MachineListQuery>,
) -> Result<(HeaderMap, Json<Vec<MachineSummary>>)> {
    let online: HashSet<Uuid> = state.relay.online_machines(authed.user.id).await;
    let mut machines = state.store.machines(authed.user.id, &online).await?;
    if query.linked {
        // A device may see every machine on the account, but only the ones it
        // is linked to (or runs) are addressable for presence and switching.
        let linked: HashSet<Uuid> = state
            .store
            .device_machines(authed.device.id)
            .await?
            .into_iter()
            .collect();
        machines
            .retain(|machine| Uuid::parse_str(&machine.id).is_ok_and(|id| linked.contains(&id)));
    }
    Ok((machine_list_headers(query.linked), Json(machines)))
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
    let response = pairing::create(&state, &authed, machine_id, request.scopes).await?;
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

async fn claim_pairing_code(
    State(state): State<AppState>,
    client: ClientIp,
    authed: Authed,
    Json(request): Json<PairingClaimCodeRequest>,
) -> Result<Json<PairingClaimResponse>> {
    // Every attempt is charged, hit or miss, against the user and the address:
    // the code is 40 bits and the limit is the brake on guessing it.
    let limit = state.config.rate_limits.pairing_code_per_hour;
    let window = Duration::from_secs(60 * 60);
    state.rate.check(
        "pairing_code_user",
        &authed.user.id.to_string(),
        limit,
        window,
    )?;
    state
        .rate
        .check("pairing_code_ip", &client.0, limit, window)?;
    let response = pairing::claim_by_code(&state, &authed, request).await?;
    Ok(Json(response))
}

async fn revoke_pairing(
    State(state): State<AppState>,
    authed: Authed,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    let pairing_id = Uuid::parse_str(&id)
        .map_err(|_| AppError::bad_request("pairing_invalid", "pairing id is not a UUID"))?;
    let machine_id = state
        .store
        .pairing_machine(authed.user.id, pairing_id)
        .await?;
    authed
        .require_on(&state, machine_id, PairScope::Admin)
        .await?;
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
    // The cursor is an opaque per-user sequence. A client that still holds an
    // RFC 3339 cursor from before the sequence existed cannot be mapped onto
    // the new order, so treat anything non-numeric as "no cursor" and replay
    // the full set instead of skipping entries.
    let since = match request.since.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(value) => value.parse::<i64>().ok(),
    };
    let (rows, pull_cursor) = state.store.skill_pull(authed.user.id, since).await?;
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
        etag: pull_cursor.map(|seq| seq.to_string()),
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
        .map(|seq| seq.to_string()))
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
// Guest links (ADR-044)
// ---------------------------------------------------------------------------

/// The fixed guest page, served from Connect's own origin.
///
/// Connect never relays a daemon's HTML: the page here is static, admitted by
/// a hash-based CSP, and fetches only JSON through the relayed routes below.
async fn guest_page(
    State(state): State<AppState>,
    client: ClientIp,
    Path((machine_id, token)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response> {
    let Ok(machine_id) = Uuid::parse_str(&machine_id) else {
        return Ok((StatusCode::NOT_FOUND, "Not found.").into_response());
    };
    if token.is_empty()
        || token.len() > 256
        || !token.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
    {
        return Ok((StatusCode::NOT_FOUND, "Not found.").into_response());
    }
    state.rate.check(
        "guest_page",
        &client.0,
        state.config.rate_limits.per_ip_per_minute,
        Duration::from_secs(60),
    )?;
    // Ask the daemon for the short-lived page-load cookie and the link state
    // it rendered. A crawler that fetches the page only creates an unclaimed
    // cookie; it never binds the link. The browser's own binding cookie goes
    // along, so a reload renders this browser's state rather than "unbound";
    // a page GET never claims the link either way.
    let cookie = guest_binding_cookie(&headers);
    let (cookie, state) = pending_page_load(&state, machine_id, &token, cookie.as_deref()).await;
    Ok(guest_page_response(cookie, state))
}

/// The value of the browser's `h4rn_guest` cookie, if it has one.
///
/// Only the guest cookie is forwarded to the daemon; every other cookie in the
/// request stays on Connect's origin. One parser serves both the page request
/// and the relayed requests, so the cookie's name has a single source.
///
/// The first pair wins: browsers send the cookie with the longest matching
/// path first (RFC 6265 §5.4), and Connect scopes the binding cookie to the
/// link's own path, so a broader duplicate never replaces it.
fn guest_cookie_value(headers: &HeaderMap) -> Option<String> {
    for header in headers.get_all(axum::http::header::COOKIE) {
        let Ok(header) = header.to_str() else {
            continue;
        };
        for pair in header.split(';') {
            let Some((name, binding)) = pair.trim().split_once('=') else {
                continue;
            };
            if name == h4rn_protocol::GUEST_COOKIE {
                return Some(binding.to_owned());
            }
        }
    }
    None
}

/// The browser's guest binding cookie, if it has a value.
fn guest_binding_cookie(headers: &HeaderMap) -> Option<String> {
    guest_cookie_value(headers).filter(|binding| !binding.is_empty())
}

/// Fetches the page-load binding cookie and rendered state from a machine's
/// daemon.
///
/// Only the daemon knows which cookie is pending and what state the link is
/// in; Connect keeps the Set-Cookie value and rebuilds it for its own path, and
/// passes the state through so an expired link opens on its notice instead of
/// briefly showing a composer that cannot submit. Connect never returns the
/// daemon's HTML.
async fn pending_page_load(
    state: &AppState,
    machine_id: Uuid,
    token: &str,
    binding: Option<&str>,
) -> (Option<String>, &'static str) {
    let Some(answer) = ask_daemon_for_page(state, machine_id, token, binding).await else {
        return (None, "active");
    };
    let page_state = page_state_from_answer(&answer);
    let cookie = answer.set_cookie.clone().or_else(|| {
        answer
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, value)| value.clone())
    });
    let cookie = cookie.and_then(|cookie| {
        rebuild_guest_cookie(
            &cookie,
            &format!("/guest/{machine_id}/{token}"),
            state.config.public_base_url.starts_with("https://"),
        )
    });
    (cookie, page_state)
}

/// Asks one machine's daemon for its guest page.
///
/// The answer's HTML is only read for its rendered state; the cookie is
/// rebuilt for Connect's own path and the HTML itself is discarded.
async fn ask_daemon_for_page(
    state: &AppState,
    machine_id: Uuid,
    token: &str,
    binding: Option<&str>,
) -> Option<h4rn_protocol::GuestHttpResponse> {
    let machine = state.store.machine(machine_id).await.ok()?;
    let daemon = machine.daemon_device_id?;
    if !state.relay.online(daemon).await {
        return None;
    }
    let request_id = Uuid::now_v7().to_string();
    let request = page_request(&request_id, machine_id, token, binding);
    let payload = serde_json::to_vec(&request).ok()?;
    let waiter = state.relay.expect_guest(&request_id, daemon).await;
    let frame = RelayFrame {
        conn_id: format!("guest-{request_id}"),
        kind: RelayFrameKind::GuestHttp,
        payload,
    };
    if state
        .relay
        .send_wait(
            daemon,
            RelayServerEvent::Frame { frame },
            Duration::from_secs(10),
        )
        .await
        .is_err()
    {
        state.relay.forget_guest(&request_id).await;
        return None;
    }
    match tokio::time::timeout(Duration::from_secs(10), waiter).await {
        Ok(Ok(answer)) => Some(answer),
        _ => {
            state.relay.forget_guest(&request_id).await;
            None
        }
    }
}

/// The page request Connect sends one daemon.
///
/// The browser's own binding cookie travels with it when the browser has one,
/// so the daemon renders the state that browser is in; without one it renders
/// the unclaimed page it always did.
fn page_request(
    request_id: &str,
    machine_id: Uuid,
    token: &str,
    binding: Option<&str>,
) -> GuestHttpRequest {
    let mut headers = vec![("accept".to_string(), "text/html".to_string())];
    if let Some(binding) = binding {
        headers.push((
            "cookie".to_string(),
            format!("{}={binding}", h4rn_protocol::GUEST_COOKIE),
        ));
    }
    GuestHttpRequest {
        request_id: request_id.to_string(),
        method: "GET".to_string(),
        path: format!("/guest/{token}"),
        public_prefix: Some(format!("/guest/{machine_id}/{token}")),
        chunked: false,
        headers,
        body: Vec::new(),
    }
}

/// The link state one daemon page answer names.
///
/// A 410 means the daemon no longer knows the token: the link was stopped or
/// replaced. That answer's body is a plain sentence, not the page, so the
/// state comes from the status code.
fn page_state_from_answer(answer: &h4rn_protocol::GuestHttpResponse) -> &'static str {
    if answer.status == StatusCode::GONE.as_u16() {
        return "revoked";
    }
    guest_page_state(&answer.body).unwrap_or("active")
}

/// The link state the daemon rendered into its page.
///
/// Connect copies only a known state token, never the daemon's HTML: an
/// unknown or injected value falls back to the active page and is then
/// corrected by the first `/state` answer.
fn guest_page_state(html: &[u8]) -> Option<&'static str> {
    let html = std::str::from_utf8(html).ok()?;
    // Only the body element's own attribute counts: the page's script also
    // reads `dataset.state`, and a stray string elsewhere must not name it.
    let (_, body) = html.split_once("<body")?;
    let marker = "data-state=\"";
    let start = body.find(marker)? + marker.len();
    let end = start + body[start..].find('"')?;
    match &body[start..end] {
        "active" => Some("active"),
        "expired" => Some("expired"),
        "revoked" => Some("revoked"),
        "disabled" => Some("disabled"),
        "unbound" => Some("unbound"),
        _ => None,
    }
}

/// The fixed page response, with its hash-based CSP.
fn guest_page_response(cookie: Option<String>, state: &str) -> Response {
    let mut response = Response::new(axum::body::Body::from(h4rn_protocol::guest_page_html(
        state,
    )));
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_str(&h4rn_protocol::guest_page_csp())
            .unwrap_or_else(|_| HeaderValue::from_static("default-src 'none'")),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    if let Some(cookie) = cookie
        && let Ok(value) = HeaderValue::from_str(&cookie)
    {
        headers.insert(axum::http::header::SET_COOKIE, value);
    }
    response
}

/// How long a chunked upload waits for the daemon's go-ahead.
///
/// A daemon from before the admission frame never sends one; it bounds the
/// buffered bytes itself, so a short wait keeps that pairing working. A current
/// daemon announces [`h4rn_protocol::RELAY_FEATURE_GUEST_ADMISSION`], and a
/// wait past this bound is an error rather than permission to upload.
const GUEST_ADMISSION_WAIT: Duration = Duration::from_secs(2);

/// Forwards one guest HTTP exchange to the machine's daemon.
///
/// The route is unauthenticated: the guest token in the path is the only
/// credential and only the daemon can check it. Connect streams the body in
/// bounded chunks and rebuilds the answer's headers, so neither a large upload
/// nor a daemon header can reach the browser or the shared relay socket
/// unchecked.
async fn guest_state_proxy(
    State(state): State<AppState>,
    client: ClientIp,
    Path((machine_id, token)): Path<(String, String)>,
    method: axum::http::Method,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: HeaderMap,
) -> Result<Response> {
    guest_proxy(
        state,
        client,
        machine_id,
        token,
        "state".to_owned(),
        method,
        uri,
        headers,
        axum::body::Body::empty(),
    )
    .await
}

async fn guest_state_since_proxy(
    State(state): State<AppState>,
    client: ClientIp,
    Path((machine_id, token, since)): Path<(String, String, String)>,
    method: axum::http::Method,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: HeaderMap,
) -> Result<Response> {
    guest_proxy(
        state,
        client,
        machine_id,
        token,
        format!("state/{since}"),
        method,
        uri,
        headers,
        axum::body::Body::empty(),
    )
    .await
}

async fn guest_message_proxy(
    State(state): State<AppState>,
    client: ClientIp,
    Path((machine_id, token)): Path<(String, String)>,
    method: axum::http::Method,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<Response> {
    guest_proxy(
        state,
        client,
        machine_id,
        token,
        "messages".to_owned(),
        method,
        uri,
        headers,
        body,
    )
    .await
}

async fn guest_blob_proxy(
    State(state): State<AppState>,
    client: ClientIp,
    Path((machine_id, token, id)): Path<(String, String, String)>,
    method: axum::http::Method,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: HeaderMap,
) -> Result<Response> {
    guest_proxy(
        state,
        client,
        machine_id,
        token,
        format!("blob/{id}"),
        method,
        uri,
        headers,
        axum::body::Body::empty(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn guest_proxy(
    state: AppState,
    client: ClientIp,
    machine_id: String,
    token: String,
    rest: String,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<Response> {
    state.rate.check(
        "guest_http",
        &client.0,
        state.config.rate_limits.per_ip_per_minute,
        Duration::from_secs(60),
    )?;
    let request_id = Uuid::now_v7().to_string();
    // Any failure after the waiter is registered must forget it, and so must
    // a browser that goes away while its request is still running: a dropped
    // handler future never reaches the cleanup below.
    let mut cleanup = GuestExchangeGuard::new(Arc::clone(&state.relay), request_id.clone());
    let outcome = guest_proxy_exchange(
        &state,
        machine_id,
        token,
        rest,
        method,
        uri,
        headers,
        body,
        &request_id,
    )
    .await;
    cleanup.disarm();
    discard_failed_exchange(&state.relay, &request_id, outcome).await
}

/// Forgets a guest exchange when its proxy future ends for any reason.
///
/// The relay keeps a waiter and any half-arrived answer under the request id.
/// A failed exchange cleans up when it returns, but a cancelled HTTP request
/// (the browser disconnects mid-answer) drops the future instead of returning,
/// so cleanup is tied to the guard's lifetime. `forget_guest` is a no-op once
/// the answer was delivered, so a successful exchange needs no special case.
struct GuestExchangeGuard {
    relay: Arc<RelayHub>,
    request_id: Option<String>,
}

impl GuestExchangeGuard {
    fn new(relay: Arc<RelayHub>, request_id: String) -> Self {
        Self {
            relay,
            request_id: Some(request_id),
        }
    }

    /// Keeps the exchange; its answer (or failure) already cleaned it up.
    fn disarm(&mut self) {
        self.request_id = None;
    }
}

impl Drop for GuestExchangeGuard {
    fn drop(&mut self) {
        let Some(request_id) = self.request_id.take() else {
            return;
        };
        let relay = Arc::clone(&self.relay);
        // Drop cannot await. The cleanup task owns both maps under one lock,
        // so it removes the waiter and the partial body together.
        tokio::spawn(async move {
            relay.forget_guest(&request_id).await;
        });
    }
}

/// Forgets a guest waiter when its exchange failed, and passes the result on.
async fn discard_failed_exchange<T>(
    relay: &crate::relay::RelayHub,
    request_id: &str,
    outcome: Result<T>,
) -> Result<T> {
    if outcome.is_err() {
        relay.forget_guest(request_id).await;
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn guest_proxy_exchange(
    state: &AppState,
    machine_id: String,
    token: String,
    rest: String,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: axum::body::Body,
    request_id: &str,
) -> Result<Response> {
    let machine_id = Uuid::parse_str(&machine_id)
        .map_err(|_| AppError::bad_request("machine_invalid", "machine_id is not a UUID"))?;
    if token.is_empty()
        || !token.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
    {
        return Err(AppError::bad_request(
            "guest_token_invalid",
            "the guest token is not valid",
        ));
    }
    if method == axum::http::Method::POST {
        same_origin_guest_post(&headers, state.config.trust_forwarded_for)?;
        if headers
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|length| length > h4rn_protocol::GUEST_MAX_EXCHANGE_BYTES)
        {
            return Err(AppError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "guest_body_too_large",
                "that message is over the guest limit",
                None,
            ));
        }
    }
    let machine = state.store.machine(machine_id).await?;
    let daemon = machine
        .daemon_device_id
        .ok_or_else(|| AppError::unavailable("daemon_missing", "the machine has no daemon"))?;
    if !state.relay.online(daemon).await {
        return Err(AppError::unavailable(
            "daemon_offline",
            "the machine's daemon is not connected",
        ));
    }

    let mut path = format!("/guest/{token}");
    if !rest.is_empty() {
        path.push('/');
        path.push_str(&rest);
    }
    if let Some(query) = uri.query() {
        path.push('?');
        path.push_str(query);
    }
    let public_prefix = format!("/guest/{machine_id}/{token}");
    let chunked = method == axum::http::Method::POST;
    let request = GuestHttpRequest {
        request_id: request_id.to_owned(),
        method: method.as_str().to_owned(),
        path,
        public_prefix: Some(public_prefix.clone()),
        chunked,
        headers: forwarded_guest_headers(&headers, state.config.trust_forwarded_for),
        body: Vec::new(),
    };
    let payload = serde_json::to_vec(&request)
        .map_err(|_| AppError::internal("serialise the guest request"))?;
    let exchange = state.relay.expect_guest_exchange(request_id, daemon).await;
    let frame = RelayFrame {
        conn_id: format!("guest-{request_id}"),
        kind: RelayFrameKind::GuestHttp,
        payload,
    };
    state
        .relay
        .send_wait(
            daemon,
            RelayServerEvent::Frame { frame },
            Duration::from_secs(10),
        )
        .await?;

    if chunked {
        // A daemon says on its relay hello whether it admits an upload before
        // the body. A connection that promised a hello and has not sent one is
        // still unknown after the grace: fail closed there rather than stream
        // to a current daemon that simply has not spoken yet. Only a socket
        // built before hellos, or one whose hello omits the feature, keeps the
        // bounded legacy path.
        let admission = state
            .relay
            .guest_admission(daemon, GUEST_ADMISSION_WAIT)
            .await;
        match admission {
            crate::relay::GuestAdmission::Announced => {
                forward_guest_body(
                    &state.relay,
                    daemon,
                    request_id,
                    true,
                    exchange.admission,
                    body,
                )
                .await?;
            }
            crate::relay::GuestAdmission::Legacy => {
                forward_guest_body(
                    &state.relay,
                    daemon,
                    request_id,
                    false,
                    exchange.admission,
                    body,
                )
                .await?;
            }
            crate::relay::GuestAdmission::Unknown => {
                // The daemon may already hold an upload slot for this
                // exchange; drop it, and let the browser retry once the hello
                // has arrived.
                cancel_guest_admission(&state.relay, daemon, request_id).await;
                return Err(AppError::unavailable(
                    "guest_negotiating",
                    "the machine is still saying what it supports; try again",
                ));
            }
        }
    }

    let answer = match tokio::time::timeout(
        Duration::from_secs(state.config.guest_answer_timeout_secs),
        exchange.response,
    )
    .await
    {
        Ok(Ok(answer)) => answer,
        Ok(Err(_)) => {
            return Err(AppError::unavailable(
                "guest_closed",
                "the machine stopped before answering",
            ));
        }
        Err(_) => {
            return Err(AppError::unavailable(
                "guest_timeout",
                "the machine did not answer in time",
            ));
        }
    };
    guest_answer_response(state, answer, &public_prefix)
}

/// Waits for the daemon's admission before forwarding a chunked guest body.
///
/// A daemon that announced [`h4rn_protocol::RELAY_FEATURE_GUEST_ADMISSION`]
/// must accept the exchange before a byte of the body leaves Connect: a wait
/// that expires is an error, never an invitation to upload. A refusal answers
/// the exchange and closes the admission, so nothing is uploaded for it. A
/// daemon that did not announce the feature keeps the short bounded wait and
/// then its own pre-authorization buffer, which is what that build implements.
async fn forward_guest_body(
    relay: &RelayHub,
    daemon: Uuid,
    request_id: &str,
    negotiated: bool,
    admission: tokio::sync::oneshot::Receiver<()>,
    body: axum::body::Body,
) -> Result<()> {
    match tokio::time::timeout(GUEST_ADMISSION_WAIT, admission).await {
        Ok(Ok(())) => stream_guest_body(relay, daemon, request_id, body).await,
        // The daemon answered instead of admitting: the refusal is already on
        // its way to the browser, and none of the body was read.
        Ok(Err(_)) => Ok(()),
        Err(_) if negotiated => {
            // The daemon may already hold an upload slot for an exchange whose
            // body will never arrive. Tell it to release the slot now instead
            // of pinning it until its idle timeout.
            cancel_guest_admission(relay, daemon, request_id).await;
            Err(AppError::unavailable(
                "guest_admission_timeout",
                "the machine did not admit the upload in time",
            ))
        }
        Err(_) => stream_guest_body(relay, daemon, request_id, body).await,
    }
}

/// Tells a daemon to forget an upload Connect will not send a body for.
///
/// Best effort: the exchange already fails, and a daemon that misses the
/// cancel still gives the slot back at its idle timeout.
async fn cancel_guest_admission(relay: &RelayHub, daemon: Uuid, request_id: &str) {
    let Ok(payload) = serde_json::to_vec(&h4rn_protocol::GuestHttpAdmission {
        request_id: request_id.to_owned(),
    }) else {
        return;
    };
    let frame = RelayFrame {
        conn_id: format!("guest-{request_id}"),
        kind: RelayFrameKind::GuestHttpCancel,
        payload,
    };
    let _ = relay
        .send_wait(
            daemon,
            RelayServerEvent::Frame { frame },
            Duration::from_secs(1),
        )
        .await;
}

/// Streams a guest upload to its daemon in frames that stay small.
async fn stream_guest_body(
    relay: &RelayHub,
    daemon: Uuid,
    request_id: &str,
    body: axum::body::Body,
) -> Result<()> {
    use futures::StreamExt as _;
    let mut stream = body.into_data_stream();
    let mut seq = 0u64;
    let mut total = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            AppError::bad_request("guest_body", "the guest request body could not be read")
        })?;
        total = total.saturating_add(chunk.len());
        if total > h4rn_protocol::GUEST_MAX_EXCHANGE_BYTES {
            return Err(AppError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "guest_body_too_large",
                "that message is over the guest limit",
                None,
            ));
        }
        for slice in chunk.chunks(h4rn_protocol::GUEST_CHUNK_BYTES) {
            send_guest_chunk(relay, daemon, request_id, seq, false, slice.to_vec()).await?;
            seq += 1;
        }
    }
    // An empty last frame terminates a body that ended on a slice boundary.
    send_guest_chunk(relay, daemon, request_id, seq, true, Vec::new()).await
}

/// Sends one body slice to the daemon.
async fn send_guest_chunk(
    relay: &RelayHub,
    daemon: Uuid,
    request_id: &str,
    seq: u64,
    last: bool,
    body: Vec<u8>,
) -> Result<()> {
    let chunk = h4rn_protocol::GuestHttpChunk {
        request_id: request_id.to_owned(),
        seq,
        last,
        body,
    };
    let payload = serde_json::to_vec(&chunk)
        .map_err(|_| AppError::internal("serialise a guest body slice"))?;
    relay
        .send_wait(
            daemon,
            RelayServerEvent::Frame {
                frame: RelayFrame {
                    conn_id: format!("guest-{request_id}"),
                    kind: RelayFrameKind::GuestHttpChunk,
                    payload,
                },
            },
            Duration::from_secs(30),
        )
        .await
}

/// Rebuilds the daemon's answer as a browser response.
///
/// Only an allow-listed content type and a few safe headers cross back. The
/// daemon's CSP, cookies and framing headers are never passed through; the
/// binding cookie is rebuilt for the public path Connect knows.
fn guest_answer_response(
    state: &AppState,
    answer: h4rn_protocol::GuestHttpResponse,
    public_prefix: &str,
) -> Result<Response> {
    build_guest_response(
        answer,
        public_prefix,
        state.config.public_base_url.starts_with("https://"),
    )
}

/// Rebuilds a relayed answer for a browser with a known transport security.
fn build_guest_response(
    answer: h4rn_protocol::GuestHttpResponse,
    public_prefix: &str,
    secure: bool,
) -> Result<Response> {
    let content_type = answer
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.to_ascii_lowercase())
        .unwrap_or_default();
    let Some(content_type) = allowed_guest_content_type(&content_type) else {
        return Err(AppError::upstream(
            "guest_content_type",
            "the machine answered with a type a guest page may not show",
        ));
    };
    let download = matches!(content_type, "application/pdf" | "application/octet-stream");
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(answer.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(CONTENT_TYPE, content_type)
        // Even a JSON answer is inert: it may never run script, load a
        // subresource or frame the page, whatever type the daemon named.
        .header(
            HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static("sandbox; default-src 'none'"),
        )
        .header(
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        )
        .header(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("no-referrer"),
        );
    if download {
        // A PDF or an opaque file opens as a download, never in Connect's tab.
        builder = builder.header(
            axum::http::header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
    for (name, value) in &answer.headers {
        match name.to_ascii_lowercase().as_str() {
            "cache-control" if value == "no-store" || value == "private, max-age=3600" => {
                builder = builder.header(axum::http::header::CACHE_CONTROL, value);
            }
            // The frame's own field wins; the header form is for a daemon built
            // before it existed, and never both at once.
            "set-cookie" if answer.set_cookie.is_none() => {
                if let Some(cookie) = rebuild_guest_cookie(value, public_prefix, secure) {
                    builder = builder.header(axum::http::header::SET_COOKIE, cookie);
                }
            }
            _ => {}
        }
    }
    // A daemon carries its binding cookie in the frame's own field; the header
    // form is accepted too for an older answer. Either way only a complete,
    // well-formed cookie is rebuilt for Connect's public path.
    if let Some(value) = answer.set_cookie.as_deref()
        && let Some(cookie) = rebuild_guest_cookie(value, public_prefix, secure)
    {
        builder = builder.header(axum::http::header::SET_COOKIE, cookie);
    }
    builder
        .body(axum::body::Body::from(answer.body))
        .map_err(|_| AppError::internal("build the guest response"))
}

/// Whether a relayed answer may be shown to a guest browser.
fn allowed_guest_content_type(value: &str) -> Option<&'static str> {
    let mime = value.split(';').next().unwrap_or("").trim();
    match mime {
        "application/json" => Some("application/json"),
        "application/pdf" => Some("application/pdf"),
        "image/png" => Some("image/png"),
        "image/jpeg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        "text/plain" => Some("text/plain; charset=utf-8"),
        "application/octet-stream" => Some("application/octet-stream"),
        _ => None,
    }
}

/// Rebuilds the daemon's binding cookie for Connect's public path.
///
/// The daemon's own `Path` names its internal route and cannot be used here, so
/// Connect accepts only the cookie it knows: the guest name, an opaque value,
/// the exact public path, and the three attributes that make it a binding
/// cookie (`HttpOnly`, `Secure`, `SameSite=Strict`). Anything else is dropped
/// rather than repaired.
fn rebuild_guest_cookie(value: &str, public_prefix: &str, secure: bool) -> Option<String> {
    let mut parts = value.split(';');
    let (name, token) = parts.next()?.split_once('=')?;
    if name.trim() != h4rn_protocol::GUEST_COOKIE {
        return None;
    }
    let token = token.trim();
    if token.is_empty()
        || token.len() > 128
        || !token.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
    {
        return None;
    }
    let expected_path = public_prefix.trim_end_matches('/');
    let mut path = None;
    let mut http_only = false;
    let mut secure_attr = false;
    let mut same_site = None;
    for attribute in parts {
        let (key, value) = attribute.split_once('=').unwrap_or((attribute, ""));
        match key.trim().to_ascii_lowercase().as_str() {
            "path" => path = Some(value.trim().to_owned()),
            "httponly" => http_only = true,
            "secure" => secure_attr = true,
            "samesite" => same_site = Some(value.trim().to_owned()),
            _ => {}
        }
    }
    if path.as_deref() != Some(expected_path) || !http_only || !secure_attr {
        return None;
    }
    if same_site.as_deref() != Some("Strict") {
        return None;
    }
    let secure = if secure { "; Secure" } else { "" };
    Some(format!(
        "{}={token}; Path={expected_path}; HttpOnly; SameSite=Strict{secure}; Max-Age={}",
        h4rn_protocol::GUEST_COOKIE,
        h4rn_protocol::GUEST_LINK_TTL_HOURS * 3600
    ))
}

/// Keeps only the guest cookie and the headers the daemon needs.
///
/// Connect's own session cookies must never reach a daemon because a guest
/// loaded the page from Connect's origin.
fn forwarded_guest_headers(
    headers: &HeaderMap,
    trust_forwarded_for: bool,
) -> Vec<(String, String)> {
    let mut forwarded = Vec::new();
    if let Some(cookie) = guest_only_cookie(headers) {
        forwarded.push(("cookie".to_owned(), cookie));
    }
    for name in ["accept", "content-type", "origin", "sec-fetch-site"] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            forwarded.push((name.to_owned(), value.to_owned()));
        }
    }
    // The daemon checks a submission's Origin against the Host the browser
    // used. Behind a trusted proxy that Host is the forwarded one.
    let host = if trust_forwarded_for {
        headers
            .get("x-forwarded-host")
            .and_then(|value| value.to_str().ok())
            .or_else(|| {
                headers
                    .get(axum::http::header::HOST)
                    .and_then(|value| value.to_str().ok())
            })
    } else {
        headers
            .get(axum::http::header::HOST)
            .and_then(|value| value.to_str().ok())
    };
    if let Some(host) = host {
        forwarded.push(("host".to_owned(), host.to_owned()));
    }
    forwarded
}

/// The `h4rn_guest` cookie from a request, ignoring every other cookie.
fn guest_only_cookie(headers: &HeaderMap) -> Option<String> {
    guest_cookie_value(headers).map(|value| format!("{}={value}", h4rn_protocol::GUEST_COOKIE))
}

/// Requires a guest submission to be a same-origin JSON POST.
fn same_origin_guest_post(headers: &HeaderMap, trust_forwarded_for: bool) -> Result<()> {
    let json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
        });
    if !json {
        return Err(AppError::forbidden(
            "guest_origin",
            "a guest message must be a JSON POST from the guest page",
        ));
    }
    if headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        == Some("cross-site")
    {
        return Err(AppError::forbidden(
            "guest_origin",
            "a guest message must come from the guest page itself",
        ));
    }
    if let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok()) {
        let authority = origin
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(origin)
            .trim_end_matches('/');
        let host = if trust_forwarded_for {
            headers
                .get("x-forwarded-host")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        } else {
            None
        }
        .or_else(|| {
            headers
                .get(axum::http::header::HOST)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        });
        if !host.is_some_and(|host| authority.eq_ignore_ascii_case(&host)) {
            return Err(AppError::forbidden(
                "guest_origin",
                "a guest message must come from the guest page itself",
            ));
        }
    }
    Ok(())
}

/// A path with guest tokens removed, for tracing.
fn route_template(path: &str) -> String {
    h4rn_protocol::redact_guest_path(path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pairing_request_cannot_ask_for_guest_access() {
        // A guest link is not a device scope (ADR-044): a client that asks for
        // one is refused at deserialisation instead of being quietly narrowed.
        let refused = serde_json::from_str::<h4rn_protocol::PairingCreateRequest>(
            r#"{"machine_id":"0199aaaa-0000-7000-8000-000000000000","scopes":["guest"]}"#,
        );
        assert!(
            refused.is_err(),
            "pairing accepted a request for the removed guest scope"
        );
        let accepted = serde_json::from_str::<h4rn_protocol::PairingCreateRequest>(
            r#"{"machine_id":"0199aaaa-0000-7000-8000-000000000000","scopes":["view","files"]}"#,
        );
        assert!(accepted.is_ok(), "a normal pairing request was refused");
    }

    #[test]
    fn the_linked_filter_is_named_only_when_it_applied() {
        // The client treats a missing marker as "an older relay ignored the
        // query", so an unfiltered list must never claim the linked scope.
        assert!(
            machine_list_headers(false)
                .get(MACHINES_FILTER_HEADER)
                .is_none(),
            "an unfiltered list must not claim the linked scope"
        );
        assert_eq!(
            machine_list_headers(true)
                .get(MACHINES_FILTER_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some("linked")
        );
    }

    #[test]
    fn a_guest_answer_crosses_back_only_as_an_allowed_type() {
        let json = h4rn_protocol::GuestHttpResponse {
            request_id: "g1".into(),
            status: 200,
            chunked: false,
            headers: vec![("content-type".into(), "application/json".into())],
            body: b"{}".to_vec(),
            set_cookie: None,
        };
        let response = build_guest_response(json, "/guest/m/t", true).expect("json answer");
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );

        // A daemon that tries to return HTML never reaches the guest browser.
        let html = h4rn_protocol::GuestHttpResponse {
            request_id: "g2".into(),
            status: 200,
            chunked: false,
            headers: vec![("content-type".into(), "text/html".into())],
            body: b"<script>alert(1)</script>".to_vec(),
            set_cookie: None,
        };
        assert!(build_guest_response(html, "/guest/m/t", true).is_err());
    }

    #[test]
    fn a_daemons_headers_are_rebuilt_not_passed_through() {
        let answer = h4rn_protocol::GuestHttpResponse {
            request_id: "g1".into(),
            status: 200,
            chunked: false,
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("content-security-policy".into(), "default-src *".into()),
                ("x-frame-options".into(), "ALLOWALL".into()),
                // A daemon built before the frame had its own field sends the
                // cookie here; Connect must not emit both.
                (
                    "set-cookie".into(),
                    "h4rn_guest=binding_1; HttpOnly; Secure; SameSite=Strict; Path=/guest/machine_1/token"
                        .into(),
                ),
            ],
            body: b"{}".to_vec(),
            // The daemon scopes its own cookie to the public prefix the relay
            // told it about, so Connect can check it exactly.
            set_cookie: Some(
                "h4rn_guest=binding_1; HttpOnly; Secure; SameSite=Strict; Path=/guest/machine_1/token; Max-Age=86400"
                    .into(),
            ),
        };
        let response =
            build_guest_response(answer, "/guest/machine_1/token", true).expect("answer");
        assert_eq!(
            response
                .headers()
                .get_all(axum::http::header::SET_COOKIE)
                .iter()
                .count(),
            1,
            "Connect sent the binding cookie twice"
        );
        // The daemon's CSP and framing headers are gone; Connect sets its own
        // sandbox policy instead of trusting the machine.
        assert_eq!(
            response
                .headers()
                .get("content-security-policy")
                .and_then(|value| value.to_str().ok()),
            Some("sandbox; default-src 'none'")
        );
        assert!(response.headers().get("x-frame-options").is_none());
        assert_eq!(
            response
                .headers()
                .get("x-content-type-options")
                .and_then(|value| value.to_str().ok()),
            Some("nosniff")
        );
        let cookie = response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .expect("binding cookie");
        // The cookie is scoped to the public path Connect serves, not the
        // daemon's internal one, or the browser would never send it back.
        assert!(cookie.contains("Path=/guest/machine_1/token"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        assert!(cookie.contains("Secure"), "{cookie}");
    }

    #[test]
    fn only_a_complete_binding_cookie_is_rebuilt() {
        let prefix = "/guest/machine_1/token";
        let good = rebuild_guest_cookie(
            "h4rn_guest=binding_1; HttpOnly; Secure; SameSite=Strict; Path=/guest/machine_1/token",
            prefix,
            true,
        );
        assert!(good.is_some(), "a complete binding cookie was refused");
        for (reason, value) in [
            (
                "a foreign name",
                "other=1; HttpOnly; Secure; SameSite=Strict; Path=/guest/machine_1/token",
            ),
            (
                "the daemon's internal path",
                "h4rn_guest=1; HttpOnly; Secure; SameSite=Strict; Path=/guest/token",
            ),
            (
                "a missing HttpOnly",
                "h4rn_guest=1; Secure; SameSite=Strict; Path=/guest/machine_1/token",
            ),
            (
                "a missing Secure",
                "h4rn_guest=1; HttpOnly; SameSite=Strict; Path=/guest/machine_1/token",
            ),
            (
                "a lax SameSite",
                "h4rn_guest=1; HttpOnly; Secure; SameSite=Lax; Path=/guest/machine_1/token",
            ),
        ] {
            assert!(
                rebuild_guest_cookie(value, prefix, true).is_none(),
                "Connect rebuilt a cookie with {reason}: {value}"
            );
        }
    }

    #[test]
    fn connect_cookies_are_never_forwarded_to_a_daemon() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("h4rn_session=secret; h4rn_guest=guest_1"),
        );
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let forwarded = forwarded_guest_headers(&headers, false);
        assert_eq!(
            forwarded
                .iter()
                .find(|(name, _)| name == "cookie")
                .map(|(_, value)| value.as_str()),
            Some("h4rn_guest=guest_1")
        );
        assert!(
            forwarded.iter().all(|(_, value)| !value.contains("secret")),
            "{forwarded:?}"
        );
    }

    #[test]
    fn a_connect_guest_post_must_be_same_origin_json() {
        let json = |origin: Option<&str>, host: Option<&str>| {
            let mut headers = HeaderMap::new();
            headers.insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            if let Some(origin) = origin {
                headers.insert(ORIGIN, HeaderValue::from_str(origin).unwrap());
            }
            if let Some(host) = host {
                headers.insert(
                    axum::http::header::HOST,
                    HeaderValue::from_str(host).unwrap(),
                );
            }
            headers
        };
        assert!(same_origin_guest_post(&json(None, None), false).is_ok());
        assert!(
            same_origin_guest_post(&json(Some("https://relay.test"), Some("relay.test")), false)
                .is_ok()
        );
        // A cross-site form or fetch cannot submit a guest message.
        assert!(
            same_origin_guest_post(&json(Some("https://evil.test"), Some("relay.test")), false)
                .is_err()
        );
        let mut form = HeaderMap::new();
        form.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        assert!(same_origin_guest_post(&form, false).is_err());
    }

    #[test]
    fn every_guest_answer_is_sandboxed_and_files_download() {
        let answer = |content_type: &str| h4rn_protocol::GuestHttpResponse {
            request_id: "g1".into(),
            status: 200,
            chunked: false,
            headers: vec![("content-type".into(), content_type.into())],
            body: b"x".to_vec(),
            set_cookie: None,
        };
        let json = build_guest_response(answer("application/json"), "/guest/m/t", true)
            .expect("json answer");
        assert_eq!(
            json.headers()
                .get("content-security-policy")
                .and_then(|value| value.to_str().ok()),
            Some("sandbox; default-src 'none'")
        );
        assert!(json.headers().get("content-disposition").is_none());

        for content_type in ["application/pdf", "application/octet-stream"] {
            let response = build_guest_response(answer(content_type), "/guest/m/t", true)
                .expect("file answer");
            assert_eq!(
                response
                    .headers()
                    .get("content-disposition")
                    .and_then(|value| value.to_str().ok()),
                Some("attachment"),
                "{content_type} must download"
            );
        }
        let image =
            build_guest_response(answer("image/png"), "/guest/m/t", true).expect("image answer");
        assert!(image.headers().get("content-disposition").is_none());
    }

    #[tokio::test]
    async fn a_failed_guest_exchange_forgets_its_waiter() {
        let hub = crate::relay::RelayHub::new();
        let daemon = Uuid::now_v7();
        let _waiter = hub.expect_guest("g1", daemon).await;
        let failed = Err::<(), AppError>(AppError::internal("the daemon went away"));
        assert!(discard_failed_exchange(&hub, "g1", failed).await.is_err());
        assert_eq!(
            hub.pending_guests().await,
            0,
            "a failed exchange kept its waiter"
        );

        // A successful exchange is left alone for its answer to arrive.
        let _waiter = hub.expect_guest("g2", daemon).await;
        assert!(discard_failed_exchange(&hub, "g2", Ok(())).await.is_ok());
        assert_eq!(hub.pending_guests().await, 1);
    }

    #[tokio::test]
    async fn both_proxy_failures_drop_a_started_chunked_answer() {
        // The two ways `guest_proxy_exchange` fails, one where the frame could
        // not be delivered and one where the answer timed out. Either must take
        // a chunked answer that already started with it, waiter and body.
        for failure in [
            AppError::unavailable("relay_backpressure", "the other device is too slow"),
            AppError::unavailable("guest_timeout", "the machine did not answer in time"),
        ] {
            let hub = crate::relay::RelayHub::new();
            let daemon = Uuid::now_v7();
            let exchange = hub.expect_guest_exchange("g1", daemon).await;
            assert!(
                hub.complete_guest(
                    daemon,
                    h4rn_protocol::GuestHttpResponse {
                        request_id: "g1".into(),
                        status: 200,
                        chunked: true,
                        headers: Vec::new(),
                        body: Vec::new(),
                        set_cookie: None,
                    }
                )
                .await,
                "the chunked answer did not start"
            );
            assert!(
                hub.complete_guest_chunk(
                    daemon,
                    h4rn_protocol::GuestHttpChunk {
                        request_id: "g1".into(),
                        seq: 0,
                        last: false,
                        body: b"first half".to_vec(),
                    }
                )
                .await,
                "the first half did not arrive"
            );

            let failed = discard_failed_exchange(&hub, "g1", Err::<(), AppError>(failure)).await;
            assert!(failed.is_err());
            assert_eq!(hub.pending_guests().await, 0, "a failure kept a waiter");
            assert_eq!(
                hub.pending_guest_bodies().await,
                0,
                "a failure kept a half-arrived body"
            );
            assert!(
                exchange.response.await.is_err(),
                "a failed exchange still delivered an answer"
            );
        }
    }

    /// A registered daemon connection whose outbound queue the test owns.
    async fn daemon_connection(
        hub: &Arc<crate::relay::RelayHub>,
        daemon: Uuid,
        announces_hello: bool,
    ) -> (tokio::sync::mpsc::Receiver<RelayServerEvent>, Uuid) {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let connection = Uuid::now_v7();
        hub.register(crate::relay::Peer {
            device_id: daemon,
            connection_id: connection,
            announces_hello,
            user_id: Uuid::now_v7(),
            machine_id: None,
            kind: "daemon".to_owned(),
            tx,
        })
        .await;
        (rx, connection)
    }

    /// A daemon connection from a build that predates relay hellos.
    async fn daemon_peer(
        hub: &Arc<crate::relay::RelayHub>,
        daemon: Uuid,
    ) -> tokio::sync::mpsc::Receiver<RelayServerEvent> {
        daemon_connection(hub, daemon, false).await.0
    }

    /// The next frame the daemon would receive, if one arrives.
    async fn next_daemon_frame(
        rx: &mut tokio::sync::mpsc::Receiver<RelayServerEvent>,
    ) -> Option<RelayFrame> {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(RelayServerEvent::Frame { frame })) => Some(frame),
            _ => None,
        }
    }

    /// A daemon connection that announced the negotiated admission protocol.
    async fn announcing_daemon(
        hub: &Arc<crate::relay::RelayHub>,
    ) -> (Uuid, tokio::sync::mpsc::Receiver<RelayServerEvent>) {
        let daemon = Uuid::now_v7();
        let (rx, connection) = daemon_connection(hub, daemon, true).await;
        hub.announce(
            daemon,
            connection,
            h4rn_protocol::RelayHello {
                features: vec![h4rn_protocol::RELAY_FEATURE_GUEST_ADMISSION.to_owned()],
            },
        )
        .await;
        (daemon, rx)
    }

    #[tokio::test]
    async fn a_negotiated_upload_waits_for_admission_then_streams() {
        let hub = Arc::new(crate::relay::RelayHub::new());
        let (daemon, mut rx) = announcing_daemon(&hub).await;
        let exchange = hub.expect_guest_exchange("g1", daemon).await;
        let body = axum::body::Body::from(vec![b'a'; 8]);
        let sending = {
            let hub = Arc::clone(&hub);
            tokio::spawn(async move {
                forward_guest_body(&hub, daemon, "g1", true, exchange.admission, body).await
            })
        };

        // The daemon announced admission, so a silent daemon means no byte
        // leaves Connect, however long it stays silent.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), rx.recv())
                .await
                .is_err(),
            "an upload crossed before the daemon admitted it"
        );
        assert!(hub.admit_guest(daemon, "g1").await, "the admission failed");

        let first = next_daemon_frame(&mut rx)
            .await
            .expect("the admitted upload never streamed");
        assert_eq!(first.kind, RelayFrameKind::GuestHttpChunk);
        let mut last = false;
        let mut frames = 0;
        while !last {
            let frame = next_daemon_frame(&mut rx)
                .await
                .expect("the upload stopped before its last frame");
            let chunk: h4rn_protocol::GuestHttpChunk =
                serde_json::from_slice(&frame.payload).expect("chunk json");
            last = chunk.last;
            frames += 1;
        }
        assert!(frames > 0);
        sending
            .await
            .expect("send task")
            .expect("an admitted upload failed to stream");
    }

    #[tokio::test]
    async fn a_negotiated_upload_that_is_never_admitted_fails_without_uploading() {
        let hub = Arc::new(crate::relay::RelayHub::new());
        let (daemon, mut rx) = announcing_daemon(&hub).await;
        let exchange = hub.expect_guest_exchange("g1", daemon).await;
        let body = axum::body::Body::from(vec![b'a'; 8]);
        let sending = {
            let hub = Arc::clone(&hub);
            tokio::spawn(async move {
                forward_guest_body(&hub, daemon, "g1", true, exchange.admission, body).await
            })
        };

        // A current daemon that is merely slow is an error, not an older
        // daemon: the old timeout-as-version guess sent the body here.
        let failed = tokio::time::timeout(Duration::from_secs(5), sending)
            .await
            .expect("the admission wait never ended")
            .expect("send task");
        assert!(failed.is_err(), "a silent daemon was treated as admitted");
        // The daemon may already hold an upload slot for this exchange; the
        // only thing Connect sends is a cancel, never a body frame.
        let frame = next_daemon_frame(&mut rx)
            .await
            .expect("the daemon was never told to release the upload");
        assert_eq!(
            frame.kind,
            RelayFrameKind::GuestHttpCancel,
            "a timed-out admission was not cancelled"
        );
        let cancel: h4rn_protocol::GuestHttpAdmission =
            serde_json::from_slice(&frame.payload).expect("cancel json");
        assert_eq!(cancel.request_id, "g1");
        assert!(
            rx.try_recv().is_err(),
            "a body crossed for an upload that was never admitted"
        );
    }

    #[tokio::test]
    async fn a_negotiated_upload_refused_by_the_daemon_sends_no_body() {
        let hub = Arc::new(crate::relay::RelayHub::new());
        let (daemon, mut rx) = announcing_daemon(&hub).await;
        let exchange = hub.expect_guest_exchange("g1", daemon).await;
        let response = exchange.response;
        let body = axum::body::Body::from(vec![b'a'; 8]);
        let sending = {
            let hub = Arc::clone(&hub);
            tokio::spawn(async move {
                forward_guest_body(&hub, daemon, "g1", true, exchange.admission, body).await
            })
        };
        // Give the upload a moment to prove it waits.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            hub.complete_guest(
                daemon,
                h4rn_protocol::GuestHttpResponse {
                    request_id: "g1".into(),
                    status: 410,
                    chunked: false,
                    headers: vec![("content-type".into(), "text/plain".into())],
                    body: b"gone".to_vec(),
                    set_cookie: None,
                }
            )
            .await,
            "the refusal was not delivered"
        );

        sending
            .await
            .expect("send task")
            .expect("a refusal failed the proxy instead of answering it");
        assert!(
            rx.try_recv().is_err(),
            "a refused upload still sent its body"
        );
        assert_eq!(hub.pending_guests().await, 0, "a refusal kept a waiter");
        let answer = response
            .await
            .expect("the refusal never reached the browser");
        assert_eq!(answer.status, 410);
    }

    #[tokio::test]
    async fn an_older_daemon_keeps_the_bounded_streaming_path() {
        let hub = Arc::new(crate::relay::RelayHub::new());
        let daemon = Uuid::now_v7();
        let mut rx = daemon_peer(&hub, daemon).await;
        assert!(
            !hub.advertises(daemon, h4rn_protocol::RELAY_FEATURE_GUEST_ADMISSION)
                .await,
            "a daemon that never sent a hello advertised a feature"
        );
        let exchange = hub.expect_guest_exchange("g1", daemon).await;
        let body = axum::body::Body::from(vec![b'a'; 8]);
        let sending = tokio::spawn(async move {
            forward_guest_body(&hub, daemon, "g1", false, exchange.admission, body).await
        });

        // A daemon that did not announce admission keeps the short bounded
        // wait and then the body, because that build bounds its own buffer.
        let first = next_daemon_frame(&mut rx)
            .await
            .expect("an older daemon never received the body");
        assert_eq!(first.kind, RelayFrameKind::GuestHttpChunk);
        let _ = sending.await.expect("send task");
    }

    #[test]
    fn a_guest_route_is_traced_by_its_template() {
        assert_eq!(
            route_template("/guest/machine_1/token_abc/state"),
            "/guest/{token}/state"
        );
        assert!(!route_template("/guest/machine_1/token_abc/state").contains("token_abc"));
        assert_eq!(route_template("/healthz"), "/healthz");
    }

    #[test]
    fn the_guest_page_is_connect_s_own_static_page() {
        let response = guest_page_response(None, "active");
        let csp = response
            .headers()
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .expect("CSP");
        assert!(csp.contains("'sha256-"), "{csp}");
        assert!(!csp.contains("unsafe-inline"), "{csp}");
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok());
        assert_eq!(content_type, Some("text/html; charset=utf-8"));
    }

    #[tokio::test]
    async fn connect_serves_the_state_the_daemon_rendered() {
        // A daemon page for an expired link says so in its body; Connect reads
        // only that state and never the HTML around it.
        let daemon_page = h4rn_protocol::guest_page_html("expired");
        assert_eq!(guest_page_state(daemon_page.as_bytes()), Some("expired"));

        let response = guest_page_response(None, "expired");
        let (parts, body) = response.into_parts();
        let body = axum::body::to_bytes(body, 256 * 1024)
            .await
            .expect("page body");
        let html = String::from_utf8(body.to_vec()).expect("utf-8 page");
        assert!(html.contains(r#"data-state="expired""#), "{html}");
        assert!(parts.headers.contains_key(CONTENT_TYPE));
    }

    #[test]
    fn an_unknown_daemon_state_never_reaches_connect_s_page() {
        assert_eq!(guest_page_state(b"<body>no state here</body>"), None);
        assert_eq!(
            guest_page_state(br#"<body data-state="active&quot;onload=alert(1)">"#),
            None,
            "Connect copied an injected state"
        );
        assert_eq!(
            guest_page_state(br#"<body data-state="totally-made-up">"#),
            None
        );
    }

    /// One daemon answer with the fields this test needs.
    fn guest_answer(status: u16, body: &[u8]) -> h4rn_protocol::GuestHttpResponse {
        h4rn_protocol::GuestHttpResponse {
            request_id: "req_1".into(),
            status,
            chunked: false,
            headers: Vec::new(),
            body: body.to_vec(),
            set_cookie: None,
        }
    }

    #[test]
    fn a_stopped_or_replaced_link_opens_revoked() {
        // The daemon answers 410 for a token it no longer knows, with a plain
        // sentence rather than the page.
        let gone = guest_answer(410, b"This link was stopped or replaced by the owner.");
        assert_eq!(page_state_from_answer(&gone), "revoked");
        // A live page still carries its own state.
        let page = h4rn_protocol::guest_page_html("active");
        assert_eq!(
            page_state_from_answer(&guest_answer(200, page.as_bytes())),
            "active"
        );
        // A bound-link page for a browser presenting no cookie is unbound.
        let unbound = h4rn_protocol::guest_page_html("unbound");
        assert_eq!(
            page_state_from_answer(&guest_answer(200, unbound.as_bytes())),
            "unbound"
        );
    }

    #[test]
    fn the_page_request_carries_the_browser_s_binding_cookie() {
        let bound = page_request("req_1", Uuid::nil(), "token_abc", Some("binding_value"));
        assert_eq!(bound.path, "/guest/token_abc");
        assert_eq!(
            bound
                .headers
                .iter()
                .find(|(name, _)| name == "cookie")
                .map(|(_, value)| value.as_str()),
            Some("h4rn_guest=binding_value"),
            "a bound browser's cookie was not forwarded with the page request"
        );
        let fresh = page_request("req_2", Uuid::nil(), "token_abc", None);
        assert!(
            fresh.headers.iter().all(|(name, _)| name != "cookie"),
            "a fresh browser sent a cookie to the daemon"
        );
    }

    #[test]
    fn only_the_guest_cookie_is_forwarded_to_the_daemon() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("session=secret; h4rn_guest=binding_value"),
        );
        assert_eq!(
            guest_binding_cookie(&headers).as_deref(),
            Some("binding_value")
        );
        let empty = HeaderMap::new();
        assert_eq!(guest_binding_cookie(&empty), None);
        // A request without the guest cookie forwards nothing.
        let mut other = HeaderMap::new();
        other.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("session=secret"),
        );
        assert_eq!(guest_binding_cookie(&other), None);
    }

    #[test]
    fn the_link_scoped_guest_cookie_wins_over_a_broader_duplicate() {
        // Browsers list the longest matching path first, so the link's own
        // binding comes before any broader `h4rn_guest` cookie.
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("h4rn_guest=link_binding; session=x; h4rn_guest=broader"),
        );
        assert_eq!(
            guest_binding_cookie(&headers).as_deref(),
            Some("link_binding")
        );
        assert_eq!(
            guest_only_cookie(&headers).as_deref(),
            Some("h4rn_guest=link_binding")
        );
    }
}
