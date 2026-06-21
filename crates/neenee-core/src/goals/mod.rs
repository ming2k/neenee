pub mod prompts;
pub mod service;
pub mod store;
pub mod tools;

pub use service::{GoalService, TurnTimer};
pub use store::GoalStore;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The persisted view of a thread/session goal.
///
/// Slimmed in ADR-0010: the status machine, token budget, and elapsed-time
/// accounting are gone. Only `objective`, `is_complete`, and timestamps
/// persist. The `thread_goals` table still carries the legacy
/// `token_budget` / `tokens_used` / `time_used_seconds` columns for
/// backward compatibility with pre-0010 databases, but they are no longer
/// read or written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadGoal {
    pub thread_id: String,
    pub goal_id: String,
    pub objective: String,
    pub is_complete: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The runtime view of a goal exposed to the agent, tools, and TUI.
///
/// Carries the durable `objective`, the in-memory `checklist` that gates
/// completion via [`Goal::can_complete`], and a single `is_complete` flag
/// that mirrors the persisted column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub objective: String,
    #[serde(default)]
    pub is_complete: bool,
    #[serde(default)]
    pub checklist: Vec<GoalChecklistItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalChecklistStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalChecklistItem {
    pub content: String,
    pub status: GoalChecklistStatus,
}

impl Goal {
    /// Whether the goal can be marked complete. Returns `true` when the
    /// checklist is empty or every item is `Completed` / `Cancelled`.
    pub fn can_complete(&self) -> bool {
        self.checklist.is_empty()
            || self.checklist.iter().all(|item| {
                matches!(
                    item.status,
                    GoalChecklistStatus::Completed | GoalChecklistStatus::Cancelled
                )
            })
    }
}

/// Token usage reported by a single turn.
///
/// Per-turn telemetry only — not booked against any goal (ADR-0010 removed
/// goal-level token accounting).
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
