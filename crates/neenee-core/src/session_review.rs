//! Session review: a periodic, transcript-aware diagnostic that replaces the
//! old round-counting stall detector (ADR-0016).
//!
//! ## Why this exists
//!
//! ADR-0009 uncapped the agentic loop on purpose: a finite per-turn round cap
//! is an arbitrary budget that trips legitimate long refactors just as readily
//! as a genuinely stuck model, and the compaction backstop plus user interrupt
//! are the right shape for keeping an unbounded loop bounded. The read-only
//! "stall detector" that came later (a hidden reflection nudge at 8 read-only
//! rounds, a hard abort at 14) walked that decision back — it re-introduced an
//! arbitrary round ceiling, and worse, its signal ("no write tool fired") is a
//! poor proxy for "stuck": a model methodically reading its way through a large
//! codebase before a refactor is *correctly* read-only for many rounds.
//!
//! This module replaces that heuristic with a smarter, cheaper-by-frequency
//! mechanism: after a generous round budget (`review_start_round`), every
//! `review_interval_rounds` the harness spawns a bounded, read-only diagnostic
//! sub-agent that actually *reads* the live transcript and renders a verdict
//! across one or more pluggable dimensions. "Is the agent looping?" is the
//! first dimension; future dimensions (context bloat, tool-error storms, plan
//! drift, …) slot in by implementing [`SessionReview`] without touching the
//! dispatch path.
//!
//! The diagnostic is advisory: it surfaces a visible alert (and, on a "stuck"
//! verdict, a one-shot reflection nudge so the model gets a chance to recover)
//! but does **not** abort the turn by default. A hard stop is opt-in via
//! `hard_stop_rounds` (default `0` = off), restoring ADR-0009's default
//! posture exactly.
//!
//! ## Extensibility
//!
//! Each dimension is a [`SessionReview`] impl that contributes an instruction
//! fragment. The runner runs a *single* diagnostic sub-agent per review point
//! and asks it to return one verdict per registered dimension, so adding a
//! dimension costs no extra model calls — just a new impl registered on the
//! agent. Dimensions stay in domain vocabulary (this module); the LLM-backed
//! runner that spawns the sub-agent lives in `neenee-agent` next to `TaskTool`.

use serde::{Deserialize, Serialize};

/// Cadence and budget knobs for session review, seeded from
/// `[agent]` in `config.toml` and mutable at runtime.
///
/// `review_start_round = 0` disables the periodic diagnostic entirely (pure
/// ADR-0009 behaviour: uncapped, no review, no alert). A finite
/// `hard_stop_rounds` is an explicit, opt-in execution budget; `0` (the
/// default) means the turn runs until the model stops, the user interrupts,
/// or context compaction cannot relieve pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewConfig {
    /// Total tool rounds that must elapse in a turn before the first periodic
    /// review fires. `0` disables review entirely. Chosen large enough that a
    /// normal turn never pays for a diagnostic; a long-running turn that might
    /// be stuck starts getting checked in.
    pub review_start_round: usize,
    /// Interval between review runs once `review_start_round` has passed.
    /// Must be `>= 1` when review is enabled; a smaller value checks more
    /// often at higher token cost.
    pub review_interval_rounds: usize,
    /// Hard-stop the turn after this many *total* tool rounds. `0` (default)
    /// means no hard stop. This is the only opt-in execution cap, kept as an
    /// escape hatch for users who want an explicit budget — the default
    /// matches ADR-0009's uncapped posture.
    pub hard_stop_rounds: usize,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            review_start_round: DEFAULT_REVIEW_START_ROUND,
            review_interval_rounds: DEFAULT_REVIEW_INTERVAL_ROUNDS,
            hard_stop_rounds: 0,
        }
    }
}

impl ReviewConfig {
    /// Pure ADR-0009 posture: no review, no hard stop. Bound for autonomous
    /// sub-agents (TaskTool) and the diagnostic sub-agent itself, so review
    /// never recurses and short-lived sub-agents never pay for a diagnostic.
    pub const fn disabled() -> Self {
        Self {
            review_start_round: 0,
            review_interval_rounds: DEFAULT_REVIEW_INTERVAL_ROUNDS,
            hard_stop_rounds: 0,
        }
    }

    /// Bound for the diagnostic sub-agent itself: review off (it is the
    /// reviewer) plus a tight hard stop so a runaway diagnostic cannot loop.
    /// The diagnostic only reasons over a handed-off transcript, so a small
    /// round budget is ample.
    pub const fn for_reviewer() -> Self {
        Self {
            review_start_round: 0,
            review_interval_rounds: DEFAULT_REVIEW_INTERVAL_ROUNDS,
            hard_stop_rounds: DEFAULT_REVIEWER_HARD_STOP,
        }
    }

    /// Whether the periodic diagnostic is enabled (`review_start_round > 0`).
    pub fn review_enabled(&self) -> bool {
        self.review_start_round > 0
    }

    /// Whether a review should fire after exactly `rounds` tool rounds this
    /// turn. True when review is enabled and `rounds` lands on a review point
    /// (`start`, `start + interval`, `start + 2*interval`, …). Returns `false`
    /// before the start line and on every round in between.
    pub fn review_due_at(&self, rounds: usize) -> bool {
        if !self.review_enabled() || self.review_interval_rounds == 0 {
            return false;
        }
        if rounds < self.review_start_round {
            return false;
        }
        let offset = rounds - self.review_start_round;
        offset.is_multiple_of(self.review_interval_rounds)
    }
}

