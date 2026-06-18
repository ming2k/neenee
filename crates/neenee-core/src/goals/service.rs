use std::time::Instant;

use tokio::sync::Semaphore;

use super::store::GoalStore;
use super::{Goal, GoalAccountingResult, GoalStatus, ThreadGoal, TokenUsage};

pub struct GoalService {
    store: GoalStore,
    accounting_lock: Semaphore,
}

impl Clone for GoalService {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            accounting_lock: Semaphore::new(1),
        }
    }
}

impl GoalService {
    pub fn new(store: GoalStore) -> Self {
        Self {
            store,
            accounting_lock: Semaphore::new(1),
        }
    }

    pub async fn get_goal(&self, thread_id: &str) -> Result<Option<Goal>, String> {
        self.store
            .get_goal(thread_id)
            .await
            .map(|goal| goal.map(runtime_goal_from_persisted))
    }

    /// Set or replace the goal for a thread. This is the user-facing entry point.
    pub async fn set_goal(
        &self,
        thread_id: &str,
        objective: &str,
        status: GoalStatus,
        token_budget: Option<i64>,
    ) -> Result<Goal, String> {
        let objective = objective.trim();
        if objective.is_empty() {
            return Err("goal objective must not be empty".to_string());
        }
        if objective.chars().count() > 4000 {
            return Err("goal objective must be at most 4000 characters".to_string());
        }
        if let Some(budget) = token_budget {
            if budget <= 0 {
                return Err("goal budget must be positive".to_string());
            }
        }

        // Acquire the accounting lock so an idle continuation cannot interleave
        // with a user mutation.
        let _permit = self
            .accounting_lock
            .acquire()
            .await
            .map_err(|e| e.to_string())?;

        let goal = self
            .store
            .replace_goal(thread_id, objective, status, token_budget)
            .await?;
        Ok(runtime_goal_from_persisted(goal))
    }

    pub async fn update_goal(
        &self,
        thread_id: &str,
        objective: Option<&str>,
        status: Option<GoalStatus>,
        token_budget: Option<Option<i64>>,
    ) -> Result<Option<Goal>, String> {
        if let Some(objective) = objective {
            if objective.trim().is_empty() {
                return Err("goal objective must not be empty".to_string());
            }
            if objective.chars().count() > 4000 {
                return Err("goal objective must be at most 4000 characters".to_string());
            }
        }
        if let Some(Some(budget)) = token_budget {
            if budget <= 0 {
                return Err("goal budget must be positive".to_string());
            }
        }

        let _permit = self
            .accounting_lock
            .acquire()
            .await
            .map_err(|e| e.to_string())?;

        let goal = self
            .store
            .update_goal(thread_id, objective, status, token_budget, None)
            .await?;
        Ok(goal.map(runtime_goal_from_persisted))
    }

    pub async fn clear_goal(&self, thread_id: &str) -> Result<bool, String> {
        let _permit = self
            .accounting_lock
            .acquire()
            .await
            .map_err(|e| e.to_string())?;
        let deleted = self.store.delete_goal(thread_id).await?;
        Ok(deleted.is_some())
    }

    pub async fn pause(&self, thread_id: &str) -> Result<Option<Goal>, String> {
        self.update_goal_status(thread_id, GoalStatus::Paused).await
    }

    pub async fn resume(&self, thread_id: &str) -> Result<Option<Goal>, String> {
        let _permit = self
            .accounting_lock
            .acquire()
            .await
            .map_err(|e| e.to_string())?;
        let Some(goal) = self.store.get_goal(thread_id).await? else {
            return Ok(None);
        };
        if !goal.status.can_be_resumed() {
            return Err(format!("cannot resume goal with status {:?}", goal.status));
        }
        let new_status = if goal.status == GoalStatus::BudgetLimited {
            GoalStatus::BudgetLimited
        } else {
            GoalStatus::Active
        };
        let updated = self
            .store
            .update_goal(thread_id, None, Some(new_status), None, Some(&goal.goal_id))
            .await?;
        Ok(updated.map(runtime_goal_from_persisted))
    }

