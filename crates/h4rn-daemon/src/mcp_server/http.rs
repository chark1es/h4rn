//! HTTP routes for outside agents: OAuth, discovery and the MCP endpoint.
//!
//! The authorize page is served by the daemon because the browser that opens it
//! belongs to the outside client, not to the h4rn app. It can show who is asking
//! and where the code would go, but it cannot grant anything: the browser has no
//! h4rn session. The person answers the request inside h4rn (Settings →
//! Sharing & access), and this page then follows the answer to the callback.
//!
//! Every route here is reachable only on the loopback listener, and every
//! request is checked against a loopback `Host` (DNS rebinding) and a same-site
//! `Origin` (cross-site form posts).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use serde_json::json;

use super::oauth::OAuthError;
use super::outside::OutsideAgents;
use super::url::{html_escape, origin_of};

/// Builds the outside-agent router.
pub fn router(service: Arc<OutsideAgents>) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/authorize/pending", get(authorize_pending))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
        .route(
            "/mcp/outside",
            post(mcp_post).get(mcp_get).delete(mcp_delete),
        )
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&service),
            labs_guard,
        ))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&service),
            aggregate_guard,
        ))
        // The loopback check is outermost: a request this daemon did not serve
        // as its own page is refused before it costs a rate-limit hit or any
        // other work.
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&service),
            loopback_guard,
        ))
        .with_state(service)
}

/// The host names a loopback listener answers to.
const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

fn host_is_loopback(value: &str) -> bool {
    let value = value.trim();
    // `[::1]:47800` and `127.0.0.1:47800`; a bare name has no port.
    let name = if let Some(rest) = value.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        value.split(':').next().unwrap_or(value)
    };
    LOOPBACK_HOSTS
        .iter()
        .any(|host| name.eq_ignore_ascii_case(host))
}

fn origin_is_loopback(value: &str) -> bool {
    origin_of(value).is_some_and(|origin| {
        let rest = origin
            .split_once("://")
            .map(|(_, rest)| rest.to_owned())
            .unwrap_or(origin);
        host_is_loopback(&rest)
    })
}

/// Whether a request's `Origin` names this listener itself.
///
/// Another loopback port is still another site: a page some other local process
/// serves on `http://127.0.0.1:1455` must not reach h4rn's mutations. Only the
/// exact `scheme://host:port` the daemon bound is h4rn's own page.
fn origin_is_listener(value: &str, listener: Option<&str>) -> bool {
    let Some(origin) = origin_of(value) else {
        return false;
    };
    match listener.and_then(origin_of) {
        Some(expected) => origin.eq_ignore_ascii_case(&expected),
        // The listener has not named its address yet. Refusing every origin
        // would break the first request of a startup; the loopback rule is the
        // best available until `set_base_url` runs.
        None => origin_is_loopback(&origin),
    }
}

/// Whether `Sec-Fetch-Site` names a request from this daemon's own page.
///
/// `same-site` is not enough: another loopback port counts as the same site and
/// is exactly what the exact-origin check exists to refuse.
fn fetch_site_allowed(value: &str) -> bool {
    value.eq_ignore_ascii_case("same-origin") || value.eq_ignore_ascii_case("none")
}

/// Reads one header that must appear at most once and be plain ASCII text.
///
/// `Err` means the request carried several copies or bytes that are not a
/// header value. Both are refusals: treating a malformed `Origin` as absent is
/// how a cross-site request used to look like a local one.
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ()> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    value.to_str().map(Some).map_err(|_| ())
}

/// Whether `value` is one origin, serialized exactly as a browser sends it.
///
/// `null`, a path, a trailing slash, an URL with credentials or two origins in
/// one value all fail: the Origin header grammar has no room for them, and a
/// lenient parse would accept a string a browser never produces.
fn origin_is_well_formed(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty() && origin_of(trimmed).as_deref() == Some(trimmed)
}

/// Whether the socket peer is this machine.
///
/// The `Host` header is the client's claim; the peer address is not. OAuth and
/// `/mcp` carry plaintext bearer credentials, so a connection from another
/// machine is refused even if it presents a loopback `Host`.
fn peer_is_local(peer: SocketAddr) -> bool {
    peer.ip().is_loopback()
}

