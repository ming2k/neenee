use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use uuid::Uuid;

use super::ThreadGoal;

/// Schema for the `thread_goals` table.
///
/// ADR-0010 slimmed the runtime/persisted goal primitive to
/// `objective` + `is_complete` + timestamps. The SQL schema keeps the
/// pre-0010 `token_budget` / `tokens_used` / `time_used_seconds` columns
/// so legacy databases still load cleanly; they are simply never read or
/// written by the new code paths. The `status` TEXT column keeps its
/// shape but only two values are written going forward: `"active"` and
/// `"complete"`. Pre-0010 statuses (`paused`, `blocked`, `usage_limited`,
/// `budget_limited`) are mapped to `"active"` on read.
const THREAD_GOALS_SCHEMA: &str = r#"
                CREATE TABLE IF NOT EXISTS thread_goals (
                    thread_id TEXT PRIMARY KEY,
                    goal_id TEXT NOT NULL,
                    objective TEXT NOT NULL,
                    status TEXT NOT NULL,
                    token_budget INTEGER,
                    tokens_used INTEGER NOT NULL DEFAULT 0,
                    time_used_seconds INTEGER NOT NULL DEFAULT 0,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL
                )
                "#;

pub struct GoalStore {
    conn: Arc<std::sync::Mutex<Connection>>,
}

impl Clone for GoalStore {
    fn clone(&self) -> Self {
        Self {
            conn: Arc::clone(&self.conn),
        }
    }
}

