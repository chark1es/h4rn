//! Tolerant parsing of Claude Code's usage-limit and rate-limit messages.
//!
//! Claude Code is not a stable protocol. Its limit messages have changed shape
//! several times ("Claude AI usage limit reached|<epoch>", "5-hour limit
//! reached ∙ resets 3pm", "Weekly limit reached · resets Monday 9am", and a
//! plain 429 `rate_limit_error` body), and the vendored CLI is often older or
//! newer than the documentation. The parser therefore treats every reset time
//! as best effort: it extracts an epoch when one is present, otherwise it
//! resolves a wall-clock phrase in the host's local timezone. A missing reset
//! time is representable — the daemon then arms no auto-resume (ADR-004).

use chrono::{
    DateTime, Datelike, Duration, Local, LocalResult, NaiveDate, NaiveDateTime, NaiveTime,
    TimeZone, Utc, Weekday,
};

/// A provider limit and the reset time its message implies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitSignal {
    /// When the limit resets, resolved to UTC when the message says so.
    pub reset_at: Option<DateTime<Utc>>,
    /// Truncated copy of the message that raised the limit, for diagnostics.
    pub message: String,
}

/// Detects a usage-limit message and extracts its reset time.
///
/// `now` is passed in so callers (and tests) get deterministic wall-clock
/// resolution. The function is deliberately permissive about wording but
/// excludes limits that are not provider capacity limits: a context-window,
/// budget, fast-mode or concurrency message must never pause a thread.
pub fn detect_limit(text: &str, now: DateTime<Utc>) -> Option<LimitSignal> {
    let lower = text.to_ascii_lowercase();
    if lower.trim().is_empty() {
        return None;
    }

    // Limits that are not about provider capacity. "Grace window" is excluded
    // because Claude Code still lets the turn finish in that mode.
    const EXCLUDED: &[&str] = &[
        "context limit",
        "fast limit",
        "grace window",
        "concurrent",
        "nesting limit",
        "budget limit",
        "watch limit",
        "export limit",
        "subagent",
    ];
    if EXCLUDED.iter().any(|needle| lower.contains(needle)) {
        return None;
    }

    let mentions_limit = lower.contains("limit")
        || lower.contains("rate_limit")
        || lower.contains("rate limit")
        || lower.contains("quota")
        || lower.contains("out of credits");
    if !mentions_limit {
        return None;
    }

    // A limit noun alone is not enough: Claude Code prints informational lines
    // such as "Your weekly limit still applies". Require a hard-stop cue.
    let hard_stop = lower.contains("reached")
        || lower.contains("exceeded")
        || lower.contains("hit your")
        || lower.contains("429")
        || lower.contains("out of credits")
        || lower.contains("insufficient")
        || lower.contains("paused");
    if !hard_stop {
        return None;
    }

    Some(LimitSignal {
        reset_at: parse_reset_time(text, now),
        message: truncate(text.trim(), 240),
    })
}

/// Extracts a reset time from a limit message.
///
/// Supported shapes, in the order they are tried:
///
/// - `<prefix>|<epoch seconds or milliseconds>` (Claude Code's oldest form).
/// - `"resetsAt": <epoch>` / `resets_at=<epoch>` from a JSON error body.
/// - `resets at 2026-11-13T15:00:00Z`, `resets at 15:00`, `resets 3pm`,
///   `resets Monday 9am`, `resets tomorrow 09:00`.
/// - `resets in 2h 10m`.
///
/// Wall-clock phrases are resolved in [`chrono::Local`], because that is what
/// the CLI prints. Epochs are UTC.
pub fn parse_reset_time(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if let Some((_, rest)) = text.split_once('|')
        && let Some(epoch) = parse_epoch_token(rest.trim())
    {
        return epoch_to_utc(epoch);
    }

    let lower = text.to_ascii_lowercase();
    for key in ["resetsat", "resets_at", "resetat", "reset_at"] {
        if let Some(index) = lower.find(key) {
            let rest = text[index + key.len()..].trim_start_matches(|c: char| {
                c.is_whitespace() || c == ':' || c == '=' || c == '"'
            });
            if let Some(epoch) = parse_epoch_token(rest) {
                return epoch_to_utc(epoch);
            }
        }
    }

    for key in ["resets at ", "resets ", "reset at ", "reset "] {
        if let Some(index) = lower.find(key) {
            let phrase = &text[index + key.len()..];
            if let Some(reset) = parse_reset_phrase(phrase, now) {
                return Some(reset);
            }
        }
    }

    None
}

