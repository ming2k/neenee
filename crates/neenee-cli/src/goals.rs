//! Goal-related display and parsing helpers.
//!
//! `load_legacy_goal_from_config` reads the pre-ADR-0010 single-goal config
//! shape so an upgrade never silently drops a user's pinned goal.
//! `format_goal_status` is the textual form surfaced in the TUI for `/goal`.

use neenee_core::{Goal, GoalChecklistItem, GoalChecklistStatus};
use neenee_store::config::Config;

/// Read the pre-ADR-0010 `harness_goal*` keys from the config file, if any.
/// Used once at startup to migrate a pinned goal into the new goal store.
pub fn load_legacy_goal_from_config() -> Option<Goal> {
    #[derive(serde::Deserialize)]
    struct LegacyGoal {
        harness_goal: Option<String>,
        #[serde(default)]
        harness_goal_completed: bool,
        #[serde(default)]
        harness_goal_checklist: Vec<GoalChecklistItem>,
    }

    let path = Config::config_file_path();
    let content = std::fs::read_to_string(path).ok()?;
    let legacy: LegacyGoal = toml::from_str(&content).ok()?;
    let objective = legacy.harness_goal?;
    Some(Goal {
        objective,
        is_complete: legacy.harness_goal_completed,
        checklist: legacy.harness_goal_checklist,
    })
}

/// Single textual rendering of a [`Goal`] for `/goal` and exports. Post-ADR-0010
/// the rendering is intentionally minimal: state label, objective, and the
/// checklist (no budget bar, no time line).
pub fn format_goal_status(goal: &Goal) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "Goal [{}]: {}",
        if goal.is_complete {
            "complete"
        } else {
            "active"
        },
        goal.objective
    ));

    if !goal.checklist.is_empty() {
        let total = goal.checklist.len();
        let done = goal
            .checklist
            .iter()
            .filter(|item| {
                matches!(
                    item.status,
                    GoalChecklistStatus::Completed | GoalChecklistStatus::Cancelled
                )
            })
            .count();
        lines.push(String::new());
        lines.push(format!("Plans ({done}/{total}):"));
        for item in &goal.checklist {
            let (glyph, label) = match item.status {
                GoalChecklistStatus::Completed => ("✓", "done"),
                GoalChecklistStatus::Cancelled => ("✗", "cancelled"),
                GoalChecklistStatus::InProgress => ("◎", "in progress"),
                GoalChecklistStatus::Pending => ("○", "pending"),
            };
            lines.push(format!(
                "  {glyph} {content}  ({label})",
                content = item.content
            ));
        }
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_status_includes_structured_checklist() {
        let goal = Goal {
            objective: "ship".to_string(),
            is_complete: false,
            checklist: vec![GoalChecklistItem {
                content: "verify".to_string(),
                status: GoalChecklistStatus::InProgress,
            }],
        };

        let status = format_goal_status(&goal);
        assert!(status.contains("Goal [active]: ship"));
        assert!(status.contains("Plans (0/1):"));
        assert!(status.contains("◎ verify  (in progress)"));
    }

    #[test]
    fn goal_status_shows_complete_state() {
        // Post-ADR-0010: no budget bar, no time line. The state label is
        // the only thing beyond objective + checklist.
        let goal = Goal {
            objective: "ship".to_string(),
            is_complete: true,
            checklist: Vec::new(),
        };
        let status = format_goal_status(&goal);
        assert!(status.contains("Goal [complete]: ship"));
        assert!(!status.contains("Budget"));
        assert!(!status.contains("tokens"));
        assert!(!status.contains("time"));
    }
}
