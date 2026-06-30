//! Token-source accounting: how many tokens came from authoritative upstream
//! usage reports vs. local estimates.
//!
//! When a provider reports real `usage` ([`crate::Provider::take_last_usage`] or
//! a [`crate::ProviderStreamEvent::Usage`]), the harness books those tokens as
//! **reported**. When it does not, the harness falls back to the local
//! char-class estimator ([`crate::estimate_tokens`]) and books them as
//! **estimated**. This module keeps a running tally so the UI can answer
//! "how accurate is my context meter?" and surface which providers/models
//! are measured vs. guessed.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// One provider+model pair's accumulated token totals, split by source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenSourceTotals {
    /// Tokens reported authoritatively by the provider's `usage` object.
    pub reported_tokens: i64,
    /// Tokens filled in by the local char-class estimator (provider reported
    /// no usage for those turns).
    pub estimated_tokens: i64,
    /// Tokens written to a prompt cache (Anthropic `cache_creation_input_tokens`
    /// — billed at a premium). A subset of `reported_tokens`, broken out so the
    /// report can show cache write volume and verify the breakpoints are
    /// creating cache entries.
    pub cache_write_tokens: i64,
    /// Tokens served from a prompt cache (Anthropic `cache_read_input_tokens` —
    /// billed at a ~0.1× discount). A subset of `reported_tokens`, broken out
    /// so the report can show cache hit volume (the payoff of caching).
    pub cache_read_tokens: i64,
}

impl TokenSourceTotals {
    /// Total tokens regardless of source.
    pub fn total(&self) -> i64 {
        self.reported_tokens + self.estimated_tokens
    }

    /// Accumulate another entry's counts into this one.
    fn add(&mut self, other: TokenSourceTotals) {
        self.reported_tokens += other.reported_tokens;
        self.estimated_tokens += other.estimated_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
    }
}

/// The key under which a provider+model's totals are accumulated: a
/// `(provider_id, model)` tuple so a session that switches providers or models
/// keeps each one's accuracy picture separate. Using a tuple (rather than a
/// `\u{1f}`-joined string) sidesteps any ambiguity when a provider/model value
/// happens to contain the separator.
fn key(provider: &str, model: &str) -> (String, String) {
    (provider.to_string(), model.to_string())
}

/// A thread-safe running ledger of token counts split by source (reported vs.
/// estimated), keyed by `(provider_id, model)`. Shared between the agent (the
/// writer — books each turn) and the TUI (the reader — renders the report).
#[derive(Debug, Default)]
pub struct TokenSourceLedger {
    /// `(provider, model)` → accumulated totals. A [`BTreeMap`] so the report
    /// iterates in a stable order.
    entries: Mutex<BTreeMap<(String, String), TokenSourceTotals>>,
}

impl TokenSourceLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// A cheap shared handle (the canonical way the agent and TUI share one
    /// ledger).
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Book one turn's token usage. When `reported` is `true`, the provider
    /// reported authoritative usage and `tokens` are real counts; when `false`,
    /// `tokens` are a local estimate.
    pub fn record(&self, provider: &str, model: &str, tokens: i64, reported: bool) {
        if tokens <= 0 {
            return;
        }
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let totals = entries.entry(key(provider, model)).or_default();
        if reported {
            totals.reported_tokens += tokens;
        } else {
            totals.estimated_tokens += tokens;
        }
    }

    /// Book one turn's reported usage, including its prompt-cache split. The
    /// cache write/read counts are folded into `reported_tokens` (they are real
    /// billed tokens) and ALSO accumulated into the cache counters so the
    /// report can surface cache hit-rate separately. `cache_*` are clamped to
    /// non-negative; callers that have none (no caching, or an estimate) pass
    /// `0, 0` — the simple [`record`](Self::record) wrapper does exactly that.
    pub fn record_reported(
        &self,
        provider: &str,
        model: &str,
        tokens: i64,
        cache_write: i64,
        cache_read: i64,
    ) {
        if tokens <= 0 {
            return;
        }
        let cw = cache_write.max(0);
        let cr = cache_read.max(0);
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let totals = entries.entry(key(provider, model)).or_default();
        totals.reported_tokens += tokens;
        totals.cache_write_tokens += cw;
        totals.cache_read_tokens += cr;
    }

    /// A snapshot of the ledger suitable for rendering (owned, no lock held).
    pub fn snapshot(&self) -> TokenSourceReport {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let rows: Vec<TokenSourceRow> = entries
            .iter()
            .map(|((provider, model), totals)| TokenSourceRow {
                provider: provider.to_string(),
                model: model.to_string(),
                totals: *totals,
            })
            .collect();
        let grand_total =
            rows.iter()
                .map(|r| r.totals)
                .fold(TokenSourceTotals::default(), |mut acc, t| {
                    acc.add(t);
                    acc
                });
        TokenSourceReport { rows, grand_total }
    }
}

/// One row of the report: a single provider+model and its source split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSourceRow {
    pub provider: String,
    pub model: String,
    pub totals: TokenSourceTotals,
}

