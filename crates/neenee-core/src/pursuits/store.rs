use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use uuid::Uuid;

use super::ThreadPursuit;

/// Schema for the `thread_pursuits` table.
///
/// ADR-0010 slimmed the runtime/persisted pursuit primitive to
/// `objective` + `is_complete` + timestamps. The SQL schema keeps the
/// pre-0010 `token_budget` / `tokens_used` / `time_used_seconds` columns
/// so legacy databases still load cleanly; they are simply never read or
/// written by the new code paths. The `status` TEXT column keeps its
/// shape but only two values are written going forward: `"active"` and
/// `"complete"`. Pre-0010 statuses (`paused`, `blocked`, `usage_limited`,
/// `budget_limited`) are mapped to `"active"` on read.
const THREAD_PURSUITS_SCHEMA: &str = r#"
                CREATE TABLE IF NOT EXISTS thread_pursuits (
                    thread_id TEXT PRIMARY KEY,
                    pursuit_id TEXT NOT NULL,
                    objective TEXT NOT NULL,
                    status TEXT NOT NULL,
                    token_budget INTEGER,
                    tokens_used INTEGER NOT NULL DEFAULT 0,
                    time_used_seconds INTEGER NOT NULL DEFAULT 0,
                    created_at_ms INTEGER NOT NULL,
                    updated_at_ms INTEGER NOT NULL
                )
                "#;

pub struct PursuitStore {
    conn: Arc<std::sync::Mutex<Connection>>,
}

/// Versioned schema migrations for the `thread_pursuits` table.
///
/// Driven by `PRAGMA user_version`: each step only runs when the database is
/// below its target version, runs inside its own transaction, and advances
/// the version only on success. See ADR-0024.
fn migrations() -> &'static [crate::db::Migration] {
    &[
        crate::db::Migration {
            version: 1,
            description: "create thread_pursuits (compat legacy thread_goals table)",
            apply: |conn| {
                if !crate::db::table_exists(conn, "thread_pursuits") {
                    // A database carried over from the pre-rename `goals.db`
                    // still has the `thread_goals` table; rename it so its
                    // rows survive under the new name.
                    if crate::db::table_exists(conn, "thread_goals") {
                        conn.execute(
                            "ALTER TABLE thread_goals RENAME TO thread_pursuits",
                            [],
                        )?;
                    } else {
                        conn.execute(THREAD_PURSUITS_SCHEMA, [])?;
                    }
                }
                Ok(())
            },
        },
        crate::db::Migration {
            version: 2,
            description: "rename legacy goal_id column -> pursuit_id",
            apply: |conn| {
                // Only runs on databases that still carry the pre-rename
                // column. Fresh databases already have `pursuit_id`, so both
                // checks are skipped.
                if !crate::db::column_exists(conn, "thread_pursuits", "pursuit_id")
                    && crate::db::column_exists(conn, "thread_pursuits", "goal_id")
                {
                    conn.execute(
                        "ALTER TABLE thread_pursuits RENAME COLUMN goal_id TO pursuit_id",
                        [],
                    )?;
                }
                Ok(())
            },
        },
    ]
}

impl Clone for PursuitStore {
    fn clone(&self) -> Self {
        Self {
            conn: Arc::clone(&self.conn),
        }
    }
}

