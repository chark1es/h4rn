//! HTTP transports: streamable HTTP (2025-03-26+) and legacy HTTP+SSE.
//!
//! Both carry the same JSON-RPC messages; they differ in how a response comes
//! back. Streamable HTTP answers a `POST` either with `application/json` or
//! with a short `text/event-stream`; legacy SSE keeps one `GET` stream open,
//! announces a message endpoint, and every request is a `POST` whose response
//! arrives on the stream. Implementing both here keeps session-id handling,
//! header injection and error mapping in one place.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde_json::Value;
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio::task::JoinHandle;

use super::{
    ConnectedUpstream, Upstream, UpstreamError, UpstreamEvent, UpstreamLimits, discover,
    initialize_params,
};
use crate::jsonrpc::{self, Incoming};
use crate::types::ServerCapabilities;

const SESSION_HEADER: &str = "mcp-session-id";
const PROTOCOL_HEADER: &str = "mcp-protocol-version";
const ACCEPT_VALUE: &str = "application/json, text/event-stream";

fn empty_capabilities() -> &'static ServerCapabilities {
    static EMPTY: std::sync::OnceLock<ServerCapabilities> = std::sync::OnceLock::new();
    EMPTY.get_or_init(ServerCapabilities::default)
}

/// One decoded SSE event.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SseEvent {
    /// `event:` field, when present.
    pub event: Option<String>,
    /// Concatenated `data:` lines.
    pub data: String,
}

/// Incremental SSE decoder.
///
/// Kept public for the tests that feed it chunks split at awkward byte
/// boundaries; production code only uses [`SseDecoder::push`].
#[derive(Default)]
pub struct SseDecoder {
    buffer: String,
}

impl SseDecoder {
    /// Adds bytes and returns every event completed by them.
    pub fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();
        while let Some((index, width)) = event_boundary(&self.buffer) {
            let block = self.buffer[..index].to_owned();
            self.buffer.drain(..index + width);
            if let Some(event) = parse_block(&block) {
                events.push(event);
            }
        }
        events
    }

    /// Bytes not yet part of a complete event.
    pub fn pending(&self) -> &str {
        &self.buffer
    }
}

fn event_boundary(buffer: &str) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for (needle, width) in [("\r\n\r\n", 4usize), ("\n\n", 2usize), ("\r\r", 2usize)] {
        if let Some(index) = buffer.find(needle) {
            best = Some(match best {
                Some((current, _)) if current <= index => (current, best.unwrap().1),
                _ => (index, width),
            });
        }
    }
    best
}

fn parse_block(block: &str) -> Option<SseEvent> {
    let mut event = SseEvent::default();
    let mut data = Vec::new();
    for line in block.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => event.event = Some(value.to_owned()),
            "data" => data.push(value.to_owned()),
            _ => {}
        }
    }
    if data.is_empty() {
        if event.event.is_some() {
            return Some(event);
        }
        return None;
    }
    event.data = data.join("\n");
    Some(event)
}

/// Decodes streamed bytes without turning a split UTF-8 sequence into U+FFFD.
///
/// SSE chunks end at arbitrary byte boundaries; an incomplete trailing sequence
/// is carried into the next chunk, while genuinely invalid bytes decode lossily.
#[derive(Default)]
struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    fn push(&mut self, bytes: &[u8]) -> String {
        let mut buffer = std::mem::take(&mut self.pending);
        buffer.extend_from_slice(bytes);
        let mut text = String::new();
        loop {
            match std::str::from_utf8(&buffer) {
                Ok(complete) => {
                    text.push_str(complete);
                    buffer.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if let Ok(prefix) = std::str::from_utf8(&buffer[..valid]) {
                        text.push_str(prefix);
                    }
                    match error.error_len() {
                        Some(len) => {
                            text.push(char::REPLACEMENT_CHARACTER);
                            buffer.drain(..valid + len);
                        }
                        None => {
                            buffer.drain(..valid);
                            break;
                        }
                    }
                }
            }
        }
        self.pending = buffer;
        text
    }
}

