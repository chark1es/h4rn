//! The local WebSocket server.
//!
//! `GET /healthz` is unauthenticated liveness for supervisors and `doctor`.
//! `GET /ws` requires the local access token and then carries the typed
//! protocol of `h4rn-protocol` (ADR-001). JSON frames are requests, responses
//! and ordered events; binary frames are reserved for PTY output and blobs.
//!
//! Resilience rules:
//! - every connection has its own bounded outbound queue;
//! - a caller that falls behind is disconnected with an event carrying the last
//!   sequence it saw, so it can reconnect and `GetThread` from there;
//! - a slow writer never blocks the thread manager or another client.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header::SEC_WEBSOCKET_PROTOCOL};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, serve};
use futures::{Sink, SinkExt, Stream, StreamExt};
use h4rn_protocol::{
    BINARY_HEADER_LEN, BinaryFrame, BinaryKind, ClientFrame, ClientRequest, DeviceId, DeviceInfo,
    Event, MAX_WEBHOOK_BODY_BYTES, PROTOCOL_VERSION, PairScope, ProtoError, ProtoErrorCode,
    ResponseBody, SeqEvent, ServerFrame, Topic,
};
use h4rn_transport::server::{AcceptOutcome, Link, LinkError, SecureService, SessionSender};
use h4rn_transport::session::AppFrame;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

use crate::auth::{TokenStore, authorize};
use crate::blobs::{BlobError, BlobStore, UploadOwner};
use crate::forge::ForgeService;
use crate::fs::{FsError, FsService, MAX_RESPONSE_BYTES, file_fits_frame, omit_file_payload};
use crate::hosts::HostInventory;
use crate::mcp::McpService;
use crate::remote::{RemoteAudit, SessionOrigin};
use crate::terminal::{Attached, OpenSpec, QueryReply, TerminalManager, run_viewer};
use crate::threads::ThreadManager;

mod forward;
mod handlers;

/// Capacity of one connection's outbound queue.
pub const OUTBOUND_CAPACITY: usize = 128;

/// How long one frame may take to reach a client before it is considered dead.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

/// Shared state for the axum router.
#[derive(Clone)]
pub struct AppState {
    /// Thread manager and live sessions.
    pub manager: Arc<ThreadManager>,
    /// Delegated child threads and their budgets (ADR-038).
    pub subagents: Arc<crate::subagents::SubagentManager>,
    /// Tracked git worktrees: subagent, thread and harness-made.
    pub worktrees: Arc<crate::worktrees::WorktreeTracker>,
    /// Host inventory.
    pub hosts: Arc<HostInventory>,
    /// Daemon-owned PTY terminals.
    pub terminals: Arc<TerminalManager>,
    /// Content-addressed attachment blobs (ADR-008).
    pub blobs: Arc<BlobStore>,
    /// Local access token.
    pub tokens: TokenStore,
    /// Workspace-scoped filesystem and git service for IDE mode.
    pub fs: Arc<FsService>,
    /// Forge accounts, clients and watched-PR checks polling.
    pub forge: Arc<ForgeService>,
    /// Thread ↔ pull request links and the PR watch.
    pub pr_links: Arc<crate::pr_links::PrLinkService>,
    /// Connected Linear workspaces, teams and issue reads.
    pub linear: Arc<crate::linear::LinearService>,
    /// Thread ↔ issue links.
    pub issue_links: Arc<crate::issue_links::IssueLinkService>,
    /// Skill registry, installers and connect sync (ADR-034, ADR-035).
    pub skills: Arc<h4rn_skills::SkillsService>,
    /// Noise-encrypted multipath transport, when enabled.
    pub secure: Option<Arc<SecureService>>,
    /// Remote-action audit trail, when the daemon is signed in to connect.
    pub remote: Option<Arc<RemoteAudit>>,
    /// Persisted device registry: the only source of remote authorisation.
    pub devices: Arc<crate::remote::DeviceRegistry>,
    /// Daemon-owned network listener and its persisted setting.
    pub network: Arc<crate::network_access::NetworkAccess>,
    /// CLIProxyAPI profiles and routing (ADR-015).
    pub proxy: Arc<h4rn_proxy::ProxyService>,
    /// The daemon-owned MCP hub (ADR-006).
    pub mcp: Arc<McpService>,
    /// Durable metering, limits and auto-resume (ADR-024).
    pub meter: Arc<h4rn_meter::MeterHub>,
    /// Harness CLI versions and updates (ADR-043).
    pub harness_versions: Arc<crate::harness_versions::HarnessVersionService>,
    /// User-configured local ACP agents.
    pub acp_agents: Arc<crate::acp_agents::AcpAgentService>,
    /// Registered sidebar projects.
    pub projects: Arc<crate::projects::ProjectService>,
    /// Title generation: coding-agent CLI or a configured text generator.
    pub text_generation: Arc<crate::text_generation::TextGenerationService>,
    /// Guest links and messages waiting for the owner (ADR-044).
    pub guest: Arc<crate::guest::GuestService>,
    /// Scheduled tasks and webhook automations.
    pub schedules: Arc<crate::schedules::ScheduleService>,
    /// Server boot time, for `/healthz`.
    pub started: Instant,
}

/// Which socket accepted a request.
///
/// Routing is explicit per listener: a webhook path is a bearer secret, so only
/// the loopback bind and binds the operator named carry it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ListenerKind {
    /// The primary bind (`--bind`).
    Primary,
    /// An extra address the operator named (`--extra-bind`).
    ExtraBind,
    /// The address discovered on the tailnet by Tailscale auto mode.
    Tailnet,
    /// The opt-in LAN listener from Settings → Network access.
    NetworkAccess,
}

impl ListenerKind {
    /// Whether this listener serves `/webhooks/{secret}`.
    ///
    /// Only the primary bind and an explicitly configured extra bind do. The
    /// tailnet address and the LAN listener carry the encrypted session routes
    /// only, so a webhook secret never leaves the listeners the operator chose.
    pub fn serves_webhooks(self) -> bool {
        matches!(self, Self::Primary | Self::ExtraBind)
    }

    /// The router this listener serves.
    ///
    /// Which guest routes exist is decided from the address the listener
    /// actually bound, not from the flag that created it: `--bind 0.0.0.0` is
    /// still the primary listener, but it is not a loopback address.
    pub fn router(self, state: AppState, bound: SocketAddr) -> Router {
        match self {
            Self::Primary | Self::ExtraBind => operator_router(state, bound),
            Self::Tailnet => core_router(state),
            Self::NetworkAccess => network_router(state),
        }
    }
}

/// Builds the router for a listener the operator chose.
///
/// It carries the webhook endpoint because the operator chose where it
/// listens. `/secure` is always registered so a remote client gets a clear
/// "not enabled" response instead of a 404, while plain WebSocket access stays
/// loopback-only (ADR-009). Direct guest routes are mounted only where the
/// bound address can answer this machine: loopback, or the wildcard address,
/// which still accepts a loopback connection.
fn operator_router(state: AppState, bound: SocketAddr) -> Router {
    let app = core_routes().route("/webhooks/{secret}", post(webhook_post));
    let app = if serves_direct_guest(bound) {
        app.merge(guest_router())
    } else {
        app
    };
    app.with_state(state)
}

/// Whether an address is this machine's own loopback.
///
/// Every address in `127.0.0.0/8` and `::1` qualifies, and an IPv4-mapped
/// address counts as the IPv4 address it names. Listener selection and guest
/// URL validation both use this one rule, so a daemon bound to `127.0.0.2`
/// serves its guest page and a link to it is accepted.
pub(crate) fn is_loopback_ip(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

/// Whether a listener bound to `bound` may serve the direct guest routes.
///
/// A direct guest link is a bearer token in a URL over plaintext HTTP, so the
/// socket must be one only this machine can reach: a loopback bind, or the
/// wildcard address, whose loopback connection is the same socket. The guest
/// handlers repeat the check on the peer address, so a wildcard bind still
/// refuses a LAN peer. A concrete non-loopback bind has no direct guest path;
/// Connect is the way to share from there. The address is canonicalised first,
/// so an IPv4-mapped bind such as `[::ffff:127.0.0.1]` is judged as the IPv4
/// loopback it names.
fn serves_direct_guest(bound: SocketAddr) -> bool {
    let ip = bound.ip().to_canonical();
    is_loopback_ip(ip) || ip.is_unspecified()
}

/// The loopback URL a listener's direct guest routes answer on, when it has any.
///
/// A wildcard bind answers a loopback connection on the same port, so the link
/// names `127.0.0.1` for an unspecified IPv4 bind and `[::1]` for an IPv6
/// loopback or unspecified bind. An IPv4-mapped bind names the IPv4 address it
/// really bound, because that is where the socket answers. A concrete
/// non-loopback bind has no direct path at all.
pub(crate) fn direct_guest_base(bound: SocketAddr) -> Option<String> {
    if !serves_direct_guest(bound) {
        return None;
    }
    let host = match bound.ip().to_canonical() {
        IpAddr::V4(ip) if ip.is_loopback() => ip.to_string(),
        IpAddr::V4(_) => "127.0.0.1".to_owned(),
        IpAddr::V6(ip) if ip.is_loopback() => "[::1]".to_owned(),
        IpAddr::V6(_) => "[::1]".to_owned(),
    };
    Some(format!("http://{host}:{}/guest", bound.port()))
}

/// Whether a peer is this machine, including an IPv4-mapped address.
///
/// A dual-stack `[::]` bind reports an IPv4 connection as `::ffff:127.0.0.1`,
/// which is not itself a loopback address. Canonicalizing first makes that
/// connection the loopback peer it really is.
fn peer_is_loopback(peer: SocketAddr) -> bool {
    is_loopback_ip(peer.ip())
}

/// Every route except the webhook endpoint.
///
/// The tailnet address serves this so the typed socket still works there while
/// a webhook bearer path does not.
fn core_router(state: AppState) -> Router {
    core_routes().with_state(state)
}

/// The route table before a state is attached.
fn core_routes() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/ws", get(ws_handler))
        .route("/secure", get(secure_handler))
        .route("/secure-mcp", get(secure_mcp_handler))
        .route(
            "/mcp",
            get(mcp_http_get)
                .post(mcp_http_post)
                .delete(mcp_http_delete),
        )
}

/// Router for the network listener: liveness and the encrypted routes.
///
/// Plaintext `/ws` is deliberately absent, so a device on the LAN cannot open
/// an unencrypted session even if it learns the access token. `/mcp` is absent
/// too: the hub endpoint carries a bearer token and stays loopback-only.
///
/// Guest routes are absent for the same reason a plaintext `/ws` is: a guest
/// link is a bearer token in a URL, and serving it over a LAN listener would
/// put that token on the network in the clear. Direct links live on loopback
/// only; a teammate on another machine reaches the link through Connect.
pub fn network_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/secure", get(secure_handler))
        .route("/secure-mcp", get(secure_mcp_handler))
        .with_state(state)
}