/// Refuses a request that did not come from this machine's own page.
///
/// A DNS-rebinding page reaches the loopback port with its own `Host`; any other
/// site's page arrives with another `Origin` or a `Sec-Fetch-Site` of
/// `cross-site` or `same-site`. A request whose peer is not this machine is
/// refused too: the `Host` header is the client's claim, the socket address is
/// not. All are refused before a handler runs, so a page the person happens to
/// visit cannot register a client or spend a code.
pub async fn loopback_guard(
    State(service): State<Arc<OutsideAgents>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    if !peer_is_local(peer) {
        return refused("this endpoint is only reachable from this machine");
    }
    let headers = request.headers();
    let host = single_header(headers, header::HOST.as_str());
    if !matches!(host, Ok(Some(host)) if host_is_loopback(host)) {
        return refused("this endpoint is only reachable through h4rn's loopback address");
    }
    let origin = match single_header(headers, header::ORIGIN.as_str()) {
        Ok(origin) => origin,
        Err(()) => {
            return refused("a request from another site cannot change anything in h4rn");
        }
    };
    let listener = service.base_url();
    if let Some(origin) = origin
        && (!origin_is_well_formed(origin) || !origin_is_listener(origin, listener.as_deref()))
    {
        return refused("a request from another site cannot change anything in h4rn");
    }
    let site = match single_header(headers, "sec-fetch-site") {
        Ok(site) => site,
        Err(()) => {
            return refused("a request from another site cannot change anything in h4rn");
        }
    };
    if let Some(site) = site
        && !fetch_site_allowed(site)
    {
        return refused("a request from another site cannot change anything in h4rn");
    }
    next.run(request).await
}

fn refused(message: &str) -> Response {
    no_store(
        (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            message.to_owned(),
        )
            .into_response(),
    )
}

/// Answers 404 for the OAuth surface while Settings → Labs keeps it off.
///
/// The routes must not exist at all while the switch is off: discovery is what
/// makes an MCP client start a sign-in, and a 403 would still say the feature
/// is here. `/mcp` itself stays mounted because harness sessions use it; an
/// outside token there is refused by [`OutsideAgents::handle_mcp`].
async fn labs_guard(
    State(service): State<Arc<OutsideAgents>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let oauth_surface = path.starts_with("/oauth/") || path.starts_with("/.well-known/oauth-");
    if oauth_surface && !service.enabled() {
        return no_store(
            (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "Not found.",
            )
                .into_response(),
        );
    }
    next.run(request).await
}

/// Caps the unauthenticated endpoints before their body is parsed.
async fn aggregate_guard(
    State(service): State<Arc<OutsideAgents>>,
    request: Request,
    next: Next,
) -> Response {
    if matches!(
        request.uri().path(),
        "/oauth/register" | "/oauth/authorize" | "/oauth/token" | "/oauth/revoke"
    ) && let Err(seconds) = service.check_aggregate()
    {
        return rate_limited(seconds);
    }
    next.run(request).await
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().expect("static"));
    response
}

fn oauth_error(error: OAuthError) -> Response {
    no_store(
        (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            json!({
                "error": error.code(),
                "error_description": error.description(),
            })
            .to_string(),
        )
            .into_response(),
    )
}

fn rate_limited(seconds: u64) -> Response {
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("too many requests; try again in {seconds}s"),
    )
        .into_response();
    if let Ok(value) = seconds.to_string().parse() {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

fn not_bound() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "the daemon has not bound its listener yet",
    )
        .into_response()
}

