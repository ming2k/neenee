//! Durable store for `/repeat` cron jobs.
//!
//! Each job is a `(cron expression, prompt)` pair plus scheduling timestamps.
//! Jobs survive restarts: the CLI opens this store at startup and re-arms the
//! scheduler with whatever it finds. Jobs auto-expire after
//! [`DEFAULT_MAX_AGE_DAYS`].

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use uuid::Uuid;

/// Recurring jobs auto-expire after this many days (a safety bound so a
/// forgotten `/repeat` does not run forever).
pub const DEFAULT_MAX_AGE_DAYS: i64 = 30;

const SCHEMA: &str = r#"
                CREATE TABLE IF NOT EXISTS repeat_jobs (
                    id TEXT PRIMARY KEY,
                    cron TEXT NOT NULL,
                    prompt TEXT NOT NULL,
                    created_at_ms INTEGER NOT NULL,
                    next_fire_ms INTEGER NOT NULL,
                    last_fire_ms INTEGER
                )
                "#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatJob {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    pub created_at: DateTime<Utc>,
    pub next_fire: DateTime<Utc>,
    pub last_fire: Option<DateTime<Utc>>,
}

pub struct RepeatStore {
    conn: Arc<std::sync::Mutex<Connection>>,
}

impl Clone for RepeatStore {
    fn clone(&self) -> Self {
        Self {
            conn: Arc::clone(&self.conn),
        }
    }
}

impl RepeatStore {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let conn = tokio::task::spawn_blocking(move || {
            Connection::open(&path).map_err(|err| format!("failed to open repeat db: {err}"))
        })
        .await
        .map_err(|err| format!("db open task failed: {err}"))??;
        let store = Self {
            conn: Arc::new(std::sync::Mutex::new(conn)),
        };
        store.migrate().await?;
        Ok(store)
    }

    /// Synchronous in-memory constructor for tests run outside an async context.
    pub fn open_in_memory_blocking() -> Result<Self, String> {
        let conn = Connection::open_in_memory()
            .map_err(|err| format!("failed to open in-memory db: {err}"))?;
        conn.execute(SCHEMA, [])
            .map_err(|err| format!("failed to create repeat_jobs table: {err}"))?;
        Ok(Self {
            conn: Arc::new(std::sync::Mutex::new(conn)),
        })
    }

    async fn migrate(&self) -> Result<(), String> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            conn.execute(SCHEMA, [])
                .map_err(|err| format!("failed to create repeat_jobs table: {err}"))?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|err| format!("migrate task failed: {err}"))?
    }

    /// Insert a new job with a precomputed first `next_fire`.
    pub async fn add(
        &self,
        cron: &str,
        prompt: &str,
        next_fire: DateTime<Utc>,
    ) -> Result<RepeatJob, String> {
        let id = Uuid::new_v4().to_string();
        let created_ms = Utc::now().timestamp_millis();
        let next_ms = next_fire.timestamp_millis();
        let job = RepeatJob {
            id: id.clone(),
            cron: cron.to_string(),
            prompt: prompt.to_string(),
            created_at: DateTime::from_timestamp_millis(created_ms).unwrap_or_else(Utc::now),
            next_fire,
            last_fire: None,
        };
        let conn = Arc::clone(&self.conn);
        let cron = cron.to_string();
        let prompt = prompt.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            conn.execute(
                "INSERT INTO repeat_jobs (id, cron, prompt, created_at_ms, next_fire_ms, last_fire_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
                params![id, cron, prompt, created_ms, next_ms],
            )
            .map_err(|err| format!("add repeat job failed: {err}"))?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|err| format!("add task failed: {err}"))??;
        Ok(job)
    }

    pub async fn list(&self) -> Result<Vec<RepeatJob>, String> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            let mut stmt = conn
                .prepare(
                    "SELECT id, cron, prompt, created_at_ms, next_fire_ms, last_fire_ms \
                     FROM repeat_jobs ORDER BY next_fire_ms ASC",
                )
                .map_err(|err| format!("prepare list failed: {err}"))?;
            let rows = stmt
                .query_map([], job_from_row)
                .map_err(|err| format!("query list failed: {err}"))?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
        })
        .await
        .map_err(|err| format!("list task failed: {err}"))?
    }

    /// Jobs whose `next_fire` is at or before `now`.
    pub async fn due(&self, now: DateTime<Utc>) -> Result<Vec<RepeatJob>, String> {
        let now_ms = now.timestamp_millis();
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            let mut stmt = conn
                .prepare(
                    "SELECT id, cron, prompt, created_at_ms, next_fire_ms, last_fire_ms \
                     FROM repeat_jobs WHERE next_fire_ms <= ?1 ORDER BY next_fire_ms ASC",
                )
                .map_err(|err| format!("prepare due failed: {err}"))?;
            let rows = stmt
                .query_map(params![now_ms], job_from_row)
                .map_err(|err| format!("query due failed: {err}"))?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
        })
        .await
        .map_err(|err| format!("due task failed: {err}"))?
    }

    /// Stamp `last_fire = now` and advance `next_fire` after a fire.
    pub async fn mark_fired(
        &self,
        id: &str,
        now: DateTime<Utc>,
        next_fire: DateTime<Utc>,
    ) -> Result<(), String> {
        let id = id.to_string();
        let now_ms = now.timestamp_millis();
        let next_ms = next_fire.timestamp_millis();
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            conn.execute(
                "UPDATE repeat_jobs SET last_fire_ms = ?1, next_fire_ms = ?2 WHERE id = ?3",
                params![now_ms, next_ms, id],
            )
            .map_err(|err| format!("mark_fired failed: {err}"))?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|err| format!("mark_fired task failed: {err}"))?
    }

    pub async fn delete(&self, id: &str) -> Result<bool, String> {
        let id = id.to_string();
        let conn = Arc::clone(&self.conn);
        let removed = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            let rows = conn
                .execute("DELETE FROM repeat_jobs WHERE id = ?1", params![id])
                .map_err(|err| format!("delete repeat job failed: {err}"))?;
            Ok::<_, String>(rows)
        })
        .await
        .map_err(|err| format!("delete task failed: {err}"))??;
        Ok(removed > 0)
    }

    /// Delete jobs older than [`DEFAULT_MAX_AGE_DAYS`]. Returns the count removed.
    pub async fn prune_expired(&self, now: DateTime<Utc>) -> Result<usize, String> {
        let cutoff_ms = (now - chrono::Duration::days(DEFAULT_MAX_AGE_DAYS)).timestamp_millis();
        let conn = Arc::clone(&self.conn);
        let removed = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().map_err(|err| err.to_string())?;
            let rows = conn
                .execute(
                    "DELETE FROM repeat_jobs WHERE created_at_ms < ?1",
                    params![cutoff_ms],
                )
                .map_err(|err| format!("prune expired failed: {err}"))?;
            Ok::<_, String>(rows)
        })
        .await
        .map_err(|err| format!("prune task failed: {err}"))??;
        Ok(removed)
    }
}