impl PursuitStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        // One-time migration from the pre-rename `goals.db`: if the new file
        // does not yet exist but the legacy one does, copy it across so an
        // existing active pursuit survives the rename. The copied table is
        // renamed to `thread_pursuits` in `migrate()`.
        if !path.exists() {
            if let Some(parent) = path.parent() {
                let legacy = parent.join("goals.db");
                if legacy.exists() {
                    let _ = std::fs::copy(&legacy, &path);
                }
            }
        }
        let path_for_task = path;
        let conn = tokio::task::spawn_blocking(move || {
            Connection::open(&path_for_task)
                .map_err(|err| format!("failed to open pursuits db: {err}"))
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
        let mut conn = Connection::open_in_memory()
            .map_err(|err| format!("failed to open in-memory db: {err}"))?;
        crate::db::migrate(&mut conn, migrations())
            .map_err(|err| format!("pursuits migrate failed: {err}"))?;
        Ok(Self {
            conn: Arc::new(std::sync::Mutex::new(conn)),
        })
    }

    async fn migrate(&self) -> Result<(), String> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().map_err(|err| err.to_string())?;
            crate::db::migrate(&mut conn, migrations())
                .map_err(|err| format!("pursuits migrate failed: {err}"))?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|err| format!("migrate task failed: {err}"))?
    }

    pub async fn get_pursuit(&self, thread_id: &str) -> Result<Option<ThreadPursuit>, String> {
        let thread_id = thread_id.to_string();
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            let mut stmt = conn
                .prepare(
                    r#"
                    SELECT
                        thread_id, pursuit_id, objective, status, created_at_ms, updated_at_ms
                    FROM thread_pursuits
                    WHERE thread_id = ?1
                    "#,
                )
                .map_err(|err| format!("prepare get_pursuit failed: {err}"))?;
            let mut rows = stmt
                .query_map([&thread_id], thread_pursuit_from_row)
                .map_err(|err| format!("query get_pursuit failed: {err}"))?;
            rows.next().transpose().map_err(|err| err.to_string())
        })
        .await
        .map_err(|err| format!("get_pursuit task failed: {err}"))?
    }

    /// Replace any existing pursuit with a brand-new active, incomplete pursuit.
    pub async fn replace_pursuit(
        &self,
        thread_id: &str,
        objective: &str,
    ) -> Result<ThreadPursuit, String> {
        let thread_id = thread_id.to_string();
        let thread_id_for_get = thread_id.clone();
        let objective = objective.to_string();
        let now_ms = Utc::now().timestamp_millis();
        let pursuit_id = Uuid::new_v4().to_string();

        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            conn.execute(
                r#"
                INSERT INTO thread_pursuits (
                    thread_id, pursuit_id, objective, status, token_budget,
                    tokens_used, time_used_seconds, created_at_ms, updated_at_ms
                ) VALUES (?1, ?2, ?3, 'active', NULL, 0, 0, ?4, ?4)
                ON CONFLICT(thread_id) DO UPDATE SET
                    pursuit_id = excluded.pursuit_id,
                    objective = excluded.objective,
                    status = excluded.status,
                    token_budget = excluded.token_budget,
                    tokens_used = 0,
                    time_used_seconds = 0,
                    created_at_ms = excluded.created_at_ms,
                    updated_at_ms = excluded.updated_at_ms
                "#,
                params![thread_id, pursuit_id, objective, now_ms],
            )
            .map_err(|err| format!("replace_pursuit failed: {err}"))?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|err| format!("replace_pursuit task failed: {err}"))??;

        self.get_pursuit(&thread_id_for_get)
            .await?
            .ok_or_else(|| "replace_pursuit succeeded but row is missing".to_string())
    }

    /// Rewrite the objective of an existing pursuit. Returns `None` if no row
    /// matches `thread_id`.
    pub async fn update_objective(
        &self,
        thread_id: &str,
        objective: &str,
    ) -> Result<Option<ThreadPursuit>, String> {
        let thread_id = thread_id.to_string();
        let thread_id_for_get = thread_id.clone();
        let objective = objective.to_string();
        let now_ms = Utc::now().timestamp_millis();

        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            conn.execute(
                r#"
                UPDATE thread_pursuits
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

        self.get_pursuit(&thread_id_for_get).await
    }

    /// Flip the pursuit's `is_complete` flag to true. Returns `None` if no row
    /// matches `thread_id` or the pursuit is already complete.
    pub async fn mark_complete(&self, thread_id: &str) -> Result<Option<ThreadPursuit>, String> {
        let thread_id = thread_id.to_string();
        let thread_id_for_get = thread_id.clone();
        let now_ms = Utc::now().timestamp_millis();

        let conn = Arc::clone(&self.conn);
        let affected = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            let rows = conn
                .execute(
                    r#"
                    UPDATE thread_pursuits
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
            return self.get_pursuit(&thread_id_for_get).await;
        }
        self.get_pursuit(&thread_id_for_get).await
    }

    pub async fn delete_pursuit(&self, thread_id: &str) -> Result<Option<ThreadPursuit>, String> {
        let thread_id = thread_id.to_string();
        let pursuit = self.get_pursuit(&thread_id).await?;
        if pursuit.is_none() {
            return Ok(None);
        }

        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            conn.execute(
                "DELETE FROM thread_pursuits WHERE thread_id = ?1",
                [&thread_id],
            )
            .map_err(|err| format!("delete_pursuit failed: {err}"))?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|err| format!("delete_pursuit task failed: {err}"))??;

        Ok(pursuit)
    }
}

