//! Nudge configuration for the deterministic read-loop guard.
//!
//! [`NudgeConfig`] is the serializable, wire-crossing DTO that governs the
//! read-loop guard's thresholds and master switch. It lives in `neenee-core`
//! (the domain layer) so the harness↔TUI protocol can carry it without a
//! `neenee-store` dependency; `neenee-store::config` re-exports it as the
//! `[principal.nudge]` TOML table, and `neenee-agent` reads it at the round
//! boundary to decide whether and how to nudge.
//!
//! Default is **disabled** — opt in via the `/config` modal or the
//! `[principal.nudge]` sub-table in `config.toml`.

use serde::{Deserialize, Serialize};

/// User-tunable nudge behaviour, deserialized from the `[principal.nudge]`
/// sub-table of `config.toml`. Governs the deterministic read-loop guard
/// (`neenee_agent::loop_guard`): when the model repeats the same read without
/// progress, the guard injects a hidden steering nudge and, if the loop
/// persists, hard-blocks the looping read for the rest of the turn.
///
/// **Default is disabled.** The guard is an opt-in safety net, not a
/// default-on interruption: a model making progress should never see a
/// nudge, and a stuck model has the `abort` tool (the user has `Esc`). Turn
/// it on when you want the harness to break read-loops automatically.
///
/// ```toml
/// [principal.nudge]
/// enabled        = true   # master switch (default false)
/// threshold      = 3      # exact-signature occurrences in window to trip
/// escalate_at    = 6      # escalate Inject -> Block at this count
/// path_threshold = 8      # same-file-many-offsets occurrences to trip
/// window         = 8      # sliding-window size (recent read rounds)
/// ```
///
/// Detection is pure signature bookkeeping (no model call) and the nudge is
/// non-terminating — the hard backstops (`hard_stop_rounds`, `abort`, `Esc`)
/// still cap. Distinct from the on-demand `/review` diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NudgeConfig {
    /// Master switch. `false` (the default) disables both the soft nudge and
    /// the hard block — the read-loop detector does not run. Wired through
    /// `Agent::set_nudge_config`; flipped off for envoys and the `/review`
    /// diagnostic regardless of user setting.
    pub enabled: bool,
    /// Sliding-window size: how many recent read-rounds are considered when
    /// judging whether a signature is recurring. Large enough to span a
    /// `A B A B` thrash, small enough that an old, since-abandoned read ages
    /// out and stops counting. Default `8`.
    pub window: usize,
    /// Exact-signature occurrences within the window that constitute a loop.
    /// Two could be a legitimate "read, glance away, re-read"; three in an
    /// 8-round window is not plausibly productive. Default `3`.
    pub threshold: u32,
    /// Escalation point: if a signature reaches this many occurrences after
    /// the first nudge was already sent, escalate from a soft `Inject` to a
    /// hard `Block` — the read signature is masked for the rest of the turn
    /// so it physically cannot recur. Default `6`.
    pub escalate_at: u32,
    /// Path-bucket threshold: occurrences of the same path bucket
    /// (`name|path`) within the window that constitute a similar-parameter
    /// loop (same file, many offsets). Higher than [`Self::threshold`] so
    /// genuine forward-paging is not mistaken for a loop. Default `8`.
    pub path_threshold: u32,
}

impl NudgeConfig {
    /// A disabled config with default thresholds — the canonical "off" state
    /// used by envoys and the `/review` diagnostic so they run unobstructed
    /// regardless of user settings.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

impl Default for NudgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            window: 8,
            threshold: 3,
            escalate_at: 6,
            path_threshold: 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled() {
        let cfg = NudgeConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.window, 8);
        assert_eq!(cfg.threshold, 3);
        assert_eq!(cfg.escalate_at, 6);
        assert_eq!(cfg.path_threshold, 8);
    }

    #[test]
    fn disabled_helper_keeps_default_thresholds() {
        let off = NudgeConfig::disabled();
        assert!(!off.enabled);
        assert_eq!(off.window, 8);
        assert_eq!(off.threshold, 3);
        assert_eq!(off.escalate_at, 6);
        assert_eq!(off.path_threshold, 8);
    }

    #[test]
    fn round_trips_through_toml() {
        let cfg = NudgeConfig {
            enabled: true,
            window: 12,
            threshold: 5,
            escalate_at: 9,
            path_threshold: 10,
        };
        let s = toml::to_string(&cfg).unwrap();
        let parsed: NudgeConfig = toml::from_str(&s).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn partial_toml_keeps_defaults() {
        // Only `enabled` is set; the rest must fall back to defaults.
        let s = "enabled = true\n";
        let parsed: NudgeConfig = toml::from_str(s).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.window, 8);
        assert_eq!(parsed.threshold, 3);
        assert_eq!(parsed.escalate_at, 6);
        assert_eq!(parsed.path_threshold, 8);
    }
}
