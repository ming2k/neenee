//! Session review: an on-demand, transcript-aware diagnostic (ADR-0018,
//! superseding the periodic round-cadence design of ADR-0016).
//!
//! ## Why this exists
//!
//! ADR-0009 uncapped the agentic loop on purpose: a finite per-turn round cap
//! is an arbitrary budget that trips legitimate long refactors just as readily
//! as a genuinely stuck model. The stall detector that came later walked that
//! back; ADR-0016 replaced it with a periodic diagnostic that fired on a fixed
//! round cadence (every `review_interval_rounds` past `review_start_round`).
//!
//! ADR-0018 drops the automatic cadence entirely. The periodic trigger cost a
//! diagnostic envoy call on *every* long turn — including legitimate ones
//! — and, because ADR-0016 kept the turn uncapped by default, the auto-trigger's
//! value during truly unattended runs was already muted: it could only nudge,
//! never abort, and an alert no one is watching does no good. The user is the
//! best judge of "this feels stuck", so review is now **on-demand**: the
//! `/review` command spawns the same bounded, read-only diagnostic envoy
//! against the live transcript and reports the verdict. No automatic firing,
//! no cadence knobs.
//!
//! The diagnostic stays advisory: it surfaces a visible verdict the user can
//! act on (interrupt with `Esc`) but does **not** abort the turn. A hard stop
//! remains opt-in via `[agent] hard_stop_rounds` (default `0` = off), the only
//! execution cap, preserving ADR-0009's uncapped default posture.
//!
//! ## Extensibility
//!
//! Each dimension is a [`SessionReview`] impl that contributes an instruction
//! fragment. The runner runs a *single* diagnostic envoy per review and
//! asks it to return one verdict per registered dimension, so adding a
//! dimension costs no extra model calls — just a new impl registered on the
//! agent. Dimensions stay in domain vocabulary (this module); the LLM-backed
//! runner that spawns the envoy lives in `neenee-agent` next to `EnvoyTool`.

use serde::{Deserialize, Serialize};

/// Default hard stop for the diagnostic envoy's own turn, so a misbehaving
/// reviewer cannot loop. Applied by the runner when it constructs the reviewer
/// (see `neenee_agent::session_review`).
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
    /// The agent appears stuck (e.g. looping without converging). Surfaced as
    /// the most prominent visible alert so the user can decide to interrupt.
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

/// One dimension of session health, evaluated by the on-demand diagnostic
/// envoy (ADR-0018).
///
/// A dimension is a *prompt fragment*: its [`instruction`](Self::instruction)
/// is appended to the diagnostic's system prompt alongside every other
/// registered dimension, and the runner asks the envoy to return one
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
    /// The instruction the diagnostic envoy evaluates for this dimension.
    /// Phrased as a question the reviewer can answer with a verdict + detail.
    fn instruction(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;

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
