//! Schedule arithmetic: turning a schedule into the next real instant.
//!
//! Every calculation takes an explicit time zone and an `after` instant, so a
//! test can move the clock by hand. Wall-clock schedules resolve through the
//! zone, which is what makes a daily 02:30 task follow daylight saving instead
//! of drifting by an hour. Intervals are absolute durations from the task's
//! creation, so "every 6 hours" stays six hours across a clock change.

use chrono::{Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use h4rn_protocol::{IntervalUnit, ScheduleSpec, parse_once_date, parse_schedule_time};

/// Why a schedule could not be turned into an instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleError(String);

impl ScheduleError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// The reason, written for the Settings form.
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Parses an IANA time zone name.
pub fn parse_timezone(name: &str) -> Result<Tz, ScheduleError> {
    name.trim().parse::<Tz>().map_err(|_| {
        ScheduleError::new(format!(
            "`{}` is not a known time zone. Use an IANA name such as Europe/Berlin.",
            name.trim()
        ))
    })
}

/// The daemon's own time zone, falling back to UTC.
///
/// `TZ` is read first so a service can pin a zone; otherwise the system zone
/// is used. An unreadable or unknown zone falls back to UTC instead of making
/// the daemon refuse to start.
pub fn daemon_timezone() -> (Tz, String) {
    let from_env = std::env::var("TZ")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| parse_timezone(&value).ok());
    let tz = from_env
        .or_else(|| iana_local_timezone().and_then(|name| parse_timezone(&name).ok()))
        .unwrap_or(chrono_tz::UTC);
    (tz, tz.name().to_owned())
}

/// The system's IANA zone name, when the platform reports one.
fn iana_local_timezone() -> Option<String> {
    if let Ok(path) = std::fs::read_link("/etc/localtime")
        && let Some(name) = path.to_str()
        && let Some((_, zone)) = name.split_once("zoneinfo/")
    {
        return Some(zone.to_owned());
    }
    std::fs::read_to_string("/etc/timezone")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Every instant a local wall clock names.
///
/// A normal time names one instant. The repeated hour of a fall-back names
/// two, earliest first. A time the spring-forward gap skipped names none, so
/// the first minute after the gap is used instead: a 02:30 task in a zone that
/// jumps 02:00 to 03:00 runs at 03:00 that day rather than not at all.
fn local_instants(tz: Tz, naive: NaiveDateTime) -> Vec<chrono::DateTime<Utc>> {
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(at) => vec![at.with_timezone(&Utc)],
        LocalResult::Ambiguous(first, second) => {
            let mut both = vec![first.with_timezone(&Utc), second.with_timezone(&Utc)];
            both.sort();
            both
        }
        LocalResult::None => {
            for minutes in 1..=180 {
                let shifted = naive + Duration::minutes(minutes);
                match tz.from_local_datetime(&shifted) {
                    LocalResult::Single(at) => return vec![at.with_timezone(&Utc)],
                    LocalResult::Ambiguous(first, _) => {
                        return vec![first.with_timezone(&Utc)];
                    }
                    LocalResult::None => {}
                }
            }
            Vec::new()
        }
    }
}

/// The earliest matching instant strictly after `after`.
fn first_after(
    tz: Tz,
    date: NaiveDate,
    time: NaiveTime,
    after: chrono::DateTime<Utc>,
) -> Option<chrono::DateTime<Utc>> {
    local_instants(tz, date.and_time(time))
        .into_iter()
        .find(|instant| *instant > after)
}