/// Parses a leading epoch token: 10 digits is seconds, 13 is milliseconds.
fn parse_epoch_token(text: &str) -> Option<i64> {
    let digits: String = text.chars().take_while(|c| c.is_ascii_digit()).collect();
    match digits.len() {
        10 | 13 => digits.parse().ok(),
        _ => None,
    }
}

fn epoch_to_utc(epoch: i64) -> Option<DateTime<Utc>> {
    if epoch.abs() > 100_000_000_000 {
        Utc.timestamp_millis_opt(epoch).single()
    } else {
        Utc.timestamp_opt(epoch, 0).single()
    }
}

/// Parses the text following "resets" (or "in") into an absolute time.
fn parse_reset_phrase(phrase: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let phrase = cut_phrase(phrase);
    let lower = phrase.to_ascii_lowercase();

    if let Some(rest) = lower.strip_prefix("in ")
        && let Some(delta) = parse_human_duration(rest)
    {
        return Some(now + delta);
    }

    if let Some(absolute) = parse_iso(phrase) {
        return Some(absolute);
    }

    let local_now = now.with_timezone(&Local);
    let today = local_now.date_naive();
    let mut rest = phrase;
    let mut explicit: Option<NaiveDate> = None;

    if let Some(stripped) = strip_word(rest, "today") {
        explicit = Some(today);
        rest = stripped;
    } else if let Some(stripped) = strip_word(rest, "tomorrow") {
        explicit = today.succ_opt();
        rest = stripped;
    } else if let Some((weekday, stripped)) = strip_weekday(rest) {
        let mut date = next_weekday(today, weekday);
        if let Some(time) = parse_clock(stripped) {
            if date.and_time(time) <= local_now.naive_local() {
                date = date.checked_add_days(chrono::Days::new(7)).unwrap_or(date);
            }
            return local_to_utc(date.and_time(time));
        }
        explicit = Some(date);
        rest = stripped;
    } else if let Some((date, stripped)) = parse_date_prefix(rest) {
        explicit = Some(date);
        rest = stripped;
    }

    let time = parse_clock(rest)?;
    match explicit {
        Some(date) => local_to_utc(date.and_time(time)),
        None => {
            let candidate = today.and_time(time);
            let date = if candidate > local_now.naive_local() {
                today
            } else {
                today.succ_opt()?
            };
            local_to_utc(date.and_time(time))
        }
    }
}

/// Cuts a phrase at the first character that cannot belong to a reset time.
fn cut_phrase(phrase: &str) -> &str {
    let phrase = phrase.trim_start_matches(['"', ' ', '\'']);
    let end = phrase
        .find(['.', '\n', '"', '}', ';', '\u{a0}'])
        .unwrap_or(phrase.len());
    phrase[..end].trim().trim_end_matches([')', ',']).trim()
}

fn strip_word<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let lower = text.to_ascii_lowercase();
    if !lower.starts_with(word) {
        return None;
    }
    let rest = &text[word.len()..];
    if rest
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(rest.trim_start())
}

fn strip_weekday(text: &str) -> Option<(Weekday, &str)> {
    const NAMES: &[(&str, Weekday)] = &[
        ("monday", Weekday::Mon),
        ("mon", Weekday::Mon),
        ("tuesday", Weekday::Tue),
        ("tues", Weekday::Tue),
        ("tue", Weekday::Tue),
        ("wednesday", Weekday::Wed),
        ("wed", Weekday::Wed),
        ("thursday", Weekday::Thu),
        ("thurs", Weekday::Thu),
        ("thu", Weekday::Thu),
        ("friday", Weekday::Fri),
        ("fri", Weekday::Fri),
        ("saturday", Weekday::Sat),
        ("sat", Weekday::Sat),
        ("sunday", Weekday::Sun),
        ("sun", Weekday::Sun),
    ];
    let lower = text.to_ascii_lowercase();
    // Longest name first, so "thurs" wins over "thu" and "tuesday" over "tue".
    for (name, weekday) in NAMES {
        if lower.starts_with(name)
            && lower[name.len()..]
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || c == ',' || c.is_ascii_digit())
        {
            return Some((*weekday, text[name.len()..].trim_start_matches([',', ' '])));
        }
    }
    None
}

fn next_weekday(today: NaiveDate, weekday: Weekday) -> NaiveDate {
    let current = today.weekday().num_days_from_monday() as i64;
    let target = weekday.num_days_from_monday() as i64;
    let ahead = (target - current).rem_euclid(7);
    today
        .checked_add_days(chrono::Days::new(ahead as u64))
        .unwrap_or(today)
}

