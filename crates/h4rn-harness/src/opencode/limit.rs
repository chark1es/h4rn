//! Tolerant parsing of OpenCode provider errors for capacity limits.
//!
//! OpenCode reports a failed execution as a `Session.StructuredError`:
//! `{ type, message, status?, response?.body?, retryAfterMs? }`. A provider
//! that is out of capacity usually surfaces as a 429 (or as prose such as
//! "too many requests" / "rate limit"), with the reset clock in one of three
//! places: `retryAfterMs`, a `Retry-After`-style value inside the response
//! body, or prose in the message. The structured value is authoritative, and
//! the prose parser is shared with the Claude Code adapter so all adapters
//! agree on what pauses a thread (ADR-004).

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use crate::parse_reset_time as parse_text_reset_time;

/// A provider limit detected in an OpenCode error payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitSignal {
    /// When the provider says capacity returns, when it says so.
    pub reset_at: Option<DateTime<Utc>>,
    /// The provider's explanation, truncated for an error surface.
    pub message: String,
}

/// Detects a capacity limit in an OpenCode `Session.StructuredError`.
///
/// A 429 status is always a limit even when the message is opaque; the
/// remaining shapes delegate to the shared tolerant message parser.
pub fn detect_limit(error: &Value, now: DateTime<Utc>) -> Option<LimitSignal> {
    let status = error["status"].as_u64();
    let message = error_message(error);
    let lower = message.to_ascii_lowercase();

    let is_limit = status == Some(429)
        || lower.contains("too many requests")
        || lower.contains("rate limit")
        || lower.contains("rate-limit")
        || lower.contains("usage limit")
        || lower.contains("quota")
        || lower.contains("429");

    if !is_limit {
        return None;
    }

    let reset_at = retry_after_reset(error, now)
        .or_else(|| structured_reset(error, now))
        .or_else(|| parse_text_reset_time(&message, now));

    Some(LimitSignal {
        reset_at,
        message: truncate(message.trim(), 240),
    })
}

/// The reset time from `error.retryAfterMs`.
///
/// Both a number of milliseconds and a numeric string are accepted because
/// the field has appeared as both shapes in provider bridges.
pub fn retry_after_reset(error: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let milliseconds = error["retryAfterMs"]
        .as_u64()
        .or_else(|| error["retryAfterMs"].as_f64().map(|value| value as u64))
        .or_else(|| {
            error["retryAfterMs"]
                .as_str()
                .and_then(|value| value.trim().parse::<u64>().ok())
        })?;
    Some(now + chrono::Duration::milliseconds(milliseconds.min(i64::MAX as u64) as i64))
}

/// Parses an epoch-millisecond event clock (`session.retry.scheduled.at`,
/// `session.status.next`) into a reset time.
pub fn event_clock(value: &Value) -> Option<DateTime<Utc>> {
    let milliseconds = value
        .as_i64()
        .or_else(|| value.as_f64().map(|value| value as i64))?;
    if milliseconds.abs() > 100_000_000_000 {
        Utc.timestamp_millis_opt(milliseconds).single()
    } else {
        Utc.timestamp_opt(milliseconds, 0).single()
    }
}

fn structured_reset(error: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    for key in ["resetsAt", "resetAt", "reset_at", "resets_at"] {
        if let Some(reset) = parse_reset_value(&error[key], now) {
            return Some(reset);
        }
    }

    let body = error["response"]["body"].as_str()?;
    if let Ok(parsed) = serde_json::from_str::<Value>(body) {
        for key in [
            "retry_after",
            "retryAfter",
            "retry_after_ms",
            "retryAfterMs",
            "resetsAt",
            "resetAt",
            "reset_at",
            "resets_at",
        ] {
            if let Some(reset) = parse_reset_value(&parsed[key], now) {
                return Some(reset);
            }
        }
    }
    parse_text_reset_time(body, now)
}

fn parse_reset_value(value: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match value {
        Value::Number(number) => {
            let number = number
                .as_i64()
                .or_else(|| number.as_f64().map(|value| value as i64))?;
            if number.abs() > 100_000_000_000 {
                Utc.timestamp_millis_opt(number).single()
            } else if number > 1_000_000_000 {
                Utc.timestamp_opt(number, 0).single()
            } else {
                // A small number is a relative duration in seconds.
                Some(now + chrono::Duration::seconds(number.max(0)))
            }
        }
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            if let Ok(number) = text.parse::<i64>() {
                return parse_reset_value(&Value::from(number), now);
            }
            if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
                return Some(parsed.with_timezone(&Utc));
            }
            parse_text_reset_time(text, now)
        }
        _ => None,
    }
}

fn error_message(error: &Value) -> String {
    if let Some(message) = error["message"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
    {
        return message.to_string();
    }
    if let Some(body) = error["response"]["body"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
    {
        return body.to_string();
    }
    error.as_str().map(str::to_string).unwrap_or_default()
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_429_with_retry_after_becomes_a_limit_with_a_clock() {
        let now = Utc::now();
        let error = json!({
            "type": "provider",
            "message": "provider returned an error",
            "status": 429,
            "retryAfterMs": 90_000
        });
        let signal = detect_limit(&error, now).expect("limit");
        assert_eq!(signal.reset_at, Some(now + chrono::Duration::seconds(90)));
    }

    #[test]
    fn prose_rate_limits_are_detected_without_a_status() {
        let now = Utc::now();
        let error = json!({
            "type": "provider",
            "message": "Rate limit exceeded. Please try again later."
        });
        let signal = detect_limit(&error, now).expect("limit");
        assert_eq!(signal.reset_at, None);
        assert!(signal.message.contains("Rate limit"));
    }

    #[test]
    fn a_structured_body_supplies_the_retry_after() {
        let now = Utc::now();
        let error = json!({
            "type": "provider",
            "message": "too many requests",
            "status": 429,
            "response": { "body": "{\"retry_after\":120}" }
        });
        let signal = detect_limit(&error, now).expect("limit");
        assert_eq!(signal.reset_at, Some(now + chrono::Duration::seconds(120)));
    }

    #[test]
    fn ordinary_errors_are_not_limits() {
        let error = json!({
            "type": "provider",
            "message": "model not found",
            "status": 404
        });
        assert_eq!(detect_limit(&error, Utc::now()), None);
    }

    #[test]
    fn event_clocks_accept_epoch_milliseconds() {
        let value = json!(1_794_549_886_137_i64);
        let parsed = event_clock(&value).expect("clock");
        assert_eq!(parsed.timestamp_millis(), 1_794_549_886_137);
    }
}
