//! HTTP routes for outside agents: OAuth, discovery and the MCP endpoint.
//!
//! The approval page is served by the daemon itself because the browser that
//! opens it belongs to the outside client, not to the h4rn app. It is plain,
//! self-contained HTML: the person sees the client's name, where the code will
//! go, and the three levels before anything is granted.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use h4rn_protocol::OutsideAgentLevel;
use serde::Deserialize;
use serde_json::json;

use super::oauth::OAuthError;
use super::outside::OutsideAgents;
use super::url::html_escape;

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
        .route("/oauth/approve", post(approve))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
        .route(
            "/mcp/outside",
            post(mcp_post).get(mcp_get).delete(mcp_delete),
        )
        .with_state(service)
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
    if let Err(seconds) = service.check_limit("register") {
        return rate_limited(seconds);
    }
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
}

async fn authorize(
    State(service): State<Arc<OutsideAgents>>,
    Query(query): Query<AuthorizeQuery>,
) -> Response {
    let Some(client_id) = query.client_id.as_deref() else {
        return bad_request("client_id is required");
    };
    if let Err(seconds) = service.check_limit(&format!("authorize:{client_id}")) {
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
    let pending = match service.store().begin_authorization(
        client_id,
        redirect_uri,
        query.state.as_deref(),
        challenge,
    ) {
        Ok(pending) => pending,
        Err(error) => return bad_request(&error.description()),
    };
    let Some(client) = service.store().client_name(client_id) else {
        return bad_request("the client is not registered");
    };
    let requested_scope = query.scope.as_deref().unwrap_or("");
    page_response(approval_page(
        &client,
        redirect_uri,
        &pending.id,
        requested_scope,
    ))
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
struct ApproveForm {
    request: String,
    decision: String,
    #[serde(default)]
    level: Option<String>,
}

async fn approve(
    State(service): State<Arc<OutsideAgents>>,
    Form(form): Form<ApproveForm>,
) -> Response {
    if let Err(seconds) = service.check_limit(&format!("approve:{}", form.request)) {
        return rate_limited(seconds);
    }
    if form.decision == "deny" {
        return match service.store().deny(&form.request) {
            Ok(redirect) => Redirect::to(&redirect.location).into_response(),
            Err(error) => bad_request(&error.description()),
        };
    }
    let Some(level) = form.level.as_deref().and_then(OutsideAgentLevel::parse) else {
        return bad_request("choose read-only, supervised or full access");
    };
    match service.store().approve(&form.request, level) {
        Ok(redirect) => Redirect::to(&redirect.location).into_response(),
        Err(error) => bad_request(&error.description()),
    }
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
}

async fn token(State(service): State<Arc<OutsideAgents>>, Form(form): Form<TokenForm>) -> Response {
    let key = format!("token:{}", form.client_id.as_deref().unwrap_or("unknown"));
    if let Err(seconds) = service.check_limit(&key) {
        return rate_limited(seconds);
    }
    let Some(client_id) = form.client_id.as_deref() else {
        return oauth_error(OAuthError::InvalidClient);
    };
    let pair = match form.grant_type.as_deref() {
        Some("authorization_code") => {
            let (Some(code), Some(verifier), Some(redirect_uri)) = (
                form.code.as_deref(),
                form.code_verifier.as_deref(),
                form.redirect_uri.as_deref(),
            ) else {
                return oauth_error(OAuthError::InvalidRequest(
                    "authorization_code needs code, code_verifier and redirect_uri".into(),
                ));
            };
            service
                .store()
                .exchange_code(code, verifier, client_id, redirect_uri)
        }
        Some("refresh_token") => match form.refresh_token.as_deref() {
            Some(token) => service.store().refresh(token, client_id),
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
    if let Err(seconds) = service.check_limit("revoke") {
        return rate_limited(seconds);
    }
    // RFC 7009: invalid tokens still get 200.
    let _ = service.store().revoke_token(&form.token);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        "{}",
    )
        .into_response()
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
    if service.store().verify_access(token).is_none() {
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
    if let Some(token) = super::outside::bearer_token(&headers) {
        let _ = service.store().revoke_token(token);
    }
    (StatusCode::NO_CONTENT, "").into_response()
}

fn approval_page(
    client_name: &str,
    redirect_uri: &str,
    request: &str,
    requested_scope: &str,
) -> String {
    let name = html_escape(client_name);
    let redirect = html_escape(redirect_uri);
    let request = html_escape(request);
    let scope = html_escape(requested_scope);
    let checked_scope = match requested_scope {
        "supervised" => "supervised",
        "full" => "full_access",
        _ => "read_only",
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>Connect {name} to h4rn</title>
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
fieldset {{ border: 0; margin: 0 0 1.5rem; padding: 0; display: grid; gap: .5rem; }}
legend {{ color: var(--muted); font-size: 12px; letter-spacing: .06em; text-transform: uppercase; margin-bottom: .4rem; }}
label.opt {{ display: grid; grid-template-columns: auto 1fr; gap: .75rem; align-items: start; background: var(--card); border: 1px solid var(--hair); border-radius: 8px; padding: .8rem .9rem; cursor: pointer; }}
label.opt:has(input:checked) {{ border-color: var(--graphite); }}
input[type=radio] {{ margin-top: .2rem; accent-color: var(--graphite); }}
.t {{ font-weight: 550; }}
.d {{ color: var(--muted); font-size: 13px; }}
.actions {{ display: flex; align-items: center; gap: .6rem; margin-top: 1.5rem; }}
.grow {{ flex: 1; }}
button {{ font: inherit; border-radius: 7px; padding: .55rem .95rem; cursor: pointer; border: 1px solid var(--hair); }}
button.primary {{ background: var(--blue); border-color: var(--blue); color: #fff; font-weight: 550; }}
button.secondary {{ background: transparent; color: var(--graphite); }}
.note {{ color: var(--muted); font-size: 12.5px; margin-top: 2rem; border-top: 1px solid var(--hair); padding-top: 1rem; }}
</style>
</head>
<body>
<main>
<p class="eyebrow">h4rn · Sharing &amp; access</p>
<h1>Connect {name} to h4rn?</h1>
<p class="lead"><strong>{name}</strong> is asking to work with h4rn on this machine.</p>
<p class="where">After approval the browser returns to <code>{redirect}</code>{scope_note}.</p>
<form method="post" action="/oauth/approve">
<input type="hidden" name="request" value="{request}">
<fieldset>
<legend>What it may do</legend>
<label class="opt">
<input type="radio" name="level" value="read_only"{checked_read}>
<span><span class="t">Read only</span><br><span class="d">List projects, threads and providers and models, and read a thread. It cannot change anything.</span></span>
</label>
<label class="opt">
<input type="radio" name="level" value="supervised"{checked_supervised}>
<span><span class="t">Supervised</span><br><span class="d">Read, and start or message threads. Every thread it starts or steers runs at approval-required.</span></span>
</label>
<label class="opt">
<input type="radio" name="level" value="full_access"{checked_full}>
<span><span class="t">Full access</span><br><span class="d">Read, start and message threads at the mode it asks for, including unattended runs.</span></span>
</label>
</fieldset>
<div class="actions">
<button class="secondary" type="submit" name="decision" value="deny">Deny</button>
<span class="grow"></span>
<button class="primary" type="submit" name="decision" value="approve">Approve</button>
</div>
</form>
<p class="note">Only approve a sign-in you just started. Anyone who can open this page on this machine can approve it, and a code is returned only to the address above.</p>
</main>
</body>
</html>"#,
        scope_note = if scope.is_empty() {
            String::new()
        } else {
            format!(" with scope <code>{scope}</code>")
        },
        checked_read = if checked_scope == "read_only" {
            " checked"
        } else {
            ""
        },
        checked_supervised = if checked_scope == "supervised" {
            " checked"
        } else {
            ""
        },
        checked_full = if checked_scope == "full_access" {
            " checked"
        } else {
            ""
        },
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
            "",
        );
        assert!(!page.contains("<script>alert(1)</script>"));
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }
}