async fn authorization_server_metadata(State(service): State<Arc<OutsideAgents>>) -> Response {
    let Some(base) = service.base_url() else {
        return not_bound();
    };
    no_store(
        Json(json!({
            "issuer": base,
            "authorization_endpoint": format!("{base}/oauth/authorize"),
            "token_endpoint": format!("{base}/oauth/token"),
            "registration_endpoint": format!("{base}/oauth/register"),
            "revocation_endpoint": format!("{base}/oauth/revoke"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none"],
            "scopes_supported": ["read", "supervised", "full"],
            "resource_indicators_supported": true,
        }))
        .into_response(),
    )
}

async fn protected_resource_metadata(State(service): State<Arc<OutsideAgents>>) -> Response {
    let Some(base) = service.base_url() else {
        return not_bound();
    };
    no_store(
        Json(json!({
            "resource": format!("{base}/mcp"),
            "authorization_servers": [base],
            "bearer_methods_supported": ["header"],
            "scopes_supported": ["read", "supervised", "full"],
            "resource_name": "h4rn",
        }))
        .into_response(),
    )
}

#[derive(Debug, Deserialize)]
struct RegisterBody {
    #[serde(default)]
    client_name: Option<String>,
    #[serde(default)]
    redirect_uris: Vec<String>,
}

async fn register(
    State(service): State<Arc<OutsideAgents>>,
    Json(body): Json<RegisterBody>,
) -> Response {
    match service
        .store()
        .register_client(body.client_name.as_deref(), &body.redirect_uris)
    {
        Ok(client) => no_store(
            (
                StatusCode::CREATED,
                [(header::CONTENT_TYPE, "application/json")],
                json!({
                    "client_id": client.client_id,
                    "client_name": client.name,
                    "redirect_uris": client.redirect_uris,
                    "grant_types": ["authorization_code", "refresh_token"],
                    "response_types": ["code"],
                    "token_endpoint_auth_method": "none",
                })
                .to_string(),
            )
                .into_response(),
        ),
        Err(error) => oauth_error(error),
    }
}

#[derive(Debug, Deserialize)]
struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

/// Resolves the audience a grant is for, or why the client's `resource` is not
/// one this daemon serves.
///
/// RFC 8707 lets the client omit `resource`; an omitted one is not "any
/// audience", it is this daemon's own MCP endpoint. Binding it here means the
/// exchange, the refresh and every later use compare the same value.
fn check_resource(
    service: &OutsideAgents,
    resource: Option<&str>,
) -> Result<Option<String>, String> {
    let mcp = service.mcp_url();
    if mcp.is_empty() {
        // No listener has been bound, so there is no audience to bind to. The
        // grant stays unbound, and a use needs no expected resource either.
        return Ok(None);
    }
    match resource.map(str::trim) {
        None | Some("") => Ok(Some(mcp)),
        Some(resource) if resource == mcp => Ok(Some(mcp)),
        Some(_) => Err(format!(
            "`resource` must name this daemon's MCP endpoint ({mcp})"
        )),
    }
}

async fn authorize(
    State(service): State<Arc<OutsideAgents>>,
    Query(query): Query<AuthorizeQuery>,
) -> Response {
    let Some(client_id) = query.client_id.as_deref() else {
        return bad_request("client_id is required");
    };
    // Per-key buckets exist only for registered clients: an unknown id cannot
    // allocate one, and the aggregate limiter already covered this request.
    if service.store().client_exists(client_id)
        && let Err(seconds) = service.check_limit(&format!("authorize:{client_id}"))
    {
        return rate_limited(seconds);
    }
    if query.response_type.as_deref() != Some("code") {
        return bad_request("response_type must be code");
    }
    if query.code_challenge_method.as_deref() != Some("S256") {
        return bad_request("code_challenge_method must be S256");
    }
    let Some(redirect_uri) = query.redirect_uri.as_deref() else {
        return bad_request("redirect_uri is required");
    };
    let Some(challenge) = query.code_challenge.as_deref() else {
        return bad_request("code_challenge is required");
    };
    let resource = match check_resource(&service, query.resource.as_deref()) {
        Ok(resource) => resource,
        Err(message) => return oauth_error(OAuthError::InvalidTarget(message)),
    };
    let pending = match service.store().begin_authorization(
        client_id,
        redirect_uri,
        query.state.as_deref(),
        challenge,
        resource.as_deref(),
    ) {
        Ok(pending) => pending,
        Err(error) => return bad_request(&error.description()),
    };
    let Some(client) = service.store().client_name(client_id) else {
        return bad_request("the client is not registered");
    };
    page_response(approval_page(
        &client,
        redirect_uri,
        &pending.id,
        query.scope.as_deref(),
    ))
}

/// Follows the person's answer once they have given one.
///
/// The browser navigates here on its own (a link or the page's refresh), which
/// is what keeps the approval form out of the cross-origin redirect: no form
/// ever targets the client's callback.
async fn authorize_pending(
    State(service): State<Arc<OutsideAgents>>,
    Query(query): Query<PendingQuery>,
) -> Response {
    let Some(request) = query.request.as_deref() else {
        return bad_request("request is required");
    };
    match service.store().approval_outcome(request) {
        Some(outcome) => no_store(Redirect::to(&outcome.location).into_response()),
        None => match service.store().pending(request) {
            Some(pending) => {
                let client = service
                    .store()
                    .client_name(&pending.client_id)
                    .unwrap_or_else(|| "An MCP client".into());
                page_response(waiting_page(&client, &pending.redirect_uri, request))
            }
            None => no_store(
                (
                    StatusCode::GONE,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    "this sign-in expired. Start it again from your MCP client.",
                )
                    .into_response(),
            ),
        },
    }
}

#[derive(Debug, Deserialize)]
struct PendingQuery {
    request: Option<String>,
}

fn page_response(html: String) -> Response {
    let mut response = Html(html).into_response();
    let headers = response.headers_mut();
    if let Ok(value) = "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
        .parse()
    {
        headers.insert(header::CONTENT_SECURITY_POLICY, value);
    }
    if let Ok(value) = "DENY".parse() {
        headers.insert(header::X_FRAME_OPTIONS, value);
    }
    if let Ok(value) = "no-referrer".parse() {
        headers.insert(header::REFERRER_POLICY, value);
    }
    no_store(response)
}

fn bad_request(message: &str) -> Response {
    no_store(
        (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            message.to_owned(),
        )
            .into_response(),
    )
}

#[derive(Debug, Deserialize)]
struct TokenForm {
    grant_type: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

async fn token(State(service): State<Arc<OutsideAgents>>, Form(form): Form<TokenForm>) -> Response {
    let Some(client_id) = form.client_id.as_deref() else {
        return oauth_error(OAuthError::InvalidClient);
    };
    // The aggregate limiter already covered this call; this bucket exists only
    // for a client the daemon has registered.
    if service.store().client_exists(client_id)
        && let Err(seconds) = service.check_limit(&format!("token:{client_id}"))
    {
        return rate_limited(seconds);
    }
    let pair = match form.grant_type.as_deref() {
        Some("authorization_code") => {
            let (Some(code), Some(verifier)) =
                (form.code.as_deref(), form.code_verifier.as_deref())
            else {
                return oauth_error(OAuthError::InvalidRequest(
                    "authorization_code needs code and code_verifier".into(),
                ));
            };
            // RFC 8707: a resource named now must be one this daemon serves, and
            // the store then checks it against the code's own binding.
            let resource = match check_resource(&service, form.resource.as_deref()) {
                Ok(resource) => resource,
                Err(message) => return oauth_error(OAuthError::InvalidTarget(message)),
            };
            service.store().exchange_code(
                code,
                verifier,
                client_id,
                form.redirect_uri.as_deref(),
                resource.as_deref(),
            )
        }
        Some("refresh_token") => match form.refresh_token.as_deref() {
            // A refresh names the audience it will use; omitting it means this
            // daemon's own. The store compares that with the grant's binding.
            Some(token) => {
                let resource = match check_resource(&service, form.resource.as_deref()) {
                    Ok(resource) => resource,
                    Err(message) => return oauth_error(OAuthError::InvalidTarget(message)),
                };
                service
                    .store()
                    .refresh(token, client_id, resource.as_deref())
            }
            None => {
                return oauth_error(OAuthError::InvalidRequest(
                    "refresh_token grant needs refresh_token".into(),
                ));
            }
        },
        Some(other) => {
            return oauth_error(OAuthError::InvalidRequest(format!(
                "unsupported grant_type `{other}`"
            )));
        }
        None => {
            return oauth_error(OAuthError::InvalidRequest("grant_type is required".into()));
        }
    };
    match pair {
        Ok(pair) => no_store(
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                json!({
                    "access_token": pair.access_token,
                    "token_type": "Bearer",
                    "expires_in": pair.expires_in,
                    "refresh_token": pair.refresh_token,
                    "scope": pair.level.scope(),
                    "resource": pair.resource,
                })
                .to_string(),
            )
                .into_response(),
        ),
        Err(error) => oauth_error(error),
    }
}

#[derive(Debug, Deserialize)]
struct RevokeForm {
    token: String,
    #[serde(default)]
    #[allow(dead_code)]
    client_id: Option<String>,
}

async fn revoke(
    State(service): State<Arc<OutsideAgents>>,
    Form(form): Form<RevokeForm>,
) -> Response {
    // RFC 7009: an unknown token still gets 200. A store that could not write
    // the revocation is a different thing: reporting success would tell the
    // client its token is dead while a restart would bring it back.
    match service.store().revoke_token(&form.token) {
        Ok(()) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            "{}",
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "application/json")],
            json!({
                "error": "server_error",
                "error_description": error.description(),
            })
            .to_string(),
        )
            .into_response(),
    }
}