    pub async fn mark_complete(&self, thread_id: &str) -> Result<Option<Goal>, String> {
        self.update_goal_status(thread_id, GoalStatus::Complete)
            .await
    }

    pub async fn mark_blocked(&self, thread_id: &str) -> Result<Option<Goal>, String> {
        self.update_goal_status(thread_id, GoalStatus::Blocked)
            .await
    }

    async fn update_goal_status(
        &self,
        thread_id: &str,
        new_status: GoalStatus,
    ) -> Result<Option<Goal>, String> {
        let _permit = self
            .accounting_lock
            .acquire()
            .await
            .map_err(|e| e.to_string())?;
        let Some(goal) = self.store.get_goal(thread_id).await? else {
            return Ok(None);
        };

        let allowed = matches!(
            (goal.status, new_status),
            (GoalStatus::Active, GoalStatus::Paused)
                | (GoalStatus::Active, GoalStatus::Blocked)
                | (GoalStatus::Active, GoalStatus::Complete)
                | (GoalStatus::Paused, GoalStatus::Active)
                | (GoalStatus::Blocked, GoalStatus::Active)
                | (GoalStatus::Blocked, GoalStatus::Complete)
                | (GoalStatus::UsageLimited, GoalStatus::Active)
                | (GoalStatus::UsageLimited, GoalStatus::Complete)
                | (GoalStatus::BudgetLimited, GoalStatus::Active)
                | (GoalStatus::BudgetLimited, GoalStatus::Complete)
                | (GoalStatus::Complete, GoalStatus::Active)
        );

        if !allowed {
            return Err(format!(
                "cannot transition goal from {:?} to {:?}",
                goal.status, new_status
            ));
        }

        let updated = self
            .store
            .update_goal(thread_id, None, Some(new_status), None, Some(&goal.goal_id))
            .await?;
        Ok(updated.map(runtime_goal_from_persisted))
    }

    /// Record the time/token cost of a finished turn against the active goal.
    pub async fn account_turn(
        &self,
        thread_id: &str,
        token_usage: TokenUsage,
        elapsed_seconds: i64,
    ) -> Result<GoalAccountingResult, String> {
        let _permit = self
            .accounting_lock
            .acquire()
            .await
            .map_err(|e| e.to_string())?;

        let Some(goal) = self.store.get_goal(thread_id).await? else {
            return Ok(GoalAccountingResult::Unchanged);
        };

        if !matches!(goal.status, GoalStatus::Active | GoalStatus::BudgetLimited) {
            return Ok(GoalAccountingResult::Unchanged);
        }

        let updated = self
            .store
            .account_usage(
                thread_id,
                elapsed_seconds,
                token_usage.total_tokens,
                Some(&goal.goal_id),
            )
            .await?;

        match updated {
            Some(goal) => Ok(GoalAccountingResult::Updated(runtime_goal_from_persisted(
                goal,
            ))),
            None => Ok(GoalAccountingResult::Unchanged),
        }
    }

    /// Convenience: check whether a thread has an active goal that should auto-continue.
    pub async fn should_auto_continue(&self, thread_id: &str) -> Result<bool, String> {
        let goal = self.get_goal(thread_id).await?;
        Ok(goal.is_some_and(|g| g.status == GoalStatus::Active))
    }

    /// Convenience: fetch the current goal and ensure it is active.
    pub async fn active_goal(&self, thread_id: &str) -> Result<Option<Goal>, String> {
        self.get_goal(thread_id)
            .await
            .map(|g| g.filter(|g| g.status == GoalStatus::Active))
    }
}

fn runtime_goal_from_persisted(goal: ThreadGoal) -> Goal {
    Goal {
        objective: goal.objective,
        status: goal.status,
        checklist: Vec::new(), // checklist is kept in-memory on Agent for now
        tokens_used: goal.tokens_used,
        token_budget: goal.token_budget,
        time_used_seconds: goal.time_used_seconds,
    }
}

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
