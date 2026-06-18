use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use uuid::Uuid;

use super::{GoalStatus, ThreadGoal};

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
            Connection::open_in_memory().map_err(|err| format!("failed to open in-memory db: {err}"))
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
                        thread_id, goal_id, objective, status, token_budget,
                        tokens_used, time_used_seconds, created_at_ms, updated_at_ms
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

    /// Replace any existing goal with a brand-new active goal.
    pub async fn replace_goal(
        &self,
        thread_id: &str,
        objective: &str,
        status: GoalStatus,
        token_budget: Option<i64>,
    ) -> Result<ThreadGoal, String> {
        let thread_id = thread_id.to_string();
        let thread_id_for_get = thread_id.clone();
        let objective = objective.to_string();
        let now_ms = Utc::now().timestamp_millis();
        let goal_id = Uuid::new_v4().to_string();
        let status = status_after_budget_limit(status, 0, token_budget);
        let status_str = status.as_str().to_string();

        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            conn.execute(
                r#"
                INSERT INTO thread_goals (
                    thread_id, goal_id, objective, status, token_budget,
                    tokens_used, time_used_seconds, created_at_ms, updated_at_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6, ?6)
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
                params![thread_id, goal_id, objective, status_str, token_budget, now_ms],
            )
            .map_err(|err| format!("replace_goal failed: {err}"))?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|err| format!("replace_goal task failed: {err}"))??;

        self.get_goal(&thread_id_for_get).await?.ok_or_else(|| {
            "replace_goal succeeded but row is missing".to_string()
        })
    }

    /// Insert a new goal only if there is no existing unfinished goal.
    pub async fn insert_goal(
        &self,
        thread_id: &str,
        objective: &str,
        status: GoalStatus,
        token_budget: Option<i64>,
    ) -> Result<Option<ThreadGoal>, String> {
        let thread_id = thread_id.to_string();
        let objective = objective.to_string();
        let now_ms = Utc::now().timestamp_millis();
        let goal_id = Uuid::new_v4().to_string();
        let status = status_after_budget_limit(status, 0, token_budget);
        let status_str = status.as_str().to_string();

        let thread_id_for_get = thread_id.clone();
        let conn = Arc::clone(&self.conn);
        let inserted = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            let rows = conn.execute(
                r#"
                INSERT INTO thread_goals (
                    thread_id, goal_id, objective, status, token_budget,
                    tokens_used, time_used_seconds, created_at_ms, updated_at_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, ?6, ?6)
                ON CONFLICT(thread_id) DO UPDATE SET
                    goal_id = excluded.goal_id,
                    objective = excluded.objective,
                    status = excluded.status,
                    token_budget = excluded.token_budget,
                    tokens_used = 0,
                    time_used_seconds = 0,
                    created_at_ms = excluded.created_at_ms,
                    updated_at_ms = excluded.updated_at_ms
                WHERE thread_goals.status = 'complete'
                "#,
                params![thread_id, goal_id, objective, status_str, token_budget, now_ms],
            )
            .map_err(|err| format!("insert_goal failed: {err}"))?;
            Ok::<_, String>(rows > 0)
        })
        .await
        .map_err(|err| format!("insert_goal task failed: {err}"))??;

        if inserted {
            self.get_goal(&thread_id_for_get).await
        } else {
            Ok(None)
        }
    }

    pub async fn update_goal(
        &self,
        thread_id: &str,
        objective: Option<&str>,
        status: Option<GoalStatus>,
        token_budget: Option<Option<i64>>,
        expected_goal_id: Option<&str>,
    ) -> Result<Option<ThreadGoal>, String> {
        let thread_id = thread_id.to_string();
        let objective = objective.map(str::to_string);
        let status = status.map(|s| status_after_budget_limit(s, 0, None));
        let status_str = status.as_ref().map(|s| s.as_str().to_string());
        let expected_goal_id = expected_goal_id.map(str::to_string);
        let now_ms = Utc::now().timestamp_millis();

        let thread_id_for_get = thread_id.clone();
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;

            // We handle four SQL shapes explicitly to avoid dynamic parameter indexing errors.
            let _rows_affected: usize = match (&objective, &status_str, &token_budget) {
                (Some(objective), Some(status), Some(token_budget)) => conn.execute(
                    r#"
                    UPDATE thread_goals
                    SET
                        objective = ?1,
                        status = CASE
                            WHEN status = 'budget_limited' AND ?2 IN ('paused', 'blocked') THEN status
                            WHEN ?2 = 'active' AND token_budget IS NOT NULL AND tokens_used >= token_budget THEN 'budget_limited'
                            ELSE ?2
                        END,
                        token_budget = ?3,
                        updated_at_ms = ?4
                    WHERE thread_id = ?5
                      AND (?6 IS NULL OR goal_id = ?6)
                    "#,
                    params![objective, status, token_budget, now_ms, thread_id, expected_goal_id],
                ),
                (Some(objective), Some(status), None) => conn.execute(
                    r#"
                    UPDATE thread_goals
                    SET
                        objective = ?1,
                        status = CASE
                            WHEN status = 'budget_limited' AND ?2 IN ('paused', 'blocked') THEN status
                            WHEN ?2 = 'active' AND token_budget IS NOT NULL AND tokens_used >= token_budget THEN 'budget_limited'
                            ELSE ?2
                        END,
                        updated_at_ms = ?3
                    WHERE thread_id = ?4
                      AND (?5 IS NULL OR goal_id = ?5)
                    "#,
                    params![objective, status, now_ms, thread_id, expected_goal_id],
                ),
                (Some(objective), None, Some(token_budget)) => conn.execute(
                    r#"
                    UPDATE thread_goals
                    SET
                        objective = ?1,
                        token_budget = ?2,
                        status = CASE
                            WHEN status = 'active' AND ?2 IS NOT NULL AND tokens_used >= ?2 THEN 'budget_limited'
                            ELSE status
                        END,
                        updated_at_ms = ?3
                    WHERE thread_id = ?4
                      AND (?5 IS NULL OR goal_id = ?5)
                    "#,
                    params![objective, token_budget, now_ms, thread_id, expected_goal_id],
                ),
                (Some(objective), None, None) => conn.execute(
                    r#"
                    UPDATE thread_goals
                    SET
                        objective = ?1,
                        updated_at_ms = ?2
                    WHERE thread_id = ?3
                      AND (?4 IS NULL OR goal_id = ?4)
                    "#,
                    params![objective, now_ms, thread_id, expected_goal_id],
                ),
                (None, Some(status), Some(token_budget)) => conn.execute(
                    r#"
                    UPDATE thread_goals
                    SET
                        status = CASE
                            WHEN status = 'budget_limited' AND ?1 IN ('paused', 'blocked') THEN status
                            WHEN ?1 = 'active' AND ?2 IS NOT NULL AND tokens_used >= ?2 THEN 'budget_limited'
                            ELSE ?1
                        END,
                        token_budget = ?2,
                        updated_at_ms = ?3
                    WHERE thread_id = ?4
                      AND (?5 IS NULL OR goal_id = ?5)
                    "#,
                    params![status, token_budget, now_ms, thread_id, expected_goal_id],
                ),
                (None, Some(status), None) => conn.execute(
                    r#"
                    UPDATE thread_goals
                    SET
                        status = CASE
                            WHEN status = 'budget_limited' AND ?1 IN ('paused', 'blocked') THEN status
                            WHEN ?1 = 'active' AND token_budget IS NOT NULL AND tokens_used >= token_budget THEN 'budget_limited'
                            ELSE ?1
                        END,
                        updated_at_ms = ?2
                    WHERE thread_id = ?3
                      AND (?4 IS NULL OR goal_id = ?4)
                    "#,
                    params![status, now_ms, thread_id, expected_goal_id],
                ),
                (None, None, Some(token_budget)) => conn.execute(
                    r#"
                    UPDATE thread_goals
                    SET
                        token_budget = ?1,
                        status = CASE
                            WHEN status = 'active' AND ?1 IS NOT NULL AND tokens_used >= ?1 THEN 'budget_limited'
                            ELSE status
                        END,
                        updated_at_ms = ?2
                    WHERE thread_id = ?3
                      AND (?4 IS NULL OR goal_id = ?4)
                    "#,
                    params![token_budget, now_ms, thread_id, expected_goal_id],
                ),
                (None, None, None) => {
                    return Ok::<_, String>(());
                }
            }
            .map_err(|err| format!("update_goal failed: {err}"))?;

            Ok::<_, String>(())
        })
        .await
        .map_err(|err| format!("update_goal task failed: {err}"))??;

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

    /// Add time and token usage to the active goal.
    pub async fn account_usage(
        &self,
        thread_id: &str,
        time_delta_seconds: i64,
        token_delta: i64,
        expected_goal_id: Option<&str>,
    ) -> Result<Option<ThreadGoal>, String> {
        let thread_id = thread_id.to_string();
        let thread_id_for_get = thread_id.clone();
        let expected_goal_id = expected_goal_id.map(str::to_string);
        let now_ms = Utc::now().timestamp_millis();

        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            conn.execute(
                r#"
                UPDATE thread_goals
                SET
                    time_used_seconds = time_used_seconds + ?1,
                    tokens_used = tokens_used + ?2,
                    status = CASE
                        WHEN status = 'active' AND token_budget IS NOT NULL AND (tokens_used + ?2) >= token_budget
                        THEN 'budget_limited'
                        ELSE status
                    END,
                    updated_at_ms = ?3
                WHERE thread_id = ?4
                  AND (?5 IS NULL OR goal_id = ?5)
                  AND status IN ('active', 'budget_limited')
                "#,
                params![time_delta_seconds, token_delta, now_ms, thread_id, expected_goal_id],
            )
            .map_err(|err| format!("account_usage failed: {err}"))?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|err| format!("account_usage task failed: {err}"))??;

        self.get_goal(&thread_id_for_get).await
    }
}

fn thread_goal_from_row(row: &rusqlite::Row) -> Result<ThreadGoal, rusqlite::Error> {
    let created_at_ms: i64 = row.get(7)?;
    let updated_at_ms: i64 = row.get(8)?;
    Ok(ThreadGoal {
        thread_id: row.get(0)?,
        goal_id: row.get(1)?,
        objective: row.get(2)?,
        status: GoalStatus::try_from(row.get::<_, String>(3)?.as_str())
            .map_err(|err| rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, err)),
            ))?,
        token_budget: row.get(4)?,
        tokens_used: row.get(5)?,
        time_used_seconds: row.get(6)?,
        created_at: DateTime::from_timestamp_millis(created_at_ms).unwrap_or_else(Utc::now),
        updated_at: DateTime::from_timestamp_millis(updated_at_ms).unwrap_or_else(Utc::now),
    })
}

fn status_after_budget_limit(
    status: GoalStatus,
    tokens_used: i64,
    token_budget: Option<i64>,
) -> GoalStatus {
    if status == GoalStatus::Active
        && token_budget.is_some_and(|budget| tokens_used >= budget)
    {
        GoalStatus::BudgetLimited
    } else {
        status
    }
}