async fn mcp_post(
    State(service): State<Arc<OutsideAgents>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    service.handle_mcp(headers, body).await
}

async fn mcp_get(State(service): State<Arc<OutsideAgents>>, headers: HeaderMap) -> Response {
    outside_get(&service, headers).await
}

/// Answers `GET /mcp` for an outside token: no server-initiated stream.
pub async fn outside_get(service: &OutsideAgents, headers: HeaderMap) -> Response {
    if !service.enabled() {
        return super::outside::outside_disabled();
    }
    let Some(token) = super::outside::bearer_token(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "connect with OAuth first",
        )
            .into_response();
    };
    if service.verify_grant(token).is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "the access token is invalid, expired or revoked",
        )
            .into_response();
    }
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST, DELETE")],
        "h4rn's outside MCP endpoint does not open a server-initiated stream",
    )
        .into_response()
}

async fn mcp_delete(State(service): State<Arc<OutsideAgents>>, headers: HeaderMap) -> Response {
    outside_delete(&service, headers).await
}

/// Revokes the bearer token on `DELETE /mcp` when it is an outside token.
pub async fn outside_delete(service: &OutsideAgents, headers: HeaderMap) -> Response {
    if !service.enabled() {
        return super::outside::outside_disabled();
    }
    if let Some(token) = super::outside::bearer_token(&headers)
        && let Err(error) = service.store().revoke_token(token)
    {
        // The revocation did not reach disk. Saying 204 would tell the client
        // it is disconnected while a restart would reconnect it.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("the revocation could not be saved: {error}"),
        )
            .into_response();
    }
    (StatusCode::NO_CONTENT, "").into_response()
}

