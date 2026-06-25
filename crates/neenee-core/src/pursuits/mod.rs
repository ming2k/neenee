//! Pursuit domain types (ADR-0005 pure-domain half).
//!
//! The persisted/I/O-bound layer — the `rusqlite`-backed `PursuitStore`, the
//! `PursuitService` facade, and the pursuit tools — lives in
//! `neenee_store::pursuits`. This module keeps only the domain shapes a
//! frontend needs without pulling in SQLite: `Pursuit` (runtime view),
//! `ThreadPursuit` (persisted view), `TokenUsage`, `TurnOutcome`, and the
//! per-turn `TurnTimer`.

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

/// The runtime view of a pursuit exposed to the agent, tools, and TUI.
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

/// Outcome returned by the agent after running one turn.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub message: crate::Message,
    pub token_usage: TokenUsage,
    pub duration_ms: u64,
}

/// Turn-level elapsed-time keeper. Kept after ADR-0010 even though
/// pursuit-level time accounting is gone, because the harness still uses it
/// for per-turn telemetry (e.g. plan-progress timestamps).
pub struct TurnTimer {
    start: Instant,
}

impl Default for TurnTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnTimer {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    pub fn elapsed_seconds(&self) -> i64 {
        self.start.elapsed().as_secs() as i64
    }
}
