use std::time::Instant;

use super::store::GoalStore;
use super::{Goal, ThreadGoal};

/// Slimmed (ADR-0010) facade over the `thread_goals` SQLite table.
///
/// The pre-0010 status machine, token budget, elapsed-time accounting,
/// `pause` / `resume` / `mark_blocked` transitions, and the
/// `accounting_lock` semaphore are all gone. The remaining surface is the
/// minimum needed to back `/goal <objective>`, `/goal edit`, `/goal done`,
/// `/goal clear`, and the `[NEENEE_GOAL_COMPLETE]` marker path.
pub struct GoalService {
    store: GoalStore,
}

impl Clone for GoalService {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
        }
    }
}

impl GoalService {
    pub fn new(store: GoalStore) -> Self {
        Self { store }
    }

    pub async fn get_goal(&self, thread_id: &str) -> Result<Option<Goal>, String> {
        self.store
            .get_goal(thread_id)
            .await
            .map(|goal| goal.map(runtime_goal_from_persisted))
    }

    /// Set or replace the goal for a thread. Always creates an active,
    /// incomplete goal — the pre-0010 `status` and `token_budget` arguments
    /// are gone.
    pub async fn set_goal(&self, thread_id: &str, objective: &str) -> Result<Goal, String> {
        let objective = objective.trim();
        if objective.is_empty() {
            return Err("goal objective must not be empty".to_string());
        }
        if objective.chars().count() > 4000 {
            return Err("goal objective must be at most 4000 characters".to_string());
        }
        let goal = self.store.replace_goal(thread_id, objective).await?;
        Ok(runtime_goal_from_persisted(goal))
    }

    /// Rewrite the objective of the existing goal. Returns `None` if no goal
    /// is set for the thread.
    pub async fn update_objective(
        &self,
        thread_id: &str,
        objective: &str,
    ) -> Result<Option<Goal>, String> {
        let objective = objective.trim();
        if objective.is_empty() {
            return Err("goal objective must not be empty".to_string());
        }
        if objective.chars().count() > 4000 {
            return Err("goal objective must be at most 4000 characters".to_string());
        }
        let goal = self.store.update_objective(thread_id, objective).await?;
        Ok(goal.map(runtime_goal_from_persisted))
    }

    pub async fn clear_goal(&self, thread_id: &str) -> Result<bool, String> {
        let deleted = self.store.delete_goal(thread_id).await?;
        Ok(deleted.is_some())
    }

    pub async fn mark_complete(&self, thread_id: &str) -> Result<Option<Goal>, String> {
        let goal = self.store.mark_complete(thread_id).await?;
        Ok(goal.map(runtime_goal_from_persisted))
    }

    /// Convenience: fetch the current goal only if it is still active
    /// (i.e. not yet marked complete).
    pub async fn active_goal(&self, thread_id: &str) -> Result<Option<Goal>, String> {
        self.get_goal(thread_id)
            .await
            .map(|g| g.filter(|g| !g.is_complete))
    }
}

fn runtime_goal_from_persisted(goal: ThreadGoal) -> Goal {
    Goal {
        objective: goal.objective,
        is_complete: goal.is_complete,
        // The checklist is intentionally not persisted — it lives in memory
        // on the Agent and is re-attached by `refresh_agent_goal`.
        checklist: Vec::new(),
    }
}

/// Turn-level elapsed-time keeper. Kept after ADR-0010 even though
/// goal-level time accounting is gone, because the harness still uses it
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