/// Parses an explicit date prefix such as `2026-11-13` or `Nov 13`.
fn parse_date_prefix(text: &str) -> Option<(NaiveDate, &str)> {
    let text = text.trim_start();

    let iso_len = text
        .bytes()
        .take_while(|byte| byte.is_ascii_digit() || *byte == b'-')
        .count();
    if iso_len >= 10
        && let Ok(date) = NaiveDate::parse_from_str(&text[..iso_len], "%Y-%m-%d")
    {
        let rest = text[iso_len..].trim_start_matches([' ', ',', 'T']);
        return Some((date, rest));
    }

    let month_word = text.split_whitespace().next()?;
    let month = month_number(month_word)?;
    let after_month = text[month_word.len()..].trim_start_matches([' ', ',']);
    let day_word = after_month.split_whitespace().next()?;
    let day: u32 = day_word.trim_end_matches([',', '.']).parse().ok()?;
    let after_day = after_month[day_word.len()..].trim_start_matches([' ', ',']);

    let mut year = Local::now().year();
    let mut rest = after_day;
    if let Some(year_word) = after_day.split_whitespace().next()
        && let Ok(parsed) = year_word.trim_end_matches([',', '.']).parse::<i32>()
    {
        year = parsed;
        rest = after_day[year_word.len()..].trim_start_matches([' ', ',']);
    }

    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    Some((date, rest))
}

fn month_number(word: &str) -> Option<u32> {
    const MONTHS: &[(&str, u32)] = &[
        ("january", 1),
        ("february", 2),
        ("march", 3),
        ("april", 4),
        ("may", 5),
        ("june", 6),
        ("july", 7),
        ("august", 8),
        ("september", 9),
        ("october", 10),
        ("november", 11),
        ("december", 12),
        ("jan", 1),
        ("feb", 2),
        ("mar", 3),
        ("apr", 4),
        ("jun", 6),
        ("jul", 7),
        ("aug", 8),
        ("sep", 9),
        ("sept", 9),
        ("oct", 10),
        ("nov", 11),
        ("dec", 12),
    ];
    let word = word.trim_end_matches(['.', ',']).to_ascii_lowercase();
    MONTHS
        .iter()
        .find_map(|(name, month)| (*name == word).then_some(*month))
}

/// Parses a clock token: `3pm`, `3:30 pm`, `15:00`, `09:00:00`.
fn parse_clock(text: &str) -> Option<NaiveTime> {
    let text = text.trim();
    let text = text.strip_prefix("at ").unwrap_or(text).trim();
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut number = String::new();
    while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b':') {
        number.push(bytes[index] as char);
        index += 1;
    }
    if number.is_empty() {
        return None;
    }
    while index < bytes.len() && bytes[index] == b' ' {
        index += 1;
    }
    let mut suffix = String::new();
    while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
        suffix.push((bytes[index] as char).to_ascii_lowercase());
        index += 1;
    }
    let suffix: String = suffix.chars().filter(|c| c.is_ascii_alphabetic()).collect();

    let mut parts = number.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = match parts.next() {
        Some(part) => part.parse().ok()?,
        None => 0,
    };
    let second: u32 = match parts.next() {
        Some(part) => part.parse().ok()?,
        None => 0,
    };

    let hour = match suffix.as_str() {
        "" | "h" => hour,
        "am" | "a" => {
            if hour == 12 {
                0
            } else {
                hour
            }
        }
        "pm" | "p" => {
            if hour == 12 {
                12
            } else {
                hour + 12
            }
        }
        _ => return None,
    };

    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    NaiveTime::from_hms_opt(hour, minute, second)
}

/// Parses durations such as `2h 10m`, `45m`, `2 hours`, `90 minutes`.
fn parse_human_duration(text: &str) -> Option<Duration> {
    let mut total_seconds: i64 = 0;
    let mut digits = String::new();
    let mut unit = String::new();

    let flush = |digits: &mut String, unit: &mut String, total: &mut i64| -> Option<()> {
        if digits.is_empty() {
            unit.clear();
            return Some(());
        }
        let value: i64 = digits.parse().ok()?;
        let seconds = match unit.as_str() {
            "h" | "hr" | "hrs" | "hour" | "hours" => value.checked_mul(3600)?,
            "m" | "min" | "mins" | "minute" | "minutes" => value.checked_mul(60)?,
            "s" | "sec" | "secs" | "second" | "seconds" => value,
            "d" | "day" | "days" => value.checked_mul(86_400)?,
            // A bare number after "in" is ambiguous; assume minutes, which is
            // how the CLI's "resets in 30" reads.
            "" => value.checked_mul(60)?,
            _ => return None,
        };
        *total = total.checked_add(seconds)?;
        digits.clear();
        unit.clear();
        Some(())
    };

    for character in text.chars() {
        if character.is_ascii_digit() {
            if !unit.is_empty() {
                flush(&mut digits, &mut unit, &mut total_seconds)?;
            }
            digits.push(character);
        } else if character.is_ascii_alphabetic() {
            unit.push(character.to_ascii_lowercase());
        } else {
            flush(&mut digits, &mut unit, &mut total_seconds)?;
        }
    }
    flush(&mut digits, &mut unit, &mut total_seconds)?;

    if total_seconds == 0 {
        None
    } else {
        Some(Duration::seconds(total_seconds))
    }
}

