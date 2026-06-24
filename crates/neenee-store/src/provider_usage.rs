//! Per-model usage telemetry, persisted under XDG state.
//!
//! Drives recency ordering in the provider picker This is
//! program-generated usage signal, not user preference: it lives under
//! `$XDG_STATE_HOME` next to `history.json`, and losing it only flattens the
//! sort order — never configuration. Favorites and the default-model pointer
//! belong in `config.toml` and are not stored here.
//!
//! The store is a flat map of model id → `UsageEntry`. Ids are stored as
//! given; preset ids are unique and there is no alias mapping.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-model usage record. Stored as a JSON object keyed by canonical model id.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct UsageEntry {
    /// Unix epoch milliseconds of the most recent activation. Milliseconds
    /// (not seconds) so two activations within the same second still order
    /// deterministically rather than colliding.
    last_used_ms: u64,
    /// Total times the model was activated. Kept for future tie-breaking and
    /// "most used" views; not used by the current recency sort.
    use_count: u64,
}

/// The on-disk usage map. Serialized as `provider_usage.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderUsage {
    #[serde(default)]
    entries: HashMap<String, UsageEntry>,
}

impl ProviderUsage {
    /// Load from the well-known state file. Returns an empty store when the
    /// file is missing or unreadable, since the data is fully rebuildable.
    pub fn load() -> Self {
        let path = paths::get().provider_usage_file();
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&content).unwrap_or_default()
    }

    /// Record an activation of `id`. Bumps `last_used_ms` to now, and
    /// increments `use_count`.
    pub fn record(&mut self, id: &str) {
        let now = now_ms();
        let entry = self.entries.entry(id.to_string()).or_default();
        // `now` is monotonic-ish per wall clock; only advance the timestamp so
        // a clock skew backwards does not erase a more recent activation.
        entry.last_used_ms = entry.last_used_ms.max(now);
        entry.use_count = entry.use_count.saturating_add(1);
    }

    /// Persist atomically, merged with whatever another `neenee` instance may
    /// have written since this store was loaded. The merge is per-key and
    /// **commutative**: each model keeps `max(last_used_ms)` and
    /// `max(use_count)` of the in-memory and on-disk values, so two instances
    /// recording concurrently never regress recency or lose an activation
    /// regardless of write order (ADR-0018). The whole reload-merge-write
    /// window is serialised by a companion `flock` so the merge reads a
    /// consistent snapshot.
    ///
    /// Best-effort: callers ignore the result since usage tracking is
    /// non-critical. `use_count` is merged by `max` (not sum) because a sum
    /// would require a per-process baseline that is not tracked; `max` still
    /// preserves recency, which is the only field the picker reads today.
    pub fn save(&self) -> Result<(), String> {
        let path = paths::get().provider_usage_file();
        let _lock = crate::fsutil::FileLock::acquire(&path)
            .map_err(|e| format!("could not lock usage file: {e}"))?;
        // Re-read under the lock so we merge against the latest on-disk state,
        // not the snapshot this process loaded at startup.
        let mut merged = ProviderUsage::load();
        for (id, entry) in &self.entries {
            let disk = merged.entries.entry(id.clone()).or_default();
            disk.last_used_ms = disk.last_used_ms.max(entry.last_used_ms);
            disk.use_count = disk.use_count.max(entry.use_count);
        }
        let bytes = serde_json::to_vec_pretty(&merged).map_err(|e| e.to_string())?;
        crate::fsutil::atomic_write_bytes(&path, &bytes).map_err(|e| e.to_string())
    }

    /// Last-used timestamp (epoch ms) for a model id. `None` when the model
    /// has never been activated, which sorts as "oldest".
    pub fn last_used_ms(&self, id: &str) -> Option<u64> {
        self.entries.get(id).map(|e| e.last_used_ms)
    }

    /// Number of times `id` was activated. `0` for unknown ids.
    ///
    /// Consumed by the provider picker's tie-breaking and future "most used"
    /// views
    #[allow(dead_code)]
    pub fn use_count(&self, id: &str) -> u64 {
        self.entries.get(id).map_or(0, |e| e.use_count)
    }
}

/// Current wall-clock time as Unix epoch milliseconds. Saturates on the
/// far-future overflow, which is irrelevant for sort ordering.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_sets_last_used_and_increments_count() {
        let mut usage = ProviderUsage::default();
        assert_eq!(usage.use_count("gemini"), 0);
        assert!(usage.last_used_ms("gemini").is_none());

        usage.record("gemini");
        assert_eq!(usage.use_count("gemini"), 1);
        let first = usage.last_used_ms("gemini").expect("recorded");

        usage.record("gemini");
        assert_eq!(usage.use_count("gemini"), 2);
        // A second activation never moves the clock backwards.
        assert!(usage.last_used_ms("gemini").unwrap() >= first);
    }

    #[test]
    fn record_stores_id_verbatim() {
        let mut usage = ProviderUsage::default();
        // Ids are stored as given; there is no alias canonicalization.
        usage.record("deepseek-v4-flash");
        assert_eq!(usage.use_count("deepseek-v4-flash"), 1);
        // A stale id does not get merged into the current entry.
        assert_eq!(usage.use_count("deepseek"), 0);
        assert!(usage.last_used_ms("deepseek").is_none());
    }

    #[test]
    fn unknown_id_has_no_last_used_and_zero_count() {
        let usage = ProviderUsage::default();
        assert!(usage.last_used_ms("never-used").is_none());
        assert_eq!(usage.use_count("never-used"), 0);
    }

    #[test]
    fn record_never_moves_clock_backwards() {
        let mut usage = ProviderUsage::default();
        usage.record("glm");
        let real_now = usage.last_used_ms("glm").unwrap();
        // Inject an artificially older timestamp directly, then record again:
        // the real clock must win, not regress toward the stale value.
        usage.entries.get_mut("glm").unwrap().last_used_ms = real_now + 3_600_000;
        usage.record("glm");
        assert!(
            usage.last_used_ms("glm").unwrap() >= real_now + 3_600_000,
            "a newer activation must not be overwritten by an older clock reading"
        );
    }

    #[test]
    fn usage_round_trips_through_json() {
        let mut usage = ProviderUsage::default();
        usage.record("qwen");
        usage.record("qwen");
        usage.record("glm");
        let json = serde_json::to_string(&usage).unwrap();
        let restored: ProviderUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.use_count("qwen"), 2);
        assert_eq!(restored.use_count("glm"), 1);
        assert!(restored.last_used_ms("qwen").is_some());
    }
}
