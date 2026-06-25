//! Value types for `/repeat` cron jobs.
//!
//! The durable `RepeatStore` (the `rusqlite`-backed persistence) lives in
//! `neenee-store::repeat`. This module holds only the pure domain types a
//! `RepeatJob` carries, so `neenee-core` stays free of I/O (ADR-0005).

use chrono::{DateTime, Utc};

/// Recurring jobs auto-expire after this many days (a safety bound so a
/// forgotten `/repeat` does not run forever).
pub const DEFAULT_MAX_AGE_DAYS: i64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepeatJob {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    pub created_at: DateTime<Utc>,
    pub next_fire: DateTime<Utc>,
    pub last_fire: Option<DateTime<Utc>>,
}
