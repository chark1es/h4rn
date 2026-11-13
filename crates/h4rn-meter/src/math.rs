//! Pure per-turn metering arithmetic.
//!
//! The meter observes timestamps, not payloads. Tool time is subtracted from
//! generation time so a turn that spends forty seconds in a build does not
//! depress its tokens-per-second number.

use chrono::{DateTime, Utc};
use h4rn_protocol::{CostSource, TurnStatus, Usage};

use crate::price::PriceTable;

/// A live snapshot of a running turn.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveSnapshot {
    /// Counters observed so far.
    pub usage: Usage,
    /// First-token latency once text arrived.
    pub ttft_ms: Option<u64>,
    /// Output tokens per generation second, excluding tool time.
    pub tps: Option<f64>,
    /// Wall-clock milliseconds since the turn started.
    pub elapsed_ms: u64,
    /// Time spent inside tool calls so far.
    pub tool_ms: u64,
}

/// A finished turn as the ledger sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct MeteredTurn {
    /// Turn start.
    pub started_at: DateTime<Utc>,
    /// Turn end.
    pub completed_at: DateTime<Utc>,
    /// How the turn ended.
    pub status: TurnStatus,
    /// Normalized counters.
    pub usage: Usage,
    /// Input tokens written to the provider's cache.
    pub cached_creation_tokens: u64,
    /// Time to first token, when text arrived.
    pub ttft_ms: Option<u64>,
    /// Output tokens per generation second.
    pub tps: Option<f64>,
    /// Full wall-clock time.
    pub wall_ms: u64,
    /// Generation time, excluding tool time.
    pub generation_ms: u64,
    /// Time spent inside tools.
    pub tool_ms: u64,
    /// Cost in millionths of a US dollar.
    pub cost_micros: u64,
    /// Where the cost came from.
    pub cost_source: CostSource,
    /// Price-table version when estimated.
    pub price_table: Option<String>,
}

/// One turn's clock and counters.
#[derive(Clone, Debug)]
pub struct TurnMeter {
    started_at: DateTime<Utc>,
    first_token_at: Option<DateTime<Utc>>,
    tools_running: u32,
    tool_started_at: Option<DateTime<Utc>>,
    tool_ms: u64,
    usage: Usage,
    cached_creation_tokens: u64,
    reported_cost_micros: Option<u64>,
}

impl TurnMeter {
    /// Starts a meter at `at`.
    pub fn new(at: DateTime<Utc>) -> Self {
        Self {
            started_at: at,
            first_token_at: None,
            tools_running: 0,
            tool_started_at: None,
            tool_ms: 0,
            usage: Usage::default(),
            cached_creation_tokens: 0,
            reported_cost_micros: None,
        }
    }

    /// Turn start.
    pub fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }

    /// Records the first streamed text token.
    ///
    /// Later calls are ignored: TTFT is the first token of the turn.
    pub fn first_token(&mut self, at: DateTime<Utc>) {
        if self.first_token_at.is_none() {
            self.first_token_at = Some(at);
        }
    }

    /// Marks a tool call as running.
    pub fn tool_started(&mut self, at: DateTime<Utc>) {
        if self.tools_running == 0 {
            self.tool_started_at = Some(at);
        }
        self.tools_running = self.tools_running.saturating_add(1);
    }

    /// Marks a tool call as finished.
    ///
    /// Overlapping tools are tracked by count, so two tools that run together
    /// contribute their union of time, not twice their duration.
    pub fn tool_finished(&mut self, at: DateTime<Utc>) {
        if self.tools_running == 0 {
            return;
        }
        self.tools_running -= 1;
        if self.tools_running == 0
            && let Some(started) = self.tool_started_at.take()
        {
            self.tool_ms = self.tool_ms.saturating_add(millis_between(started, at));
        }
    }

    /// Merges counters reported by the harness.
    pub fn add_usage(&mut self, usage: &Usage) {
        self.usage.merge(usage);
        if usage.cost_micros > 0 {
            self.reported_cost_micros = Some(
                self.reported_cost_micros
                    .unwrap_or(0)
                    .saturating_add(usage.cost_micros),
            );
        }
    }

    /// Records the cache-creation tokens the adapter parsed from its raw report.
    pub fn set_cached_creation_tokens(&mut self, tokens: u64) {
        self.cached_creation_tokens = tokens;
    }

    /// Replaces the harness-reported cost, when the adapter reports it later.
    pub fn set_reported_cost_micros(&mut self, cost_micros: u64) {
        if cost_micros > 0 {
            self.reported_cost_micros = Some(cost_micros);
        }
    }

    /// Replaces the counters with the turn's authoritative final usage.
    ///
    /// Adapters usually stream incremental `Usage` events and then repeat the
    /// turn total on `TurnCompleted`; replacing avoids counting the same tokens
    /// twice while still catching an adapter that reports only once.
    pub fn set_final_usage(&mut self, usage: &Usage) {
        self.usage = usage.clone();
        if usage.cost_micros > 0 {
            self.reported_cost_micros = Some(usage.cost_micros);
        }
    }

    /// A live snapshot for `UsageTick`.
    pub fn live(&self, now: DateTime<Utc>) -> LiveSnapshot {
        let elapsed_ms = millis_between(self.started_at, now);
        let tool_ms = self.tool_ms + self.open_tool_ms(now);
        let generation_ms = self
            .generation_ms(now)
            .min(elapsed_ms)
            .saturating_sub(tool_ms);
        LiveSnapshot {
            usage: self.usage.clone(),
            ttft_ms: self
                .first_token_at
                .map(|first| millis_between(self.started_at, first)),
            tps: tps(self.usage.output_tokens, generation_ms),
            elapsed_ms,
            tool_ms,
        }
    }

    /// Closes the meter and prices the turn.
    pub fn finish(
        &mut self,
        at: DateTime<Utc>,
        status: TurnStatus,
        prices: &PriceTable,
        model: &str,
    ) -> MeteredTurn {
        // A tool still running when the turn ended contributes through `at`.
        let tool_ms = self.tool_ms + self.open_tool_ms(at);
        let wall_ms = millis_between(self.started_at, at);
        let generation_ms = self.generation_ms(at).min(wall_ms).saturating_sub(tool_ms);
        let estimate = match self.reported_cost_micros {
            Some(cost) if cost > 0 => crate::price::CostEstimate {
                cost_micros: cost,
                source: CostSource::HarnessReported,
                price_table: None,
            },
            _ => prices.estimate(
                model,
                self.usage.input_tokens,
                self.usage.output_tokens,
                self.usage.cached_input_tokens,
                self.cached_creation_tokens,
            ),
        };
        MeteredTurn {
            started_at: self.started_at,
            completed_at: at,
            status,
            usage: self.usage.clone(),
            cached_creation_tokens: self.cached_creation_tokens,
            ttft_ms: self
                .first_token_at
                .map(|first| millis_between(self.started_at, first)),
            tps: tps(self.usage.output_tokens, generation_ms),
            wall_ms,
            generation_ms,
            tool_ms,
            cost_micros: estimate.cost_micros,
            cost_source: estimate.source,
            price_table: estimate.price_table,
        }
    }

    fn generation_ms(&self, now: DateTime<Utc>) -> u64 {
        let start = self.first_token_at.unwrap_or(self.started_at);
        millis_between(start, now)
    }

    fn open_tool_ms(&self, now: DateTime<Utc>) -> u64 {
        self.tool_started_at
            .map(|started| millis_between(started, now))
            .unwrap_or(0)
    }
}