/// The next instant a schedule names, strictly after `after`.
///
/// `anchor` is when the task was created; interval schedules count from it.
/// Returns `Ok(None)` for a once schedule whose date has passed. An invalid or
/// never-firing expression is an error so the form can explain it instead of
/// silently saving a task that will never run.
pub fn next_after(
    spec: &ScheduleSpec,
    tz: Tz,
    anchor: chrono::DateTime<Utc>,
    after: chrono::DateTime<Utc>,
) -> Result<Option<chrono::DateTime<Utc>>, ScheduleError> {
    match spec {
        ScheduleSpec::Once { date, time } => {
            let date = parse_once_date(date).ok_or_else(|| {
                ScheduleError::new("The date must be a real calendar date, written YYYY-MM-DD.")
            })?;
            let time = parse_schedule_time(time).ok_or_else(|| {
                ScheduleError::new("The time must be a real time of day, written HH:MM.")
            })?;
            Ok(first_after(tz, date, time, after))
        }
        ScheduleSpec::Every { count, unit } => {
            if *count == 0 {
                return Err(ScheduleError::new("An interval needs at least one unit."));
            }
            let step = match unit {
                IntervalUnit::Minutes => Duration::minutes(i64::from(*count)),
                IntervalUnit::Hours => Duration::hours(i64::from(*count)),
            };
            let elapsed = after.signed_duration_since(anchor);
            let steps = if elapsed < Duration::zero() {
                1
            } else {
                elapsed.num_seconds() / step.num_seconds() + 1
            };
            Ok(Some(
                anchor + step * i32::try_from(steps).unwrap_or(i32::MAX),
            ))
        }
        ScheduleSpec::Daily { time } => {
            let time = parse_schedule_time(time).ok_or_else(|| {
                ScheduleError::new("The time must be a real time of day, written HH:MM.")
            })?;
            let local = after.with_timezone(&tz);
            for offset in 0..=2 {
                let Some(date) = local
                    .date_naive()
                    .checked_add_signed(Duration::days(offset))
                else {
                    break;
                };
                if let Some(next) = first_after(tz, date, time, after) {
                    return Ok(Some(next));
                }
            }
            Err(ScheduleError::new("No next daily run could be found."))
        }
        ScheduleSpec::Weekly { days, time } => {
            if days.is_empty() {
                return Err(ScheduleError::new(
                    "A weekly schedule needs at least one weekday.",
                ));
            }
            if days.iter().any(|day| !(1..=7).contains(day)) {
                return Err(ScheduleError::new(
                    "Weekdays are numbers from 1 (Monday) to 7 (Sunday).",
                ));
            }
            let time = parse_schedule_time(time).ok_or_else(|| {
                ScheduleError::new("The time must be a real time of day, written HH:MM.")
            })?;
            let local = after.with_timezone(&tz);
            for offset in 0..=14 {
                let Some(date) = local
                    .date_naive()
                    .checked_add_signed(Duration::days(offset))
                else {
                    break;
                };
                if !days.contains(&(date.weekday().number_from_monday() as u8)) {
                    continue;
                }
                if let Some(next) = first_after(tz, date, time, after) {
                    return Ok(Some(next));
                }
            }
            Err(ScheduleError::new("No next weekly run could be found."))
        }
        ScheduleSpec::Cron { expression } => {
            let cron = CronExpr::parse(expression)?;
            cron.next_after(tz, after)
        }
        ScheduleSpec::Webhook => Ok(None),
    }
}

/// Validates a schedule without needing a clock reading.
pub fn validate(spec: &ScheduleSpec) -> Result<(), ScheduleError> {
    match spec {
        ScheduleSpec::Once { date, time } => {
            parse_once_date(date)
                .ok_or_else(|| ScheduleError::new("The date must be YYYY-MM-DD."))?;
            parse_schedule_time(time)
                .ok_or_else(|| ScheduleError::new("The time must be HH:MM."))?;
            Ok(())
        }
        ScheduleSpec::Every { count, .. } => {
            if *count == 0 {
                Err(ScheduleError::new("An interval needs at least one unit."))
            } else {
                Ok(())
            }
        }
        ScheduleSpec::Daily { time } => parse_schedule_time(time)
            .map(|_| ())
            .ok_or_else(|| ScheduleError::new("The time must be HH:MM.")),
        ScheduleSpec::Weekly { days, time } => {
            if days.is_empty() {
                return Err(ScheduleError::new(
                    "A weekly schedule needs at least one weekday.",
                ));
            }
            if days.iter().any(|day| !(1..=7).contains(day)) {
                return Err(ScheduleError::new("Weekdays are 1 through 7."));
            }
            parse_schedule_time(time)
                .map(|_| ())
                .ok_or_else(|| ScheduleError::new("The time must be HH:MM."))
        }
        ScheduleSpec::Cron { expression } => CronExpr::parse(expression).map(|_| ()),
        ScheduleSpec::Webhook => Ok(()),
    }
}

/// A parsed five-field cron expression.
///
/// Fields are `minute hour day-of-month month day-of-week`. Each accepts `*`,
/// a number, a range (`1-5`), a step (`*/15`, `1-30/2`) and comma lists.
/// Weekdays are 0 (Sunday) through 6, with 7 also accepted as Sunday. Names
/// such as `MON` are not part of this contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronExpr {
    minutes: Vec<u32>,
    hours: Vec<u32>,
    days_of_month: Vec<u32>,
    months: Vec<u32>,
    days_of_week: Vec<u32>,
    day_of_month_wildcard: bool,
    day_of_week_wildcard: bool,
}

