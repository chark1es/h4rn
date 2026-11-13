//! Tolerant parsing of Codex rate-limit notifications and error messages.
//!
//! Codex reports capacity in two places. `account/rateLimits/updated` carries
//! structured windows with `resetsAt` as Unix epoch seconds, and a failed turn
//! carries a free-form `error.message` that may mention a reset in prose. The
//! structured value is authoritative, but real servers have shipped the same
//! field as a number, a numeric string and an RFC 3339 string, so the parser
//! accepts all three and falls back to the prose parser for wallet-clock text.

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use crate::{LimitSignal, parse_reset_time as parse_text_reset_time};

/// Detects a provider limit in a Codex error message.
///
/// Codex's transport errors often read `429 Too Many Requests` without the
/// word "limit"; those are still capacity limits. Everything else delegates to
/// the shared tolerant message parser so both adapters agree on what pauses a
/// thread (ADR-004).
pub fn detect_limit(text: &str, now: DateTime<Utc>) -> Option<LimitSignal> {
    let lower = text.to_ascii_lowercase();
    if lower.trim().is_empty() {
        return None;
    }

    if lower.contains("too many requests") || lower.contains("429") {
        return Some(LimitSignal {
            reset_at: parse_reset_time(&Value::String(text.to_string()), now),
            message: truncate(text.trim(), 240),
        });
    }

    crate::detect_limit(text, now)
}

/// Extracts a reset time from a Codex `resetsAt` value.
///
/// Accepted shapes, in order: epoch seconds or milliseconds as a JSON number,
/// the same as a string, an RFC 3339 timestamp, and any prose the shared parser
/// understands (`resets at 3pm`, `resets in 2h`, ...).
pub fn parse_reset_time(value: &Value, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match value {
        Value::Number(number) => {
            let epoch = number
                .as_i64()
                .or_else(|| number.as_f64().map(|value| value as i64))?;
            epoch_to_utc(epoch)
        }
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            if let Ok(epoch) = text.parse::<i64>() {
                return epoch_to_utc(epoch);
            }
            if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
                return Some(parsed.with_timezone(&Utc));
            }
            parse_text_reset_time(text, now)
        }
        _ => None,
    }
}

/// Returns the reset time when a rate-limit snapshot says the account is out.
///
/// `None` means the snapshot is an ordinary utilisation update and must not
/// pause the thread. `Some(None)` means the account is out but the server did
/// not publish a reset clock, which the daemon represents as a limit without
/// auto-resume (ADR-004).
pub fn rate_limit_hit(params: &Value, now: DateTime<Utc>) -> Option<Option<DateTime<Utc>>> {
    let limits = &params["rateLimits"];
    let reached = limits["rateLimitReachedType"].as_str().is_some()
        || limits["spendControlReached"].as_bool() == Some(true)
        || window_used_percent(&limits["primary"]) >= 100
        || window_used_percent(&limits["secondary"]) >= 100;

    if !reached {
        return None;
    }

    let reset = ["primary", "secondary"]
        .iter()
        .find_map(|key| {
            let window = &limits[*key];
            if window_used_percent(window) >= 100 {
                parse_reset_time(&window["resetsAt"], now)
            } else {
                None
            }
        })
        .or_else(|| parse_reset_time(&limits["primary"]["resetsAt"], now))
        .or_else(|| parse_reset_time(&limits["secondary"]["resetsAt"], now));

    Some(reset)
}

fn window_used_percent(window: &Value) -> u64 {
    window["usedPercent"]
        .as_u64()
        .or_else(|| window["usedPercent"].as_f64().map(|value| value as u64))
        .unwrap_or(0)
}

fn epoch_to_utc(epoch: i64) -> Option<DateTime<Utc>> {
    if epoch.abs() > 100_000_000_000 {
        Utc.timestamp_millis_opt(epoch).single()
    } else {
        Utc.timestamp_opt(epoch, 0).single()
    }
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
    use super::*;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 12, 12, 0, 0).unwrap()
    }

    #[test]
    fn parses_epoch_seconds_and_milliseconds() {
        assert_eq!(
            parse_reset_time(&json!(1_794_805_200), now())
                .expect("seconds")
                .timestamp(),
            1_794_805_200
        );
        assert_eq!(
            parse_reset_time(&json!(1_794_805_200_000i64), now())
                .expect("millis")
                .timestamp_millis(),
            1_794_805_200_000
        );
    }

    #[test]
    fn parses_numeric_and_iso_strings() {
        assert_eq!(
            parse_reset_time(&json!("1794805200"), now())
                .expect("numeric string")
                .timestamp(),
            1_794_805_200
        );
        assert_eq!(
            parse_reset_time(&json!("2026-11-13T15:00:00Z"), now())
                .expect("iso")
                .timestamp(),
            1_794_582_000
        );
    }

    #[test]
    fn falls_back_to_prose_reset_times() {
        use chrono::Local;

        let reset = parse_reset_time(&json!("resets at 2026-11-13 15:00"), now()).expect("prose");
        let expected = Local
            .with_ymd_and_hms(2026, 11, 13, 15, 0, 0)
            .single()
            .expect("local time")
            .with_timezone(&Utc);
        assert_eq!(reset, expected);
    }

    #[test]
    fn only_reports_a_limit_when_the_account_is_out() {
        let ordinary = json!({
            "rateLimits": {
                "primary": {"usedPercent": 20, "resetsAt": 1_794_805_200},
                "secondary": {"usedPercent": 38, "resetsAt": 1_795_071_423},
                "rateLimitReachedType": null,
                "spendControlReached": null
            }
        });
        assert_eq!(rate_limit_hit(&ordinary, now()), None);

        let exhausted = json!({
            "rateLimits": {
                "primary": {"usedPercent": 100, "resetsAt": 1_794_805_200},
                "secondary": null,
                "rateLimitReachedType": null,
                "spendControlReached": null
            }
        });
        assert_eq!(
            rate_limit_hit(&exhausted, now())
                .expect("limit")
                .map(|at| at.timestamp()),
            Some(1_794_805_200)
        );
    }

    #[test]
    fn recognizes_429_transport_errors() {
        let signal = detect_limit("stream error: 429 Too Many Requests", now()).expect("limit");
        assert!(signal.reset_at.is_none());
    }
}
