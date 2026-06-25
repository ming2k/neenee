//! `/review` reporting helper extracted from `main.rs`. Renders the verdicts
//! of an on-demand session review as a durable text report for the
//! conversation stream — complements the transient activity-bar alert (which
//! carries only the worst status + details) by listing every dimension.

use neenee_core::{ReviewStatus, ReviewVerdict};

/// Render the verdicts of an on-demand `/review` as a durable text report for
/// the conversation stream. Complements the transient activity-bar alert
/// (which carries only the worst status + details) by listing every dimension
/// with its status label and the reviewer's detail sentence.
pub fn format_review_report(verdicts: &[ReviewVerdict], rounds: usize) -> String {
    let worst = verdicts.iter().map(|v| v.status).max();
    let headline = match worst {
        None => {
            return format!(
                "Session review (~{rounds} tool rounds): no review dimensions registered."
            );
        }
        Some(ReviewStatus::Healthy) => {
            format!("Session review (~{rounds} tool rounds): no concerns found.")
        }
        Some(status) => {
            format!(
                "Session review (~{rounds} tool rounds) — verdict: {}.",
                status.label()
            )
        }
    };
    let mut lines = vec![headline];
    for verdict in verdicts {
        let detail = verdict.detail.trim();
        if detail.is_empty() {
            lines.push(format!(
                "  • {} — {}",
                verdict.dimension,
                verdict.status.label()
            ));
        } else {
            lines.push(format!(
                "  • {} — {}: {}",
                verdict.dimension,
                verdict.status.label(),
                detail
            ));
        }
    }
    lines.push("Interrupt the turn with Esc if it looks stuck.".to_string());
    lines.join("\n")
}