/// A streamable-HTTP connection.
pub struct StreamableHttpUpstream {
    name: String,
    url: String,
    headers: BTreeMap<String, String>,
    client: reqwest::Client,
    session_id: Mutex<Option<String>>,
    protocol_version: String,
    next_id: AtomicI64,
    server_info: std::sync::OnceLock<Option<String>>,
    capabilities: std::sync::OnceLock<ServerCapabilities>,
    instructions: std::sync::OnceLock<Option<String>>,
}

impl StreamableHttpUpstream {
    async fn post(&self, body: &Value) -> Result<reqwest::Response, UpstreamError> {
        let mut request = self
            .client
            .post(&self.url)
            .header(ACCEPT, ACCEPT_VALUE)
            .header(CONTENT_TYPE, "application/json")
            .header(PROTOCOL_HEADER, &self.protocol_version)
            .json(body);
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        if let Some(session) = self.session_id.lock().await.as_ref() {
            request = request.header(SESSION_HEADER, session);
        }
        let response = request.send().await.map_err(|error| {
            UpstreamError::Transport(format!("HTTP request to \"{}\" failed: {error}", self.name))
        })?;
        if let Some(session) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
        {
            *self.session_id.lock().await = Some(session.to_owned());
        }
        Ok(response)
    }
}

#[async_trait]
impl Upstream for StreamableHttpUpstream {
    fn server_info(&self) -> Option<&str> {
        self.server_info.get().and_then(|info| info.as_deref())
    }

    fn capabilities(&self) -> &ServerCapabilities {
        self.capabilities
            .get()
            .unwrap_or_else(|| empty_capabilities())
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.get().and_then(|text| text.as_deref())
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, UpstreamError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = jsonrpc::request(id, method, params);
        let response = tokio::time::timeout(timeout, self.post(&body))
            .await
            .map_err(|_| UpstreamError::Timeout)??;
        parse_http_response(response, id, timeout).await
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), UpstreamError> {
        let response = self.post(&jsonrpc::notification(method, params)).await?;
        if response.status().is_success() || response.status().as_u16() == 202 {
            return Ok(());
        }
        Err(UpstreamError::Transport(format!(
            "\"{}\" rejected a notification with HTTP {}",
            self.name,
            response.status()
        )))
    }

    async fn shutdown(&self) {
        // Streamable HTTP has no persistent process; deleting the session is
        // best-effort and a failure means the server already forgot it.
        let session = self.session_id.lock().await.take();
        if let Some(session) = session {
            let mut request = self
                .client
                .delete(&self.url)
                .header(SESSION_HEADER, session);
            for (key, value) in &self.headers {
                request = request.header(key, value);
            }
            let _ = request.send().await;
        }
    }
}

async fn parse_http_response(
    response: reqwest::Response,
    id: i64,
    timeout: Duration,
) -> Result<Value, UpstreamError> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(UpstreamError::Transport(format!(
            "the upstream answered HTTP {status}: {}",
            body.chars().take(512).collect::<String>()
        )));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    if content_type.contains("text/event-stream") {
        let text = tokio::time::timeout(timeout, response.text())
            .await
            .map_err(|_| UpstreamError::Timeout)?
            .map_err(|error| {
                UpstreamError::Transport(format!("could not read the event stream: {error}"))
            })?;
        let mut decoder = SseDecoder::default();
        for event in decoder.push(&text) {
            let value: Value = match serde_json::from_str(&event.data) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if let Some(outcome) = response_outcome(value, id) {
                return outcome;
            }
        }
        return Err(UpstreamError::Protocol(format!(
            "the event stream ended without a response for request {id}"
        )));
    }
    let text = tokio::time::timeout(timeout, response.text())
        .await
        .map_err(|_| UpstreamError::Timeout)?
        .map_err(|error| {
            UpstreamError::Transport(format!("could not read the response body: {error}"))
        })?;
    let value: Value = serde_json::from_str(&text).map_err(|error| {
        UpstreamError::Protocol(format!("the upstream returned invalid JSON: {error}"))
    })?;
    if let Some(outcome) = response_outcome(value.clone(), id) {
        return outcome;
    }
    if let Value::Array(items) = value {
        for item in items {
            if let Some(outcome) = response_outcome(item, id) {
                return outcome;
            }
        }
    }
    Err(UpstreamError::Protocol(format!(
        "the upstream returned no response for request {id}"
    )))
}