/// Capacity of a guest message body, including base64 overhead.
const GUEST_BODY_LIMIT: usize = 16 * 1024 * 1024;

/// Routes the guest page reads.
///
/// A message carries up to 10 MB of files, which is about 13.4 MB as base64 in
/// JSON, so these routes raise axum's default body limit for themselves.
fn guest_router() -> Router<AppState> {
    Router::new()
        .route("/guest/{token}", get(guest_page_handler))
        .route("/guest/{token}/state", get(guest_state_handler))
        .route(
            "/guest/{token}/state/{since}",
            get(guest_state_since_handler),
        )
        .route("/guest/{token}/messages", post(guest_message_handler))
        .route("/guest/{token}/blob/{id}", get(guest_blob_handler))
        .layer(DefaultBodyLimit::max(GUEST_BODY_LIMIT))
}

/// Streamable-HTTP MCP endpoint for harness sessions (ADR-006).
async fn mcp_http_post(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    crate::mcp::http_post(state.mcp.http_state(), headers, body).await
}

async fn mcp_http_get(State(state): State<AppState>) -> Response {
    crate::mcp::http_get(state.mcp.http_state()).await
}

async fn mcp_http_delete(State(state): State<AppState>, headers: HeaderMap) -> Response {
    crate::mcp::http_delete(state.mcp.http_state(), headers).await
}

// ---------------------------------------------------------------------------
// Guest links (ADR-044)
// ---------------------------------------------------------------------------

/// The guest page for one link token.
async fn guest_page_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = guest_peer_refused(peer) {
        return response;
    }
    guest_exchange(
        &state,
        crate::guest::Method::Get,
        format!("/guest/{token}"),
        None,
        &headers,
        Vec::new(),
    )
    .await
}

/// The guest snapshot, with history from the start.
async fn guest_state_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = guest_peer_refused(peer) {
        return response;
    }
    guest_exchange(
        &state,
        crate::guest::Method::Get,
        format!("/guest/{token}/state"),
        None,
        &headers,
        Vec::new(),
    )
    .await
}

/// The guest snapshot after one sequence.
async fn guest_state_since_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((token, since)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = guest_peer_refused(peer) {
        return response;
    }
    guest_exchange(
        &state,
        crate::guest::Method::Get,
        format!("/guest/{token}/state/{since}"),
        None,
        &headers,
        Vec::new(),
    )
    .await
}

/// One guest message with its files.
///
/// The link and its browser binding are checked from the headers *before* the
/// body is read: an unauthenticated upload is refused without buffering it.
async fn guest_message_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    if let Some(response) = guest_peer_refused(peer) {
        return response;
    }
    let input = guest_input(
        crate::guest::Method::Post,
        format!("/guest/{token}/messages"),
        None,
        &headers,
        Vec::new(),
    );
    if let Err(output) = state.guest.check_submission(&input).await {
        return guest_output(output);
    }
    let body = match read_guest_body(body, GUEST_BODY_LIMIT).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    // The submission runs in its own task. A browser that disconnects while
    // the row is being written must not cancel it half-way: that would leave a
    // `reserving` row, its files and its quota behind until the next restart.
    let guest = Arc::clone(&state.guest);
    let task = tokio::spawn(async move {
        let mut input = input;
        input.body = body;
        guest.handle(input).await
    });
    match task.await {
        Ok(output) => guest_output(output),
        Err(error) => guest_output(crate::guest::GuestHttpOutput::text(
            500,
            &format!("The message could not be submitted: {error}"),
        )),
    }
}

/// One attachment the thread shows.
async fn guest_blob_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((token, id)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = guest_peer_refused(peer) {
        return response;
    }
    // Forward the value unchanged: the guest surface decides what `1` and `0`
    // mean, and a bare `?thumbnail=0` must not turn into a thumbnail request.
    let suffix = query
        .get("thumbnail")
        .map(|value| format!("?thumbnail={value}"))
        .unwrap_or_default();
    guest_exchange(
        &state,
        crate::guest::Method::Get,
        format!("/guest/{token}/blob/{id}{suffix}"),
        None,
        &headers,
        Vec::new(),
    )
    .await
}

/// Refuses a direct guest request from a peer that is not this machine.
///
/// The direct routes are mounted only on a loopback or wildcard bind, but a
/// wildcard socket also accepts LAN peers. This is the second guard, the same
/// one `/ws` makes. It answers 404 like an unmounted route: a network peer
/// must not be able to confirm that a guest path exists.
fn guest_peer_refused(peer: SocketAddr) -> Option<Response> {
    (!peer_is_loopback(peer))
        .then(|| guest_output(crate::guest::GuestHttpOutput::text(404, "Not found.")))
}

/// Runs one exchange through the guest surface and renders its answer.
pub(crate) async fn guest_exchange(
    state: &AppState,
    method: crate::guest::Method,
    path: String,
    public_prefix: Option<String>,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> Response {
    let output = state
        .guest
        .handle(guest_input(method, path, public_prefix, headers, body))
        .await;
    guest_output(output)
}

/// Builds the guest surface's input from request headers.
pub(crate) fn guest_input(
    method: crate::guest::Method,
    path: String,
    public_prefix: Option<String>,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> crate::guest::GuestHttpInput {
    let cookie = headers
        .get(axum::http::header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(guest_cookie);
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    crate::guest::GuestHttpInput {
        method,
        path,
        public_prefix,
        cookie,
        content_type: header("content-type"),
        origin: header("origin"),
        host: header("host"),
        fetch_site: header("sec-fetch-site"),
        body,
    }
}

/// Reads a guest request body, refusing it as soon as it passes the limit.
async fn read_guest_body(body: axum::body::Body, limit: usize) -> Result<Vec<u8>, Box<Response>> {
    use futures::StreamExt as _;
    let mut stream = body.into_data_stream();
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return Err(Box::new(guest_output(crate::guest::GuestHttpOutput::text(
                400,
                "The message body could not be read.",
            ))));
        };
        if buffer.len().saturating_add(chunk.len()) > limit {
            return Err(Box::new(guest_output(crate::guest::GuestHttpOutput::text(
                413,
                "A message holds at most 10 MB of files.",
            ))));
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(buffer)
}

/// Extracts the guest cookie from a `Cookie` header.
pub(crate) fn guest_cookie(header: &str) -> Option<String> {
    header.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == h4rn_protocol::GUEST_COOKIE).then(|| value.to_owned())
    })
}

/// Turns a guest answer into an HTTP response.
pub(crate) fn guest_output(output: crate::guest::GuestHttpOutput) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(output.content_type),
    );
    for (name, value) in output.headers {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(name),
            axum::http::HeaderValue::try_from(value),
        ) {
            headers.insert(name, value);
        }
    }
    if let Some(cookie) = output.set_cookie
        && let Ok(value) = axum::http::HeaderValue::try_from(cookie)
    {
        headers.insert(axum::http::header::SET_COOKIE, value);
    }
    (
        StatusCode::from_u16(output.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        headers,
        output.body,
    )
        .into_response()
}

/// One webhook delivery.
///
/// The path is the secret, the method is POST (axum answers every other method
/// with 405), and the body is capped below. The path is checked and a rate
/// slot taken *before* the body is read, the query is parsed or the headers
/// are copied, so a wrong path costs no body read. The run starts on its own
/// task, so a harness that takes seconds to start never delays the 202.
async fn webhook_post(
    State(state): State<AppState>,
    Path(secret): Path<String>,
    request: axum::http::Request<axum::body::Body>,
) -> Response {
    serve_webhook_request(&state.schedules, secret, request).await
}

/// The webhook handler's logic, without axum's shared state.
///
/// Exposed to the crate so a test can hand it a body that fails if it is ever
/// read and prove a wrong path is refused before that happens.
pub(crate) async fn serve_webhook_request(
    schedules: &crate::schedules::ScheduleService,
    secret: String,
    request: axum::http::Request<axum::body::Body>,
) -> Response {
    let admission = match schedules.begin_webhook(&secret).await {
        crate::schedules::WebhookStart::Start(admission) => *admission,
        crate::schedules::WebhookStart::NotFound => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "no webhook task owns this path" })),
            )
                .into_response();
        }
        crate::schedules::WebhookStart::RateLimited => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "error": "too many deliveries; try again later" })),
            )
                .into_response();
        }
        crate::schedules::WebhookStart::Skipped => {
            return (
                StatusCode::ACCEPTED,
                Json(json!({
                    "status": "skipped",
                    "reason": "another delivery for this task was already starting",
                })),
            )
                .into_response();
        }
        crate::schedules::WebhookStart::Rejected(reason) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": reason }))).into_response();
        }
    };

    // Only now read the request: the secret is known good and a delivery slot
    // is held.
    let query: std::collections::BTreeMap<String, String> = request
        .uri()
        .query()
        .map(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default();
    let headers = request
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_owned()))
        })
        .collect();
    let body = match axum::body::to_bytes(request.into_body(), MAX_WEBHOOK_BODY_BYTES + 1).await {
        Ok(body) if body.len() <= MAX_WEBHOOK_BODY_BYTES => {
            String::from_utf8_lossy(&body).into_owned()
        }
        Ok(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({ "error": "the body is larger than the webhook allows" })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({ "error": "the body is larger than the webhook allows" })),
            )
                .into_response();
        }
    };
    let request = crate::schedules::WebhookRequest {
        method: "POST".to_owned(),
        path: format!("{}{secret}", crate::schedules::WEBHOOK_PREFIX),
        query,
        headers,
        body,
        secret: Some(secret),
    };
    match schedules.deliver_webhook(admission, request).await {
        crate::schedules::WebhookOutcome::Accepted => {
            (StatusCode::ACCEPTED, Json(json!({ "status": "accepted" }))).into_response()
        }
        crate::schedules::WebhookOutcome::Skipped => {
            (StatusCode::ACCEPTED, Json(json!({ "status": "skipped" }))).into_response()
        }
        crate::schedules::WebhookOutcome::NotFound => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no webhook task owns this path" })),
        )
            .into_response(),
        crate::schedules::WebhookOutcome::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "too many deliveries; try again later" })),
        )
            .into_response(),
        crate::schedules::WebhookOutcome::Rejected(reason) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": reason }))).into_response()
        }
    }
}

