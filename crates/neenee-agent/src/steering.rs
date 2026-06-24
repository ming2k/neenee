//! Built-in steering nudges (ADR-0030).
//!
//! One home for "condition → one-shot latch → inject a hidden user message"
//! steering. Deliberately separate from the hooks bus (ADR-0025): nudges are
//! harness-internal, always-on, and read `TurnState`, which user-configurable
//! hooks cannot. This mirrors the posture ADR-0025 set for `SessionReview` and
//! `ContextReliefGate` — internal engines stay off the user bus.
//!
//! Stage 1 ships a single concrete nudge ([`LoopingNudge`]). It is kept as a
//! struct, not a `Nudge` trait: with one implementation a trait is pure overhead
//! (the YAGNI bar ADR-0025 applied when deleting the one-shot traits). When
//! `should_nudge_verify` / `should_nudge_todos` migrate in (ADR-0030 Stage 3),
//! the shared shape earns a trait.

use neenee_core::{ReviewStatus, ReviewVerdict};

use crate::agent::TurnState;

/// Collect the detail from any `Stuck` verdict. `Watch` is intentionally
/// excluded: the reviewer's contract is that `Watch` is worth showing the user
/// but not worth steering the model, so it must not trigger a nudge.
pub(crate) fn stuck_detail(verdicts: &[ReviewVerdict]) -> Option<String> {
    let stuck: Vec<&ReviewVerdict> = verdicts
        .iter()
        .filter(|v| v.status == ReviewStatus::Stuck)
        .collect();
    if stuck.is_empty() {
        return None;
    }
    Some(
        stuck
            .iter()
            .map(|v| v.detail.as_str())
            .filter(|d| !d.is_empty())
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// Anti-anchoring nudge for a detected exploration loop (ADR-0030 Stage 1).
///
/// Injected once per turn after the in-loop semantic review returns a `Stuck`
/// verdict. The round-boundary site in `agent.rs` runs `review_now`, stores the
/// verdict detail on `TurnState::loop_signal`, then asks this nudge for its
/// prompt. The one-shot latch is `TurnState::loop_review_fired`, owned by the
/// caller — the nudge itself is stateless.
pub(crate) struct LoopingNudge;

impl LoopingNudge {
    /// The hidden user message. Breaks the self-reinforcing read-only
    /// trajectory by naming the behaviour, forbidding its repetition, and
    /// demanding a forward action — without aborting the turn (the user keeps
    /// `Esc` and the opt-in `hard_stop_rounds` as the hard backstop).
    pub(crate) fn prompt(state: &TurnState) -> String {
        let detail = state.loop_signal.as_deref().unwrap_or_default();
        let notes = if detail.is_empty() {
            String::new()
        } else {
            format!("\n\nReviewer notes: {detail}")
        };
        format!(
            "You are repeating the same or near-identical read-only actions \
             without making a change or converging on an answer. Stop \
             re-reading files, ranges, or queries you have already seen — the \
             information already in context is sufficient. Take a forward \
             action now: make an edit, run a command, or state precisely what \
             is still missing and how you will obtain it. Do not repeat a \
             read unless you can name the exact new fact you expect it to \
             return.{notes}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(status: ReviewStatus, detail: &str) -> ReviewVerdict {
        ReviewVerdict {
            dimension: "looping".into(),
            status,
            detail: detail.into(),
        }
    }

    #[test]
    fn stuck_detail_collects_only_stuck() {
        let v = vec![
            verdict(ReviewStatus::Watch, "w"),
            verdict(ReviewStatus::Stuck, "looping on a.rs"),
        ];
        assert_eq!(stuck_detail(&v).as_deref(), Some("looping on a.rs"));
    }

    #[test]
    fn stuck_detail_none_when_only_watch() {
        let v = vec![verdict(ReviewStatus::Watch, "w")];
        assert!(stuck_detail(&v).is_none());
    }

    #[test]
    fn stuck_detail_none_when_healthy() {
        let v = vec![verdict(ReviewStatus::Healthy, "")];
        assert!(stuck_detail(&v).is_none());
    }

    #[test]
    fn stuck_detail_joins_multiple() {
        let v = vec![
            verdict(ReviewStatus::Stuck, "a"),
            verdict(ReviewStatus::Stuck, "b"),
        ];
        assert_eq!(stuck_detail(&v).as_deref(), Some("a; b"));
    }

    #[test]
    fn nudge_prompt_names_the_loop_and_demands_action() {
        let mut state = TurnState::default();
        state.loop_signal = Some("re-reading src/a.rs".into());
        let p = LoopingNudge::prompt(&state);
        assert!(p.contains("Stop re-reading"));
        assert!(p.contains("forward action"));
        assert!(p.contains("re-reading src/a.rs"));
    }

    #[test]
    fn nudge_prompt_omits_notes_when_empty() {
        let state = TurnState::default();
        let p = LoopingNudge::prompt(&state);
        assert!(!p.contains("Reviewer notes"));
    }
}
