//! Capability probes (ADR-015).
//!
//! Nothing here parses a version to decide a feature. The flow is:
//!
//! 1. `HEAD /healthz` for liveness.
//! 2. A keyless `GET /v0/management/debug`: 404 means the routes are absent,
//!    401/403 means they exist and the response headers identify the build.
//! 3. Authenticated route probes (`/usage-queue`, `/v8/config`,
//!    `/v8/performance`, `/api-key-usage`, `/auth-files`, quota providers).
//! 4. Header-selected model-shape probes (`/v1/models` twice, `/v1beta/models`).
//!
//! A 404 only means "absent" once step 2 established that management routes
//! are live at all; otherwise "disabled" and "absent" are indistinguishable.

use std::time::{Duration, Instant};

use chrono::Utc;
use h4rn_protocol::{
    ProxyCapabilities, ProxyDiagnostic, ProxyDiagnosticLevel, ProxyManagementAccess,
    ProxyTestReport,
};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Client, Response, StatusCode};
use serde_json::Value;

/// Timeout for one probe request.
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);

/// What to probe.
#[derive(Clone, Debug)]
pub struct ProbeTarget {
    /// Root URL without a trailing slash.
    pub base_url: String,
    /// Management key, when stored.
    pub management_key: Option<String>,
    /// Client API key, when stored.
    pub api_key: Option<String>,
}

impl ProbeTarget {
    /// Builds a target.
    pub fn new(
        base_url: impl Into<String>,
        management_key: Option<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            management_key,
            api_key,
        }
    }
}

/// Runs every probe and returns capabilities plus diagnostics.
pub async fn probe(http: &Client, target: &ProbeTarget) -> ProxyTestReport {
    // A profile may be entered with `/v1`; probes target the listener root.
    let mut normalized = target.clone();
    normalized.base_url = crate::overrides::root_base(&normalized.base_url);
    let target = &normalized;
    let mut diagnostics = Vec::new();
    let mut capabilities = ProxyCapabilities::unreachable(Utc::now());
    let started = Instant::now();
    let health = health(http, &target.base_url).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    if let Err(error) = health {
        diagnostics.push(error);
        return ProxyTestReport {
            capabilities,
            latency_ms,
            diagnostics,
        };
    }
    capabilities.reachable = true;

    probe_management(http, target, &mut capabilities, &mut diagnostics).await;
    probe_management_routes(http, target, &mut capabilities, &mut diagnostics).await;
    probe_models(http, target, &mut capabilities, &mut diagnostics).await;

    diagnostics.sort_by_key(|diagnostic| match diagnostic.level {
        ProxyDiagnosticLevel::Error => 0,
        ProxyDiagnosticLevel::Warning => 1,
        ProxyDiagnosticLevel::Info => 2,
    });

    ProxyTestReport {
        capabilities,
        latency_ms,
        diagnostics,
    }
}

async fn health(http: &Client, base_url: &str) -> Result<(), ProxyDiagnostic> {
    let url = format!("{base_url}/healthz");
    let response = match http.head(&url).timeout(PROBE_TIMEOUT).send().await {
        Ok(response) => response,
        // Some reverse proxies refuse HEAD; a GET is still cheap.
        Err(_) => futures_get(http, &url).await.map_err(|_| {
            ProxyDiagnostic::new(
                ProxyDiagnosticLevel::Error,
                "unreachable",
                "could not reach the proxy's /healthz endpoint",
                Some(
                    "Check the base URL, that CLIProxyAPI is running, and DNS/TLS reachability"
                        .into(),
                ),
            )
        })?,
    };
    if response.status().is_success() {
        Ok(())
    } else {
        Err(ProxyDiagnostic::new(
            ProxyDiagnosticLevel::Error,
            "unhealthy",
            format!("/healthz returned HTTP {}", response.status()),
            Some("Check the proxy's logs; h4rn only routes to a healthy listener".into()),
        ))
    }
}

async fn futures_get(http: &Client, url: &str) -> reqwest::Result<Response> {
    http.get(url).timeout(PROBE_TIMEOUT).send().await
}

