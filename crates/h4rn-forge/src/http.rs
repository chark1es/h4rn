//! Shared HTTP plumbing: auth headers, conditional GETs and rate limiting.
//!
//! Every forge client owns one [`ForgeHttp`] for one account. That is the
//! natural scope for a token, an ETag cache and a request budget; sharing them
//! across accounts would let one account's remaining quota throttle another.
//!
//! Behaviour:
//! - GitHub (and anything else that sends an `ETag`) gets conditional GETs and
//!   a small in-memory response cache. A `304 Not Modified` returns the cached
//!   body without spending a second API request.
//! - `Retry-After` and `X-RateLimit-*` are read on every response. A request
//!   that would exceed the documented remaining quota is refused with
//!   [`ForgeError::RateLimited`] rather than retried in a tight loop.
//! - A local token bucket caps requests per minute per account so a buggy
//!   caller cannot drain a user's quota.
//! - Pagination follows `Link: <...>; rel="next"` and also understands
//!   page-number cursors for forges that do not send `Link`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, TimeZone, Utc};
use h4rn_protocol::ForgeKind;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RETRY_AFTER};
use reqwest::{Method, StatusCode};
use serde_json::Value;

use crate::error::{ForgeError, Result};

/// Default local request budget per account, requests per minute.
pub const DEFAULT_REQUESTS_PER_MINUTE: u32 = 120;

/// How long a cached ETag response is trusted before an unconditional GET.
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);

/// Maximum cached responses per account.
const CACHE_ENTRIES: usize = 256;

/// Headers every request carries.
pub struct ForgeHttp {
    client: reqwest::Client,
    forge: ForgeKind,
    base_url: String,
    auth: Option<(HeaderName, HeaderValue)>,
    accept: String,
    cache: Mutex<HashMap<String, CachedResponse>>,
    limiter: RateLimiter,
}

struct CachedResponse {
    body: String,
    content_type: Option<String>,
    etag: String,
    stored_at: Instant,
}

/// One raw response.
#[derive(Clone, Debug)]
pub struct HttpResponse {
    /// HTTP status after resolving a conditional hit.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Response body as text.
    pub body: String,
    /// Content type, when reported.
    pub content_type: Option<String>,
    /// Whether the body came from the ETag cache.
    pub from_cache: bool,
}

impl ForgeHttp {
    /// Creates a client for one account.
    pub fn new(
        forge: ForgeKind,
        base_url: &str,
        auth: Option<(HeaderName, String)>,
        accept: &str,
        requests_per_minute: u32,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("h4rn-forge/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| ForgeError::Network(error.to_string()))?;
        let auth = match auth {
            Some((name, value)) => {
                let value = HeaderValue::from_str(&value).map_err(|_| {
                    ForgeError::InvalidRequest("token is not a valid header".into())
                })?;
                Some((name, value))
            }
            None => None,
        };
        Ok(Self {
            client,
            forge,
            base_url: base_url.trim_end_matches('/').to_owned(),
            auth,
            accept: accept.to_owned(),
            cache: Mutex::new(HashMap::new()),
            limiter: RateLimiter::new(requests_per_minute.max(1)),
        })
    }

    /// Forge this client talks to.
    pub fn forge(&self) -> ForgeKind {
        self.forge
    }

    /// Web base URL this client was built for.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Performs a request, applying conditional GET and rate limiting.
    pub async fn request(
        &self,
        method: Method,
        url: &str,
        body: Option<&Value>,
        conditional: bool,
    ) -> Result<HttpResponse> {
        self.limiter.acquire().await?;

        let mut request = self.client.request(method.clone(), url).header(
            reqwest::header::ACCEPT,
            if self.accept.is_empty() {
                "*/*"
            } else {
                &self.accept
            },
        );
        if let Some((name, value)) = &self.auth {
            request = request.header(name, value);
        }

        let cache_key = url.to_owned();
        if conditional && method == Method::GET {
            if let Some(cached) = self.cached(&cache_key) {
                request = request.header(reqwest::header::IF_NONE_MATCH, cached.etag);
            }
        }

        if let Some(body) = body {
            request = request.json(body);
        }

        let response = request
            .send()
            .await
            .map_err(|error| ForgeError::Network(redact_network_error(&error)))?;

        let status = response.status();
        let headers = response.headers().clone();
        self.limiter.observe(&headers);

        if status == StatusCode::NOT_MODIFIED
            && conditional
            && method == Method::GET
            && let Some(cached) = self.cached(&cache_key)
        {
            return Ok(HttpResponse {
                status: StatusCode::OK,
                headers,
                body: cached.body,
                content_type: cached.content_type,
                from_cache: true,
            });
        }

        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(self.rate_limited(&headers, "the forge returned 429"));
        }
        if status == StatusCode::FORBIDDEN
            && let Some(error) = self.forbidden_rate_limit(&headers)
        {
            return Err(error);
        }

        let content_type = headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let body = response
            .text()
            .await
            .map_err(|error| ForgeError::Network(redact_network_error(&error)))?;

        if method == Method::GET
            && status.is_success()
            && let Some(etag) = headers
                .get(reqwest::header::ETAG)
                .and_then(|value| value.to_str().ok())
        {
            self.store(
                cache_key,
                CachedResponse {
                    body: body.clone(),
                    content_type: content_type.clone(),
                    etag: etag.to_owned(),
                    stored_at: Instant::now(),
                },
            );
        }

        Ok(HttpResponse {
            status,
            headers,
            body,
            content_type,
            from_cache: false,
        })
    }

