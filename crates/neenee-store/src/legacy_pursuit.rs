//! One-shot legacy pursuit migration helper (ADR-0032).
//!
//! Before ADR-0032, per-session pursuits lived in a standalone SQLite database
//! (`pursuits.db`, table `thread_pursuits`, keyed by session id). ADR-0032
//! folded pursuit persistence into `SessionStore` (`SessionData.pursuit`). This
//! module reads the old database once at startup so an existing active pursuit
//! survives the upgrade. The file is left on disk (a downgrade can recover it),
//! but it is never read again after the first successful migration.
//!
//! This is intentionally a standalone function with no dependency on the rest
//! of the (deleted) `pursuits` module: it opens a raw `rusqlite` connection,
//! runs one query, and returns the row for the given session id.

use neenee_core::Pursuit;
use rusqlite::Connection;
use std::path::Path;

/// Read the legacy `thread_pursuits` row for `session_id`, if any.
///
/// Returns `None` when the database file does not exist, the table is absent,
/// or no row matches. Pre-0010 statuses (`paused`, `blocked`, `usage_limited`,
/// `budget_limited`) are mapped to `is_complete = false` (active), matching the
/// ADR-0010 read-time mapping the old store used.
pub fn read_legacy_pursuit(db_path: &Path, session_id: &str) -> Option<Pursuit> {
    if !db_path.exists() {
        return None;
    }
    let conn = Connection::open(db_path).ok()?;
    // The table may be `thread_pursuits` (post-rename) or `thread_goals`
    // (pre-rename). Try the new name first, fall back to the old one.
    let row = conn
        .query_row(
            "SELECT objective, status FROM thread_pursuits WHERE thread_id = ?1",
            rusqlite::params![session_id],
            |row| {
                let objective: String = row.get(0)?;
                let status: String = row.get(1)?;
                Ok((objective, status))
            },
        )
        .or_else(|_| {
            conn.query_row(
                "SELECT objective, status FROM thread_goals WHERE thread_id = ?1",
                rusqlite::params![session_id],
                |row| {
                    let objective: String = row.get(0)?;
                    let status: String = row.get(1)?;
                    Ok((objective, status))
                },
            )
        })
        .ok()?;
    let (objective, status) = row;
    Some(Pursuit {
        objective,
        is_complete: status == "complete",
    })
}