fn parse_iso(text: &str) -> Option<DateTime<Utc>> {
    let candidate = cut_phrase(text);
    if let Ok(parsed) = DateTime::parse_from_rfc3339(candidate) {
        return Some(parsed.with_timezone(&Utc));
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(candidate, format) {
            return local_to_utc(naive);
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(candidate, "%Y-%m-%d") {
        return local_to_utc(date.and_time(NaiveTime::MIN));
    }
    None
}

fn local_to_utc(naive: NaiveDateTime) -> Option<DateTime<Utc>> {
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(value) => Some(value.with_timezone(&Utc)),
        LocalResult::Ambiguous(first, second) => Some(first.min(second).with_timezone(&Utc)),
        LocalResult::None => None,
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
    use chrono::Timelike;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 12, 12, 0, 0).unwrap()
    }

    #[test]
    fn parses_the_legacy_pipe_epoch() {
        let signal =
            detect_limit("Claude AI usage limit reached|1794805200", now()).expect("limit");
        assert_eq!(
            signal.reset_at,
            Some(Utc.timestamp_opt(1_794_805_200, 0).single().unwrap())
        );
    }

    #[test]
    fn parses_a_millisecond_epoch() {
        let reset = parse_reset_time("usage limit reached|1794805200000", now()).expect("reset");
        assert_eq!(reset.timestamp_millis(), 1_794_805_200_000);
    }

    #[test]
    fn parses_a_five_hour_wall_clock_reset() {
        let reset = parse_reset_time("5-hour limit reached ∙ resets 3pm", now()).expect("reset");
        assert!(reset > now());
        assert_eq!(reset.with_timezone(&Local).hour(), 15);
    }

    #[test]
    fn parses_a_weekly_named_day() {
        let reset =
            parse_reset_time("Weekly limit reached · resets Monday 9am", now()).expect("reset");
        assert_eq!(reset.with_timezone(&Local).weekday(), Weekday::Mon);
        assert_eq!(reset.with_timezone(&Local).hour(), 9);
        assert!(reset > now());
    }

    #[test]
    fn parses_an_iso_reset() {
        let reset = parse_reset_time(
            "API Error: 429 rate_limit_error resets at 2026-11-13T15:00:00Z",
            now(),
        )
        .expect("reset");
        assert_eq!(reset, Utc.with_ymd_and_hms(2026, 11, 13, 15, 0, 0).unwrap());
    }

    #[test]
    fn parses_a_relative_reset() {
        let reset = parse_reset_time("limit reached, resets in 2h 10m", now()).expect("reset");
        assert_eq!(reset, now() + Duration::minutes(130));
    }

    #[test]
    fn parses_a_json_resets_at_key() {
        let reset = parse_reset_time(r#"{"resetsAt": 1794805200}"#, now()).expect("reset");
        assert_eq!(reset.timestamp(), 1_794_805_200);
    }

    #[test]
    fn ignores_informational_limit_copy() {
        assert!(
            detect_limit(
                "Your weekly limit still applies, and responses may pause",
                now()
            )
            .is_none()
        );
        assert!(detect_limit("Context limit reached · 200k tokens", now()).is_none());
        assert!(detect_limit("Fast limit reached and temporarily disabled", now()).is_none());
        assert!(detect_limit("All good, nothing to see", now()).is_none());
    }

    #[test]
    fn parses_a_429_body_without_a_reset() {
        let signal = detect_limit(
            r#"API Error: 429 {"type":"error","error":{"type":"rate_limit_error","message":"Number of request tokens has exceeded your per-minute rate limit"}}"#,
            now(),
        )
        .expect("limit");
        assert!(signal.reset_at.is_none());
    }
}