async fn probe_management(
    http: &Client,
    target: &ProbeTarget,
    capabilities: &mut ProxyCapabilities,
    diagnostics: &mut Vec<ProxyDiagnostic>,
) {
    let url = format!("{}/v0/management/debug", target.base_url);
    let response = match http.get(&url).timeout(PROBE_TIMEOUT).send().await {
        Ok(response) => response,
        Err(_) => {
            diagnostics.push(ProxyDiagnostic::new(
                ProxyDiagnosticLevel::Warning,
                "management_probe_failed",
                "could not probe the management API",
                Some("The client API may still work; retry the test".into()),
            ));
            capabilities.management = ProxyManagementAccess::Unknown;
            return;
        }
    };
    read_capability_headers(response.headers(), capabilities);

    match response.status() {
        StatusCode::NOT_FOUND => {
            capabilities.management = ProxyManagementAccess::Disabled;
            diagnostics.push(ProxyDiagnostic::new(
                ProxyDiagnosticLevel::Warning,
                "management_disabled",
                "the management API is not enabled on this proxy (routes return 404)",
                Some(
                    "Set `remote-management.secret-key` in CLIProxyAPI's config.yaml (or \
                     MANAGEMENT_PASSWORD) and restart it; h4rn can still use the client API"
                        .into(),
                ),
            ));
        }
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            capabilities.management = ProxyManagementAccess::NeedsKey;
            if target.management_key.is_none() {
                diagnostics.push(ProxyDiagnostic::new(
                    ProxyDiagnosticLevel::Warning,
                    "management_key_missing",
                    "the management API is available but no management key is stored",
                    Some(
                        "Add the management key to this profile to read aliases, cooldowns and \
                         optional routes"
                            .into(),
                    ),
                ));
            }
        }
        status if status.is_success() => {
            capabilities.management = ProxyManagementAccess::Open;
            diagnostics.push(ProxyDiagnostic::new(
                ProxyDiagnosticLevel::Warning,
                "management_open",
                "the management API answered without a key",
                Some(
                    "Configure `remote-management.secret-key` so the management plane is not open"
                        .into(),
                ),
            ));
        }
        status => {
            capabilities.management = ProxyManagementAccess::Unknown;
            diagnostics.push(ProxyDiagnostic::new(
                ProxyDiagnosticLevel::Warning,
                "management_unexpected_status",
                format!("the keyless management probe returned HTTP {status}"),
                Some("Retry; if this persists, check the proxy's reverse-proxy rules".into()),
            ));
        }
    }
}

