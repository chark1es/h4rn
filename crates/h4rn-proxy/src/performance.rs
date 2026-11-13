//! Fork-only `/v8/management/performance` reader (ADR-024).
//!
//! The endpoint is the only throughput surface in CLIProxyAPI: it reports one
//! record per upstream call with output tokens, TTFT, duration and TPS. This
//! module is a thin typed reader; attribution and dedupe against h4rn's own
//! observations happen in `h4rn-meter`.

use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::error::{ProxyError, ProxyErrorKind};
use crate::probe::ProbeTarget;

/// One upstream call reported by the proxy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PerformanceCall {
    /// Proxy request id, when the fork reports one.
    #[serde(default)]
    pub id: serde_json::Value,
    /// Provider that served the call.
    #[serde(default)]
    pub provider: Option<String>,
    /// Model id.
    #[serde(default)]
    pub model: Option<String>,
    /// When the request started.
    pub requested_at: DateTime<Utc>,
    /// Whether the upstream call failed.
    #[serde(default)]
    pub failed: bool,
    /// Completion tokens the upstream reported.
    #[serde(default)]
    pub output_tokens: u64,
    /// Request latency in milliseconds.
    #[serde(default)]
    pub latency_ms: Option<u64>,
    /// Time to first token.
    #[serde(default)]
    pub ttft_ms: Option<u64>,
    /// Generation duration.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// Output tokens per generation second.
    #[serde(default)]
    pub tps: Option<f64>,
    /// How the fork measured time (`generation`, `end_to_end`, `mixed`, `unavailable`).
    #[serde(default)]
    pub timing_basis: Option<String>,
    /// Whether the request streamed.
    #[serde(default)]
    pub stream: Option<bool>,
}

impl PerformanceCall {
    /// A stable id, falling back to the provider/model/time tuple.
    pub fn stable_id(&self) -> String {
        match &self.id {
            serde_json::Value::String(value) if !value.is_empty() => value.clone(),
            serde_json::Value::Number(value) => value.to_string(),
            _ => format!(
                "{}-{}-{}",
                self.provider.as_deref().unwrap_or("unknown"),
                self.model.as_deref().unwrap_or("unknown"),
                self.requested_at.timestamp_millis()
            ),
        }
    }

    /// Completion time estimated from the start plus the measured duration.
    pub fn completed_at(&self) -> DateTime<Utc> {
        self.requested_at + chrono::Duration::milliseconds(self.duration_ms.unwrap_or(0) as i64)
    }
}

#[derive(Debug, Deserialize)]
struct PerformanceResponse {
    #[serde(default)]
    requests: Vec<PerformanceCall>,
}

/// Reads one page of per-call performance records.
pub async fn read_performance(
    http: &Client,
    target: &ProbeTarget,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    limit: u32,
) -> Result<Vec<PerformanceCall>, ProxyError> {
    let response = http
        .get(format!(
            "{}/v8/management/performance",
            crate::overrides::root_base(&target.base_url)
        ))
        .query(&[
            ("from", since.to_rfc3339()),
            ("to", until.to_rfc3339()),
            ("limit", limit.to_string()),
        ])
        .header(
            "Authorization",
            format!(
                "Bearer {}",
                target.management_key.clone().unwrap_or_default()
            ),
        )
        .send()
        .await
        .map_err(|error| {
            ProxyError::new(
                ProxyErrorKind::Upstream,
                format!("read proxy performance: {error}"),
            )
            .with_hint("Check that CLIProxyAPI is running and the management key is current")
        })?;
    if !response.status().is_success() {
        return Err(ProxyError::new(
            ProxyErrorKind::Upstream,
            format!(
                "the proxy answered {} for /v8/management/performance",
                response.status()
            ),
        )
        .with_hint("This route is fork-only; upstream CLIProxyAPI does not expose it"));
    }
    let body: PerformanceResponse = response.json().await.map_err(|error| {
        ProxyError::new(
            ProxyErrorKind::Upstream,
            format!("decode proxy performance: {error}"),
        )
    })?;
    Ok(body.requests)
}