fn thread_pursuit_from_row(row: &rusqlite::Row) -> Result<ThreadPursuit, rusqlite::Error> {
    let status: String = row.get(3)?;
    // Post-ADR-0010 only "active" and "complete" are written. Any pre-0010
    // status (paused / blocked / usage_limited / budget_limited) maps to
    // active on read — the user loses paused/blocked state across the
    // upgrade, which is acceptable because those states no longer exist.
    let is_complete = status == "complete";
    let created_at_ms: i64 = row.get(4)?;
    let updated_at_ms: i64 = row.get(5)?;
    Ok(ThreadPursuit {
        thread_id: row.get(0)?,
        pursuit_id: row.get(1)?,
        objective: row.get(2)?,
        is_complete,
        created_at: DateTime::from_timestamp_millis(created_at_ms).unwrap_or_else(Utc::now),
        updated_at: DateTime::from_timestamp_millis(updated_at_ms).unwrap_or_else(Utc::now),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn fresh_db_has_latest_user_version() {
        let store = PursuitStore::open_in_memory_blocking().unwrap();
        let conn = store.conn.lock().unwrap();
        assert_eq!(user_version(&conn), 2);
        assert!(crate::db::table_exists(&conn, "thread_pursuits"));
    }

    #[test]
    fn legacy_thread_goals_table_migrates() {
        // Simulate a pre-rename database: a thread_goals table carrying the
        // old goal_id column plus a row of data.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE thread_goals (
                thread_id TEXT PRIMARY KEY,
                goal_id   TEXT NOT NULL,
                objective TEXT NOT NULL,
                status    TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO thread_goals (thread_id, goal_id, objective, status, created_at_ms, updated_at_ms) \
             VALUES ('t1', 'g1', 'do the thing', 'active', 100, 200)",
            [],
        )
        .unwrap();

        crate::db::migrate(&mut conn, migrations()).unwrap();

        assert_eq!(user_version(&conn), 2);
        assert!(crate::db::table_exists(&conn, "thread_pursuits"));
        assert!(!crate::db::table_exists(&conn, "thread_goals"));
        assert!(crate::db::column_exists(&conn, "thread_pursuits", "pursuit_id"));
        // The legacy row survives the table + column rename.
        let (objective, pursuit_id): (String, String) = conn
            .query_row(
                "SELECT objective, pursuit_id FROM thread_pursuits WHERE thread_id = 't1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(objective, "do the thing");
        assert_eq!(pursuit_id, "g1");
    }

    #[test]
    fn idempotent_migrate() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrate(&mut conn, migrations()).unwrap();
        let v1 = user_version(&conn);
        // Running again against an already-current database is a no-op.
        crate::db::migrate(&mut conn, migrations()).unwrap();
        let v2 = user_version(&conn);
        assert_eq!(v1, v2);
        assert_eq!(v1, 2);
    }
}
