use super::store::PursuitStore;
use neenee_core::pursuits::{Pursuit, ThreadPursuit};

/// Slimmed (ADR-0010) facade over the `thread_pursuits` SQLite table.
///
/// The pre-0010 status machine, token budget, elapsed-time accounting,
/// `pause` / `resume` / `mark_blocked` transitions, and the
/// `accounting_lock` semaphore are all gone. The remaining surface is the
/// minimum needed to back `/pursuit <objective>`, `/pursuit edit`, `/pursuit done`,
/// `/pursuit clear`, and the `[NEENEE_PURSUIT_COMPLETE]` marker path.
pub struct PursuitService {
    store: PursuitStore,
}

impl Clone for PursuitService {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
        }
    }
}

impl PursuitService {
    pub fn new(store: PursuitStore) -> Self {
        Self { store }
    }

    pub async fn get_pursuit(&self, thread_id: &str) -> Result<Option<Pursuit>, String> {
        self.store
            .get_pursuit(thread_id)
            .await
            .map(|pursuit| pursuit.map(runtime_pursuit_from_persisted))
    }

    /// Set or replace the pursuit for a thread. Always creates an active,
    /// incomplete pursuit — the pre-0010 `status` and `token_budget` arguments
    /// are gone.
    pub async fn set_pursuit(&self, thread_id: &str, objective: &str) -> Result<Pursuit, String> {
        let objective = objective.trim();
        if objective.is_empty() {
            return Err("pursuit objective must not be empty".to_string());
        }
        if objective.chars().count() > 4000 {
            return Err("pursuit objective must be at most 4000 characters".to_string());
        }
        let pursuit = self.store.replace_pursuit(thread_id, objective).await?;
        Ok(runtime_pursuit_from_persisted(pursuit))
    }

    /// Rewrite the objective of the existing pursuit. Returns `None` if no pursuit
    /// is set for the thread.
    pub async fn update_objective(
        &self,
        thread_id: &str,
        objective: &str,
    ) -> Result<Option<Pursuit>, String> {
        let objective = objective.trim();
        if objective.is_empty() {
            return Err("pursuit objective must not be empty".to_string());
        }
        if objective.chars().count() > 4000 {
            return Err("pursuit objective must be at most 4000 characters".to_string());
        }
        let pursuit = self.store.update_objective(thread_id, objective).await?;
        Ok(pursuit.map(runtime_pursuit_from_persisted))
    }

    pub async fn clear_pursuit(&self, thread_id: &str) -> Result<bool, String> {
        let deleted = self.store.delete_pursuit(thread_id).await?;
        Ok(deleted.is_some())
    }

    pub async fn mark_complete(&self, thread_id: &str) -> Result<Option<Pursuit>, String> {
        let pursuit = self.store.mark_complete(thread_id).await?;
        Ok(pursuit.map(runtime_pursuit_from_persisted))
    }

    /// Convenience: fetch the current pursuit only if it is still active
    /// (i.e. not yet marked complete).
    pub async fn active_pursuit(&self, thread_id: &str) -> Result<Option<Pursuit>, String> {
        self.get_pursuit(thread_id)
            .await
            .map(|g| g.filter(|g| !g.is_complete))
    }
}

fn runtime_pursuit_from_persisted(pursuit: ThreadPursuit) -> Pursuit {
    Pursuit {
        objective: pursuit.objective,
        is_complete: pursuit.is_complete,
    }
}