/// Owner half of cross-machine MCP sharing (ADR-006).
///
/// The peer authenticates with the same Noise identity it uses for `/secure`;
/// the hub then applies the `server -> machine` grant and audits each call.
async fn secure_mcp_handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    let Some(service) = state.secure.clone() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "the secure transport is not enabled on this daemon",
            })),
        )
            .into_response();
    };
    let mcp = Arc::clone(&state.mcp);
    ws.on_upgrade(move |socket| async move {
        let link = Box::new(WsLink { socket });
        mcp.serve_secure_mcp(service, link).await;
    })
}

/// Unauthenticated liveness response.
pub async fn healthz(State(state): State<AppState>) -> Json<serde_json::Value> {
    let threads = state.manager.list_threads(&Default::default()).await.len();
    let active_turns = state.manager.active_turn_count().await;
    Json(json!({
        "status": "ok",
        "version": crate::version(),
        "threads": threads,
        "active_turns": active_turns,
        "uptime_ms": state.started.elapsed().as_millis() as u64,
    }))
}

async fn ws_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Plain WebSocket is unencrypted. It is only allowed on loopback; every
    // remote path must use `/secure`, where Noise authenticates and encrypts.
    if !peer_is_loopback(peer) {
        return (
            StatusCode::FORBIDDEN,
            Json(ProtoError::new(
                ProtoErrorCode::Unauthorized,
                "the plain WebSocket endpoint is loopback-only",
                "Connect remotely over /secure, which requires the Noise handshake",
            )),
        )
            .into_response();
    }

    let protocol_header = headers
        .get(SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok());

    let grant = match authorize(
        &state.tokens,
        query.get("token").map(String::as_str),
        protocol_header,
    ) {
        Ok(grant) => grant,
        Err(error) => return (StatusCode::UNAUTHORIZED, Json(error)).into_response(),
    };

    let upgrade = match grant.protocol {
        // Echo one of the offered subprotocols, which browsers require before
        // they accept the socket. `bearer, <token>` echoes `bearer`; the
        // `bearer.<token>` shape echoes the exact offered value.
        Some(protocol) => ws.protocols([protocol]),
        None => ws,
    };
    upgrade.on_upgrade(move |socket| handle_socket(socket, state, SessionOrigin::local()))
}

/// Upgrades to the Noise-encrypted multipath transport.
///
/// A client dials this route on every candidate path; each socket gets a fresh
/// handshake (ADR-010) and is bound to the logical session named in the packet
/// header. The decrypted application frames then flow through the same
/// [`handle_socket`] loop as plain WebSocket traffic.
async fn secure_handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    let Some(service) = state.secure.clone() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "the secure transport is not enabled on this daemon",
            })),
        )
            .into_response();
    };
    ws.on_upgrade(move |socket| async move {
        let link = Box::new(WsLink { socket });
        match service.accept(link).await {
            Ok(AcceptOutcome::New(handle)) => {
                if refuse_peer_daemon(&state, &handle.device) {
                    handle.into_parts().0.close();
                    return;
                }
                let mut origin = SessionOrigin::secure(handle.device);
                origin.device_id = state.devices.device_for_key(&handle.device);
                let (sender, frames) = handle.into_parts();
                handle_socket(SecureSocket { sender, frames }, state, origin).await;
            }
            // The path attached to a live session. Its application loop is
            // already running, so there is nothing to start here: returning
            // drops the raw socket and leaves the path task to drive it.
            Ok(AcceptOutcome::Resumed { session_id }) => {
                tracing::debug!(
                    session = %hex::encode(session_id),
                    "secure session resumed on a new path"
                );
            }
            Err(error) => tracing::warn!(%error, "secure transport handshake failed"),
        }
    })
}

/// Whether `key` is a registered peer daemon. Peer daemons authenticate on
/// `/secure-mcp` only: on the client protocol route they get no session at all,
/// whatever the allowlist says.
fn refuse_peer_daemon(state: &AppState, key: &[u8; h4rn_transport::keys::KEY_LEN]) -> bool {
    let peer = state
        .devices
        .authorization(key)
        .is_some_and(|auth| auth.role == crate::remote::Role::Host);
    if peer {
        tracing::warn!(
            key = %hex::encode(key),
            "refused a peer daemon key on the client route; peers may only use /secure-mcp"
        );
    }
    peer
}

/// Runs one accepted secure session through the normal protocol loop.
///
/// Both `/secure` and the relay connector call this, so a relayed session is
/// indistinguishable from a direct one after the Noise handshake. `origin`
/// carries the authenticated device identity for the audit trail.
pub(crate) async fn serve_secure_link(
    service: Arc<SecureService>,
    link: Box<dyn h4rn_transport::server::Link>,
    state: AppState,
    origin: SessionOrigin,
) {
    match service.accept(link).await {
        Ok(AcceptOutcome::New(handle)) => {
            if refuse_peer_daemon(&state, &handle.device) {
                handle.into_parts().0.close();
                return;
            }
            // Identity is the key the Noise handshake authenticated. A device id
            // the relay put in its open frame is only a claim: it is replaced by
            // the registry's answer for that key, even when they disagree.
            let mut origin = origin;
            origin.device_key = Some(handle.device);
            origin.device_id = state.devices.device_for_key(&handle.device);
            let (sender, frames) = handle.into_parts();
            handle_socket(SecureSocket { sender, frames }, state, origin).await;
        }
        Ok(AcceptOutcome::Resumed { session_id }) => {
            tracing::debug!(
                session = %hex::encode(session_id),
                "secure session resumed on a new path"
            );
        }
        Err(error) => tracing::warn!(%error, "secure transport handshake failed"),
    }
}

/// Adapts an axum WebSocket to the transport's packet-oriented [`Link`].
struct WsLink {
    socket: WebSocket,
}

#[async_trait::async_trait]
impl Link for WsLink {
    async fn send_packet(&mut self, packet: Vec<u8>) -> Result<(), LinkError> {
        self.socket
            .send(Message::Binary(packet.into()))
            .await
            .map_err(|error| LinkError::Transport(error.to_string()))
    }

    async fn recv_packet(&mut self) -> Result<Option<Vec<u8>>, LinkError> {
        loop {
            match self.socket.recv().await {
                Some(Ok(Message::Binary(bytes))) => return Ok(Some(bytes.to_vec())),
                Some(Ok(Message::Close(_))) | None => return Ok(None),
                // Text/ping frames are not transport packets; keep reading.
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(LinkError::Transport(error.to_string())),
            }
        }
    }
}

/// Presents an established secure session as a `Sink`/`Stream` of WebSocket
/// messages, so [`handle_socket`] is reused unchanged.
struct SecureSocket {
    sender: SessionSender,
    frames: mpsc::Receiver<AppFrame>,
}