async fn probe_management_routes(
    http: &Client,
    target: &ProbeTarget,
    capabilities: &mut ProxyCapabilities,
    diagnostics: &mut Vec<ProxyDiagnostic>,
) {
    let Some(key) = target.management_key.as_deref() else {
        return;
    };
    if !matches!(
        capabilities.management,
        ProxyManagementAccess::NeedsKey
            | ProxyManagementAccess::Open
            | ProxyManagementAccess::Available
    ) {
        return;
    }

    let mut rejected = false;
    let routes = [
        ("/v0/management/usage-queue", "usage_queue"),
        ("/v0/management/usage", "legacy_usage"),
        ("/v0/management/api-key-usage", "api_key_usage"),
    ];
    for (path, feature) in routes {
        match get_status(http, &format!("{}{path}", target.base_url), key).await {
            Ok(Status::Ok(response)) => {
                let status = response.status();
                let ok = status.is_success();
                match feature {
                    "usage_queue" => capabilities.usage_queue = ok,
                    "legacy_usage" => capabilities.legacy_usage = ok,
                    "api_key_usage" => capabilities.api_key_usage = ok,
                    _ => {}
                }
            }
            Ok(Status::Rejected) => rejected = true,
            Ok(Status::Absent) => {}
            Err(()) => {}
        }
    }

    if rejected {
        capabilities.management = ProxyManagementAccess::NeedsKey;
        diagnostics.push(ProxyDiagnostic::new(
            ProxyDiagnosticLevel::Error,
            "management_key_rejected",
            "the management API rejected the stored key",
            Some(
                "Replace the management key on this profile with the value of \
                 `remote-management.secret-key`, or the `MANAGEMENT_PASSWORD` environment value"
                    .into(),
            ),
        ));
        return;
    }
    if capabilities.management != ProxyManagementAccess::Open {
        capabilities.management = ProxyManagementAccess::Available;
    }

    // v8 config layout + ETag.
    if let Ok(Status::Ok(response)) = get_status(
        http,
        &format!("{}/v8/management/config", target.base_url),
        key,
    )
    .await
    {
        if response.status().is_success() {
            capabilities.v8_config = true;
            capabilities.v8_etag = response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
        }
    }

    if let Ok(Status::Ok(response)) = get_status(
        http,
        &format!("{}/v8/management/performance?limit=1", target.base_url),
        key,
    )
    .await
        && response.status().is_success()
    {
        capabilities.performance = true;
    }

    match post_status(
        http,
        &format!("{}/v8/management/credentials/verify", target.base_url),
        key,
    )
    .await
    {
        // 400/422 means the route exists but the empty body is invalid; only
        // 404 proves the upstream-only route is absent.
        Ok(Status::Ok(response)) => {
            capabilities.credentials_verify = response.status() != StatusCode::NOT_FOUND;
        }
        Ok(Status::Absent) => capabilities.credentials_verify = false,
        Ok(Status::Rejected) | Err(()) => {}
    }

    if let Ok(Status::Ok(response)) = get_status(
        http,
        &format!(
            "{}/v0/management/auth-files?page=1&page_size=1",
            target.base_url
        ),
        key,
    )
    .await
        && response.status().is_success()
    {
        capabilities.cooldowns = true;
    }

    if let Ok(Status::Ok(response)) = get_status(
        http,
        &format!("{}/v0/management/quota/providers", target.base_url),
        key,
    )
    .await
        && response.status().is_success()
        && let Ok(value) = response.json::<Value>().await
    {
        capabilities.quota_providers = value
            .get("providers")
            .and_then(Value::as_array)
            .is_some_and(|providers| !providers.is_empty());
    }

    if capabilities.management == ProxyManagementAccess::Available
        && !capabilities.v8_config
        && !capabilities.usage_queue
        && !capabilities.api_key_usage
    {
        diagnostics.push(ProxyDiagnostic::new(
            ProxyDiagnosticLevel::Info,
            "management_basic",
            "the management API is reachable but exposes none of the optional v8/usage routes",
            Some("This looks like an older CLIProxyAPI; client routing is unaffected".into()),
        ));
    }
}

async fn probe_models(
    http: &Client,
    target: &ProbeTarget,
    capabilities: &mut ProxyCapabilities,
    diagnostics: &mut Vec<ProxyDiagnostic>,
) {
    // OpenAI shape: no identifying header at all.
    let openai = match client_get(http, &format!("{}/v1/models", target.base_url), target).await {
        Ok(response) => response,
        Err(_) => {
            diagnostics.push(ProxyDiagnostic::new(
                ProxyDiagnosticLevel::Warning,
                "models_unreachable",
                "the client model list could not be read",
                Some("Check the client API key and the proxy's /v1 route".into()),
            ));
            return;
        }
    };
    if openai.status().is_success() {
        let value = openai.json::<Value>().await.unwrap_or(Value::Null);
        capabilities.openai_models = value
            .get("data")
            .and_then(Value::as_array)
            .is_some_and(|data| !data.is_empty());
        capabilities.fork_model_enrichment = value
            .get("data")
            .and_then(Value::as_array)
            .is_some_and(|data| {
                data.iter().any(|entry| {
                    entry.get("max_output_length").is_some()
                        || entry.get("reasoning_parameters").is_some()
                })
            });
    } else {
        diagnostics.push(auth_diagnostic(openai.status(), "client model list"));
    }

    // Claude shape: Anthropic-Version selects the format, not the path.
    let claude = client_get_with_header(
        http,
        &format!("{}/v1/models", target.base_url),
        target,
        "anthropic-version",
        "2023-06-01",
    )
    .await;
    if let Ok(response) = claude
        && response.status().is_success()
    {
        let value = response.json::<Value>().await.unwrap_or(Value::Null);
        capabilities.claude_models =
            value
                .get("data")
                .and_then(Value::as_array)
                .is_some_and(|data| {
                    !data.is_empty()
                        && data.iter().any(|entry| {
                            entry.get("type").and_then(Value::as_str) == Some("model")
                                || entry.get("max_input_tokens").is_some()
                        })
                });
    }

    // Gemini shape.
    if let Ok(response) =
        client_get(http, &format!("{}/v1beta/models", target.base_url), target).await
        && response.status().is_success()
    {
        let value = response.json::<Value>().await.unwrap_or(Value::Null);
        capabilities.gemini_models = value
            .get("models")
            .and_then(Value::as_array)
            .is_some_and(|models| !models.is_empty());
    }
}