/// A full snapshot of the ledger: per-row breakdown + a grand total.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenSourceReport {
    pub rows: Vec<TokenSourceRow>,
    pub grand_total: TokenSourceTotals,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_reported_and_estimated_separately() {
        let ledger = TokenSourceLedger::new();
        ledger.record("openai", "gpt-4o", 100, true);
        ledger.record("openai", "gpt-4o", 50, false);
        let report = ledger.snapshot();
        assert_eq!(report.rows.len(), 1);
        let row = &report.rows[0];
        assert_eq!(row.provider, "openai");
        assert_eq!(row.model, "gpt-4o");
        assert_eq!(row.totals.reported_tokens, 100);
        assert_eq!(row.totals.estimated_tokens, 50);
        assert_eq!(row.totals.total(), 150);
    }

    #[test]
    fn separates_providers_and_models() {
        let ledger = TokenSourceLedger::new();
        ledger.record("openai", "gpt-4o", 100, true);
        ledger.record("gemini", "gemini-2.5", 80, true);
        ledger.record("kimi", "k2", 30, false);
        let report = ledger.snapshot();
        assert_eq!(report.rows.len(), 3);
        assert_eq!(report.grand_total.reported_tokens, 180);
        assert_eq!(report.grand_total.estimated_tokens, 30);
    }

    #[test]
    fn ignores_non_positive_tokens() {
        let ledger = TokenSourceLedger::new();
        ledger.record("openai", "gpt-4o", 0, true);
        ledger.record("openai", "gpt-4o", -5, false);
        assert!(ledger.snapshot().rows.is_empty());
    }

    #[test]
    fn snapshot_is_stable_order() {
        let ledger = TokenSourceLedger::new();
        ledger.record("zeta", "z1", 10, true);
        ledger.record("alpha", "a1", 10, true);
        let report = ledger.snapshot();
        // BTreeMap keeps alphabetical order by the composite key.
        assert_eq!(report.rows[0].provider, "alpha");
        assert_eq!(report.rows[1].provider, "zeta");
    }

    #[test]
    fn round_trips_provider_model_containing_the_old_separator() {
        // Regression: the old `\u{1f}`-joined string key would mis-split a
        // provider/model that itself contained the separator byte. A tuple key
        // makes the boundary structural and unambiguous.
        let ledger = TokenSourceLedger::new();
        ledger.record("custom\u{1f}relay", "model\u{1f}v2", 40, true);
        let row = &ledger.snapshot().rows[0];
        assert_eq!(row.provider, "custom\u{1f}relay");
        assert_eq!(row.model, "model\u{1f}v2");
        assert_eq!(row.totals.reported_tokens, 40);
    }

    #[test]
    fn record_reported_books_cache_breakout() {
        // The cache-aware overload folds write/read into reported_tokens AND
        // accumulates them as a separate breakout, so the report can show
        // hit-rate without losing the real billed total.
        let ledger = TokenSourceLedger::new();
        // Turn 1: a cache write (the first turn populates the cache).
        ledger.record_reported("anthropic", "claude-sonnet-4-5", 13200, 5000, 0);
        // Turn 2: a cache read (subsequent turn hits the cache).
        ledger.record_reported("anthropic", "claude-sonnet-4-5", 8200, 0, 8000);
        let row = &ledger.snapshot().rows[0];
        assert_eq!(
            row.totals.reported_tokens, 21400,
            "all reported tokens summed"
        );
        assert_eq!(row.totals.cache_write_tokens, 5000);
        assert_eq!(row.totals.cache_read_tokens, 8000);
        assert_eq!(row.totals.estimated_tokens, 0);
    }

    #[test]
    fn record_reported_clamps_negative_cache_counts() {
        // A malformed usage object shouldn't corrupt the ledger: negative cache
        // counts are clamped to zero rather than subtracting from the total.
        let ledger = TokenSourceLedger::new();
        ledger.record_reported("anthropic", "claude", 1000, -50, -10);
        let row = &ledger.snapshot().rows[0];
        assert_eq!(row.totals.reported_tokens, 1000);
        assert_eq!(row.totals.cache_write_tokens, 0);
        assert_eq!(row.totals.cache_read_tokens, 0);
    }

    #[test]
    fn record_reported_ignores_non_positive_total() {
        // Parity with the plain `record` guard: a zero/negative total is a
        // no-op even when cache counts are present.
        let ledger = TokenSourceLedger::new();
        ledger.record_reported("anthropic", "claude", 0, 100, 200);
        ledger.record_reported("anthropic", "claude", -5, 100, 200);
        assert!(ledger.snapshot().rows.is_empty());
    }

    #[test]
    fn grand_total_aggregates_cache_counters() {
        // `snapshot` folds cache counters into the grand total via `add`, so a
        // multi-provider report surfaces the session-wide cache hit volume.
        let ledger = TokenSourceLedger::new();
        ledger.record_reported("anthropic", "claude-opus", 5000, 1000, 3000);
        ledger.record_reported("openai", "gpt-4o", 2000, 0, 0);
        let report = ledger.snapshot();
        assert_eq!(report.grand_total.reported_tokens, 7000);
        assert_eq!(report.grand_total.cache_write_tokens, 1000);
        assert_eq!(report.grand_total.cache_read_tokens, 3000);
    }
}