/// Default for [`ReviewConfig::review_start_round`]. Large enough that an
/// ordinary turn (explore + edit + verify + update) never triggers a
/// diagnostic; only genuinely long turns start getting checked.
pub const DEFAULT_REVIEW_START_ROUND: usize = 64;

/// Default for [`ReviewConfig::review_interval_rounds`]. The window between
/// checks balances signal freshness against diagnostic token cost.
pub const DEFAULT_REVIEW_INTERVAL_ROUNDS: usize = 16;

/// Default hard stop for the diagnostic sub-agent's own turn, so a misbehaving
/// reviewer cannot loop. See [`ReviewConfig::for_reviewer`].
pub const DEFAULT_REVIEWER_HARD_STOP: usize = 12;

/// The outcome of one review dimension.
///
/// `detail` is the diagnostic's own explanation, surfaced verbatim in the TUI
/// alert so the user can judge whether to interrupt. Kept free-form because
/// the valuable signal is the reviewer's reasoning, not a rigid schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewVerdict {
    /// Matches the [`SessionReview::id`] this verdict answers.
    pub dimension: String,
    pub status: ReviewStatus,
    pub detail: String,
}

impl ReviewVerdict {
    /// A single healthy verdict with no detail — the "nothing to report"
    /// sentinel used when the diagnostic succeeds but finds no concern.
    pub fn healthy(dimension: &str) -> Self {
        Self {
            dimension: dimension.to_string(),
            status: ReviewStatus::Healthy,
            detail: String::new(),
        }
    }
}

/// The diagnostic's judgement for a dimension.
///
/// Ordered so that the worst verdict across dimensions wins when the runner
/// collapses the set into one alert (`Stuck` dominates `Watch` dominates
/// `Healthy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReviewStatus {
    /// No concern detected. Clears any prior alert.
    Healthy,
    /// The agent is progressing but slowly, repetitively, or riskily — worth
    /// showing the user but not worth nudging the model. Surfaced as a
    /// visible, non-alarming alert.
    Watch,
    /// The agent appears stuck (e.g. looping without converging). Triggers a
    /// one-shot reflection nudge so the model gets a chance to recover, plus
    /// the visible alert.
    Stuck,
}

impl ReviewStatus {
    /// One short human-facing word for the alert, lowercased.
    pub fn label(self) -> &'static str {
        match self {
            ReviewStatus::Healthy => "ok",
            ReviewStatus::Watch => "watch",
            ReviewStatus::Stuck => "stuck",
        }
    }
}

/// One dimension of session health, evaluated by the periodic diagnostic
/// sub-agent (ADR-0016).
///
/// A dimension is a *prompt fragment*: its [`instruction`](Self::instruction)
/// is appended to the diagnostic's system prompt alongside every other
/// registered dimension, and the runner asks the sub-agent to return one
/// JSON verdict per dimension. This keeps adding a dimension cheap (no extra
/// model call) and keeps dimension logic out of the dispatch path.
///
/// Built-in dimensions live in `neenee-agent` (the first is `LoopingReview`);
/// the trait itself stays here in domain vocabulary so the runner resolves
/// dimensions without re-implementing them.
pub trait SessionReview: Send + Sync + std::fmt::Debug {
    /// Stable, machine-friendly id (e.g. `"looping"`). Used to key the
    /// returned verdict back to the dimension.
    fn id(&self) -> &'static str;
    /// Short human-facing label for the TUI alert (e.g. `"Exploration loop"`).
    fn label(&self) -> &'static str;
    /// The instruction the diagnostic sub-agent evaluates for this dimension.
    /// Phrased as a question the reviewer can answer with a verdict + detail.
    fn instruction(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_due_at_lands_on_review_points_only() {
        let cfg = ReviewConfig {
            review_start_round: 64,
            review_interval_rounds: 16,
            hard_stop_rounds: 0,
        };
        // Before the start line: never due.
        for r in 0..64 {
            assert!(
                !cfg.review_due_at(r),
                "round {r} before start must not be due"
            );
        }
        // On the start line and every interval thereafter.
        assert!(cfg.review_due_at(64));
        assert!(!cfg.review_due_at(65));
        assert!(cfg.review_due_at(80));
        assert!(cfg.review_due_at(96));
        assert!(!cfg.review_due_at(100));
    }

    #[test]
    fn review_disabled_when_start_is_zero() {
        let cfg = ReviewConfig::disabled();
        assert!(!cfg.review_enabled());
        // No round ever triggers a review when disabled.
        assert!(!cfg.review_due_at(0));
        assert!(!cfg.review_due_at(1000));
    }

    #[test]
    fn status_ordering_makes_stuck_dominate() {
        assert!(ReviewStatus::Stuck > ReviewStatus::Watch);
        assert!(ReviewStatus::Watch > ReviewStatus::Healthy);
        let worst = [
            ReviewStatus::Healthy,
            ReviewStatus::Stuck,
            ReviewStatus::Watch,
        ]
        .into_iter()
        .max()
        .unwrap();
        assert_eq!(worst, ReviewStatus::Stuck);
    }
}
