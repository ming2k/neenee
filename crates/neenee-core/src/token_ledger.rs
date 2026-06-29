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
    }
}

/// The key under which a provider+model's totals are accumulated.
///
/// Stored as `(provider_id, model)` so a session that switches providers or
/// models keeps each one's accuracy picture separate.
fn key(provider: &str, model: &str) -> String {
    format!("{provider}\u{1f}{model}")
}

/// A thread-safe running ledger of token counts split by source (reported vs.
/// estimated), keyed by `(provider_id, model)`. Shared between the agent (the
/// writer — books each turn) and the TUI (the reader — renders the report).
#[derive(Debug, Default)]
pub struct TokenSourceLedger {
    /// `(provider \u{1f} model)` → accumulated totals. A [`BTreeMap`] so the
    /// report iterates in a stable order.
    entries: Mutex<BTreeMap<String, TokenSourceTotals>>,
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

    /// A snapshot of the ledger suitable for rendering (owned, no lock held).
    pub fn snapshot(&self) -> TokenSourceReport {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let rows: Vec<TokenSourceRow> = entries
            .iter()
            .map(|(k, totals)| {
                let (provider, model) = k.split_once('\u{1f}').unwrap_or((k, ""));
                TokenSourceRow {
                    provider: provider.to_string(),
                    model: model.to_string(),
                    totals: *totals,
                }
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
}
