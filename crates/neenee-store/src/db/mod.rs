//! Pragmatic, dependency-free SQLite schema migrations built on the
//! [`PRAGMA user_version`](https://www.sqlite.org/pragma.html#pragma_user_version)
//! mechanism that ships with SQLite itself.
//!
//! Each store owns a `&'static [Migration]` list and calls [`migrate`] on
//! every connection open. Migrations are version-number driven (never
//! "probe by swallowing errors"), transactional per step, and idempotent:
//! re-running against an already-current database is a no-op.
//!
//! This module deliberately avoids external crates such as `refinery` or
//! `sqlx`. For an embedded, single-binary application with a handful of
//! tables the native pragma is sufficient and keeps the dependency surface
//! minimal (see ADR-0024).

use rusqlite::Connection;
use rusqlite::params;

/// Returns `true` if a table named `name` exists in `conn`'s schema.
///
/// Used by migrations to decide whether to run DDL idempotently instead of
/// relying on swallowed errors from `ALTER TABLE`.
pub fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        params![name],
        |_| Ok(()),
    )
    .is_ok()
}

/// Returns `true` if column `col` exists on `table`.
///
/// `PRAGMA table_info` does not accept bound parameters, but `table` is
/// always a compile-time literal supplied by the migration code, so there is
/// no injection surface.
pub fn column_exists(conn: &Connection, table: &str, col: &str) -> bool {
    let sql = format!("PRAGMA table_info({table})");
    conn.prepare(&sql)
        .and_then(|mut stmt| {
            let names: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(Result::ok)
                .collect();
            Ok(names.iter().any(|name| name == col))
        })
        .unwrap_or(false)
}

/// One schema migration step.
///
/// `apply` runs inside a transaction opened by [`migrate`]. On success the
/// `user_version` pragma is advanced to `version`; on error the transaction
/// rolls back and the error propagates, leaving `user_version` untouched so
/// the next open retries the same step.
pub struct Migration {
    /// Monotonically increasing target version this step produces.
    pub version: i64,
    /// Human-readable description, surfaced in migration logs.
    pub description: &'static str,
    /// The DDL/DML to run. A plain function pointer keeps migration lists
    /// as compile-time constant arrays with no heap allocation.
    pub apply: fn(&Connection) -> Result<(), rusqlite::Error>,
}

/// Apply every migration in `migrations` whose `version` is greater than the
/// database's current `user_version`, in ascending order.
///
/// Each step runs in its own transaction:
/// 1. `apply(&tx)` performs the schema change.
/// 2. `PRAGMA user_version = N` records the new version.
/// 3. The transaction commits.
///
/// A failure in step 1 rolls back the schema change and leaves `user_version`
/// unchanged, so reopening the database retries the same step idempotently.
/// Databases already at or above the highest migration version are a no-op.
pub fn migrate(conn: &mut Connection, migrations: &[Migration]) -> Result<(), rusqlite::Error> {
    let mut current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    for step in migrations {
        if step.version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        (step.apply)(&tx)?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", step.version))?;
        tx.commit()?;
        tracing::info!(
            version = step.version,
            desc = step.description,
            "db migrated"
        );
        current = step.version;
    }
    Ok(())
}
