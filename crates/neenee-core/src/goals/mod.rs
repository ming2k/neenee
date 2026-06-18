pub mod prompts;
pub mod service;
pub mod store;
pub mod tools;

pub use service::{GoalService, TurnTimer};
pub use store::GoalStore;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The persisted view of a thread/session goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadGoal {
    pub thread_id: String,
    pub goal_id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub time_used_seconds: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The runtime view of a goal exposed to the agent, tools, and TUI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub objective: String,
    pub status: GoalStatus,
    #[serde(default)]
    pub checklist: Vec<GoalChecklistItem>,
    #[serde(default)]
    pub tokens_used: i64,
    #[serde(default)]
    pub token_budget: Option<i64>,
    #[serde(default)]
    pub time_used_seconds: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

impl GoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::UsageLimited => "usage_limited",
            Self::BudgetLimited => "budget_limited",
            Self::Complete => "complete",
        }
    }

    pub fn is_active(self) -> bool {
        self == Self::Active
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::BudgetLimited | Self::Complete)
    }

    pub fn can_be_resumed(self) -> bool {
        matches!(
            self,
            Self::Paused | Self::Blocked | Self::UsageLimited | Self::BudgetLimited
        )
    }
}

impl TryFrom<&str> for GoalStatus {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, String> {
        match value {
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            "blocked" => Ok(Self::Blocked),
            "usage_limited" => Ok(Self::UsageLimited),
            "budget_limited" => Ok(Self::BudgetLimited),
            "complete" => Ok(Self::Complete),
            other => Err(format!("unknown goal status `{other}`")),
        }
    }
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
    pub fn can_complete(&self) -> bool {
        self.checklist.is_empty()
            || self
                .checklist
                .iter()
                .all(|item| matches!(item.status, GoalChecklistStatus::Completed | GoalChecklistStatus::Cancelled))
    }

    pub fn remaining_tokens(&self) -> Option<i64> {
        self.token_budget.map(|budget| (budget - self.tokens_used).max(0))
    }
}

/// Token usage reported by a single turn.
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

/// Result of accounting one turn against the active goal.
#[derive(Debug, Clone)]
pub enum GoalAccountingResult {
    /// Goal state changed (e.g. became BudgetLimited).
    Updated(Goal),
    /// No active goal or usage did not change state.
    Unchanged,
}
