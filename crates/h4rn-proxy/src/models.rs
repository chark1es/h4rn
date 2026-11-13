//! Model listing and alias merging.
//!
//! `/v1/models` is three endpoints wearing one path: the response shape is
//! selected by request headers, not by path. h4rn asks twice — once with no
//! identifying header for the OpenAI shape and once with `Anthropic-Version`
//! for the Claude shape — and separately asks `/v1beta/models` for Gemini.
//! Aliases are merged from the management config when a management key is
//! available; the config's secret values are never copied into the catalogue.

use std::collections::BTreeMap;

use chrono::Utc;
use h4rn_protocol::{ProxyModel, ProxyModelCatalog, ProxyModelShape, ProxyModelSource};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Client, StatusCode};
use serde_json::Value;

use crate::error::{ProxyError, ProxyErrorKind};
use crate::profiles::ResolvedProfile;

const MODEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Reads the merged catalogue for one profile.
pub async fn list_models(
    http: &Client,
    profile: &ResolvedProfile,
) -> Result<ProxyModelCatalog, ProxyError> {
    let mut models: Vec<ProxyModel> = Vec::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut errors = Vec::new();
    let base = crate::overrides::root_base(&profile.base_url);

    match fetch_openai(http, profile, &base).await {
        Ok(entries) => merge(&mut models, &mut seen, entries),
        Err(error) => errors.push(error),
    }
    match fetch_claude(http, profile, &base).await {
        Ok(entries) => merge(&mut models, &mut seen, entries),
        Err(error) => errors.push(error),
    }
    match fetch_gemini(http, profile, &base).await {
        Ok(entries) => merge(&mut models, &mut seen, entries),
        Err(error) => errors.push(error),
    }

    if models.is_empty() {
        let detail = errors
            .first()
            .map(|error| error.to_string())
            .unwrap_or_else(|| "the proxy returned no models".into());
        // A rejected key is the actionable case; do not bury it under a
        // generic upstream error just because a second shape also returned 401.
        let unauthorized = !errors.is_empty()
            && errors
                .iter()
                .all(|error| error.kind == ProxyErrorKind::Unauthorized);
        let error = if unauthorized {
            ProxyError::new(ProxyErrorKind::Unauthorized, detail)
                .with_hint("Replace the API key on this profile with a key from `access.api-keys`")
        } else {
            ProxyError::new(
                ProxyErrorKind::Upstream,
                format!("could not read any model from the proxy: {detail}"),
            )
            .with_hint(
                "Check the base URL and API key; CLIProxyAPI serves /v1/models, \
                 /v1/models with Anthropic-Version, and /v1beta/models",
            )
        };
        return Err(error);
    }

    let mut aliases_merged = false;
    if let Some(config) = fetch_management_config(http, profile, &base).await {
        aliases_merged = merge_aliases(&config, &mut models, &mut seen);
    }

    models.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(ProxyModelCatalog {
        profile_id: profile.id.clone(),
        models,
        aliases_merged,
        fetched_at: Utc::now(),
    })
}

fn merge(
    models: &mut Vec<ProxyModel>,
    seen: &mut BTreeMap<String, usize>,
    entries: Vec<ProxyModel>,
) {
    for entry in entries {
        if let Some(index) = seen.get(&entry.id).copied() {
            let existing = &mut models[index];
            if existing.display_name.is_none() {
                existing.display_name = entry.display_name;
            }
            if existing.context_length.is_none() {
                existing.context_length = entry.context_length;
            }
            if existing.max_output_tokens.is_none() {
                existing.max_output_tokens = entry.max_output_tokens;
            }
            existing.fork_enriched |= entry.fork_enriched;
            continue;
        }
        seen.insert(entry.id.clone(), models.len());
        models.push(entry);
    }
}

