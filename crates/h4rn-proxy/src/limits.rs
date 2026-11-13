//! Credential health and cooldowns from the management API.
//!
//! CLIProxyAPI has no client-side rate limiter; what it has is per-credential
//! cooldowns with an optional model scope. Those are the only durable "limit"
//! signal available, so h4rn reads `/v0/management/auth-files` and maps
//! `CooldownView` into [`ProxyLimits`] for the Phase 5 auto-resume/failover
//! path.

use chrono::{DateTime, Utc};
use h4rn_protocol::{ProxyCooldown, ProxyCredential, ProxyLimits};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Client, StatusCode};
use serde_json::Value;

use crate::error::ProxyError;
use crate::profiles::ResolvedProfile;

const LIMIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Reads credential health for one profile.
pub async fn read_limits(
    http: &Client,
    profile: &ResolvedProfile,
) -> Result<ProxyLimits, ProxyError> {
    let Some(key) = profile.management_key.as_deref() else {
        return Ok(ProxyLimits {
            profile_id: profile.id.clone(),
            available: false,
            credentials: Vec::new(),
            observed_at: Utc::now(),
            note: Some("add a management key to read credential cooldowns".into()),
        });
    };

    let response = http
        .get(format!(
            "{}/v0/management/auth-files?page=1&page_size=200",
            crate::overrides::root_base(&profile.base_url)
        ))
        .timeout(LIMIT_TIMEOUT)
        .header(AUTHORIZATION, bearer(key))
        .send()
        .await
        .map_err(|_| {
            ProxyError::new(
                crate::error::ProxyErrorKind::Upstream,
                "could not reach the management auth-files endpoint",
            )
            .with_hint("Check that CLIProxyAPI is running and the management key is correct")
        })?;

    if matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        return Ok(ProxyLimits {
            profile_id: profile.id.clone(),
            available: false,
            credentials: Vec::new(),
            observed_at: Utc::now(),
            note: Some("the management key was rejected while reading credential health".into()),
        });
    }
    if !response.status().is_success() {
        return Ok(ProxyLimits {
            profile_id: profile.id.clone(),
            available: false,
            credentials: Vec::new(),
            observed_at: Utc::now(),
            note: Some(format!(
                "the auth-files endpoint returned HTTP {}; this proxy may predate cooldown reporting",
                response.status()
            )),
        });
    }

    let value: Value = response.json().await.map_err(|error| {
        ProxyError::new(
            crate::error::ProxyErrorKind::Upstream,
            format!("the auth-files response is not valid JSON: {error}"),
        )
    })?;

    let credentials = value
        .get("files")
        .and_then(Value::as_array)
        .map(|files| files.iter().map(credential).collect())
        .unwrap_or_default();

    Ok(ProxyLimits {
        profile_id: profile.id.clone(),
        available: true,
        credentials,
        observed_at: Utc::now(),
        note: None,
    })
}

fn credential(file: &Value) -> ProxyCredential {
    ProxyCredential {
        auth_index: string(file, &["auth_index"]),
        name: string(file, &["name"]),
        provider: string(file, &["provider"]),
        status: string(file, &["status"]),
        disabled: file
            .get("disabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        success: file.get("success").and_then(Value::as_u64),
        failed: file.get("failed").and_then(Value::as_u64),
        supports_quota: file
            .get("supports_quota")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        cooldowns: file
            .get("cooldowns")
            .and_then(Value::as_array)
            .map(|cooldowns| cooldowns.iter().map(cooldown).collect())
            .unwrap_or_default(),
    }
}

fn cooldown(value: &Value) -> ProxyCooldown {
    ProxyCooldown {
        scope: string(value, &["scope"]),
        model_key: string(value, &["model_key"]),
        reason: string(value, &["reason"]),
        retry_at: string(value, &["retry_at"]).and_then(|value| {
            DateTime::parse_from_rfc3339(&value)
                .ok()
                .map(|time| time.with_timezone(&Utc))
        }),
        remaining_seconds: value.get("remaining_seconds").and_then(Value::as_u64),
        backoff_level: value
            .get("backoff_level")
            .and_then(Value::as_u64)
            .and_then(|level| u32::try_from(level).ok()),
        http_status: value
            .get("http_status")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok()),
    }
}

fn string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
}

fn bearer(key: &str) -> HeaderValue {
    let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
        .unwrap_or_else(|_| HeaderValue::from_static("Bearer"));
    value.set_sensitive(true);
    value
}