fn response_outcome(value: Value, id: i64) -> Option<Result<Value, UpstreamError>> {
    let matches = value
        .get("id")
        .and_then(jsonrpc::numeric_id)
        .is_some_and(|candidate| candidate == id);
    if !matches {
        return None;
    }
    if let Some(error) = value.get("error") {
        let error = serde_json::from_value(error.clone()).ok()?;
        return Some(Err(UpstreamError::Rpc(error)));
    }
    Some(Ok(value.get("result").cloned().unwrap_or(Value::Null)))
}

/// Connects to a streamable-HTTP endpoint and performs the handshake.
pub async fn connect_streamable(
    name: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    limits: &UpstreamLimits,
    _events: broadcast::Sender<UpstreamEvent>,
) -> Result<ConnectedUpstream, UpstreamError> {
    let client = reqwest::Client::builder().build().map_err(|error| {
        UpstreamError::Transport(format!("could not build an HTTP client: {error}"))
    })?;
    let upstream = Arc::new(StreamableHttpUpstream {
        name: name.to_owned(),
        url: url.to_owned(),
        headers: headers.clone(),
        client,
        session_id: Mutex::new(None),
        protocol_version: crate::LATEST_PROTOCOL_VERSION.to_owned(),
        next_id: AtomicI64::new(1),
        server_info: std::sync::OnceLock::new(),
        capabilities: std::sync::OnceLock::new(),
        instructions: std::sync::OnceLock::new(),
    });
    let initialize = upstream
        .request("initialize", initialize_params(), limits.request_timeout)
        .await?;
    let capabilities =
        ServerCapabilities::from_json(initialize.get("capabilities").unwrap_or(&Value::Null));
    let _ = upstream.capabilities.set(capabilities);
    let _ = upstream.server_info.set(
        initialize
            .get("serverInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    );
    let _ = upstream.instructions.set(
        initialize
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_owned),
    );
    upstream
        .notify("notifications/initialized", Value::Null)
        .await?;
    discover(upstream, limits).await
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, UpstreamError>>>>>;

/// A legacy HTTP+SSE connection.
pub struct LegacySseUpstream {
    name: String,
    post_url: String,
    headers: BTreeMap<String, String>,
    client: reqwest::Client,
    pending: Pending,
    next_id: AtomicI64,
    closed: AtomicBool,
    reader: Mutex<Option<JoinHandle<()>>>,
    server_info: std::sync::OnceLock<Option<String>>,
    capabilities: std::sync::OnceLock<ServerCapabilities>,
    instructions: std::sync::OnceLock<Option<String>>,
}

impl LegacySseUpstream {
    async fn post(&self, body: &Value) -> Result<(), UpstreamError> {
        let mut request = self
            .client
            .post(&self.post_url)
            .header(CONTENT_TYPE, "application/json")
            .json(body);
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        let response = request.send().await.map_err(|error| {
            UpstreamError::Transport(format!("HTTP request to \"{}\" failed: {error}", self.name))
        })?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(UpstreamError::Transport(format!(
            "\"{}\" rejected a message with HTTP {}",
            self.name,
            response.status()
        )))
    }
}

#[async_trait]
impl Upstream for LegacySseUpstream {
    fn server_info(&self) -> Option<&str> {
        self.server_info.get().and_then(|info| info.as_deref())
    }

    fn capabilities(&self) -> &ServerCapabilities {
        self.capabilities
            .get()
            .unwrap_or_else(|| empty_capabilities())
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.get().and_then(|text| text.as_deref())
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, UpstreamError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        if let Err(error) = self.post(&jsonrpc::request(id, method, params)).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(UpstreamError::Transport(format!(
                "\"{}\" closed before answering",
                self.name
            ))),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(UpstreamError::Timeout)
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), UpstreamError> {
        self.post(&jsonrpc::notification(method, params)).await
    }

    async fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(reader) = self.reader.lock().await.take() {
            reader.abort();
        }
        let mut pending = self.pending.lock().await;
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(UpstreamError::Transport(
                "the server was stopped".into(),
            )));
        }
    }
}