fn auth_diagnostic(status: StatusCode, what: &str) -> ProxyDiagnostic {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ProxyDiagnostic::new(
            ProxyDiagnosticLevel::Error,
            "client_key_rejected",
            format!("the proxy rejected the client API key for the {what}"),
            Some("Replace the API key on this profile with one from `access.api-keys`".into()),
        ),
        StatusCode::NOT_FOUND => ProxyDiagnostic::new(
            ProxyDiagnosticLevel::Warning,
            "client_route_absent",
            format!("{what} returned 404"),
            Some("Check that the base URL points at the CLIProxyAPI listener root".into()),
        ),
        status => ProxyDiagnostic::new(
            ProxyDiagnosticLevel::Warning,
            "client_route_status",
            format!("{what} returned HTTP {status}"),
            Some("Retry; if this persists, check the proxy's logs".into()),
        ),
    }
}

fn read_capability_headers(headers: &HeaderMap, capabilities: &mut ProxyCapabilities) {
    capabilities.version = header(headers, "x-cpa-version");
    capabilities.commit = header(headers, "x-cpa-commit");
    capabilities.build_date = header(headers, "x-cpa-build-date");
    capabilities.plugin_support = header(headers, "x-cpa-support-plugin").as_deref() == Some("1");
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

async fn client_get(http: &Client, url: &str, target: &ProbeTarget) -> reqwest::Result<Response> {
    let request = http.get(url).timeout(PROBE_TIMEOUT);
    let request = match target.api_key.as_deref() {
        Some(key) => request.header(AUTHORIZATION, bearer(key)),
        None => request,
    };
    request.send().await
}

async fn client_get_with_header(
    http: &Client,
    url: &str,
    target: &ProbeTarget,
    name: &'static str,
    value: &'static str,
) -> reqwest::Result<Response> {
    let request = http.get(url).timeout(PROBE_TIMEOUT).header(name, value);
    let request = match target.api_key.as_deref() {
        Some(key) => request.header(AUTHORIZATION, bearer(key)),
        None => request,
    };
    request.send().await
}

fn bearer(key: &str) -> HeaderValue {
    let mut value = HeaderValue::from_str(&format!("Bearer {key}")).unwrap_or_else(|_| {
        // A key with characters that cannot be a header value is unusable; the
        // request will be rejected upstream and no key is echoed.
        HeaderValue::from_static("Bearer")
    });
    value.set_sensitive(true);
    value
}

enum Status {
    Ok(Response),
    Rejected,
    Absent,
}

async fn get_status(http: &Client, url: &str, key: &str) -> Result<Status, ()> {
    let response = http
        .get(url)
        .timeout(PROBE_TIMEOUT)
        .header(AUTHORIZATION, bearer(key))
        .send()
        .await
        .map_err(|_| ())?;
    Ok(classify(response))
}

async fn post_status(http: &Client, url: &str, key: &str) -> Result<Status, ()> {
    let response = http
        .post(url)
        .timeout(PROBE_TIMEOUT)
        .header(AUTHORIZATION, bearer(key))
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|_| ())?;
    Ok(classify(response))
}

fn classify(response: Response) -> Status {
    match response.status() {
        StatusCode::NOT_FOUND => Status::Absent,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Status::Rejected,
        _ => Status::Ok(response),
    }
}