impl CronExpr {
    /// Parses the five whitespace-separated fields.
    pub fn parse(expression: &str) -> Result<Self, ScheduleError> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(ScheduleError::new(
                "A cron expression has five fields: minute hour day-of-month month day-of-week.",
            ));
        }
        let minutes = parse_field(fields[0], 0, 59, "minute")?;
        let hours = parse_field(fields[1], 0, 23, "hour")?;
        let days_of_month = parse_field(fields[2], 1, 31, "day of month")?;
        let months = parse_field(fields[3], 1, 12, "month")?;
        let mut days_of_week = parse_field(fields[4], 0, 7, "day of week")?;
        for day in &mut days_of_week {
            if *day == 7 {
                *day = 0;
            }
        }
        days_of_week.sort_unstable();
        days_of_week.dedup();
        Ok(Self {
            minutes,
            hours,
            days_of_month,
            months,
            days_of_week,
            day_of_month_wildcard: fields[2] == "*",
            day_of_week_wildcard: fields[4] == "*",
        })
    }

    /// Whether a local date matches the calendar fields.
    fn matches_date(&self, date: NaiveDate) -> bool {
        if !self.months.contains(&date.month()) {
            return false;
        }
        let dom = self.days_of_month.contains(&date.day());
        let dow = self
            .days_of_week
            .contains(&date.weekday().num_days_from_sunday());
        match (self.day_of_month_wildcard, self.day_of_week_wildcard) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            // Standard cron treats a restricted day-of-month and day-of-week
            // as "either", not "both".
            (false, false) => dom || dow,
        }
    }

    /// The next matching minute strictly after `after` in `tz`.
    pub fn next_after(
        &self,
        tz: Tz,
        after: chrono::DateTime<Utc>,
    ) -> Result<Option<chrono::DateTime<Utc>>, ScheduleError> {
        let start = after.with_timezone(&tz).date_naive();
        // Ten years covers a February 29th expression with room to spare while
        // keeping an impossible date (31 February) a quick validation error.
        for offset in 0..=3660 {
            let Some(date) = start.checked_add_signed(Duration::days(offset)) else {
                break;
            };
            if !self.matches_date(date) {
                continue;
            }
            for hour in &self.hours {
                for minute in &self.minutes {
                    let naive = date.and_hms_opt(*hour, *minute, 0).ok_or_else(|| {
                        ScheduleError::new("The cron expression names an invalid time.")
                    })?;
                    if let Some(next) = local_instants(tz, naive)
                        .into_iter()
                        .find(|instant| *instant > after)
                    {
                        return Ok(Some(next));
                    }
                }
            }
        }
        Err(ScheduleError::new(
            "This cron expression never matches a real date.",
        ))
    }
}

fn parse_field(
    field: &str,
    minimum: u32,
    maximum: u32,
    name: &str,
) -> Result<Vec<u32>, ScheduleError> {
    if field.is_empty() {
        return Err(ScheduleError::new(format!("The {name} field is empty.")));
    }
    let mut values = Vec::new();
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step = step.parse::<u32>().map_err(|_| {
                    ScheduleError::new(format!("The {name} step `{step}` is not a number."))
                })?;
                if step == 0 {
                    return Err(ScheduleError::new(format!(
                        "The {name} step must be at least 1."
                    )));
                }
                (range, step)
            }
            None => (part, 1),
        };
        let (start, end) = if range == "*" {
            (minimum, maximum)
        } else if let Some((start, end)) = range.split_once('-') {
            let start = parse_number(start, minimum, maximum, name)?;
            let end = parse_number(end, minimum, maximum, name)?;
            if start > end {
                return Err(ScheduleError::new(format!(
                    "The {name} range `{range}` runs backwards."
                )));
            }
            (start, end)
        } else {
            let value = parse_number(range, minimum, maximum, name)?;
            (value, value)
        };
        let mut value = start;
        while value <= end {
            values.push(value);
            value += step;
        }
    }
    values.sort_unstable();
    values.dedup();
    if values.is_empty() {
        return Err(ScheduleError::new(format!(
            "The {name} field names no value."
        )));
    }
    Ok(values)
}

fn parse_number(value: &str, minimum: u32, maximum: u32, name: &str) -> Result<u32, ScheduleError> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_| ScheduleError::new(format!("`{value}` is not a {name} number.")))?;
    if !(minimum..=maximum).contains(&parsed) {
        return Err(ScheduleError::new(format!(
            "The {name} `{parsed}` must be between {minimum} and {maximum}."
        )));
    }
    Ok(parsed)
}

