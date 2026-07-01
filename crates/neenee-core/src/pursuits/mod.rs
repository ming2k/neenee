//! Pursuit domain types (ADR-0005 pure-domain half).
//!
//! The persisted/I/O-bound layer — the `rusqlite`-backed `PursuitStore` and the
//! `PursuitService` facade — lives in `neenee_store::pursuits`. This module
//! keeps only the domain shapes a frontend needs without pulling in SQLite:
//! `Pursuit` (runtime view), `ThreadPursuit` (persisted view), `TokenUsage`,
//! `RoundOutcome`, and the per-turn `RoundTimer`. The pursuit lifecycle is driven
//! by the `/pursue` slash command, the in-turn stop-gate, and the
//! `[NEENEE_PURSUIT_COMPLETE]` marker; there are no model-facing pursuit tools
//! (ADR-0031).

pub mod prompts;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// The persisted view of a thread/session pursuit.
///
/// Slimmed in ADR-0010: the status machine, token budget, and elapsed-time
/// accounting are gone. Only `objective`, `is_complete`, and timestamps
/// persist. The `thread_pursuits` table still carries the legacy
/// `token_budget` / `tokens_used` / `time_used_seconds` columns for
/// backward compatibility with pre-0010 databases, but they are no longer
/// read or written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadPursuit {
    pub thread_id: String,
    pub pursuit_id: String,
    pub objective: String,
    pub is_complete: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The runtime view of a pursuit exposed to the agent and TUI.
///
/// Carries the durable `objective` and a single `is_complete` flag that
/// mirrors the persisted column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pursuit {
    pub objective: String,
    #[serde(default)]
    pub is_complete: bool,
}

/// Token usage reported by a single turn.
///
/// Per-turn telemetry only — not booked against any pursuit (ADR-0010 removed
/// pursuit-level token accounting).
///
/// `cache_creation_input_tokens` / `cache_read_input_tokens` carry Anthropic
/// prompt-caching counts: Anthropic's `input_tokens` reports ONLY the uncached
/// dynamic suffix, so the cache write/read counts must be tracked separately
/// (and added into `prompt_tokens`/`total_tokens`) or the context meter would
/// undercount every cached turn. They stay `0` for providers without caching
/// (OpenAI-style auto-caching surfaces its discount as `cached_tokens`, not
/// these fields).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    /// Tokens written to the prompt cache this turn (billed at a premium by
    /// Anthropic; absent on providers without explicit caching).
    pub cache_creation_input_tokens: i64,
    /// Tokens served from the prompt cache this turn (billed at a steep
    /// discount by Anthropic; absent on providers without explicit caching).
    pub cache_read_input_tokens: i64,
}

/// Outcome returned by the agent after running one turn.
#[derive(Debug, Clone)]
pub struct RoundOutcome {
    pub message: crate::Message,
    pub token_usage: TokenUsage,
    pub duration_ms: u64,
}

/// Turn-level elapsed-time keeper. Kept after ADR-0010 even though
/// pursuit-level time accounting is gone, because the harness still uses it
/// for per-turn telemetry (e.g. plan-progress timestamps).
pub struct RoundTimer {
    start: Instant,
}

impl Default for RoundTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl RoundTimer {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    pub fn elapsed_seconds(&self) -> i64 {
        self.start.elapsed().as_secs() as i64
    }
}