    /// GETs JSON and maps non-success statuses to forge errors.
    pub async fn get_json(&self, url: &str) -> Result<Value> {
        let response = self.request(Method::GET, url, None, true).await?;
        self.json(response)
    }

    /// GETs JSON without a conditional request (list endpoints whose ETag is
    /// not worth caching).
    pub async fn get_json_fresh(&self, url: &str) -> Result<Value> {
        let response = self.request(Method::GET, url, None, false).await?;
        self.json(response)
    }

    /// GETs a text body, returning its content type.
    pub async fn get_text(&self, url: &str) -> Result<(String, Option<String>)> {
        let response = self.request(Method::GET, url, None, false).await?;
        if !response.status.is_success() {
            return Err(self.status_error(&response));
        }
        Ok((response.body, response.content_type))
    }

    /// POSTs JSON and maps the response.
    pub async fn post_json(&self, url: &str, body: &Value) -> Result<Value> {
        let response = self.request(Method::POST, url, Some(body), false).await?;
        self.json(response)
    }

    /// PATCHes JSON and maps the response.
    pub async fn patch_json(&self, url: &str, body: &Value) -> Result<Value> {
        let response = self.request(Method::PATCH, url, Some(body), false).await?;
        self.json(response)
    }

    /// Maps a raw response to JSON, turning status failures into the right
    /// error class.
    pub fn json(&self, response: HttpResponse) -> Result<Value> {
        if !response.status.is_success() {
            return Err(self.status_error(&response));
        }
        if response.body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&response.body).map_err(|error| {
            ForgeError::Parse(format!(
                "{} returned a non-JSON body: {error}",
                self.forge.display_name()
            ))
        })
    }

    /// Classifies a failed response.
    pub fn status_error(&self, response: &HttpResponse) -> ForgeError {
        let message = response_message(&response.body);
        match response.status {
            StatusCode::UNAUTHORIZED => ForgeError::Unauthorized {
                forge: self.forge,
                message,
            },
            StatusCode::FORBIDDEN => ForgeError::Forbidden {
                forge: self.forge,
                message,
            },
            StatusCode::NOT_FOUND => ForgeError::NotFound(message),
            StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY => {
                ForgeError::Conflict(message)
            }
            StatusCode::BAD_REQUEST => ForgeError::InvalidRequest(message),
            status if status.is_server_error() => ForgeError::Network(format!(
                "{} returned HTTP {}: {message}",
                self.forge.display_name(),
                status.as_u16()
            )),
            status => ForgeError::Http {
                status: status.as_u16(),
                message,
            },
        }
    }

    /// Maps a failed response to a rate-limit error when its headers say so.
    pub fn forbidden_rate_limit(&self, headers: &HeaderMap) -> Option<ForgeError> {
        let remaining = header_u64(headers, "x-ratelimit-remaining");
        if remaining != Some(0) && headers.get(RETRY_AFTER).is_none() {
            return None;
        }
        Some(self.rate_limited(headers, "the forge refused the request"))
    }

    fn rate_limited(&self, headers: &HeaderMap, message: &str) -> ForgeError {
        let retry_after_secs = retry_after(headers);
        let retry_at = retry_at(headers);
        ForgeError::RateLimited {
            forge: self.forge,
            retry_after_secs,
            retry_at,
            message: message.to_owned(),
        }
    }

    fn cached(&self, key: &str) -> Option<CachedResponse> {
        let cache = self.cache.lock().ok()?;
        let cached = cache.get(key)?;
        if cached.stored_at.elapsed() > CACHE_TTL {
            return None;
        }
        Some(CachedResponse {
            body: cached.body.clone(),
            content_type: cached.content_type.clone(),
            etag: cached.etag.clone(),
            stored_at: cached.stored_at,
        })
    }

    fn store(&self, key: String, response: CachedResponse) {
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        if cache.len() >= CACHE_ENTRIES
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, value)| value.stored_at)
                .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(key, response);
    }
}