fn page_shell(title: &str, refresh: Option<&str>, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>{title}</title>
{refresh}
<style>
:root {{ color-scheme: light dark; --paper:#f7f6f3; --graphite:#1c1c1a; --muted:#6b6a66; --hair:#e1dfda; --blue:#2563eb; --card:#ffffff; }}
@media (prefers-color-scheme: dark) {{ :root {{ --paper:#171715; --graphite:#f1efe9; --muted:#a3a19a; --hair:#302f2b; --card:#1f1f1c; }} }}
* {{ box-sizing: border-box; }}
body {{ margin:0; background:var(--paper); color:var(--graphite); font:400 14px/22px Geist, ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; }}
main {{ max-width: 34rem; margin: 8vh auto; padding: 0 1.25rem; }}
h1 {{ font-size: 17px; line-height: 22px; margin: 0 0 .5rem; font-weight: 600; letter-spacing: -.01em; }}
.lead {{ margin: 0 0 .35rem; }}
.where {{ color: var(--muted); font-size: 12px; line-height: 16px; margin: 0 0 1.75rem; overflow-wrap: anywhere; }}
code {{ font: 12.5px/19px ui-monospace, SFMono-Regular, Menlo, monospace; background: var(--card); border:1px solid var(--hair); border-radius: 5px; padding: 1px 5px; }}
.step {{ background: var(--card); border: 1px solid var(--hair); border-radius: 8px; padding: .85rem .95rem; margin: 0 0 1rem; }}
.step .t {{ font-weight: 600; }}
.step .d {{ color: var(--muted); font-size: 12px; line-height: 16px; }}
.note {{ color: var(--muted); font-size: 12px; line-height: 16px; margin-top: 2rem; border-top: 1px solid var(--hair); padding-top: 1rem; }}
a {{ color: var(--blue); }}
</style>
</head>
<body>
<main>
{body}
</main>
</body>
</html>"#,
        title = html_escape(title),
        refresh = refresh.unwrap_or(""),
        body = body,
    )
}

/// The page a client's browser lands on. It grants nothing; it explains.
fn approval_page(
    client_name: &str,
    redirect_uri: &str,
    request: &str,
    scope: Option<&str>,
) -> String {
    let name = html_escape(client_name);
    let origin = html_escape(&origin_of(redirect_uri).unwrap_or_else(|| redirect_uri.to_owned()));
    let request = html_escape(request);
    let scope_note = match scope.filter(|value| !value.is_empty()) {
        Some(scope) => format!(
            " It asked for the scope <code>{}</code>.",
            html_escape(scope)
        ),
        None => String::new(),
    };
    page_shell(
        &format!("Connect {client_name} to h4rn"),
        Some(&format!(
            r#"<meta http-equiv="refresh" content="2; url=/oauth/authorize/pending?request={}">"#,
            html_escape(request.as_str())
        )),
        &format!(
            r#"<h1>Connect {name} to h4rn?</h1>
<p class="lead"><strong>{name}</strong> is asking to work with h4rn on this machine. That name is
unverified: the app chose it when it registered.</p>
<p class="where">If you approve, this browser returns to <code>{origin}</code>.{scope_note}</p>
<div class="step">
<div class="t">Approve this sign-in in h4rn</div>
<div class="d">Open h4rn → Settings → Sharing &amp; access. The request appears at the top, with the
same callback, and nothing is granted until you pick a level there.</div>
</div>
<p class="where">This page refreshes itself. If nothing happens,
<a href="/oauth/authorize/pending?request={request}">continue</a>.</p>
<p class="note">Anyone who can reach h4rn on this machine still needs your h4rn session to approve.
A client cannot approve itself.</p>"#
        ),
    )
}

/// The page shown while the request waits for the person.
fn waiting_page(client_name: &str, redirect_uri: &str, request: &str) -> String {
    let name = html_escape(client_name);
    let origin = html_escape(&origin_of(redirect_uri).unwrap_or_else(|| redirect_uri.to_owned()));
    let request = html_escape(request);
    page_shell(
        &format!("Waiting for h4rn to approve {client_name}"),
        Some(&format!(
            r#"<meta http-equiv="refresh" content="2; url=/oauth/authorize/pending?request={}">"#,
            html_escape(request.as_str())
        )),
        &format!(
            r#"<h1>Waiting for h4rn</h1>
<p class="lead">Approve <strong>{name}</strong> in h4rn → Settings → Sharing &amp; access. The
browser returns to <code>{origin}</code> as soon as you do, or when you deny it.</p>
<p class="where">Still waiting… <a href="/oauth/authorize/pending?request={request}">Check again</a></p>"#
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_approval_page_escapes_the_client_name() {
        let page = approval_page(
            "<script>alert(1)</script>",
            "http://localhost:1455/cb",
            "req-1",
            Some(""),
        );
        assert!(!page.contains("<script>alert(1)</script>"));
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }

    #[test]
    fn an_origin_must_be_exactly_one_validly_serialised_value() {
        assert!(origin_is_well_formed("http://127.0.0.1:1455"));
        assert!(origin_is_well_formed("https://example.com"));
        // A browser never sends these, and a lenient parse used to accept them.
        assert!(!origin_is_well_formed("null"));
        assert!(!origin_is_well_formed("http://127.0.0.1:1455/"));
        assert!(!origin_is_well_formed("http://127.0.0.1:1455/evil"));
        assert!(!origin_is_well_formed("http://127.0.0.1:1455?x=1"));
        assert!(!origin_is_well_formed("https://user:pass@example.com"));
        assert!(!origin_is_well_formed("http://a.example http://b.example"));
        assert!(!origin_is_well_formed("HTTP://EXAMPLE.COM"));
        assert!(!origin_is_well_formed(""));
    }

    #[test]
    fn a_header_that_is_missing_is_absent_but_a_broken_one_is_not() {
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(single_header(&headers, "origin"), Ok(None));
        headers.insert(
            axum::http::header::ORIGIN,
            "http://127.0.0.1:1455".parse().unwrap(),
        );
        assert_eq!(
            single_header(&headers, "origin"),
            Ok(Some("http://127.0.0.1:1455"))
        );
        headers.append(
            axum::http::header::ORIGIN,
            "http://evil.example".parse().unwrap(),
        );
        assert_eq!(single_header(&headers, "origin"), Err(()));

        let mut bytes = axum::http::HeaderMap::new();
        bytes.insert(
            axum::http::header::ORIGIN,
            axum::http::HeaderValue::from_bytes(b"http://\xff").expect("opaque value"),
        );
        assert_eq!(single_header(&bytes, "origin"), Err(()));
    }

    #[test]
    fn another_machine_is_not_a_local_peer() {
        let local: SocketAddr = "127.0.0.1:5000".parse().expect("addr");
        let other: SocketAddr = "192.168.1.20:5000".parse().expect("addr");
        assert!(peer_is_local(local));
        assert!(!peer_is_local(other));
    }

    #[test]
    fn the_approval_pages_follow_the_typography_scale() {
        for page in [
            approval_page("Claude Code", "http://localhost:1455/cb", "req-1", None),
            waiting_page("Claude Code", "http://localhost:1455/cb", "req-1"),
        ] {
            assert!(!page.contains("eyebrow"), "{page}");
            assert!(!page.contains("text-transform: uppercase"), "{page}");
            // DESIGN.md: prose 14px/22px 400, heading 17px/22px 600.
            assert!(page.contains("font:400 14px/22px"), "{page}");
            assert!(
                page.contains("font-size: 17px; line-height: 22px"),
                "{page}"
            );
        }
    }

    #[test]
    fn the_approval_page_shows_the_callback_origin_and_no_form() {
        let page = approval_page("Claude Code", "http://localhost:1455/cb", "req-1", None);
        assert!(page.contains("http://localhost:1455"));
        assert!(!page.contains("<form"));
        assert!(!page.contains("full_access"));
        assert!(page.contains("unverified"));
    }

    #[test]
    fn loopback_hosts_are_the_only_ones_admitted() {
        assert!(host_is_loopback("127.0.0.1:47800"));
        assert!(host_is_loopback("localhost:47800"));
        assert!(host_is_loopback("[::1]:47800"));
        assert!(host_is_loopback("localhost"));
        assert!(!host_is_loopback("evil.example"));
        assert!(!host_is_loopback("127.0.0.1.evil.example:47800"));
        assert!(!host_is_loopback("192.168.1.5:47800"));
    }

    #[test]
    fn origins_must_match_the_listener_exactly() {
        let listener = Some("http://127.0.0.1:47800");
        assert!(origin_is_listener("http://127.0.0.1:47800", listener));
        assert!(origin_is_listener("http://127.0.0.1:47800/", listener));
        // Another loopback service is another site, even on this machine.
        assert!(!origin_is_listener("http://127.0.0.1:1455", listener));
        assert!(!origin_is_listener("http://localhost:47800", listener));
        assert!(!origin_is_listener("https://127.0.0.1:47800", listener));
        assert!(!origin_is_listener("https://evil.example", listener));
        assert!(!origin_is_listener("not a url", listener));
    }

    #[test]
    fn only_same_origin_and_no_fetch_site_are_admitted() {
        assert!(fetch_site_allowed("same-origin"));
        assert!(fetch_site_allowed("none"));
        assert!(!fetch_site_allowed("same-site"));
        assert!(!fetch_site_allowed("cross-site"));
        assert!(!fetch_site_allowed(""));
    }
}
