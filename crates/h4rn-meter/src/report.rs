//! Pure aggregation over ledger rows.
//!
//! Keeping the math out of SQL makes the dedupe rule, the TPS mean and the
//! active/idle split testable without a database. The store filters by range
//! and scope prefix; this module produces the exact report the wire carries.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Timelike, Utc};
use h4rn_protocol::{
    UsageBucket, UsageBucketPoint, UsageGroup, UsageGroupBy, UsageRange, UsageReport, UsageScope,
    UsageTotals,
};

use crate::ledger::UsageRow;

/// A usage query as the reporter sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportQuery {
    /// What to aggregate over.
    pub scope: UsageScope,
    /// Thread id or workspace path for those scopes.
    pub scope_id: Option<String>,
    /// Half-open time range.
    pub range: UsageRange,
    /// Breakdown.
    pub group_by: UsageGroupBy,
    /// Optional series.
    pub bucket: Option<UsageBucket>,
}

/// Rows that are allowed to contribute to totals.
///
/// Self-metered rows always count. A proxy row counts only when it was not
/// matched to a self-metered row with the same `(thread_id, run_id)` or
/// correlation id (or the same provider/model inside the dedupe window).
pub fn counted_rows(rows: &[UsageRow]) -> Vec<&UsageRow> {
    rows.iter()
        .filter(|row| match row.source {
            h4rn_protocol::UsageProvenance::SelfMetered => true,
            h4rn_protocol::UsageProvenance::ProxyReported => !row.deduped,
        })
        .collect()
}

/// Builds a report from already-range-filtered rows.
pub fn build_report(rows: &[UsageRow], query: &ReportQuery) -> UsageReport {
    let rows: Vec<&UsageRow> = rows
        .iter()
        .filter(|row| in_scope(row, query))
        .filter(|row| in_range(row, &query.range))
        .collect();
    let counted: Vec<&UsageRow> = rows
        .iter()
        .copied()
        .filter(|row| match row.source {
            h4rn_protocol::UsageProvenance::SelfMetered => true,
            h4rn_protocol::UsageProvenance::ProxyReported => !row.deduped,
        })
        .collect();

    let report_totals = totals(&counted);
    let groups = match query.group_by {
        UsageGroupBy::None => Vec::new(),
        group_by => {
            let mut buckets: BTreeMap<String, Vec<&UsageRow>> = BTreeMap::new();
            for row in &counted {
                buckets
                    .entry(group_key(row, group_by))
                    .or_default()
                    .push(row);
            }
            buckets
                .into_iter()
                .map(|(key, rows)| UsageGroup {
                    key,
                    totals: totals(&rows),
                })
                .collect()
        }
    };
    let buckets = query
        .bucket
        .map(|width| buckets(&counted, query, width))
        .unwrap_or_default();

    UsageReport {
        scope: query.scope,
        scope_id: query.scope_id.clone(),
        range: query.range.clone(),
        group_by: query.group_by,
        bucket: query.bucket,
        totals: report_totals,
        buckets,
        groups,
    }
}

fn in_scope(row: &UsageRow, query: &ReportQuery) -> bool {
    match query.scope {
        UsageScope::Thread => match &query.scope_id {
            Some(thread) => row.thread_id.as_deref() == Some(thread.as_str()),
            None => true,
        },
        UsageScope::Workspace => match &query.scope_id {
            Some(workspace) => row.workspace.as_deref() == Some(workspace.as_str()),
            None => true,
        },
        UsageScope::Machine => match &query.scope_id {
            Some(machine) => row.machine.as_deref() == Some(machine.as_str()),
            None => true,
        },
        UsageScope::Global => true,
    }
}

fn in_range(row: &UsageRow, range: &UsageRange) -> bool {
    if let Some(since) = range.since
        && row.started_at < since
    {
        return false;
    }
    if let Some(until) = range.until
        && row.started_at >= until
    {
        return false;
    }
    true
}

fn group_key(row: &UsageRow, group_by: UsageGroupBy) -> String {
    match group_by {
        UsageGroupBy::None => String::new(),
        UsageGroupBy::Thread => row.thread_id.clone().unwrap_or_else(|| "unscoped".into()),
        UsageGroupBy::Workspace => row.workspace.clone().unwrap_or_else(|| "unscoped".into()),
        UsageGroupBy::Machine => row.machine.clone().unwrap_or_else(|| "unscoped".into()),
        UsageGroupBy::Harness => row.harness.clone().unwrap_or_else(|| "unknown".into()),
        UsageGroupBy::Model => row.model.clone().unwrap_or_else(|| "unknown".into()),
    }
}

