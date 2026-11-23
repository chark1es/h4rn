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

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, Request, State};
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

/// Refuses a request that did not come from this machine's own page.
///
/// A DNS-rebinding page reaches the loopback port with its own `Host`; any other
/// site's page arrives with another `Origin` or a `Sec-Fetch-Site` of
/// `cross-site` or `same-site`. All are refused before a handler runs, so a page
/// the person happens to visit cannot register a client or spend a code.
pub async fn loopback_guard(
    State(service): State<Arc<OutsideAgents>>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    if !host.is_some_and(host_is_loopback) {
        return refused("this endpoint is only reachable through h4rn's loopback address");
    }
    let listener = service.base_url();
    if let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        && !origin_is_listener(origin, listener.as_deref())
    {
        return refused("a request from another site cannot change anything in h4rn");
    }
    if let Some(site) = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
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

/// Why the client's `resource` is not one this daemon serves.
fn check_resource(
    service: &OutsideAgents,
    resource: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(resource) = resource else {
        return Ok(None);
    };
    let mcp = service.mcp_url();
    if mcp.is_empty() {
        return Ok(None);
    }
    if resource.trim() != mcp {
        return Err(format!(
            "`resource` must name this daemon's MCP endpoint ({mcp})"
        ));
    }
    Ok(Some(mcp))
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
            Some(token) => service
                .store()
                .refresh(token, client_id, form.resource.as_deref()),
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
body {{ margin:0; background:var(--paper); color:var(--graphite); font:15px/1.55 Geist, ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; }}
main {{ max-width: 34rem; margin: 8vh auto; padding: 0 1.25rem; }}
.eyebrow {{ color: var(--muted); font-size: 12px; letter-spacing: .08em; text-transform: uppercase; margin: 0 0 1.25rem; }}
h1 {{ font-size: 1.5rem; line-height: 1.25; margin: 0 0 .5rem; font-weight: 600; letter-spacing: -.01em; }}
.lead {{ margin: 0 0 .35rem; }}
.where {{ color: var(--muted); font-size: 13px; margin: 0 0 1.75rem; overflow-wrap: anywhere; }}
code {{ font: 12.5px ui-monospace, SFMono-Regular, Menlo, monospace; background: var(--card); border:1px solid var(--hair); border-radius: 5px; padding: 1px 5px; }}
.step {{ background: var(--card); border: 1px solid var(--hair); border-radius: 8px; padding: .85rem .95rem; margin: 0 0 1rem; }}
.step .t {{ font-weight: 550; }}
.step .d {{ color: var(--muted); font-size: 13px; }}
.note {{ color: var(--muted); font-size: 12.5px; margin-top: 2rem; border-top: 1px solid var(--hair); padding-top: 1rem; }}
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
            r#"<p class="eyebrow">h4rn · Sharing &amp; access</p>
<h1>Connect {name} to h4rn?</h1>
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
            r#"<p class="eyebrow">h4rn · Sharing &amp; access</p>
<h1>Waiting for h4rn</h1>
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
