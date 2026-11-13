//! Deterministic price estimates with provenance.
//!
//! A harness that reports a cost wins; otherwise h4rn multiplies tokens by a
//! versioned per-model table. An unknown model is **unpriced** (zero, with
//! [`CostSource::Unknown`]), never silently treated as free.

use std::collections::BTreeMap;

use h4rn_protocol::CostSource;

/// Per-million-token prices in millionths of a US dollar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelPrice {
    /// Prompt tokens, including cached input.
    pub input_per_million: u64,
    /// Completion tokens.
    pub output_per_million: u64,
    /// Cached read input tokens.
    pub cached_read_per_million: u64,
    /// Cached creation (write) input tokens.
    pub cached_creation_per_million: u64,
}

/// A versioned price table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriceTable {
    version: String,
    entries: BTreeMap<String, ModelPrice>,
    /// Prefixes are matched longest-first, so `claude-opus` beats `claude`.
    prefixes: Vec<(String, ModelPrice)>,
}

impl Default for PriceTable {
    fn default() -> Self {
        Self::builtin()
    }
}

impl PriceTable {
    /// An empty table. Every estimate is [`CostSource::Unknown`].
    pub fn empty(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            entries: BTreeMap::new(),
            prefixes: Vec::new(),
        }
    }

    /// The table h4rn ships, with a small set of list prices.
    ///
    /// These are estimates for the user's own dashboard; the harness-reported
    /// cost is the system of record when it exists.
    pub fn builtin() -> Self {
        let mut table = Self::empty("h4rn-list-2026-11");
        table.insert_prefix(
            "claude-opus",
            ModelPrice {
                input_per_million: 15_000_000,
                output_per_million: 75_000_000,
                cached_read_per_million: 1_500_000,
                cached_creation_per_million: 18_750_000,
            },
        );
        table.insert_prefix(
            "claude-sonnet",
            ModelPrice {
                input_per_million: 3_000_000,
                output_per_million: 15_000_000,
                cached_read_per_million: 300_000,
                cached_creation_per_million: 3_750_000,
            },
        );
        table.insert_prefix(
            "claude-haiku",
            ModelPrice {
                input_per_million: 800_000,
                output_per_million: 4_000_000,
                cached_read_per_million: 80_000,
                cached_creation_per_million: 1_000_000,
            },
        );
        table.insert_prefix(
            "gpt-5",
            ModelPrice {
                input_per_million: 1_250_000,
                output_per_million: 10_000_000,
                cached_read_per_million: 125_000,
                cached_creation_per_million: 1_250_000,
            },
        );
        table.insert_prefix(
            "gpt-4",
            ModelPrice {
                input_per_million: 2_500_000,
                output_per_million: 10_000_000,
                cached_read_per_million: 1_250_000,
                cached_creation_per_million: 2_500_000,
            },
        );
        table.insert_prefix(
            "gemini",
            ModelPrice {
                input_per_million: 1_250_000,
                output_per_million: 5_000_000,
                cached_read_per_million: 312_500,
                cached_creation_per_million: 1_250_000,
            },
        );
        table
    }

    /// Table version recorded on every estimated row.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// An exact model entry, then the longest matching prefix.
    pub fn price_for(&self, model: &str) -> Option<&ModelPrice> {
        if let Some(price) = self.entries.get(model) {
            return Some(price);
        }
        self.prefixes
            .iter()
            .find(|(prefix, _)| model.starts_with(prefix.as_str()))
            .map(|(_, price)| price)
    }

    /// Adds an exact model entry.
    pub fn insert(&mut self, model: impl Into<String>, price: ModelPrice) {
        self.entries.insert(model.into(), price);
    }

    /// Adds a prefix fallback.
    pub fn insert_prefix(&mut self, prefix: impl Into<String>, price: ModelPrice) {
        let prefix = prefix.into();
        self.prefixes.push((prefix.clone(), price));
        self.prefixes
            .sort_by_key(|left| std::cmp::Reverse(left.0.len()));
    }

    /// Estimates a turn's cost from its normalized counters.
    pub fn estimate(
        &self,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        cached_read_tokens: u64,
        cached_creation_tokens: u64,
    ) -> CostEstimate {
        let Some(price) = self.price_for(model) else {
            return CostEstimate {
                cost_micros: 0,
                source: CostSource::Unknown,
                price_table: None,
            };
        };
        // `input_tokens` includes cached input. Price the non-cached remainder
        // at the input rate and the cached parts at their own rates.
        let plain_input = input_tokens
            .saturating_sub(cached_read_tokens)
            .saturating_sub(cached_creation_tokens);
        let cost = tokens_at(plain_input, price.input_per_million)
            .saturating_add(tokens_at(output_tokens, price.output_per_million))
            .saturating_add(tokens_at(cached_read_tokens, price.cached_read_per_million))
            .saturating_add(tokens_at(
                cached_creation_tokens,
                price.cached_creation_per_million,
            ));
        CostEstimate {
            cost_micros: cost,
            source: CostSource::PriceTable,
            price_table: Some(self.version.clone()),
        }
    }
}

fn tokens_at(tokens: u64, micros_per_million: u64) -> u64 {
    // tokens * micros / 1_000_000, rounded down. u128 avoids overflow on large
    // cached-context turns while staying exact.
    ((u128::from(tokens) * u128::from(micros_per_million)) / 1_000_000) as u64
}

/// A cost number and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CostEstimate {
    /// Cost in millionths of a US dollar.
    pub cost_micros: u64,
    /// Provenance.
    pub source: CostSource,
    /// Price-table version for [`CostSource::PriceTable`].
    pub price_table: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_prefix_lookup_are_deterministic() {
        let table = PriceTable::builtin();
        assert!(table.price_for("claude-opus-5-5").is_some());
        assert!(table.price_for("claude-sonnet-5-5").is_some());
        assert!(table.price_for("unknown-model").is_none());
    }

    #[test]
    fn estimates_price_cached_and_plain_input_separately() {
        let table = PriceTable::builtin();
        let estimate = table.estimate("claude-sonnet", 1_000_000, 100_000, 400_000, 100_000);
        // 500k plain * $3/M + 100k out * $15/M + 400k read * $0.30/M
        // + 100k creation * $3.75/M = 1.5 + 1.5 + 0.12 + 0.375 = 3.495 USD.
        assert_eq!(estimate.cost_micros, 3_495_000);
        assert_eq!(estimate.source, CostSource::PriceTable);
        assert_eq!(estimate.price_table.as_deref(), Some(table.version()));
    }

    #[test]
    fn an_unknown_model_is_unpriced_not_free() {
        let estimate = PriceTable::builtin().estimate("mystery", 10, 10, 0, 0);
        assert_eq!(estimate.cost_micros, 0);
        assert_eq!(estimate.source, CostSource::Unknown);
    }
}
