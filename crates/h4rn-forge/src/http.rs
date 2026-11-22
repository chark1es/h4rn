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
    /// The configured credential in every form it can be echoed back in: the
    /// raw token, its percent-encoded spellings, the Base64 basic credential
    /// and the decoded `user:token` pair. An error body that quotes one of
    /// them is redacted before the message leaves this client.
    secrets: Vec<String>,
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
        let secrets = auth
            .as_ref()
            .and_then(|(_, value)| value.to_str().ok())
            .map(secret_forms)
            .unwrap_or_default();
        Ok(Self {
            client,
            forge,
            base_url: base_url.trim_end_matches('/').to_owned(),
            auth,
            secrets,
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
            .map_err(|error| ForgeError::Network(self.sanitize(&error.to_string())))?;

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
            .map_err(|error| ForgeError::Network(self.sanitize(&error.to_string())))?;

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
        let message = self.sanitize(&response_message(&response.body));
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

    /// Removes the configured credential, then anything that looks like a
    /// token, from a message before it reaches a client or a log.
    ///
    /// The account's own secret is matched verbatim first: an Azure DevOps
    /// pasted in a JSON error body, a Basic header echoed back, or a
    /// percent-encoded copy inside a URL all contain it. The pattern-based
    /// [`redact`] pass stays as a second layer for credentials that do not
    /// belong to this account (a proxy or a shared error page).
    fn sanitize(&self, value: &str) -> String {
        let mut output = value.to_owned();
        for secret in &self.secrets {
            if output.contains(secret) {
                output = output.replace(secret, "[redacted]");
            }
        }
        redact(&output)
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

/// Percent-encodes one URL path segment.
///
/// `form_urlencoded` encodes a space as `+`, which is a literal plus in a
/// path; this keeps the RFC 3986 unreserved characters and percent-encodes
/// everything else, including `/`.
pub fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Extracts a `page` query parameter from a URL.
pub fn page_of(url: &str) -> Option<u32> {
    let url = url::Url::parse(url).ok()?;
    url.query_pairs()
        .find(|(key, _)| key == "page")
        .and_then(|(_, value)| value.parse().ok())
}

/// Whether `candidate` is on the same origin (scheme, host and port) as
/// `base`, and carries no credentials of its own.
///
/// Pagination links are followed only when this is true, so a forge (or a
/// compromised proxy) cannot point the next request at another host and
/// receive the account's Authorization header.
pub fn same_origin(candidate: &str, base: &str) -> bool {
    let (Ok(candidate), Ok(base)) = (url::Url::parse(candidate), url::Url::parse(base)) else {
        return false;
    };
    if !candidate.username().is_empty() || candidate.password().is_some() {
        return false;
    }
    candidate.scheme() == base.scheme()
        && candidate.host_str() == base.host_str()
        && candidate.port_or_known_default() == base.port_or_known_default()
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

/// Token prefixes that identify a forge credential. Azure DevOps personal
/// access tokens have no prefix of their own, so they cannot be recognised by
/// shape; the Atlassian and GitHub/GitLab forms can.
const TOKEN_PREFIXES: [&str; 6] = ["ghp_", "gho_", "glpat-", "github_pat_", "ATBB", "ATATT"];

/// Removes anything that looks like a token from a forge message.
///
/// The prefix is found anywhere in the text, not just at the start of a
/// whitespace-delimited word: an error body often quotes it
/// (`{"message":"bad token ghp_..."}`, `token=glpat-...`). The run of token
/// characters after the prefix is replaced too, and the rest of the text
/// (spacing included) is left exactly as it was.
pub fn redact(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some((index, prefix)) = find_token_prefix(rest) {
        output.push_str(&rest[..index]);
        output.push_str("[redacted]");
        let after_prefix = &rest[index + prefix.len()..];
        rest = &after_prefix[token_run_len(after_prefix)..];
    }
    output.push_str(rest);
    output
}

/// The earliest known token prefix and its position.
fn find_token_prefix(value: &str) -> Option<(usize, &'static str)> {
    TOKEN_PREFIXES
        .iter()
        .filter_map(|prefix| value.find(prefix).map(|index| (index, *prefix)))
        .min_by_key(|(index, _)| *index)
}

/// Length of the token characters at the start of `value`.
///
/// Quotes, colons, commas, brackets, braces and whitespace all end a token,
/// which is what makes a JSON body redactable.
fn token_run_len(value: &str) -> usize {
    value
        .char_indices()
        .find(|(_, ch)| {
            !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '~' | '+' | '/' | '='))
        })
        .map(|(index, _)| index)
        .unwrap_or(value.len())
}

/// Every spelling of a configured credential that an error body or a URL can
/// echo back.
///
/// The auth header carries the token in one of three shapes: `Basic <b64>`
/// (Bitbucket API tokens and Azure DevOps PATs), `Bearer <token>` (GitHub,
/// Bitbucket access tokens) and `token <token>` (Gitea/Forgejo), or the bare
/// token in a custom header (GitLab). Each is reduced to the secret it holds,
/// then the percent-encoded forms are added: an error body can quote a token
/// copied out of a URL, where `/`, `+` and `:` arrive escaped.
fn secret_forms(header: &str) -> Vec<String> {
    use base64::Engine as _;

    let header = header.trim();
    let mut secrets = vec![header.to_owned()];
    if let Some(encoded) = header.strip_prefix("Basic ") {
        secrets.push(encoded.to_owned());
        if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded)
            && let Ok(decoded) = String::from_utf8(decoded)
        {
            if let Some((_, token)) = decoded.split_once(':') {
                secrets.push(token.to_owned());
            }
            secrets.push(decoded);
        }
    } else if let Some(token) = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("token "))
        .or_else(|| header.strip_prefix("Token "))
    {
        secrets.push(token.to_owned());
    }

    for secret in secrets.clone() {
        let form = url::form_urlencoded::byte_serialize(secret.as_bytes()).collect::<String>();
        if form != secret {
            secrets.push(form);
        }
        let path = encode_path_segment(&secret);
        if path != secret {
            secrets.push(path);
        }
    }

    // A Base64 credential decodes to `user:token`; an empty username leaves
    // the whole token. Short values are not credentials and would mangle
    // ordinary words.
    secrets.retain(|secret| secret.chars().count() >= 4);
    // Longest first so a Base64 credential is replaced whole, not in pieces.
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    secrets
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
/// Bitbucket Cloud API tokens use `email:api-token`; Azure DevOps personal
/// access tokens use an empty username (`:pat`).
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
    fn path_segments_encode_spaces_and_separators() {
        assert_eq!(encode_path_segment("My Project"), "My%20Project");
        assert_eq!(encode_path_segment("a/b"), "a%2Fb");
        assert_eq!(encode_path_segment("widget-1.2_~"), "widget-1.2_~");
    }

    #[test]
    fn same_origin_ignores_the_path_but_not_the_host() {
        assert!(same_origin(
            "https://api.bitbucket.org/2.0/repositories/a/b/pullrequests?page=2",
            "https://api.bitbucket.org/2.0"
        ));
        assert!(!same_origin(
            "https://evil.example/2.0/repositories?page=2",
            "https://api.bitbucket.org/2.0"
        ));
        assert!(!same_origin(
            "http://api.bitbucket.org/2.0/repositories?page=2",
            "https://api.bitbucket.org/2.0"
        ));
        assert!(!same_origin(
            "https://api.bitbucket.org:8443/2.0/repositories?page=2",
            "https://api.bitbucket.org/2.0"
        ));
        assert!(!same_origin(
            "https://token@api.bitbucket.org/2.0/repositories?page=2",
            "https://api.bitbucket.org/2.0"
        ));
    }

    #[test]
    fn messages_redact_tokens() {
        assert_eq!(
            redact("bad token ghp_abc123 here"),
            "bad token [redacted] here"
        );
        assert_eq!(redact("glpat-xyz"), "[redacted]");
        assert_eq!(redact("ATBBtoken"), "[redacted]");
        assert_eq!(redact("ATATT3xFfGF0abc"), "[redacted]");
    }

    #[test]
    fn redaction_finds_tokens_next_to_punctuation() {
        // A quoted token inside a JSON error body, with the surrounding text
        // and spacing left intact.
        let body = r#"{"error":"bad token","detail":"ATATT3xFfGF0abc","retry":false}"#;
        assert_eq!(
            redact(body),
            r#"{"error":"bad token","detail":"[redacted]","retry":false}"#
        );
        assert_eq!(
            redact("token=glpat-abc123, host:bitbucket.org"),
            "token=[redacted], host:bitbucket.org"
        );
        assert_eq!(
            redact("Authorization: Bearer ATBBxyz\nnext line"),
            "Authorization: Bearer [redacted]\nnext line",
            "a newline stays a newline"
        );
        assert_eq!(
            redact(r#"["ghp_quoted", "plain"]"#),
            r#"["[redacted]", "plain"]"#
        );
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

    fn error_for(client: &ForgeHttp, body: String) -> String {
        let response = HttpResponse {
            status: StatusCode::BAD_REQUEST,
            headers: HeaderMap::new(),
            body,
            content_type: None,
            from_cache: false,
        };
        client.status_error(&response).to_string()
    }

    #[test]
    fn configured_azure_pat_is_redacted_quoted_and_as_a_basic_header() {
        use base64::Engine as _;
        let pat = "azp/7Kq+2f9Xr3Lm0";
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!(":{pat}"));
        let client = ForgeHttp::new(
            ForgeKind::AzureDevOps,
            "https://dev.azure.test/acme",
            basic_auth_header("", pat),
            "application/json",
            120,
        )
        .unwrap();
        // A quoted PAT, the Base64 Basic credential echoed in the same JSON
        // message, and the percent-encoded spelling a URL would carry.
        let body = format!(
            r#"{{"message":"bad PAT \"{pat}\" in Authorization: Basic {encoded}; saw azp%2F7Kq%2B2f9Xr3Lm0"}}"#
        );
        let message = error_for(&client, body);
        assert!(!message.contains(pat), "{message}");
        assert!(!message.contains(&encoded), "{message}");
        assert!(!message.contains("azp%2F7Kq%2B2f9Xr3Lm0"), "{message}");
        assert!(message.contains("[redacted]"), "{message}");
    }

    #[test]
    fn pattern_redaction_stays_as_a_second_layer() {
        let client = ForgeHttp::new(
            ForgeKind::AzureDevOps,
            "https://dev.azure.test/acme",
            basic_auth_header("", "azp_configured"),
            "application/json",
            120,
        )
        .unwrap();
        let message = error_for(
            &client,
            r#"{"message":"proxy said token ghp_outsider leaked"}"#.into(),
        );
        assert!(
            message.contains("[redacted]"),
            "a credential that is not this account's is still hidden: {message}"
        );
        assert!(!message.contains("ghp_outsider"), "{message}");
    }
}