impl GoalStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let conn = tokio::task::spawn_blocking(move || {
            Connection::open(&path).map_err(|err| format!("failed to open goals db: {err}"))
        })
        .await
        .map_err(|err| format!("db open task failed: {err}"))??;

        let store = Self {
            conn: Arc::new(std::sync::Mutex::new(conn)),
        };
        store.migrate().await?;
        Ok(store)
    }

    pub async fn open_in_memory() -> Result<Self, String> {
        let conn = tokio::task::spawn_blocking(|| {
            Connection::open_in_memory()
                .map_err(|err| format!("failed to open in-memory db: {err}"))
        })
        .await
        .map_err(|err| format!("db open task failed: {err}"))??;

        let store = Self {
            conn: Arc::new(std::sync::Mutex::new(conn)),
        };
        store.migrate().await?;
        Ok(store)
    }

    /// Synchronous in-memory constructor for tests that run outside an async
    /// context (the schema setup is cheap and never blocks meaningfully).
    pub fn open_in_memory_blocking() -> Result<Self, String> {
        let conn = Connection::open_in_memory()
            .map_err(|err| format!("failed to open in-memory db: {err}"))?;
        conn.execute(THREAD_GOALS_SCHEMA, [])
            .map_err(|err| format!("failed to create thread_goals table: {err}"))?;
        Ok(Self {
            conn: Arc::new(std::sync::Mutex::new(conn)),
        })
    }

    async fn migrate(&self) -> Result<(), String> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            conn.execute(THREAD_GOALS_SCHEMA, [])
                .map_err(|err| format!("failed to create thread_goals table: {err}"))?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|err| format!("migrate task failed: {err}"))?
    }

    pub async fn get_goal(&self, thread_id: &str) -> Result<Option<ThreadGoal>, String> {
        let thread_id = thread_id.to_string();
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            let mut stmt = conn
                .prepare(
                    r#"
                    SELECT
                        thread_id, goal_id, objective, status, created_at_ms, updated_at_ms
                    FROM thread_goals
                    WHERE thread_id = ?1
                    "#,
                )
                .map_err(|err| format!("prepare get_goal failed: {err}"))?;
            let mut rows = stmt
                .query_map([&thread_id], thread_goal_from_row)
                .map_err(|err| format!("query get_goal failed: {err}"))?;
            rows.next().transpose().map_err(|err| err.to_string())
        })
        .await
        .map_err(|err| format!("get_goal task failed: {err}"))?
    }

    /// Replace any existing goal with a brand-new active, incomplete goal.
    pub async fn replace_goal(
        &self,
        thread_id: &str,
        objective: &str,
    ) -> Result<ThreadGoal, String> {
        let thread_id = thread_id.to_string();
        let thread_id_for_get = thread_id.clone();
        let objective = objective.to_string();
        let now_ms = Utc::now().timestamp_millis();
        let goal_id = Uuid::new_v4().to_string();

        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            conn.execute(
                r#"
                INSERT INTO thread_goals (
                    thread_id, goal_id, objective, status, token_budget,
                    tokens_used, time_used_seconds, created_at_ms, updated_at_ms
                ) VALUES (?1, ?2, ?3, 'active', NULL, 0, 0, ?4, ?4)
                ON CONFLICT(thread_id) DO UPDATE SET
                    goal_id = excluded.goal_id,
                    objective = excluded.objective,
                    status = excluded.status,
                    token_budget = excluded.token_budget,
                    tokens_used = 0,
                    time_used_seconds = 0,
                    created_at_ms = excluded.created_at_ms,
                    updated_at_ms = excluded.updated_at_ms
                "#,
                params![thread_id, goal_id, objective, now_ms],
            )
            .map_err(|err| format!("replace_goal failed: {err}"))?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|err| format!("replace_goal task failed: {err}"))??;

        self.get_goal(&thread_id_for_get)
            .await?
            .ok_or_else(|| "replace_goal succeeded but row is missing".to_string())
    }

    /// Rewrite the objective of an existing goal. Returns `None` if no row
    /// matches `thread_id`.
    pub async fn update_objective(
        &self,
        thread_id: &str,
        objective: &str,
    ) -> Result<Option<ThreadGoal>, String> {
        let thread_id = thread_id.to_string();
        let thread_id_for_get = thread_id.clone();
        let objective = objective.to_string();
        let now_ms = Utc::now().timestamp_millis();

        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            conn.execute(
                r#"
                UPDATE thread_goals
                SET objective = ?1, updated_at_ms = ?2
                WHERE thread_id = ?3
                "#,
                params![objective, now_ms, thread_id],
            )
            .map_err(|err| format!("update_objective failed: {err}"))?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|err| format!("update_objective task failed: {err}"))??;

        self.get_goal(&thread_id_for_get).await
    }

    /// Flip the goal's `is_complete` flag to true. Returns `None` if no row
    /// matches `thread_id` or the goal is already complete.
    pub async fn mark_complete(&self, thread_id: &str) -> Result<Option<ThreadGoal>, String> {
        let thread_id = thread_id.to_string();
        let thread_id_for_get = thread_id.clone();
        let now_ms = Utc::now().timestamp_millis();

        let conn = Arc::clone(&self.conn);
        let affected = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            let rows = conn
                .execute(
                    r#"
                    UPDATE thread_goals
                    SET status = 'complete', updated_at_ms = ?1
                    WHERE thread_id = ?2 AND status = 'active'
                    "#,
                    params![now_ms, thread_id],
                )
                .map_err(|err| format!("mark_complete failed: {err}"))?;
            Ok::<_, String>(rows)
        })
        .await
        .map_err(|err| format!("mark_complete task failed: {err}"))??;

        if affected == 0 {
            // Either no row exists, or it was already complete. Either way,
            // surface the current state to the caller.
            return self.get_goal(&thread_id_for_get).await;
        }
        self.get_goal(&thread_id_for_get).await
    }

    pub async fn delete_goal(&self, thread_id: &str) -> Result<Option<ThreadGoal>, String> {
        let thread_id = thread_id.to_string();
        let goal = self.get_goal(&thread_id).await?;
        if goal.is_none() {
            return Ok(None);
        }

        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            conn.execute(
                "DELETE FROM thread_goals WHERE thread_id = ?1",
                [&thread_id],
            )
            .map_err(|err| format!("delete_goal failed: {err}"))?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|err| format!("delete_goal task failed: {err}"))??;

        Ok(goal)
    }
}

fn thread_goal_from_row(row: &rusqlite::Row) -> Result<ThreadGoal, rusqlite::Error> {
    let status: String = row.get(3)?;
    // Post-ADR-0010 only "active" and "complete" are written. Any pre-0010
    // status (paused / blocked / usage_limited / budget_limited) maps to
    // active on read — the user loses paused/blocked state across the
    // upgrade, which is acceptable because those states no longer exist.
    let is_complete = status == "complete";
    let created_at_ms: i64 = row.get(4)?;
    let updated_at_ms: i64 = row.get(5)?;
    Ok(ThreadGoal {
        thread_id: row.get(0)?,
        goal_id: row.get(1)?,
        objective: row.get(2)?,
        is_complete,
        created_at: DateTime::from_timestamp_millis(created_at_ms).unwrap_or_else(Utc::now),
        updated_at: DateTime::from_timestamp_millis(updated_at_ms).unwrap_or_else(Utc::now),
    })
}