/// Connects to a legacy HTTP+SSE endpoint and performs the handshake.
pub async fn connect_legacy_sse(
    name: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    limits: &UpstreamLimits,
    events: broadcast::Sender<UpstreamEvent>,
) -> Result<ConnectedUpstream, UpstreamError> {
    let client = reqwest::Client::builder().build().map_err(|error| {
        UpstreamError::Transport(format!("could not build an HTTP client: {error}"))
    })?;
    let mut request = client.get(url).header(ACCEPT, "text/event-stream");
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let response = tokio::time::timeout(limits.request_timeout, request.send())
        .await
        .map_err(|_| UpstreamError::Timeout)?
        .map_err(|error| UpstreamError::Transport(format!("SSE connect failed: {error}")))?;
    if !response.status().is_success() {
        return Err(UpstreamError::Transport(format!(
            "the SSE endpoint answered HTTP {}",
            response.status()
        )));
    }

    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let (endpoint_tx, endpoint_rx) = oneshot::channel();
    let reader = tokio::spawn(read_sse_stream(
        response,
        Arc::clone(&pending),
        url.to_owned(),
        name.to_owned(),
        events,
        endpoint_tx,
    ));
    let post_url = tokio::time::timeout(limits.request_timeout, endpoint_rx)
        .await
        .map_err(|_| UpstreamError::Timeout)?
        .map_err(|_| {
            UpstreamError::Protocol("the SSE stream closed before its endpoint event".into())
        })??;

    let upstream = Arc::new(LegacySseUpstream {
        name: name.to_owned(),
        post_url,
        headers: headers.clone(),
        client,
        pending,
        next_id: AtomicI64::new(1),
        closed: AtomicBool::new(false),
        reader: Mutex::new(Some(reader)),
        server_info: std::sync::OnceLock::new(),
        capabilities: std::sync::OnceLock::new(),
        instructions: std::sync::OnceLock::new(),
    });

    let initialize = upstream
        .request("initialize", initialize_params(), limits.request_timeout)
        .await?;
    let capabilities =
        ServerCapabilities::from_json(initialize.get("capabilities").unwrap_or(&Value::Null));
    let _ = upstream.capabilities.set(capabilities);
    let _ = upstream.server_info.set(
        initialize
            .get("serverInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    );
    let _ = upstream.instructions.set(
        initialize
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_owned),
    );
    upstream
        .notify("notifications/initialized", Value::Null)
        .await?;
    discover(upstream, limits).await
}

async fn read_sse_stream(
    response: reqwest::Response,
    pending: Pending,
    base_url: String,
    name: String,
    events: broadcast::Sender<UpstreamEvent>,
    endpoint_tx: oneshot::Sender<Result<String, UpstreamError>>,
) {
    let mut endpoint_tx = Some(endpoint_tx);
    let mut decoder = SseDecoder::default();
    let mut utf8 = Utf8Stream::default();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                if let Some(sender) = endpoint_tx.take() {
                    let _ = sender.send(Err(UpstreamError::Transport(error.to_string())));
                }
                break;
            }
        };
        let text = utf8.push(&chunk);
        for event in decoder.push(&text) {
            match event.event.as_deref() {
                Some("endpoint") | None if endpoint_tx.is_some() => {
                    if let Some(sender) = endpoint_tx.take() {
                        let _ = sender.send(Ok(resolve_endpoint(&base_url, event.data.trim())));
                    }
                }
                _ => {}
            }
            if event.event.as_deref() == Some("endpoint") || endpoint_tx.is_some() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            match jsonrpc::parse_message(value) {
                Ok(Incoming::Response { id, result, error }) => {
                    let Some(numeric) = jsonrpc::numeric_id(&id) else {
                        continue;
                    };
                    let Some(sender) = pending.lock().await.remove(&numeric) else {
                        continue;
                    };
                    let outcome = match (result, error) {
                        (_, Some(error)) => Err(UpstreamError::Rpc(error)),
                        (Some(result), None) => Ok(result),
                        (None, None) => Err(UpstreamError::Protocol(
                            "a response carried neither result nor error".into(),
                        )),
                    };
                    let _ = sender.send(outcome);
                }
                Ok(Incoming::Notification { method, .. }) => {
                    let event = match method.as_str() {
                        "notifications/tools/list_changed" => Some(UpstreamEvent::ToolsChanged {
                            server: name.clone(),
                        }),
                        "notifications/resources/list_changed" => {
                            Some(UpstreamEvent::ResourcesChanged {
                                server: name.clone(),
                            })
                        }
                        "notifications/prompts/list_changed" => {
                            Some(UpstreamEvent::PromptsChanged {
                                server: name.clone(),
                            })
                        }
                        _ => None,
                    };
                    if let Some(event) = event {
                        let _ = events.send(event);
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(sender) = endpoint_tx.take() {
        let _ = sender.send(Err(UpstreamError::Protocol(
            "the SSE stream closed before its endpoint event".into(),
        )));
    }
    let mut pending = pending.lock().await;
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err(UpstreamError::Transport(format!("\"{name}\" closed"))));
    }
}

fn resolve_endpoint(base_url: &str, endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return endpoint.to_owned();
    }
    match reqwest::Url::parse(base_url) {
        Ok(url) => url
            .join(endpoint)
            .map(|joined| joined.to_string())
            .unwrap_or_else(|_| endpoint.to_owned()),
        Err(_) => endpoint.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_decoder_survives_split_chunks() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push("event: endpoint\ndata: /mess").is_empty());
        let events = decoder.push("age\n\n");
        assert_eq!(
            events,
            vec![SseEvent {
                event: Some("endpoint".into()),
                data: "/message".into(),
            }]
        );
    }

    #[test]
    fn sse_data_survives_a_utf8_character_split_across_chunks() {
        let text = "event: message\ndata: {\"text\":\"🚀日本\"}\n\n";
        let bytes = text.as_bytes();
        for split in 0..=bytes.len() {
            let mut stream = Utf8Stream::default();
            let mut decoder = SseDecoder::default();
            let mut events = decoder.push(&stream.push(&bytes[..split]));
            events.extend(decoder.push(&stream.push(&bytes[split..])));
            assert_eq!(events.len(), 1, "split at byte {split}");
            assert_eq!(
                events[0].data, "{\"text\":\"🚀日本\"}",
                "split at byte {split}"
            );
        }
    }

    #[test]
    fn sse_decoder_joins_multiline_data_and_ignores_comments() {
        let mut decoder = SseDecoder::default();
        let events = decoder.push(": keepalive\n\nevent: message\ndata: {\"a\":\ndata: 1}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message"));
        assert_eq!(events[0].data, "{\"a\":\n1}");
    }

    #[test]
    fn endpoints_resolve_relative_to_the_stream_url() {
        assert_eq!(
            resolve_endpoint("http://127.0.0.1:9/sse", "/message?sessionId=x"),
            "http://127.0.0.1:9/message?sessionId=x"
        );
        assert_eq!(
            resolve_endpoint("http://host/sse", "https://other/message"),
            "https://other/message"
        );
    }
}