async fn fetch_openai(
    http: &Client,
    profile: &ResolvedProfile,
    base: &str,
) -> Result<Vec<ProxyModel>, ProxyError> {
    let response = request(http, &format!("{base}/v1/models"), profile, None).await?;
    ensure_success(response.status(), "the OpenAI model list")?;
    let value: Value = response.json().await.map_err(|error| {
        ProxyError::new(
            ProxyErrorKind::Upstream,
            format!("the OpenAI model list returned invalid JSON: {error}"),
        )
    })?;
    Ok(value
        .get("data")
        .and_then(Value::as_array)
        .map(|data| {
            data.iter()
                .filter_map(|entry| {
                    let id = entry.get("id").and_then(Value::as_str)?;
                    Some(ProxyModel {
                        id: id.to_owned(),
                        display_name: string(entry, &["display_name", "name"]),
                        shape: ProxyModelShape::OpenAi,
                        source: ProxyModelSource::OpenAi,
                        alias_of: None,
                        owned_by: string(entry, &["owned_by"]),
                        context_length: number(entry, &["context_length", "max_context_length"]),
                        max_output_tokens: number(
                            entry,
                            &["max_output_length", "max_completion_tokens"],
                        ),
                        fork_enriched: entry.get("max_output_length").is_some()
                            || entry.get("reasoning_parameters").is_some(),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn fetch_claude(
    http: &Client,
    profile: &ResolvedProfile,
    base: &str,
) -> Result<Vec<ProxyModel>, ProxyError> {
    let response = request(
        http,
        &format!("{base}/v1/models"),
        profile,
        Some(("anthropic-version", "2023-06-01")),
    )
    .await?;
    ensure_success(response.status(), "the Claude model list")?;
    let value: Value = response.json().await.map_err(|error| {
        ProxyError::new(
            ProxyErrorKind::Upstream,
            format!("the Claude model list returned invalid JSON: {error}"),
        )
    })?;
    Ok(value
        .get("data")
        .and_then(Value::as_array)
        .map(|data| {
            data.iter()
                .filter_map(|entry| {
                    let id = entry.get("id").and_then(Value::as_str)?;
                    Some(ProxyModel {
                        id: id.to_owned(),
                        display_name: string(entry, &["display_name", "name"]),
                        shape: ProxyModelShape::Claude,
                        source: ProxyModelSource::Claude,
                        alias_of: None,
                        owned_by: string(entry, &["owned_by"]),
                        context_length: number(entry, &["max_input_tokens"]),
                        max_output_tokens: number(entry, &["max_tokens"]),
                        fork_enriched: false,
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn fetch_gemini(
    http: &Client,
    profile: &ResolvedProfile,
    base: &str,
) -> Result<Vec<ProxyModel>, ProxyError> {
    let response = request(http, &format!("{base}/v1beta/models"), profile, None).await?;
    ensure_success(response.status(), "the Gemini model list")?;
    let value: Value = response.json().await.map_err(|error| {
        ProxyError::new(
            ProxyErrorKind::Upstream,
            format!("the Gemini model list returned invalid JSON: {error}"),
        )
    })?;
    Ok(value
        .get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|entry| {
                    let name = entry.get("name").and_then(Value::as_str)?;
                    let id = name.strip_prefix("models/").unwrap_or(name);
                    Some(ProxyModel {
                        id: id.to_owned(),
                        display_name: string(entry, &["displayName"]),
                        shape: ProxyModelShape::Gemini,
                        source: ProxyModelSource::Gemini,
                        alias_of: None,
                        owned_by: None,
                        context_length: number(entry, &["inputTokenLimit"]),
                        max_output_tokens: number(entry, &["outputTokenLimit"]),
                        fork_enriched: false,
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn request(
    http: &Client,
    url: &str,
    profile: &ResolvedProfile,
    header: Option<(&str, &str)>,
) -> Result<reqwest::Response, ProxyError> {
    let mut builder = http.get(url).timeout(MODEL_TIMEOUT);
    if let Some((name, value)) = header {
        builder = builder.header(name, value);
    }
    if let Some(key) = profile.api_key.as_deref() {
        builder = builder.header(AUTHORIZATION, bearer(key));
    }
    builder.send().await.map_err(|_| {
        ProxyError::new(
            ProxyErrorKind::Upstream,
            "the proxy model endpoint could not be reached",
        )
        .with_hint("Check that CLIProxyAPI is running and the base URL points at its root")
    })
}

fn ensure_success(status: StatusCode, what: &str) -> Result<(), ProxyError> {
    if status.is_success() {
        return Ok(());
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(ProxyError::new(
            ProxyErrorKind::Unauthorized,
            format!("the proxy rejected the API key for {what}"),
        )
        .with_hint("Replace the API key on this profile with a key from `access.api-keys`"));
    }
    Err(ProxyError::new(
        ProxyErrorKind::Upstream,
        format!("{what} returned HTTP {status}"),
    ))
}

async fn fetch_management_config(
    http: &Client,
    profile: &ResolvedProfile,
    base: &str,
) -> Option<Value> {
    let key = profile.management_key.as_ref()?;
    for path in ["/v0/management/config", "/v8/management/config"] {
        let Ok(response) = http
            .get(format!("{base}{path}"))
            .timeout(MODEL_TIMEOUT)
            .header(AUTHORIZATION, bearer(key))
            .send()
            .await
        else {
            continue;
        };
        if response.status().is_success()
            && let Ok(value) = response.json::<Value>().await
            && value.is_object()
        {
            return Some(value);
        }
    }
    None
}

fn merge_aliases(
    config: &Value,
    models: &mut Vec<ProxyModel>,
    seen: &mut BTreeMap<String, usize>,
) -> bool {
    let mut merged = false;

    if let Some(channels) = config
        .get("oauth")
        .and_then(|oauth| oauth.get("model-alias"))
        .and_then(Value::as_object)
    {
        for (channel, entries) in channels {
            let Some(entries) = entries.as_array() else {
                continue;
            };
            for entry in entries {
                let Some(name) = entry.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let alias = entry.get("alias").and_then(Value::as_str).unwrap_or(name);
                let display = string(entry, &["display-name", "display_name"]);
                let shape = shape_for_channel(channel);
                if upsert_alias(
                    models,
                    seen,
                    alias,
                    name,
                    display,
                    ProxyModelSource::Alias,
                    shape,
                ) {
                    merged = true;
                }
            }
        }
    }

    if let Some(providers) = config.get("openai-compatibility").and_then(Value::as_array) {
        for provider in providers {
            let Some(entries) = provider.get("models").and_then(Value::as_array) else {
                continue;
            };
            for entry in entries {
                let Some(name) = entry.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let alias = entry.get("alias").and_then(Value::as_str).unwrap_or(name);
                let display = string(entry, &["display-name", "display_name"]);
                if upsert_alias(
                    models,
                    seen,
                    alias,
                    name,
                    display,
                    ProxyModelSource::Compatibility,
                    ProxyModelShape::OpenAi,
                ) {
                    merged = true;
                }
            }
        }
    }

    if let Some(providers) = config.get("api-keys").and_then(Value::as_object) {
        for (channel, credentials) in providers {
            let Some(credentials) = credentials.as_array() else {
                continue;
            };
            for credential in credentials {
                let Some(entries) = credential.get("models").and_then(Value::as_array) else {
                    continue;
                };
                for entry in entries {
                    let Some(name) = entry.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let alias = entry.get("alias").and_then(Value::as_str).unwrap_or(name);
                    let display = string(entry, &["display-name", "display_name"]);
                    if upsert_alias(
                        models,
                        seen,
                        alias,
                        name,
                        display,
                        ProxyModelSource::Credential,
                        shape_for_channel(channel),
                    ) {
                        merged = true;
                    }
                }
            }
        }
    }

    merged
}

#[allow(clippy::too_many_arguments)]
fn upsert_alias(
    models: &mut Vec<ProxyModel>,
    seen: &mut BTreeMap<String, usize>,
    id: &str,
    upstream: &str,
    display_name: Option<String>,
    source: ProxyModelSource,
    shape: ProxyModelShape,
) -> bool {
    if let Some(index) = seen.get(id).copied() {
        let existing = &mut models[index];
        if existing.alias_of.is_none() && existing.id != upstream {
            existing.alias_of = Some(upstream.to_owned());
            return true;
        }
        return false;
    }
    seen.insert(id.to_owned(), models.len());
    models.push(ProxyModel {
        id: id.to_owned(),
        display_name,
        shape,
        source,
        alias_of: (id != upstream).then(|| upstream.to_owned()),
        owned_by: None,
        context_length: None,
        max_output_tokens: None,
        fork_enriched: false,
    });
    true
}

fn shape_for_channel(channel: &str) -> ProxyModelShape {
    match channel.to_ascii_lowercase().as_str() {
        "claude" | "vertex" | "aistudio" | "antigravity" => ProxyModelShape::Claude,
        "gemini" | "gemini-interactions" => ProxyModelShape::Gemini,
        _ => ProxyModelShape::OpenAi,
    }
}

fn string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
}

fn number(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
}

fn bearer(key: &str) -> HeaderValue {
    let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
        .unwrap_or_else(|_| HeaderValue::from_static("Bearer"));
    value.set_sensitive(true);
    value
}
