pub mod prompts;
pub mod service;
pub mod store;
pub mod tools;

pub use service::{PursuitService, TurnTimer};
pub use store::PursuitStore;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