impl Stream for SecureSocket {
    type Item = Result<Message, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.frames.poll_recv(cx) {
            Poll::Ready(Some(frame)) => Poll::Ready(Some(Ok(if frame.binary {
                Message::Binary(frame.payload.into())
            } else {
                Message::Text(String::from_utf8_lossy(&frame.payload).into_owned().into())
            }))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Sink<Message> for SecureSocket {
    type Error = std::io::Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        let this = self.get_mut();
        let frame = match item {
            Message::Text(text) => AppFrame::text(text.as_str()),
            Message::Binary(bytes) => AppFrame::binary(bytes.to_vec()),
            // The secure transport carries its own liveness pings; a WebSocket
            // ping from the application layer has no equivalent here.
            Message::Ping(_) | Message::Pong(_) => return Ok(()),
            Message::Close(_) => {
                this.sender.close();
                return Ok(());
            }
        };
        this.sender
            .send(frame)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::BrokenPipe, error.to_string()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().sender.close();
        Poll::Ready(Ok(()))
    }
}

/// One WebSocket message queued for a connection.
///
/// The writer task owns the socket and turns these into text or binary
/// messages, so terminal viewers can enqueue output without touching the sink.
#[derive(Debug)]
pub enum Outbound {
    /// A JSON protocol frame.
    Text(String),
    /// A framed binary message (PTY output, snapshot, blob chunk or ack).
    Binary(Vec<u8>),
}

struct Connection {
    state: AppState,
    hello_done: bool,
    topics: Arc<RwLock<Vec<Topic>>>,
    ready: Arc<AtomicBool>,
    last_seq: Arc<AtomicU64>,
    device: Option<DeviceInfo>,
    viewer_id: crate::terminal::ViewerId,
    tx: mpsc::Sender<Outbound>,
    /// Who opened this session, for the remote audit trail.
    origin: SessionOrigin,
    /// One filesystem watcher subscription per requested workspace.
    watches: HashMap<String, JoinHandle<()>>,
    /// Set when this connection must be dropped with a resume hint.
    overflowed: Arc<AtomicBool>,
    /// Wakes the writer loop when a forwarder overflows this connection.
    notify: Arc<Notify>,
}

impl Connection {
    /// The device id used for ownership and audit.
    ///
    /// A remote session's `Hello` is a claim. The identity comes from the key
    /// the Noise handshake authenticated: the registry's connect id for that
    /// key, or the key itself when the pairing has no connect id (a direct
    /// LAN/tailnet pairing). A loopback session keeps its `Hello` identity.
    fn device_id(&self) -> Option<DeviceId> {
        if self.origin.is_remote() {
            self.origin
                .device_id
                .clone()
                .or_else(|| self.origin.device_key.map(hex::encode))
                .map(DeviceId::new)
        } else {
            self.device.as_ref().map(|device| device.device_id.clone())
        }
    }

    /// Who may touch this session's blob uploads.
    ///
    /// A remote session is bound to the Noise key its handshake authenticated,
    /// never to a `Hello` claim or a connection; the same device can resume
    /// after reconnecting, but another device cannot. Loopback is the machine's
    /// owner.
    fn upload_owner(&self) -> Result<UploadOwner, ProtoError> {
        if !self.origin.is_remote() {
            return Ok(UploadOwner::Local);
        }
        self.origin
            .device_key
            .map(UploadOwner::Device)
            .ok_or_else(|| {
                ProtoError::new(
                    ProtoErrorCode::Unauthorized,
                    "this session has no authenticated device key",
                    "Pair the device again",
                )
            })
    }

    /// The transport frame bound for this session, when it has one.
    ///
    /// The plain WebSocket is loopback-only and has no frame bound of its own,
    /// so a large listing or editor read is delivered whole. The Noise
    /// transport closes a session whose frame is over its limit, so fs
    /// responses on a secure session are kept inside one frame.
    fn frame_limit(&self) -> Option<usize> {
        self.origin.is_remote().then_some(MAX_RESPONSE_BYTES)
    }

    /// Records one remote action, if this session came from another device.
    fn audit(
        &self,
        action: &str,
        target_type: Option<&str>,
        target_id: Option<&str>,
        metadata: serde_json::Value,
    ) {
        record_audit(
            &self.state,
            &self.origin,
            action,
            target_type,
            target_id,
            metadata,
        );
    }

    /// Runs one provider Test off the read loop.
    ///
    /// The provider may take its whole timeout to answer, and the connection
    /// must keep answering other requests meanwhile. The reply is framed here
    /// and sent when the work finishes; nothing about it depends on the loop.
    fn spawn_text_generator_test(&self, id: u64, request: ClientRequest) {
        let state = self.state.clone();
        let origin = self.origin.clone();
        let tx = self.tx.clone();
        let frame_limit = self.frame_limit();
        tokio::spawn(async move {
            let result = handlers::text_generation::test_off_loop(&state, &origin, request).await;
            let frame = match result {
                Ok(body) => response_frame(id, body, frame_limit),
                Err(error) => error_frame(id, error),
            };
            let _ = tx.send(Outbound::Text(frame)).await;
        });
    }
}

/// Records one remote action, when this daemon keeps an audit trail.
pub(crate) fn record_audit(
    state: &AppState,
    origin: &SessionOrigin,
    action: &str,
    target_type: Option<&str>,
    target_id: Option<&str>,
    metadata: serde_json::Value,
) {
    if let Some(audit) = &state.remote {
        audit.record(origin, action, target_type, target_id, metadata);
    }
}

async fn handle_socket<S, E>(socket: S, state: AppState, origin: SessionOrigin)
where
    S: Sink<Message, Error = E> + Stream<Item = Result<Message, E>> + Unpin + Send + 'static,
    E: std::fmt::Debug + Send + 'static,
{
    let (sink, mut stream) = socket.split();
    let (tx, rx) = mpsc::channel::<Outbound>(OUTBOUND_CAPACITY);
    let overflowed = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(Notify::new());
    let last_seq = Arc::new(AtomicU64::new(0));
    let topics = Arc::new(RwLock::new(vec![Topic::All]));
    let ready = Arc::new(AtomicBool::new(false));

    let writer = tokio::spawn(writer_loop(
        sink,
        rx,
        Arc::clone(&overflowed),
        Arc::clone(&notify),
        Arc::clone(&last_seq),
    ));

    let viewer_id = state.terminals.next_viewer_id();
    let mut connection = Connection {
        state,
        hello_done: false,
        topics,
        ready,
        last_seq,
        device: None,
        viewer_id,
        tx: tx.clone(),
        origin,
        watches: HashMap::new(),
        overflowed: Arc::clone(&overflowed),
        notify: Arc::clone(&notify),
    };
    let journal_forward = tokio::spawn(forward::forward_loop(
        connection.state.manager.subscribe(),
        forward::ForwardContext::for_connection(&connection),
        forward::ForwardConfig::JOURNAL,
        |event: crate::journal::WireEvent, _last| {
            Some(forward::ForwardFrame::durable(
                ServerFrame::Event {
                    seq: event.seq,
                    event: event.event,
                },
                event.seq,
            ))
        },
    ));
    let service_forwards = handlers::spawn_forwarders(&connection);

    // Requests run to completion in order: a handler owns steps that must not
    // be abandoned half-way (a turn marked active before its journal append,
    // a thread created before its session starts). Work that must outlive the
    // socket — a guest submission, Send or Dismiss — spawns its own task, so
    // closing the peer never cancels it.
    while let Some(message) = stream.next().await {
        let Ok(message) = message else {
            break;
        };
        match message {
            Message::Text(text) => {
                let Some(reply) = connection.handle_text(&text).await else {
                    continue;
                };
                if connection.send(Outbound::Text(reply)).await.is_err() {
                    break;
                }
            }
            Message::Binary(bytes) => {
                if let Some(reply) = connection.handle_binary(&bytes) {
                    if connection.send(reply).await.is_err() {
                        break;
                    }
                }
            }
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    journal_forward.abort();
    for handle in service_forwards {
        handle.abort();
    }
    // Detaching a socket never kills a terminal; it only removes this
    // connection's viewers so the PTY keeps running for the next attach.
    connection
        .state
        .terminals
        .detach_viewer(connection.viewer_id);
    connection.stop_watches();
    drop(connection);
    drop(tx);
    let _ = writer.await;
}

impl Connection {
    /// Aborts every filesystem watcher task owned by this connection.
    ///
    /// Watcher tasks hold a sender clone, so they would otherwise outlive the
    /// socket and keep pushing into a queue nobody reads.
    fn stop_watches(&mut self) {
        for (_, handle) in self.watches.drain() {
            handle.abort();
        }
    }

    /// Returns an error when the request names a host this daemon does not run.
    fn local_host_error(&self, host: &h4rn_protocol::HostId) -> Option<ProtoError> {
        if self.state.hosts.local_id() == *host {
            None
        } else {
            Some(ProtoError::new(
                ProtoErrorCode::Unsupported,
                "that host is not this daemon",
                "Open the IDE on the host that owns the workspace",
            ))
        }
    }
    async fn handle_text(&mut self, text: &str) -> Option<String> {
        let frame: ClientFrame = match serde_json::from_str(text) {
            Ok(frame) => frame,
            Err(error) => {
                return Some(error_frame(
                    0,
                    ProtoError::new(
                        ProtoErrorCode::InvalidRequest,
                        format!("frame is not valid client JSON: {error}"),
                        "Send a ClientFrame with an integer id and a tagged body",
                    ),
                ));
            }
        };
        let id = frame.id;
        let reply_id = id;
        let body = frame.body;

        if !self.hello_done && !matches!(&body, ClientRequest::Hello { .. }) {
            return Some(error_frame(
                id,
                ProtoError::new(
                    ProtoErrorCode::Unauthorized,
                    "the connection has not completed the Hello handshake",
                    "Send Hello with the device identity before other requests",
                ),
            ));
        }

        if let Err(error) =
            crate::remote::authorize_scopes(&self.state.devices, &self.origin, &body)
        {
            tracing::warn!(
                origin = %self.origin.label(),
                needs = body.required_scope().as_str(),
                "refused a request outside the device's scopes"
            );
            self.audit(
                "scope.denied",
                None,
                None,
                serde_json::json!({"needs": body.required_scope().as_str()}),
            );
            return Some(error_frame(id, error));
        }

        // A provider Test can take tens of seconds. Run it in its own task so
        // this connection keeps answering everything else, and send the framed
        // reply when it finishes.
        if matches!(&body, ClientRequest::TestTextGenerator { .. }) {
            self.spawn_text_generator_test(id, body);
            return None;
        }

        // A commit hook, a remote or a model can take a while. Answer git
        // actions from their own task so this socket keeps serving frames.
        if handlers::Service::of(&body) == Some(handlers::Service::GitActions)
            && handlers::git_actions::defers_reply(&body)
        {
            let state = self.state.clone();
            let origin = self.origin.clone();
            let tx = self.tx.clone();
            let frame_limit = self.frame_limit();
            tokio::spawn(async move {
                let frame =
                    match handlers::git_actions::handle_deferred(&state, &origin, body).await {
                        Ok(body) => response_frame(id, body, frame_limit),
                        Err(error) => error_frame(id, error),
                    };
                let _ = tx.send(Outbound::Text(frame)).await;
            });
            return None;
        }

        let result: Result<ResponseBody, ProtoError> = match handlers::Service::of(&body) {
            // A probe starts a process and can take seconds. It answers from its
            // own task, so this socket keeps serving frames meanwhile.
            Some(_) if handlers::acp_agents::defers_reply(&body) => {
                let state = self.state.clone();
                let tx = self.tx.clone();
                let frame_limit = self.frame_limit();
                tokio::spawn(async move {
                    let frame = match handlers::acp_agents::handle_deferred(&state, body).await {
                        Ok(body) => response_frame(id, body, frame_limit),
                        Err(error) => error_frame(id, error),
                    };
                    let _ = tx.send(Outbound::Text(frame)).await;
                });
                return None;
            }
            Some(service) => service.handle(self, body).await,
            None => match body {
                ClientRequest::Hello {
                    protocol_version,
                    device,
                } => {
                    if protocol_version != PROTOCOL_VERSION {
                        Err(ProtoError::new(
                            ProtoErrorCode::Unsupported,
                            format!(
                                "protocol version {protocol_version} is not supported (daemon speaks {PROTOCOL_VERSION})"
                            ),
                            "Update the client or the daemon so both speak the same protocol major",
                        ))
                    } else {
                        self.hello_done = true;
                        self.ready.store(true, Ordering::Relaxed);
                        self.device = Some(device.clone());
                        *self
                            .topics
                            .write()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = vec![Topic::All];
                        Ok(ResponseBody::Hello {
                            protocol_version: PROTOCOL_VERSION,
                            server_version: crate::version().to_owned(),
                            device_id: device.device_id,
                            daemon_machine_id: self
                                .state
                                .remote
                                .as_ref()
                                .and_then(|remote| remote.machine_id()),
                            local_host: self.state.hosts.local_id(),
                            hosts: self.state.hosts.summaries(),
                            daemon_timezone: crate::schedules::daemon_timezone().1,
                        })
                    }
                }
                ClientRequest::PairingConfirmations => Ok(ResponseBody::PairingConfirmations {
                    confirmations: self.state.devices.pending_confirmations(),
                }),
                ClientRequest::ConfirmPairing { device_id, confirm } => {
                    let (decided, persisted) = self
                        .state
                        .devices
                        .decide_pending(device_id.as_str(), confirm);
                    let result = match decided {
                        Some((key, true)) => {
                            if let Some(service) = &self.state.secure {
                                service.allow(key);
                            }
                            self.audit(
                                "pairing.confirmed",
                                Some("device"),
                                Some(device_id.as_str()),
                                json!({"key": hex::encode(key)}),
                            );
                            Ok(ResponseBody::Ack)
                        }
                        Some((key, false)) => {
                            if let Some(service) = &self.state.secure {
                                service.revoke(key);
                            }
                            self.audit(
                                "pairing.rejected",
                                Some("device"),
                                Some(device_id.as_str()),
                                json!({"key": hex::encode(key)}),
                            );
                            Ok(ResponseBody::Ack)
                        }
                        None => Err(ProtoError::new(
                            ProtoErrorCode::NotFound,
                            format!("no pending pairing for {device_id}"),
                            "Refresh Settings → Devices; the pairing may already be decided",
                        )),
                    };
                    match persisted {
                        Ok(()) => result,
                        Err(error) => Err(ProtoError::new(
                            ProtoErrorCode::Internal,
                            format!("the pairing decision could not be saved: {error}"),
                            "Free disk space or fix the data directory, then try again. A device that could not be revoked stays blocked in memory.",
                        )),
                    }
                }
                ClientRequest::ListHosts => Ok(ResponseBody::Hosts {
                    hosts: self.state.hosts.summaries(),
                }),
                ClientRequest::ListThreads { filter } => Ok(ResponseBody::Threads {
                    threads: self.state.manager.list_threads(&filter).await,
                }),
                ClientRequest::GetThread {
                    thread_id,
                    since_seq,
                } => self
                    .state
                    .manager
                    .get_thread(&thread_id, since_seq)
                    .await
                    .map(|snapshot| ResponseBody::Thread {
                        summary: snapshot.summary,
                        events: snapshot
                            .events
                            .into_iter()
                            .map(|event| SeqEvent {
                                seq: event.seq,
                                event: event.event,
                            })
                            .collect(),
                        next_seq: snapshot.next_seq,
                    })
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::CreateThread {
                    host,
                    harness,
                    agent_id,
                    workspace_path,
                    model,
                    prompt,
                    attachments,
                    runtime_mode,
                    reasoning_effort,
                    fast,
                    worktree,
                } => {
                    let request = crate::threads::NewThread {
                        thread_id: None,
                        parent_thread_id: None,
                        subagent_id: None,
                        host,
                        harness,
                        agent_id,
                        workspace_path: workspace_path.clone(),
                        model,
                        prompt,
                        attachments,
                        runtime_mode,
                        reasoning_effort,
                        fast,
                        worktree,
                    };
                    match self.state.manager.create_thread(request).await {
                        Ok(created) => {
                            // A thread's workspace becomes an authorised IDE root.
                            // This is the only way a workspace is added at runtime,
                            // so IDE mode cannot reach a path the user never opened.
                            // Authorise the folder the thread actually uses: an ACP
                            // agent with a fixed directory replaces the one sent.
                            let workspace_path = created.summary.workspace_path.clone();
                            if let Err(error) = self.state.fs.allow_root(&workspace_path) {
                                tracing::warn!(%workspace_path, %error, "IDE root not authorised");
                            }
                            self.audit(
                                "thread.created",
                                Some("thread"),
                                Some(created.thread_id.as_ref()),
                                json!({"workspace": workspace_path}),
                            );
                            Ok(ResponseBody::ThreadCreated {
                                thread_id: created.thread_id,
                                turn_id: created.turn_id,
                                summary: created.summary,
                            })
                        }
                        Err(error) => Err(error.into_proto()),
                    }
                }
                ClientRequest::SendMessage {
                    thread_id,
                    text,
                    attachments,
                } => self
                    .state
                    .manager
                    .send_message(&thread_id, text, attachments)
                    .await
                    .map(|accepted| ResponseBody::MessageAccepted {
                        thread_id: accepted.thread_id,
                        turn_id: accepted.turn_id,
                        seq: accepted.seq,
                    })
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::Interrupt { thread_id } => self
                    .state
                    .manager
                    .interrupt(&thread_id)
                    .await
                    .map(|()| ResponseBody::Ack)
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::RespondApproval {
                    thread_id,
                    approval_id,
                    decision,
                    option_id,
                } => self
                    .state
                    .manager
                    .respond_approval(
                        &thread_id,
                        &approval_id,
                        decision,
                        option_id,
                        self.device_id(),
                    )
                    .await
                    .map(|()| ResponseBody::Ack)
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::SetThreadTab {
                    thread_id,
                    tab,
                    pinned,
                } => {
                    let result = self
                        .state
                        .manager
                        .set_thread_tab(&thread_id, tab, pinned)
                        .await
                        .map(|_| ResponseBody::Ack)
                        .map_err(crate::threads::ManagerError::into_proto);
                    // Settling is h4rn's archive: the guest link ends and its
                    // waiting files go with it (ADR-044).
                    if result.is_ok() && tab == h4rn_protocol::ThreadTab::Settled {
                        if let Err(error) = self.state.guest.thread_settled(&thread_id).await {
                            tracing::warn!(%error, thread = %thread_id.as_ref(), "could not end the guest link on settle");
                        }
                    }
                    result
                }
                ClientRequest::SetThreadRuntimeMode { thread_id, mode } => self
                    .state
                    .manager
                    .set_thread_runtime_mode(&thread_id, mode)
                    .await
                    .map(|_| ResponseBody::Ack)
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::SetThreadModel { thread_id, model } => self
                    .state
                    .manager
                    .set_thread_model(&thread_id, model)
                    .await
                    .map(|_| ResponseBody::Ack)
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::SetThreadReasoningEffort { thread_id, effort } => self
                    .state
                    .manager
                    .set_thread_reasoning_effort(&thread_id, effort)
                    .await
                    .map(|_| ResponseBody::Ack)
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::SetThreadFast { thread_id, fast } => self
                    .state
                    .manager
                    .set_thread_fast(&thread_id, fast)
                    .await
                    .map(|_| ResponseBody::Ack)
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::Snooze { thread_id, until } => self
                    .state
                    .manager
                    .snooze(&thread_id, until)
                    .await
                    .map(|_| ResponseBody::Ack)
                    .map_err(crate::threads::ManagerError::into_proto),
                ClientRequest::Subscribe { topics, since_seq } => {
                    *self
                        .topics
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = topics;
                    // Hold the manager's append lock across read + enqueue +
                    // ready so no event falls between replay and live, and so
                    // the journal head is the exact replay boundary.
                    let guard = self.state.manager.lock_append().await;
                    match since_seq {
                        None => {
                            let head = self.state.manager.journal_head().await;
                            match head {
                                Ok(replay_through) => {
                                    self.last_seq.store(replay_through, Ordering::Relaxed);
                                    self.ready.store(true, Ordering::Relaxed);
                                    drop(guard);
                                    Ok(ResponseBody::Subscribed { replay_through })
                                }
                                Err(error) => {
                                    drop(guard);
                                    Err(error.into_proto())
                                }
                            }
                        }
                        Some(since) => {
                            let replay = self.state.manager.replay_global_since(since).await;
                            let head = match replay {
                                Ok(events) => {
                                    for event in events {
                                        let frame = ServerFrame::Event {
                                            seq: event.seq,
                                            event: event.event.clone(),
                                        };
                                        if let Ok(text) = serde_json::to_string(&frame)
                                            && self.send(Outbound::Text(text)).await.is_err()
                                        {
                                            break;
                                        }
                                    }
                                    self.state.manager.journal_head().await
                                }
                                Err(error) => Err(error),
                            };
                            match head {
                                Ok(replay_through) => {
                                    self.last_seq.store(replay_through, Ordering::Relaxed);
                                    self.ready.store(true, Ordering::Relaxed);
                                    drop(guard);
                                    Ok(ResponseBody::Subscribed { replay_through })
                                }
                                Err(error) => {
                                    drop(guard);
                                    Err(error.into_proto())
                                }
                            }
                        }
                    }
                }
                ClientRequest::TerminalOpen {
                    thread_id,
                    host,
                    cwd,
                    shell,
                    args,
                    cols,
                    rows,
                } => match self.require_device() {
                    Ok(device) => {
                        let opened = self
                            .state
                            .terminals
                            .open(
                                OpenSpec {
                                    thread_id,
                                    host,
                                    cwd,
                                    shell,
                                    args,
                                    cols,
                                    rows,
                                },
                                &device,
                            )
                            .await;
                        if let Ok(terminal) = &opened {
                            self.audit(
                            "terminal.opened",
                            Some("terminal"),
                            Some(terminal.terminal_id.as_ref()),
                            json!({"thread_id": terminal.thread_id.as_ref().map(|id| id.as_ref())}),
                        );
                        }
                        opened
                            .map(|terminal| ResponseBody::TerminalOpened { terminal })
                            .map_err(crate::terminal::TerminalError::into_proto)
                    }
                    Err(error) => Err(error),
                },
                ClientRequest::TerminalAttach {
                    terminal_id,
                    cols,
                    rows,
                    reply_positions_v2,
                } => self
                    .state
                    .terminals
                    .attach(
                        &terminal_id,
                        self.viewer_id,
                        self.tx.clone(),
                        cols,
                        rows,
                        reply_positions_v2,
                    )
                    .map(|attached| {
                        let Attached {
                            terminal,
                            snapshot,
                            output,
                            viewer,
                            session,
                        } = attached;
                        let start_seq = snapshot.seq;
                        let ansi = snapshot.ansi;
                        // The viewer task frames output and applies backpressure
                        // until this connection closes or the terminal exits.
                        tokio::spawn(run_viewer(
                            session,
                            self.viewer_id,
                            viewer,
                            output,
                            start_seq,
                        ));
                        ResponseBody::TerminalAttached {
                            terminal,
                            snapshot: ansi,
                            start_seq,
                        }
                    })
                    .map_err(crate::terminal::TerminalError::into_proto),
                ClientRequest::TerminalList { thread_id } => Ok(ResponseBody::Terminals {
                    terminals: self.state.terminals.list(thread_id.as_ref()),
                }),
                ClientRequest::TerminalResize {
                    terminal_id,
                    cols,
                    rows,
                } => self
                    .state
                    .terminals
                    .resize(&terminal_id, self.viewer_id, cols, rows)
                    .map(|()| ResponseBody::Ack)
                    .map_err(crate::terminal::TerminalError::into_proto),
                ClientRequest::TerminalAck {
                    terminal_id,
                    up_to_chunk,
                } => self
                    .state
                    .terminals
                    .ack(&terminal_id, self.viewer_id, up_to_chunk)
                    .map(|()| ResponseBody::Ack)
                    .map_err(crate::terminal::TerminalError::into_proto),
                ClientRequest::TerminalClose { terminal_id } => self
                    .state
                    .terminals
                    .close(&terminal_id)
                    .await
                    .map(|()| ResponseBody::Ack)
                    .map_err(crate::terminal::TerminalError::into_proto),
                // ------------------------------------------------------------------
                // IDE mode: workspace-scoped filesystem, search, watch and git.
                // Every path is resolved daemon-side; the client never sends an
                // absolute target.
                // ------------------------------------------------------------------
                ClientRequest::FsList {
                    host,
                    workspace,
                    path,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    fs_block(fs, move |fs| fs.list(&workspace, &path))
                        .await
                        .map(|listing| ResponseBody::FsListing { listing })
                }
                ClientRequest::FsRead {
                    host,
                    workspace,
                    path,
                    bytes_base64,
                    max_bytes,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let frame_limit = self.frame_limit();
                    fs_block(fs, move |fs| {
                        fs.read_with(&workspace, &path, bytes_base64, max_bytes, frame_limit)
                    })
                    .await
                    .map(|file| ResponseBody::FsFile { file })
                }
                ClientRequest::FsWrite {
                    host,
                    workspace,
                    path,
                    content,
                    base_etag,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let device = self.device_id().map(|id| id.as_str().to_owned());
                    let audit_path = path.clone();
                    let audit_workspace = workspace.clone();
                    let written = fs_block(fs, move |fs| {
                        fs.write(
                            &workspace,
                            &path,
                            &content,
                            base_etag.as_deref(),
                            device.as_deref(),
                        )
                    })
                    .await;
                    if written.is_ok() {
                        self.audit(
                            "file.written",
                            Some("file"),
                            Some(&audit_path),
                            json!({"workspace": audit_workspace}),
                        );
                    }
                    written.map(|result| ResponseBody::FsWritten { result })
                }
                ClientRequest::FsCreate {
                    host,
                    workspace,
                    path,
                    kind,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let device = self.device_id().map(|id| id.as_str().to_owned());
                    fs_block(fs, move |fs| {
                        fs.create(&workspace, &path, kind, device.as_deref())
                    })
                    .await
                    .map(|entry| ResponseBody::FsEntryCreated { entry })
                }
                ClientRequest::FsRename {
                    host,
                    workspace,
                    from,
                    to,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let device = self.device_id().map(|id| id.as_str().to_owned());
                    fs_block(fs, move |fs| {
                        fs.rename(&workspace, &from, &to, device.as_deref())
                    })
                    .await
                    .map(|entry| ResponseBody::FsEntryCreated { entry })
                }
                ClientRequest::FsDelete {
                    host,
                    workspace,
                    path,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let device = self.device_id().map(|id| id.as_str().to_owned());
                    fs_block(fs, move |fs| {
                        fs.delete(&workspace, &path, device.as_deref())
                    })
                    .await
                    .map(|result| ResponseBody::FsDeleted { result })
                }
                ClientRequest::FsRestore {
                    host,
                    workspace,
                    trash_id,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let device = self.device_id().map(|id| id.as_str().to_owned());
                    fs_block(fs, move |fs| {
                        fs.restore(&workspace, &trash_id, device.as_deref())
                    })
                    .await
                    .map(|path| ResponseBody::FsRestored { path })
                }
                ClientRequest::FsSearch {
                    host,
                    workspace,
                    query,
                    regex,
                    case_sensitive,
                    glob,
                    max_results,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let searched = tokio::task::spawn_blocking(move || {
                        fs.search(
                            &workspace,
                            &query,
                            regex,
                            case_sensitive,
                            glob.as_deref(),
                            max_results,
                        )
                    })
                    .await;
                    let (search_id, mut rx) = match searched {
                        Ok(Ok(value)) => value,
                        Ok(Err(error)) => return Some(error_frame(id, error.into_proto())),
                        Err(error) => {
                            return Some(error_frame(
                                id,
                                ProtoError::plain(
                                    ProtoErrorCode::Internal,
                                    format!("search task failed: {error}"),
                                ),
                            ));
                        }
                    };

                    let tx = self.tx.clone();
                    let seq = self.last_seq.load(Ordering::Relaxed);
                    let response_id = search_id.clone();
                    tokio::spawn(async move {
                        while let Some(batch) = rx.recv().await {
                            let frame = ServerFrame::Event {
                                seq,
                                event: Event::FsSearchBatch {
                                    search_id: batch.search_id,
                                    results: batch.results,
                                    done: batch.done,
                                    truncated: batch.truncated,
                                },
                            };
                            let Ok(text) = serde_json::to_string(&frame) else {
                                continue;
                            };
                            if tx.send(Outbound::Text(text)).await.is_err() {
                                break;
                            }
                        }
                    });
                    Ok(ResponseBody::FsSearchStarted {
                        search_id: response_id,
                    })
                }
                ClientRequest::FsWatch {
                    host,
                    workspace,
                    paths,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    let requested = workspace.clone();
                    let watched = tokio::task::spawn_blocking(move || fs.watch(&requested)).await;
                    let root = match watched {
                        Ok(Ok(value)) => value,
                        Ok(Err(error)) => return Some(error_frame(id, error.into_proto())),
                        Err(error) => {
                            return Some(error_frame(
                                id,
                                ProtoError::plain(
                                    ProtoErrorCode::Internal,
                                    format!("watcher task failed: {error}"),
                                ),
                            ));
                        }
                    };
                    let canonical = root.display().to_string();
                    let mut rx = self.state.fs.subscribe();
                    let tx = self.tx.clone();
                    let seq = self.last_seq.load(Ordering::Relaxed);
                    let filter = paths.clone();
                    let event_workspace = canonical.clone();
                    if let Some(previous) = self.watches.remove(&workspace) {
                        previous.abort();
                    }
                    let handle = tokio::spawn(async move {
                        while let Ok(change) = rx.recv().await {
                            if change.workspace != root {
                                continue;
                            }
                            let inside = filter.is_empty()
                                || filter.iter().any(|path| {
                                    change.path == *path
                                        || change.path.starts_with(&format!("{path}/"))
                                });
                            if !inside {
                                continue;
                            }
                            let frame = ServerFrame::Event {
                                seq,
                                event: Event::FsChanged {
                                    workspace: event_workspace.clone(),
                                    path: change.path,
                                    change: change.change,
                                    at: chrono::Utc::now(),
                                },
                            };
                            let Ok(text) = serde_json::to_string(&frame) else {
                                continue;
                            };
                            if tx.send(Outbound::Text(text)).await.is_err() {
                                break;
                            }
                        }
                    });
                    self.watches.insert(workspace, handle);
                    Ok(ResponseBody::FsWatching {
                        workspace: canonical,
                        paths,
                    })
                }
                ClientRequest::FsUnwatch { workspace } => {
                    if let Some(handle) = self.watches.remove(&workspace) {
                        handle.abort();
                    }
                    Ok(ResponseBody::Ack)
                }
                ClientRequest::FsDiff {
                    host,
                    workspace,
                    path,
                    against,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    fs_block(fs, move |fs| fs.diff(&workspace, &path, &against))
                        .await
                        .map(|diff| ResponseBody::FsDiff { diff })
                }
                ClientRequest::FsTurnChanges {
                    host,
                    workspace,
                    thread_id,
                    turn_id,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    fs_block(fs, move |fs| {
                        fs.turn_changes(&workspace, &thread_id, &turn_id)
                    })
                    .await
                    .map(|changes| ResponseBody::FsTurnChanges { changes })
                }
                ClientRequest::FsDiffFiles {
                    host,
                    workspace,
                    against,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    fs_block(fs, move |fs| fs.diff_files(&workspace, &against))
                        .await
                        .map(|files| ResponseBody::FsDiffFiles { files })
                }
                ClientRequest::FsDiffCommits {
                    host,
                    workspace,
                    base,
                } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    fs_block(fs, move |fs| fs.diff_commits(&workspace, base.as_deref()))
                        .await
                        .map(|commits| ResponseBody::FsDiffCommits { commits })
                }
                ClientRequest::FsBranches { host, workspace } => {
                    if let Some(error) = self.local_host_error(&host) {
                        return Some(error_frame(id, error));
                    }
                    let fs = Arc::clone(&self.state.fs);
                    fs_block(fs, move |fs| fs.branches(&workspace))
                        .await
                        .map(|branches| ResponseBody::FsBranches { branches })
                }
                ClientRequest::BeginUpload {
                    name,
                    size,
                    mime_hint,
                } => match self.upload_owner() {
                    Ok(owner) => self
                        .state
                        .blobs
                        .begin_upload(owner, &name, size, mime_hint)
                        .map(|upload| ResponseBody::UploadStarted { upload })
                        .map_err(BlobError::into_proto),
                    Err(error) => Err(error),
                },
                ClientRequest::CommitUpload { upload_id, blake3 } => {
                    let committed = match self.upload_owner() {
                        Ok(owner) => {
                            self.state
                                .blobs
                                .commit_upload(owner, &upload_id, &blake3)
                                .await
                        }
                        Err(error) => return Some(error_frame(reply_id, error)),
                    };
                    committed
                        .map(|blob| ResponseBody::UploadCommitted { blob })
                        .map_err(BlobError::into_proto)
                }
                ClientRequest::AbortUpload { upload_id } => match self.upload_owner() {
                    Ok(owner) => self
                        .state
                        .blobs
                        .abort_upload(owner, &upload_id)
                        .map(|()| ResponseBody::Ack)
                        .map_err(BlobError::into_proto),
                    Err(error) => Err(error),
                },
                ClientRequest::QueryUpload { upload_id } => match self.upload_owner() {
                    Ok(owner) => self
                        .state
                        .blobs
                        .query_upload(owner, &upload_id)
                        .map(|upload| ResponseBody::UploadStatus { upload })
                        .map_err(BlobError::into_proto),
                    Err(error) => Err(error),
                },
                ClientRequest::GetBlob {
                    id,
                    range,
                    thumbnail,
                } => match self.state.blobs.download(&id, range, thumbnail).await {
                    Ok((download, bytes)) => {
                        let reply = response_frame(
                            reply_id,
                            ResponseBody::Blob {
                                download: download.clone(),
                            },
                            self.frame_limit(),
                        );
                        if self.send(Outbound::Text(reply)).await.is_err() {
                            return None;
                        }
                        for (seq, chunk) in
                            bytes.chunks(crate::blobs::DOWNLOAD_CHUNK_SIZE).enumerate()
                        {
                            let frame = BinaryFrame {
                                kind: BinaryKind::BlobChunk,
                                stream_id: download.stream_id,
                                seq: seq as u64,
                                payload: chunk.to_vec(),
                            }
                            .encode();
                            if self.send(Outbound::Binary(frame)).await.is_err() {
                                return None;
                            }
                        }
                        return None;
                    }
                    Err(error) => Err(error.into_proto()),
                },
                other => Err(not_implemented(&other)),
            },
        };

        Some(match result {
            Ok(body) => response_frame(id, body, self.frame_limit()),
            Err(error) => error_frame(id, error),
        })
    }

    fn require_device(&self) -> Result<DeviceInfo, ProtoError> {
        let mut device = self.device.clone().ok_or_else(|| {
            ProtoError::new(
                ProtoErrorCode::Unauthorized,
                "the connection has not completed the Hello handshake",
                "Send Hello with the device identity before opening a terminal",
            )
        })?;
        if self.origin.is_remote() {
            // Never let a remote client name the device that owns its
            // terminals or writes: use the registry/verified key instead.
            device.device_id = self.device_id().ok_or_else(|| {
                ProtoError::new(
                    ProtoErrorCode::Unauthorized,
                    "this session has no verified device identity",
                    "Pair the device again so the daemon can record its key",
                )
            })?;
        }
        Ok(device)
    }

    /// Handles one client-to-daemon binary frame.
    ///
    /// PTY input is the only binary frame a client sends today. It is applied
    /// directly and errors are reported as events because the frame carries no
    /// request id to answer.
    fn handle_binary(&mut self, bytes: &[u8]) -> Option<Outbound> {
        let seq = self.last_seq.load(Ordering::Relaxed);
        if !self.hello_done {
            return Some(Outbound::Text(binary_error_frame(
                seq,
                ProtoError::new(
                    ProtoErrorCode::Unauthorized,
                    "the connection has not completed the Hello handshake",
                    "Send Hello with the device identity before binary frames",
                ),
            )));
        }
        let frame = match BinaryFrame::decode(bytes) {
            Ok(frame) => frame,
            Err(error) => {
                return Some(Outbound::Text(binary_error_frame(
                    seq,
                    ProtoError::new(
                        ProtoErrorCode::InvalidRequest,
                        format!("malformed binary frame: {error}"),
                        format!(
                            "The binary header is {BINARY_HEADER_LEN} bytes; see the protocol docs"
                        ),
                    ),
                )));
            }
        };
        match frame.kind {
            // A viewer typing into a PTY.
            BinaryKind::PtyInput => {
                // Stream ids are small sequential integers, so possession of
                // one proves nothing: the same scope a TerminalAttach needs,
                // and a viewer this very connection attached, are required.
                if let Err(error) = crate::remote::authorize_scope(
                    &self.state.devices,
                    &self.origin,
                    PairScope::Terminal,
                ) {
                    return Some(Outbound::Text(binary_error_frame(seq, error)));
                }
                // `seq` is a flag word for input: bit 0 marks xterm's own
                // answer to a terminal query, bits 1-32 name the output chunk
                // that carried the query's first byte and bits 33-52 name the
                // query's index among the queries that start there. An old
                // client sends only bit 0; an old daemon ignored `seq`
                // altogether and wrote the frame as ordinary input, which
                // still works.
                let reply = if frame.seq & h4rn_protocol::PTY_INPUT_REPLY == 0 {
                    QueryReply::No
                } else {
                    match h4rn_protocol::pty_input_reply_position(frame.seq) {
                        Some(position) => QueryReply::At {
                            position,
                            ordinal: h4rn_protocol::pty_input_reply_ordinal(frame.seq),
                        },
                        None => QueryReply::Unpositioned,
                    }
                };
                match self.state.terminals.input_by_stream(
                    frame.stream_id,
                    self.viewer_id,
                    &frame.payload,
                    reply,
                ) {
                    Ok(()) => None,
                    Err(error) => Some(Outbound::Text(binary_error_frame(seq, error.into_proto()))),
                }
            }
            // An attachment uploader sending the next chunk. Acknowledge on the
            // same stream so the sender can advance without a round trip.
            BinaryKind::BlobChunk => {
                if let Err(error) = crate::remote::authorize_scope(
                    &self.state.devices,
                    &self.origin,
                    PairScope::Approve,
                ) {
                    return Some(Outbound::Text(binary_error_frame(seq, error)));
                }
                let owner = match self.upload_owner() {
                    Ok(owner) => owner,
                    Err(error) => return Some(Outbound::Text(binary_error_frame(seq, error))),
                };
                match self.state.blobs.append_chunk(
                    owner,
                    frame.stream_id,
                    frame.seq,
                    &frame.payload,
                ) {
                    Ok(_) => Some(Outbound::Binary(
                        BinaryFrame::new(BinaryKind::Ack, frame.stream_id, frame.seq).encode(),
                    )),
                    Err(error) => Some(Outbound::Text(binary_error_frame(seq, error.into_proto()))),
                }
            }
            other => Some(Outbound::Text(binary_error_frame(
                seq,
                ProtoError::new(
                    ProtoErrorCode::InvalidRequest,
                    format!("binary {other:?} frames are not accepted from clients"),
                    "Clients send only pty_input and blob_chunk; output is daemon-to-client",
                ),
            ))),
        }
    }

    async fn send(&self, message: Outbound) -> Result<(), mpsc::error::SendError<Outbound>> {
        self.tx.send(message).await
    }
}

async fn writer_loop<S>(
    mut sink: futures::stream::SplitSink<S, Message>,
    mut rx: mpsc::Receiver<Outbound>,
    overflowed: Arc<AtomicBool>,
    notify: Arc<Notify>,
    last_seq: Arc<AtomicU64>,
) where
    S: Sink<Message> + Unpin + Send + 'static,
{
    loop {
        tokio::select! {
            biased;
            _ = notify.notified(), if overflowed.load(Ordering::Relaxed) => {
                let hint = resume_hint(last_seq.load(Ordering::Relaxed));
                let _ = tokio::time::timeout(WRITE_TIMEOUT, sink.send(Message::Text(hint.into()))).await;
                break;
            }
            message = rx.recv() => {
                let Some(message) = message else { break; };
                match message {
                    Outbound::Text(text) => {
                        let sent = tokio::time::timeout(
                            WRITE_TIMEOUT,
                            sink.send(Message::Text(text.into())),
                        )
                        .await;
                        if sent.is_err() {
                            tracing::warn!("client write timed out; closing the connection");
                            break;
                        }
                    }
                    Outbound::Binary(bytes) => {
                        let sent = tokio::time::timeout(
                            WRITE_TIMEOUT,
                            sink.send(Message::Binary(bytes.into())),
                        )
                        .await;
                        if sent.is_err() {
                            tracing::warn!("client write timed out; closing the connection");
                            break;
                        }
                    }
                }
            }
        }
    }
    let _ = sink.send(Message::Close(None)).await;
}

fn resume_hint(last_seq: u64) -> String {
    let frame = ServerFrame::Event {
        seq: last_seq,
        event: Event::Error {
            thread_id: None,
            message: "this client fell behind the live event stream".into(),
            hint: Some(format!(
                "Reconnect, then call GetThread with since_seq {last_seq} to replay without gaps"
            )),
            retryable: true,
        },
    };
    serde_json::to_string(&frame).unwrap_or_else(|_| {
        r#"{"kind":"event","seq":0,"event":{"type":"error","thread_id":null,"message":"fell behind","hint":null,"retryable":true}}"#
            .to_owned()
    })
}

/// Runs one blocking filesystem operation off the async runtime.
async fn fs_block<T, F>(fs: Arc<FsService>, work: F) -> Result<T, ProtoError>
where
    T: Send + 'static,
    F: FnOnce(&FsService) -> Result<T, FsError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || work(&fs))
        .await
        .map_err(|error| {
            ProtoError::plain(
                ProtoErrorCode::Internal,
                format!("filesystem task failed: {error}"),
            )
        })?
        .map_err(FsError::into_proto)
}

fn response_frame(id: u64, body: ResponseBody, frame_limit: Option<usize>) -> String {
    // Only a secure session has a frame limit, and only an fs response can
    // carry a payload large enough to hit it. The read path already omits a
    // payload it knows will not fit; this is the last guard for a body built
    // or grown past that bound. A plain loopback WebSocket enforces nothing:
    // its thread listings and 2 MiB editor reads are delivered whole.
    let body = match (frame_limit, body) {
        (Some(limit), ResponseBody::FsFile { file }) if !file_fits_frame(&file, limit) => {
            ResponseBody::FsFile {
                file: omit_file_payload(file),
            }
        }
        (_, body) => body,
    };
    // The response is serialized exactly once, when it is sent.
    serde_json::to_string(&ServerFrame::Response {
        id,
        result: Ok(body),
    })
    .unwrap_or_default()
}

fn error_frame(id: u64, error: ProtoError) -> String {
    serde_json::to_string(&ServerFrame::Response {
        id,
        result: Err(error),
    })
    .unwrap_or_default()
}

fn binary_error_frame(seq: u64, error: ProtoError) -> String {
    serde_json::to_string(&ServerFrame::Event {
        seq,
        event: Event::Error {
            thread_id: None,
            message: error.message,
            hint: error.hint,
            retryable: false,
        },
    })
    .unwrap_or_default()
}

/// Names a request that this phase intentionally does not implement.
///
/// These are part of the wire contract but no code path serves them yet, so
/// they report [`ProtoErrorCode::NotImplemented`] rather than `Unsupported`
/// (which is reserved for unknown protocol versions and capabilities).
fn not_implemented(request: &ClientRequest) -> ProtoError {
    ProtoError::not_implemented(request_name(request))
}

fn request_name(request: &ClientRequest) -> &'static str {
    match request {
        ClientRequest::FsList { .. } => "fs_list",
        ClientRequest::FsRead { .. } => "fs_read",
        ClientRequest::FsWrite { .. } => "fs_write",
        ClientRequest::McpList => "mcp_list",
        ClientRequest::McpUpsert { .. } => "mcp_upsert",
        ClientRequest::McpRemove { .. } => "mcp_remove",
        ClientRequest::SkillsList { .. } => "skills_list",
        ClientRequest::SkillsSetEnabled { .. } => "skills_set_enabled",
        ClientRequest::UsageQuery { .. } => "usage_query",
        ClientRequest::ForgeListAccounts => "forge_list_accounts",
        ClientRequest::ForgeAddAccount { .. } => "forge_add_account",
        ClientRequest::ForgeRemoveAccount { .. } => "forge_remove_account",
        ClientRequest::ForgeDetectRepo { .. } => "forge_detect_repo",
        ClientRequest::ForgeListPrs { .. } => "forge_list_prs",
        ClientRequest::ForgeGetPr { .. } => "forge_get_pr",
        ClientRequest::ForgeGetPrDiff { .. } => "forge_get_pr_diff",
        ClientRequest::ForgeListPrComments { .. } => "forge_list_pr_comments",
        ClientRequest::ForgeListChecks { .. } => "forge_list_checks",
        ClientRequest::ForgeListPrCommits { .. } => "forge_list_pr_commits",
        ClientRequest::ForgeRerunChecks { .. } => "forge_rerun_checks",
        ClientRequest::ListFailoverOptions { .. } => "list_failover_options",
        ClientRequest::FailOver { .. } => "fail_over",
        ClientRequest::ForgeReply { .. } => "forge_reply",
        ClientRequest::ForgeComment { .. } => "forge_comment",
        ClientRequest::ForgeOpenPr { .. } => "forge_open_pr",
        ClientRequest::GitStatus { .. } => "git_status",
        ClientRequest::GitCommit { .. } => "git_commit",
        ClientRequest::GitPush { .. } => "git_push",
        ClientRequest::GenerateGitCommitMessage { .. } => "generate_git_commit_message",
        ClientRequest::GeneratePullRequestText { .. } => "generate_pull_request_text",
        ClientRequest::ForgeHandPrToAgent { .. } => "forge_hand_pr_to_agent",
        ClientRequest::GetNetworkAccess => "get_network_access",
        ClientRequest::SetNetworkAccess { .. } => "set_network_access",
        ClientRequest::CreateDirectPairing { .. } => "create_direct_pairing",
        ClientRequest::CancelDirectPairing => "cancel_direct_pairing",
        _ => "request",
    }
}