/// The next page URL from a `Link` header.
pub fn next_link(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(reqwest::header::LINK)?.to_str().ok()?;
    for part in value.split(',') {
        let part = part.trim();
        if !part.contains("rel=\"next\"") {
            continue;
        }
        let start = part.find('<')? + 1;
        let end = part[start..].find('>')? + start;
        return Some(part[start..end].to_owned());
    }
    None
}

/// Extracts a `page` query parameter from a URL.
pub fn page_of(url: &str) -> Option<u32> {
    let url = url::Url::parse(url).ok()?;
    url.query_pairs()
        .find(|(key, _)| key == "page")
        .and_then(|(_, value)| value.parse().ok())
}

fn retry_after(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

fn retry_at(headers: &HeaderMap) -> Option<DateTime<Utc>> {
    headers
        .get("x-ratelimit-reset")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<i64>().ok())
        .and_then(|seconds| Utc.timestamp_opt(seconds, 0).single())
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
}

fn response_message(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "no message".into();
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        for key in ["message", "error", "detail", "errors"] {
            if let Some(message) = value.get(key) {
                if let Some(text) = message.as_str() {
                    return redact(text);
                }
                return redact(&message.to_string());
            }
        }
    }
    redact(trimmed)
}

/// Removes anything that looks like a token from a forge message.
pub fn redact(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for word in value.split_whitespace() {
        if word.starts_with("ghp_")
            || word.starts_with("gho_")
            || word.starts_with("glpat-")
            || word.starts_with("github_pat_")
            || word.starts_with("ATBB")
        {
            output.push_str("[redacted]");
        } else {
            output.push_str(word);
        }
        output.push(' ');
    }
    output.trim_end().to_owned()
}

fn redact_network_error(error: &reqwest::Error) -> String {
    let value = error.to_string();
    // reqwest includes the URL but never an Authorization header; strip a
    // token if the URL itself carried one (GitLab accepts no such form, but a
    // user-supplied custom base URL might).
    redact(&value)
}

/// A local token bucket plus the forge's documented remaining quota.
struct RateLimiter {
    budget: Mutex<Budget>,
    capacity: f64,
    refill_per_second: f64,
}

struct Budget {
    tokens: f64,
    last_refill: Instant,
    remaining: Option<u64>,
    reset_at: Option<Instant>,
}

impl RateLimiter {
    fn new(requests_per_minute: u32) -> Self {
        let capacity = f64::from(requests_per_minute);
        Self {
            budget: Mutex::new(Budget {
                tokens: capacity,
                last_refill: Instant::now(),
                remaining: None,
                reset_at: None,
            }),
            capacity,
            refill_per_second: capacity / 60.0,
        }
    }