fn totals(rows: &[&UsageRow]) -> UsageTotals {
    let mut totals = UsageTotals::default();
    let mut ttft_sum = 0u64;
    let mut ttft_count = 0u64;
    let mut tps_sum = 0.0f64;
    let mut tps_count = 0u64;
    let mut threads: BTreeMap<String, (DateTime<Utc>, DateTime<Utc>, u64)> = BTreeMap::new();

    for row in rows {
        totals.input_tokens = totals.input_tokens.saturating_add(row.tokens_in);
        totals.output_tokens = totals.output_tokens.saturating_add(row.tokens_out);
        totals.cached_input_tokens = totals
            .cached_input_tokens
            .saturating_add(row.tokens_cached_read);
        totals.cached_creation_tokens = totals
            .cached_creation_tokens
            .saturating_add(row.tokens_cached_creation);
        totals.cost_micros = totals.cost_micros.saturating_add(row.cost_micros);
        totals.wall_ms = totals.wall_ms.saturating_add(row.wall_ms);
        totals.generation_ms = totals.generation_ms.saturating_add(row.generation_ms);
        totals.turns = totals.turns.saturating_add(1);
        if let Some(ttft) = row.ttft_ms {
            ttft_sum = ttft_sum.saturating_add(ttft);
            ttft_count = ttft_count.saturating_add(1);
        }
        if let Some(tps) = row.tps {
            tps_sum += tps;
            tps_count += 1;
        }
        let key = row
            .thread_id
            .clone()
            .unwrap_or_else(|| format!("row:{}", row.row_id));
        threads
            .entry(key)
            .and_modify(|entry| {
                entry.0 = entry.0.min(row.started_at);
                entry.1 = entry.1.max(row.completed_at);
                entry.2 = entry.2.saturating_add(row.wall_ms);
            })
            .or_insert((row.started_at, row.completed_at, row.wall_ms));
    }

    totals.ttft_ms = ttft_sum.checked_div(ttft_count).unwrap_or(0);
    totals.tps = if tps_count == 0 {
        0.0
    } else {
        tps_sum / tps_count as f64
    };
    totals.active_ms = totals.wall_ms;
    for (start, end, active) in threads.values() {
        let runtime = (*end - *start).num_milliseconds().max(0) as u64;
        totals.runtime_ms = totals.runtime_ms.saturating_add(runtime);
        totals.idle_ms = totals
            .idle_ms
            .saturating_add(runtime.saturating_sub(*active));
    }
    totals
}

fn buckets(rows: &[&UsageRow], query: &ReportQuery, width: UsageBucket) -> Vec<UsageBucketPoint> {
    let Some((first, last)) = rows
        .iter()
        .map(|row| (row.started_at, row.completed_at))
        .fold(
            None,
            |acc: Option<(DateTime<Utc>, DateTime<Utc>)>, (start, end)| {
                Some(match acc {
                    Some((first, last)) => (first.min(start), last.max(end)),
                    None => (start, end),
                })
            },
        )
    else {
        return Vec::new();
    };
    let first = floor(first, width).max(
        query
            .range
            .since
            .map(|value| floor(value, width))
            .unwrap_or_else(|| floor(first, width)),
    );
    let last = query
        .range
        .until
        .map(|value| floor(value, width))
        .unwrap_or_else(|| floor(last, width));

    let mut grouped: BTreeMap<DateTime<Utc>, Vec<&UsageRow>> = BTreeMap::new();
    for row in rows {
        grouped
            .entry(floor(row.started_at, width))
            .or_default()
            .push(row);
    }

    let mut points = Vec::new();
    let mut cursor = first;
    let mut guard = 0;
    while cursor <= last && guard < 1_000 {
        let next = advance(cursor, width);
        let totals = grouped
            .get(&cursor)
            .map(|rows| totals(rows))
            .unwrap_or_default();
        points.push(UsageBucketPoint {
            start: cursor,
            end: next,
            totals,
        });
        cursor = next;
        guard += 1;
    }
    points
}

fn floor(value: DateTime<Utc>, width: UsageBucket) -> DateTime<Utc> {
    match width {
        UsageBucket::Hour => value
            .with_minute(0)
            .and_then(|value| value.with_second(0))
            .and_then(|value| value.with_nanosecond(0))
            .unwrap_or(value),
        UsageBucket::Day => value
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .map(|value| value.and_utc())
            .unwrap_or(value),
    }
}