fn millis_between(start: DateTime<Utc>, end: DateTime<Utc>) -> u64 {
    (end - start).num_milliseconds().max(0) as u64
}

fn tps(output_tokens: u64, generation_ms: u64) -> Option<f64> {
    (generation_ms > 0 && output_tokens > 0)
        .then(|| output_tokens as f64 * 1000.0 / generation_ms as f64)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    fn at(ms: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap() + chrono::Duration::milliseconds(ms)
    }

    #[test]
    fn ttft_is_the_first_token_and_tps_excludes_tool_time() {
        let mut meter = TurnMeter::new(at(0));
        meter.first_token(at(1_000));
        meter.add_usage(&Usage {
            input_tokens: 100,
            output_tokens: 200,
            ..Usage::default()
        });
        meter.tool_started(at(2_000));
        meter.tool_finished(at(7_000));
        let turn = meter.finish(
            at(11_000),
            TurnStatus::Completed,
            &PriceTable::builtin(),
            "x",
        );

        assert_eq!(turn.ttft_ms, Some(1_000));
        assert_eq!(turn.wall_ms, 11_000);
        assert_eq!(turn.tool_ms, 5_000);
        // Generation window is first token..end (10s) minus 5s of tools.
        assert_eq!(turn.generation_ms, 5_000);
        assert_eq!(turn.tps, Some(40.0));
    }

    #[test]
    fn overlapping_tools_count_their_union() {
        let mut meter = TurnMeter::new(at(0));
        meter.tool_started(at(1_000));
        meter.tool_started(at(2_000));
        meter.tool_finished(at(4_000));
        meter.tool_finished(at(6_000));
        let turn = meter.finish(
            at(10_000),
            TurnStatus::Completed,
            &PriceTable::builtin(),
            "x",
        );
        assert_eq!(turn.tool_ms, 5_000);
    }

    #[test]
    fn a_running_tool_at_completion_still_contributes() {
        let mut meter = TurnMeter::new(at(0));
        meter.tool_started(at(1_000));
        let turn = meter.finish(at(5_000), TurnStatus::Failed, &PriceTable::builtin(), "x");
        assert_eq!(turn.tool_ms, 4_000);
    }

    #[test]
    fn harness_cost_wins_over_the_price_table() {
        let mut meter = TurnMeter::new(at(0));
        meter.add_usage(&Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cost_micros: 42,
            ..Usage::default()
        });
        let turn = meter.finish(
            at(10),
            TurnStatus::Completed,
            &PriceTable::builtin(),
            "claude-opus",
        );
        assert_eq!(turn.cost_micros, 42);
        assert_eq!(turn.cost_source, CostSource::HarnessReported);
        assert_eq!(turn.price_table, None);
    }

    #[test]
    fn live_snapshot_is_bounded_by_now() {
        let mut meter = TurnMeter::new(at(0));
        meter.first_token(at(500));
        meter.add_usage(&Usage {
            output_tokens: 10,
            ..Usage::default()
        });
        meter.tool_started(at(1_000));
        let live = meter.live(at(2_000));
        assert_eq!(live.ttft_ms, Some(500));
        assert_eq!(live.elapsed_ms, 2_000);
        assert_eq!(live.tool_ms, 1_000);
    }
}
