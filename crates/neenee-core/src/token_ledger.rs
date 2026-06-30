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
    /// Reported input tokens (Anthropic: includes cache write+read). `0` for
    /// estimated rounds, which carry no input/output split.
    pub prompt_tokens: i64,
    /// Reported output tokens. `0` for estimated rounds.
    pub completion_tokens: i64,
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
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
    }
}

/// One turn's token counts, kept per `(provider, model)` as the "line items"
/// of the bill so the report can show a per-round breakdown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRound {
    /// `true` = authoritative provider usage; `false` = local char-class estimate.
    pub reported: bool,
    /// Reported input tokens (includes cache write+read for Anthropic).
    pub prompt_tokens: i64,
    /// Reported output tokens.
    pub completion_tokens: i64,
    /// Total tokens booked this round.
    pub total_tokens: i64,
    /// Anthropic `cache_creation_input_tokens` for this round.
    pub cache_write_tokens: i64,
    /// Anthropic `cache_read_input_tokens` for this round.
    pub cache_read_tokens: i64,
}

/// Internal per-key accumulator: running totals plus the ordered line items.
#[derive(Debug, Default)]
struct Entry {
    totals: TokenSourceTotals,
    rounds: Vec<TokenRound>,
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
    /// `(provider, model)` → accumulator (totals + per-round line items). A
    /// [`BTreeMap`] so the report iterates in a stable order.
    entries: Mutex<BTreeMap<(String, String), Entry>>,
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

    /// Book one turn as a line item — the single entry point all the public
    /// recorders funnel through. It appends the round and folds it into the
    /// running totals. Non-positive totals are ignored; negative io/cache
    /// counts are clamped to zero.
    pub fn record_round(&self, provider: &str, model: &str, round: TokenRound) {
        if round.total_tokens <= 0 {
            return;
        }
        let round = TokenRound {
            prompt_tokens: round.prompt_tokens.max(0),
            completion_tokens: round.completion_tokens.max(0),
            cache_write_tokens: round.cache_write_tokens.max(0),
            cache_read_tokens: round.cache_read_tokens.max(0),
            ..round
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.entry(key(provider, model)).or_default();
        if round.reported {
            entry.totals.reported_tokens += round.total_tokens;
            entry.totals.prompt_tokens += round.prompt_tokens;
            entry.totals.completion_tokens += round.completion_tokens;
            entry.totals.cache_write_tokens += round.cache_write_tokens;
            entry.totals.cache_read_tokens += round.cache_read_tokens;
        } else {
            entry.totals.estimated_tokens += round.total_tokens;
        }
        entry.rounds.push(round);
    }

    /// Book one turn's token usage. When `reported` is `true`, the provider
    /// reported authoritative usage and `tokens` are real counts; when `false`,
    /// `tokens` are a local estimate.
    pub fn record(&self, provider: &str, model: &str, tokens: i64, reported: bool) {
        self.record_round(
            provider,
            model,
            TokenRound {
                reported,
                total_tokens: tokens,
                ..Default::default()
            },
        );
    }

    /// Book one turn's reported usage, including its prompt-cache split. The
    /// cache write/read counts are tracked as a breakout (they're already
    /// folded into `tokens` by the provider's usage parser); `cache_*` are
    /// clamped to non-negative. Callers with no caching pass `0, 0`.
    pub fn record_reported(
        &self,
        provider: &str,
        model: &str,
        tokens: i64,
        cache_write: i64,
        cache_read: i64,
    ) {
        self.record_round(
            provider,
            model,
            TokenRound {
                reported: true,
                total_tokens: tokens,
                cache_write_tokens: cache_write,
                cache_read_tokens: cache_read,
                ..Default::default()
            },
        );
    }

    /// A snapshot of the ledger suitable for rendering (owned, no lock held).
    pub fn snapshot(&self) -> TokenSourceReport {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let rows: Vec<TokenSourceRow> = entries
            .iter()
            .map(|((provider, model), entry)| TokenSourceRow {
                provider: provider.to_string(),
                model: model.to_string(),
                totals: entry.totals,
                rounds: entry.rounds.clone(),
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
    /// The ordered per-turn line items behind `totals`.
    pub rounds: Vec<TokenRound>,
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

    #[test]
    fn record_keeps_per_round_line_items() {
        // Each booking appends an ordered line item and splits input/output for
        // reported rounds, powering the detail drill-in.
        let ledger = TokenSourceLedger::new();
        ledger.record_round(
            "anthropic",
            "claude",
            TokenRound {
                reported: true,
                prompt_tokens: 1000,
                completion_tokens: 200,
                total_tokens: 1200,
                cache_write_tokens: 800,
                cache_read_tokens: 0,
            },
        );
        ledger.record("anthropic", "claude", 50, false);
        let row = &ledger.snapshot().rows[0];
        assert_eq!(row.rounds.len(), 2);
        assert!(row.rounds[0].reported);
        assert_eq!(row.rounds[0].prompt_tokens, 1000);
        assert_eq!(row.rounds[0].completion_tokens, 200);
        assert!(!row.rounds[1].reported);
        assert_eq!(row.rounds[1].total_tokens, 50);
        assert_eq!(row.totals.prompt_tokens, 1000);
        assert_eq!(row.totals.completion_tokens, 200);
        assert_eq!(row.totals.reported_tokens, 1200);
        assert_eq!(row.totals.estimated_tokens, 50);
    }
}