fn job_from_row(row: &rusqlite::Row) -> Result<RepeatJob, rusqlite::Error> {
    let id: String = row.get(0)?;
    let cron: String = row.get(1)?;
    let prompt: String = row.get(2)?;
    let created_at_ms: i64 = row.get(3)?;
    let next_fire_ms: i64 = row.get(4)?;
    let last_fire_ms: Option<i64> = row.get(5)?;
    Ok(RepeatJob {
        id,
        cron,
        prompt,
        created_at: DateTime::from_timestamp_millis(created_at_ms).unwrap_or_else(Utc::now),
        next_fire: DateTime::from_timestamp_millis(next_fire_ms).unwrap_or_else(Utc::now),
        last_fire: last_fire_ms.and_then(DateTime::from_timestamp_millis),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn store() -> RepeatStore {
        RepeatStore::open_in_memory_blocking().unwrap()
    }

    #[tokio::test]
    async fn add_list_due_lifecycle() {
        let store = store();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let job = store
            .add("*/5 * * * *", "check the deploy", now)
            .await
            .unwrap();
        assert_eq!(job.cron, "*/5 * * * *");
        assert!(job.last_fire.is_none());

        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);

        // Due at `now`: next_fire == now is inclusive.
        let due = store.due(now).await.unwrap();
        assert_eq!(due.len(), 1);

        // Advance: mark fired, next fire in the future.
        store
            .mark_fired(&job.id, now, now + chrono::Duration::minutes(5))
            .await
            .unwrap();
        let due_after = store.due(now).await.unwrap();
        assert!(due_after.is_empty());

        let fetched = store.list().await.unwrap();
        assert!(fetched[0].last_fire.is_some());
    }

    #[tokio::test]
    async fn delete_removes_a_job() {
        let store = store();
        let now = Utc::now();
        let job = store.add("* * * * *", "p", now).await.unwrap();
        assert!(store.delete(&job.id).await.unwrap());
        assert!(!store.delete(&job.id).await.unwrap());
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn prune_expired_removes_old_jobs_only() {
        let store = store();
        let now = Utc::now();
        let fresh = store.add("* * * * *", "fresh", now).await.unwrap();
        // Simulate an old job by backdating created_at via direct SQL.
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO repeat_jobs (id, cron, prompt, created_at_ms, next_fire_ms, last_fire_ms) \
                 VALUES ('old', '* * * * *', 'old', ?1, ?2, NULL)",
                params![
                    (now - chrono::Duration::days(DEFAULT_MAX_AGE_DAYS + 1)).timestamp_millis(),
                    now.timestamp_millis()
                ],
            )
            .unwrap();
        }
        let removed = store.prune_expired(now).await.unwrap();
        assert_eq!(removed, 1);
        let remaining: Vec<String> = store.list().await.unwrap().into_iter().map(|j| j.id).collect();
        assert!(remaining.contains(&fresh.id));
        assert!(!remaining.contains(&"old".to_string()));
    }
}