/// A human sentence for a schedule, used in the list and the docs.
pub fn describe(spec: &ScheduleSpec) -> String {
    match spec {
        ScheduleSpec::Once { date, time } => format!("Once on {date} at {time}"),
        ScheduleSpec::Every { count, unit } => {
            format!("Every {count} {}", unit.as_str())
        }
        ScheduleSpec::Daily { time } => format!("Daily at {time}"),
        ScheduleSpec::Weekly { days, time } => {
            let names: Vec<&str> = days.iter().filter_map(|day| weekday_name(*day)).collect();
            format!("Weekly on {} at {time}", names.join(", "))
        }
        ScheduleSpec::Cron { expression } => format!("Cron `{expression}`"),
        ScheduleSpec::Webhook => "On webhook".to_owned(),
    }
}

fn weekday_name(day: u8) -> Option<&'static str> {
    match day {
        1 => Some("Mon"),
        2 => Some("Tue"),
        3 => Some("Wed"),
        4 => Some("Thu"),
        5 => Some("Fri"),
        6 => Some("Sat"),
        7 => Some("Sun"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(text: &str) -> chrono::DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339(text)
            .expect("timestamp")
            .with_timezone(&Utc)
    }

    fn berlin() -> Tz {
        "Europe/Berlin".parse().expect("zone")
    }

    fn new_york() -> Tz {
        "America/New_York".parse().expect("zone")
    }

    #[test]
    fn cron_parses_ranges_lists_and_steps() {
        let cron = CronExpr::parse("*/15 9-17 1,15 * 1-5").expect("cron");
        assert_eq!(cron.minutes, vec![0, 15, 30, 45]);
        assert_eq!(cron.hours, (9..=17).collect::<Vec<_>>());
        assert_eq!(cron.days_of_month, vec![1, 15]);
        assert_eq!(cron.days_of_week, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn cron_rejects_invalid_input() {
        for expression in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "5-1 * * * *",
            "a * * * *",
            "1,,2 * * * *",
        ] {
            assert!(
                CronExpr::parse(expression).is_err(),
                "{expression:?} should not parse"
            );
        }
    }

    #[test]
    fn cron_next_follows_fields_and_time_zone() {
        let cron = CronExpr::parse("30 9 * * 1-5").expect("cron");
        // Friday 2026-11-20 10:00 Berlin -> Monday 09:30 Berlin.
        let next = cron
            .next_after(berlin(), utc("2026-11-20T09:00:00Z"))
            .expect("next")
            .expect("a next run");
        assert_eq!(next, utc("2026-11-23T08:30:00Z"));
    }

    #[test]
    fn cron_day_of_month_and_weekday_are_either_when_both_restricted() {
        // The 1st of the month OR a Monday.
        let cron = CronExpr::parse("0 0 1 * 1").expect("cron");
        let next = cron
            .next_after(chrono_tz::UTC, utc("2026-11-03T00:00:00Z"))
            .expect("next")
            .expect("a next run");
        // 2026-11-09 is the next Monday, before 2026-12-01.
        assert_eq!(next, utc("2026-11-09T00:00:00Z"));
    }

    #[test]
    fn cron_can_reach_a_leap_day_and_rejects_an_impossible_date() {
        let leap = CronExpr::parse("0 0 29 2 *").expect("cron");
        let next = leap
            .next_after(chrono_tz::UTC, utc("2026-01-01T00:00:00Z"))
            .expect("next")
            .expect("a leap day");
        assert_eq!(next, utc("2028-02-29T00:00:00Z"));

        let impossible = CronExpr::parse("0 0 31 2 *").expect("parses");
        assert!(
            impossible
                .next_after(chrono_tz::UTC, utc("2026-01-01T00:00:00Z"))
                .is_err()
        );
    }

    #[test]
    fn once_uses_the_task_time_zone() {
        let spec = ScheduleSpec::Once {
            date: "2026-12-01".into(),
            time: "09:30".into(),
        };
        let next = next_after(
            &spec,
            berlin(),
            utc("2026-11-01T00:00:00Z"),
            utc("2026-11-20T00:00:00Z"),
        )
        .expect("next")
        .expect("a run");
        assert_eq!(next, utc("2026-12-01T08:30:00Z"));
    }

    #[test]
    fn a_past_once_has_no_next_run() {
        let spec = ScheduleSpec::Once {
            date: "2026-01-01".into(),
            time: "09:30".into(),
        };
        assert_eq!(
            next_after(
                &spec,
                berlin(),
                utc("2025-01-01T00:00:00Z"),
                utc("2026-11-20T00:00:00Z")
            )
            .expect("next"),
            None
        );
    }

    #[test]
    fn intervals_count_from_the_anchor_and_do_not_drift_across_dst() {
        let spec = ScheduleSpec::Every {
            count: 6,
            unit: IntervalUnit::Hours,
        };
        let anchor = utc("2026-03-28T00:00:00Z");
        // Berlin springs forward on 2026-03-29 at 02:00.
        let next = next_after(&spec, berlin(), anchor, utc("2026-03-29T01:00:00Z"))
            .expect("next")
            .expect("a run");
        assert_eq!(next, utc("2026-03-29T06:00:00Z"));
    }

    #[test]
    fn daily_follows_dst_instead_of_drifting() {
        let spec = ScheduleSpec::Daily {
            time: "09:30".into(),
        };
        let after = utc("2026-03-28T10:00:00Z");
        let next = next_after(&spec, berlin(), after, after)
            .expect("next")
            .expect("a run");
        // 09:30 CEST on 2026-03-29 is 07:30 UTC.
        assert_eq!(next, utc("2026-03-29T07:30:00Z"));

        let winter = ScheduleSpec::Daily {
            time: "09:30".into(),
        };
        let after = utc("2026-10-24T10:00:00Z");
        let next = next_after(&winter, berlin(), after, after)
            .expect("next")
            .expect("a run");
        // 09:30 CET on 2026-10-25 is 08:30 UTC.
        assert_eq!(next, utc("2026-10-25T08:30:00Z"));
    }

    #[test]
    fn a_daily_time_skipped_by_the_gap_runs_at_the_first_valid_minute() {
        let spec = ScheduleSpec::Daily {
            time: "02:30".into(),
        };
        let after = utc("2026-03-28T12:00:00Z");
        let next = next_after(&spec, berlin(), after, after)
            .expect("next")
            .expect("a run");
        // 02:30 does not exist on 2026-03-29; the gap ends at 03:00 CEST.
        assert_eq!(next, utc("2026-03-29T01:00:00Z"));
    }

    #[test]
    fn a_daily_time_in_the_repeated_hour_runs_once_at_the_earliest_instant() {
        let spec = ScheduleSpec::Daily {
            time: "02:30".into(),
        };
        let after = utc("2026-10-24T12:00:00Z");
        let next = next_after(&spec, berlin(), after, after)
            .expect("next")
            .expect("a run");
        // 02:30 CEST first, then 02:30 CET. The first is chosen.
        assert_eq!(next, utc("2026-10-25T00:30:00Z"));

        // If the first occurrence already passed, the second one is next.
        let later = next_after(&spec, berlin(), after, utc("2026-10-25T00:45:00Z"))
            .expect("next")
            .expect("a run");
        assert_eq!(later, utc("2026-10-25T01:30:00Z"));
    }

    #[test]
    fn weekly_picks_the_next_selected_day() {
        let spec = ScheduleSpec::Weekly {
            days: vec![1, 3, 5],
            time: "08:00".into(),
        };
        // Friday after the run -> Monday.
        let next = next_after(
            &spec,
            new_york(),
            utc("2026-11-01T00:00:00Z"),
            utc("2026-11-20T14:00:00Z"),
        )
        .expect("next")
        .expect("a run");
        assert_eq!(next, utc("2026-11-23T13:00:00Z"));
    }

    #[test]
    fn weekly_and_daily_reject_bad_times_and_days() {
        for spec in [
            ScheduleSpec::Weekly {
                days: vec![],
                time: "09:00".into(),
            },
            ScheduleSpec::Weekly {
                days: vec![9],
                time: "09:00".into(),
            },
            ScheduleSpec::Daily { time: "9".into() },
        ] {
            assert!(validate(&spec).is_err(), "{spec:?} should be invalid");
        }
    }

    #[test]
    fn an_interval_needs_a_nonzero_count() {
        let spec = ScheduleSpec::Every {
            count: 0,
            unit: IntervalUnit::Minutes,
        };
        assert!(validate(&spec).is_err());
    }

    #[test]
    fn unknown_time_zones_are_explained() {
        let error = parse_timezone("Mars/Olympus_Mons").expect_err("unknown");
        assert!(error.message().contains("IANA"));
        assert!(parse_timezone("Europe/Berlin").is_ok());
    }

    #[test]
    fn description_names_the_schedule() {
        assert_eq!(
            describe(&ScheduleSpec::Weekly {
                days: vec![1, 3],
                time: "08:00".into(),
            }),
            "Weekly on Mon, Wed at 08:00"
        );
    }
}