/// Serves the router on one listener until `shutdown` resolves.
pub async fn serve_listener(
    listener: TcpListener,
    app: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    // `ConnectInfo` is what lets `/ws` reject non-loopback peers and force
    // them onto the Noise transport.
    serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

/// Wraps a listener address for diagnostics.
pub fn bound_addr(listener: &TcpListener) -> Option<SocketAddr> {
    listener.local_addr().ok()
}

/// Spawns one axum server per listener and waits for a shutdown signal.
///
/// Each listener carries its own router, because which routes a socket serves
/// depends on the kind of listener it is (see [`ListenerKind`]).
pub(crate) async fn serve_all(
    listeners: Vec<(TcpListener, Router)>,
    shutdown: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    let (tx, rx) = watch::channel(false);
    let mut handles: Vec<JoinHandle<std::io::Result<()>>> = Vec::new();
    for (listener, app) in listeners {
        let mut rx = rx.clone();
        handles.push(tokio::spawn(async move {
            serve_listener(listener, app, async move {
                let _ = rx.changed().await;
            })
            .await
        }));
    }

    shutdown.await;
    let _ = tx.send(true);
    for handle in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "listener stopped with an error"),
            Err(error) => tracing::warn!(%error, "listener task failed"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_routes_are_exposed_only_on_operator_chosen_listeners() {
        assert!(ListenerKind::Primary.serves_webhooks());
        assert!(ListenerKind::ExtraBind.serves_webhooks());
        assert!(!ListenerKind::Tailnet.serves_webhooks());
        assert!(!ListenerKind::NetworkAccess.serves_webhooks());
    }

    #[test]
    fn direct_guest_routes_need_an_address_this_machine_can_reach_as_loopback() {
        assert!(serves_direct_guest("127.0.0.1:47800".parse().unwrap()));
        assert!(serves_direct_guest("[::1]:47800".parse().unwrap()));
        // The wildcard bind answers a loopback connection on the same socket;
        // the handlers' peer check keeps a LAN peer out.
        assert!(serves_direct_guest("0.0.0.0:47800".parse().unwrap()));
        assert!(serves_direct_guest("[::]:47800".parse().unwrap()));
        // Every address in 127.0.0.0/8 is loopback, not just 127.0.0.1.
        assert!(serves_direct_guest("127.0.0.2:47800".parse().unwrap()));
        // An IPv4-mapped bind names the IPv4 loopback it really is.
        assert!(serves_direct_guest(
            "[::ffff:127.0.0.1]:47800".parse().unwrap()
        ));
        assert!(serves_direct_guest(
            "[::ffff:127.0.0.2]:47800".parse().unwrap()
        ));
        // A concrete network address is not a place a bearer token may live.
        assert!(!serves_direct_guest("192.168.1.9:47800".parse().unwrap()));
        assert!(!serves_direct_guest("100.64.1.5:47800".parse().unwrap()));
    }

    #[test]
    fn a_direct_link_names_the_loopback_form_of_its_listener() {
        assert_eq!(
            direct_guest_base("127.0.0.1:47800".parse().unwrap()).as_deref(),
            Some("http://127.0.0.1:47800/guest")
        );
        // A concrete loopback bind names the address it really bound.
        assert_eq!(
            direct_guest_base("127.0.0.2:47800".parse().unwrap()).as_deref(),
            Some("http://127.0.0.2:47800/guest")
        );
        // An IPv4-mapped bind names the IPv4 loopback address, which is where
        // the socket answers; `[::1]` would have no listener.
        assert_eq!(
            direct_guest_base("[::ffff:127.0.0.1]:47800".parse().unwrap()).as_deref(),
            Some("http://127.0.0.1:47800/guest")
        );
        assert_eq!(
            direct_guest_base("[::ffff:127.0.0.2]:47800".parse().unwrap()).as_deref(),
            Some("http://127.0.0.2:47800/guest")
        );
        // The wildcard bind answers this machine on loopback, so the link uses
        // the address that actually reaches it.
        assert_eq!(
            direct_guest_base("0.0.0.0:47800".parse().unwrap()).as_deref(),
            Some("http://127.0.0.1:47800/guest")
        );
        assert_eq!(
            direct_guest_base("[::1]:47800".parse().unwrap()).as_deref(),
            Some("http://[::1]:47800/guest")
        );
        assert_eq!(
            direct_guest_base("[::]:47800".parse().unwrap()).as_deref(),
            Some("http://[::1]:47800/guest")
        );
        // A concrete network listener serves no direct guest route, so it must
        // not hand out a URL that cannot answer.
        assert_eq!(
            direct_guest_base("192.168.1.9:47800".parse().unwrap()),
            None
        );
    }

    #[test]
    fn an_ipv4_mapped_peer_is_this_machine() {
        assert!(peer_is_loopback("127.0.0.1:52000".parse().unwrap()));
        assert!(peer_is_loopback("[::1]:52000".parse().unwrap()));
        assert!(peer_is_loopback(
            "[::ffff:127.0.0.1]:52000".parse().unwrap()
        ));
        assert!(!peer_is_loopback("192.168.1.9:52000".parse().unwrap()));
        assert!(!peer_is_loopback(
            "[::ffff:192.168.1.9]:52000".parse().unwrap()
        ));
    }

    #[test]
    fn resume_hint_names_the_exact_sequence() {
        let hint = resume_hint(42);
        assert!(hint.contains("since_seq 42"));
        let frame: ServerFrame = serde_json::from_str(&hint).unwrap();
        assert!(matches!(frame, ServerFrame::Event { seq: 42, .. }));
    }

    #[test]
    fn later_phase_requests_are_named() {
        let request = ClientRequest::McpList;
        assert_eq!(request_name(&request), "mcp_list");
        let error = not_implemented(&request);
        assert_eq!(error.code, ProtoErrorCode::NotImplemented);
        assert!(error.message.contains("not implemented"));
        assert!(error.hint.unwrap().contains("mcp_list"));
    }

    fn thread_summary_with_title(title: String) -> h4rn_protocol::ThreadSummary {
        use h4rn_protocol::{AgentState, HarnessKind, HostId, ThreadId, ThreadSummary, ThreadTab};

        ThreadSummary {
            thread_id: ThreadId::new("th_large"),
            parent_thread_id: None,
            host: HostId::new("host_test"),
            harness: HarnessKind::ClaudeCode,
            agent_id: None,
            title,
            workspace_path: "/tmp/work".into(),
            branch: None,
            model: "test-model".into(),
            runtime_mode: h4rn_protocol::RuntimeMode::ApprovalRequired,
            reasoning_effort: None,
            reasoning_efforts: Vec::new(),
            models: Vec::new(),
            fast: false,
            tab: ThreadTab::Working,
            placed_by_user: false,
            pinned: false,
            snoozed_until: None,
            last_activity: chrono::Utc::now(),
            agent_state: AgentState::Done,
            usage: h4rn_protocol::Usage::default(),
            ttft_ms: None,
            context_window: None,
            context_fill_percent: None,
            needs_attention: false,
            pending_guest_requests: 0,
            unread: false,
            resume_at: None,
            limit: None,
        }
    }

    #[test]
    fn a_large_thread_listing_is_delivered_whole_on_the_plain_path() {
        use h4rn_protocol::ResponseBody;

        // The plain WebSocket is loopback-only and has no frame bound, so a
        // listing over the secure frame size must not be refused or truncated.
        let summary = thread_summary_with_title("t".repeat(600 * 1024));
        let frame = response_frame(
            7,
            ResponseBody::Threads {
                threads: vec![summary],
            },
            None,
        );
        assert!(
            frame.len() > h4rn_transport::DEFAULT_MAX_FRAME_BYTES,
            "the listing is meant to exceed one secure frame"
        );
        let decoded: ServerFrame = serde_json::from_str(&frame).expect("valid frame");
        match decoded {
            ServerFrame::Response {
                id: 7,
                result: Ok(ResponseBody::Threads { threads }),
            } => assert_eq!(threads[0].title.len(), 600 * 1024),
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn an_oversized_file_response_is_omitted_on_a_secure_session() {
        use h4rn_protocol::{FsFileContent, FsOmitted, ResponseBody};

        // A response built outside the read path (or one that escaped the
        // service's own bound) must still never reach the secure transport
        // oversized: `FrameTooLarge` closes the whole paired session.
        let file = FsFileContent {
            path: "report.html".into(),
            content: Some("\"".repeat(400 * 1024)),
            etag: Some("etag".into()),
            encoding: "utf-8".into(),
            size_bytes: 400 * 1024,
            binary: false,
            truncated: false,
            bytes_base64: None,
            bytes_omitted: None,
            content_omitted: None,
            modified_at: None,
            hint: None,
        };
        let frame = response_frame(7, ResponseBody::FsFile { file }, Some(MAX_RESPONSE_BYTES));
        assert!(
            frame.len() <= MAX_RESPONSE_BYTES,
            "{} bytes is over the {} byte response bound",
            frame.len(),
            MAX_RESPONSE_BYTES
        );
        let decoded: ServerFrame = serde_json::from_str(&frame).expect("valid frame");
        match decoded {
            ServerFrame::Response {
                id: 7,
                result: Ok(ResponseBody::FsFile { file }),
            } => {
                assert_eq!(file.content_omitted, Some(FsOmitted::TooLarge));
                assert!(file.content.is_none());
                assert!(file.truncated);
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn a_large_file_response_is_not_omitted_on_the_plain_path() {
        use h4rn_protocol::{FsFileContent, ResponseBody};

        // A 1 MiB editor read is inside the documented 2 MiB limit. With no
        // transport frame bound there is nothing to omit it for.
        let content = "a".repeat(1024 * 1024);
        let file = FsFileContent {
            path: "notes.txt".into(),
            content: Some(content.clone()),
            etag: Some("etag".into()),
            encoding: "utf-8".into(),
            size_bytes: content.len() as u64,
            binary: false,
            truncated: false,
            bytes_base64: None,
            bytes_omitted: None,
            content_omitted: None,
            modified_at: None,
            hint: None,
        };
        let frame = response_frame(7, ResponseBody::FsFile { file }, None);
        assert!(frame.len() > h4rn_transport::DEFAULT_MAX_FRAME_BYTES);
        let decoded: ServerFrame = serde_json::from_str(&frame).expect("valid frame");
        match decoded {
            ServerFrame::Response {
                id: 7,
                result: Ok(ResponseBody::FsFile { file }),
            } => {
                assert!(file.content_omitted.is_none());
                assert_eq!(file.content.as_deref(), Some(content.as_str()));
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}