fn advance(value: DateTime<Utc>, width: UsageBucket) -> DateTime<Utc> {
    value
        + match width {
            UsageBucket::Hour => Duration::hours(1),
            UsageBucket::Day => Duration::days(1),
        }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;
    use h4rn_protocol::{CostSource, UsageProvenance};

    use super::*;

    fn row(
        id: &str,
        thread: &str,
        start_ms: i64,
        wall_ms: u64,
        output: u64,
        source: UsageProvenance,
        deduped: bool,
    ) -> UsageRow {
        let start = Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap()
            + Duration::milliseconds(start_ms);
        UsageRow {
            row_id: id.into(),
            thread_id: Some(thread.into()),
            run_id: Some(format!("run_{id}")),
            workspace: Some("/repo".into()),
            machine: Some("host".into()),
            account: None,
            harness: Some("claude-code".into()),
            model: Some("model".into()),
            provider: Some("anthropic".into()),
            credential: None,
            source,
            correlation_id: None,
            tokens_in: 1,
            tokens_out: output,
            tokens_cached_read: 0,
            tokens_cached_creation: 0,
            cost_micros: 1,
            cost_source: CostSource::PriceTable,
            price_table: Some("test".into()),
            ttft_ms: Some(100),
            tps: Some(10.0),
            generation_ms: wall_ms,
            wall_ms,
            started_at: start,
            completed_at: start + Duration::milliseconds(wall_ms as i64),
            ts: start,
            raw: None,
            deduped,
            synced: false,
        }
    }

    fn query(scope: UsageScope, group_by: UsageGroupBy) -> ReportQuery {
        ReportQuery {
            scope,
            scope_id: None,
            range: UsageRange::default(),
            group_by,
            bucket: None,
        }
    }

    #[test]
    fn proxy_rows_that_were_deduped_do_not_count() {
        let rows = vec![
            row("a", "t1", 0, 1_000, 2, UsageProvenance::SelfMetered, false),
            row("b", "t1", 0, 1_000, 2, UsageProvenance::ProxyReported, true),
        ];
        let report = build_report(&rows, &query(UsageScope::Global, UsageGroupBy::None));
        assert_eq!(report.totals.turns, 1);
        assert_eq!(report.totals.output_tokens, 2);
    }

    #[test]
    fn active_and_idle_span_turns_and_restarts() {
        // Two turns on one thread, ten seconds apart, one second each.
        let rows = vec![
            row("a", "t1", 0, 1_000, 1, UsageProvenance::SelfMetered, false),
            row(
                "b",
                "t1",
                10_000,
                1_000,
                1,
                UsageProvenance::SelfMetered,
                false,
            ),
        ];
        let report = build_report(&rows, &query(UsageScope::Thread, UsageGroupBy::None));
        assert_eq!(report.totals.runtime_ms, 11_000);
        assert_eq!(report.totals.active_ms, 2_000);
        assert_eq!(report.totals.idle_ms, 9_000);
    }

    #[test]
    fn groups_and_hour_buckets_are_sorted() {
        let rows = vec![
            row("a", "t1", 0, 1_000, 1, UsageProvenance::SelfMetered, false),
            row(
                "b",
                "t2",
                3_600_000,
                1_000,
                3,
                UsageProvenance::SelfMetered,
                false,
            ),
        ];
        let mut query = query(UsageScope::Global, UsageGroupBy::Thread);
        query.bucket = Some(UsageBucket::Hour);
        let report = build_report(&rows, &query);
        assert_eq!(report.groups.len(), 2);
        assert_eq!(report.groups[0].key, "t1");
        assert_eq!(report.buckets.len(), 2);
        assert_eq!(report.buckets[0].totals.output_tokens, 1);
        assert_eq!(report.buckets[1].totals.output_tokens, 3);
    }

    #[test]
    fn tps_and_ttft_are_means_of_metered_rows() {
        let mut first = row("a", "t1", 0, 1_000, 10, UsageProvenance::SelfMetered, false);
        first.ttft_ms = Some(100);
        first.tps = Some(10.0);
        let mut second = row("b", "t1", 0, 1_000, 10, UsageProvenance::SelfMetered, false);
        second.ttft_ms = Some(300);
        second.tps = Some(30.0);
        let report = build_report(
            &[first, second],
            &query(UsageScope::Global, UsageGroupBy::None),
        );
        assert_eq!(report.totals.ttft_ms, 200);
        assert_eq!(report.totals.tps, 20.0);
    }
}