    async fn acquire(&self) -> Result<()> {
        loop {
            let wait = {
                let Ok(mut budget) = self.budget.lock() else {
                    return Err(ForgeError::Network("rate limiter poisoned".into()));
                };
                let elapsed = budget.last_refill.elapsed().as_secs_f64();
                budget.last_refill = Instant::now();
                budget.tokens =
                    (budget.tokens + elapsed * self.refill_per_second).min(self.capacity);

                let mut retry_wait = None;
                if budget.remaining == Some(0) {
                    match budget.reset_at {
                        Some(reset) if reset <= Instant::now() => {
                            budget.remaining = None;
                            budget.reset_at = None;
                        }
                        Some(reset) => {
                            let wait = reset.saturating_duration_since(Instant::now());
                            if wait > Duration::from_secs(60) {
                                return Err(ForgeError::RateLimited {
                                    forge: ForgeKind::GitHub,
                                    retry_after_secs: Some(wait.as_secs()),
                                    retry_at: None,
                                    message: "the request budget is exhausted".into(),
                                });
                            }
                            retry_wait = Some(wait);
                        }
                        None => {}
                    }
                }

                if let Some(wait) = retry_wait {
                    Some(wait)
                } else if budget.tokens >= 1.0 {
                    budget.tokens -= 1.0;
                    return Ok(());
                } else {
                    Some(Duration::from_secs_f64(
                        (1.0 - budget.tokens) / self.refill_per_second,
                    ))
                }
            };

            match wait {
                Some(wait) if !wait.is_zero() => tokio::time::sleep(wait).await,
                _ => tokio::task::yield_now().await,
            }
        }
    }

    fn observe(&self, headers: &HeaderMap) {
        let Ok(mut budget) = self.budget.lock() else {
            return;
        };
        if let Some(remaining) = header_u64(headers, "x-ratelimit-remaining") {
            budget.remaining = Some(remaining);
        }
        if let Some(reset) = header_u64(headers, "x-ratelimit-reset") {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_secs())
                .unwrap_or(0);
            let wait = reset.saturating_sub(now).min(300);
            budget.reset_at = Some(Instant::now() + Duration::from_secs(wait));
        }
        if headers.get(RETRY_AFTER).is_some() {
            budget.remaining = Some(0);
            if let Some(seconds) = retry_after(headers) {
                budget.reset_at = Some(Instant::now() + Duration::from_secs(seconds.min(300)));
            }
        }
    }
}

/// A header name/value pair for the forge clients.
pub fn auth_header(name: &'static str, value: &str) -> Option<(HeaderName, String)> {
    let name = HeaderName::from_static(name);
    Some((name, value.to_owned()))
}

/// An HTTP Basic credential header, base64-encoded as the RFC requires.
///
/// Bitbucket Cloud app passwords use `username:app-password`; Azure DevOps
/// personal access tokens use an empty username (`:pat`).
pub fn basic_auth_header(username: &str, secret: &str) -> Option<(HeaderName, String)> {
    use base64::Engine as _;
    let credentials = format!("{username}:{secret}");
    let encoded = base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes());
    auth_header("authorization", &format!("Basic {encoded}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_link_parsing_handles_multiple_relations() {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::LINK,
            HeaderValue::from_static(
                "<https://api.github.com/repos/a/b/pulls?page=1>; rel=\"prev\", \
                 <https://api.github.com/repos/a/b/pulls?page=3>; rel=\"next\", \
                 <https://api.github.com/repos/a/b/pulls?page=9>; rel=\"last\"",
            ),
        );
        assert_eq!(
            next_link(&headers).as_deref(),
            Some("https://api.github.com/repos/a/b/pulls?page=3")
        );
        assert_eq!(page_of("https://example.test/x?page=3&per_page=2"), Some(3));
    }

    #[test]
    fn no_next_link_is_none() {
        let headers = HeaderMap::new();
        assert!(next_link(&headers).is_none());
    }

    #[test]
    fn messages_redact_tokens() {
        assert_eq!(
            redact("bad token ghp_abc123 here"),
            "bad token [redacted] here"
        );
        assert_eq!(redact("glpat-xyz"), "[redacted]");
        assert_eq!(redact("ATBBtoken"), "[redacted]");
    }

    #[test]
    fn basic_auth_encodes_both_parts() {
        use base64::Engine as _;
        let (_, value) = basic_auth_header("alice", "s3cret").unwrap();
        let encoded = value.trim_start_matches("Basic ");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            b"alice:s3cret"
        );
        let (_, azure) = basic_auth_header("", "pat").unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(azure.trim_start_matches("Basic "))
                .unwrap(),
            b":pat"
        );
    }
}
