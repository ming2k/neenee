//! Goal-related display and parsing helpers.
//!
//! `load_legacy_goal_from_config` reads the pre-ADR-0010 single-goal config
//! shape so an upgrade never silently drops a user's pinned goal.
//! `format_goal_status` is the textual form surfaced in the TUI for `/goal`.

use neenee_core::Goal;
use neenee_store::config::Config;

/// Read the pre-ADR-0010 `harness_goal*` keys from the config file, if any.
/// Used once at startup to migrate a pinned goal into the new goal store.
pub fn load_legacy_goal_from_config() -> Option<Goal> {
    #[derive(serde::Deserialize)]
    struct LegacyGoal {
        harness_goal: Option<String>,
        #[serde(default)]
        harness_goal_completed: bool,
    }

    let path = Config::config_file_path();
    let content = std::fs::read_to_string(path).ok()?;
    let legacy: LegacyGoal = toml::from_str(&content).ok()?;
    let objective = legacy.harness_goal?;
    Some(Goal {
        objective,
        is_complete: legacy.harness_goal_completed,
    })
}

/// Single textual rendering of a [`Goal`] for `/goal` and exports: state
/// label and objective.
pub fn format_goal_status(goal: &Goal) -> String {
    format!(
        "Goal [{}]: {}",
        if goal.is_complete { "complete" } else { "active" },
        goal.objective
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_status_shows_active_state() {
        let goal = Goal {
            objective: "ship".to_string(),
            is_complete: false,
        };
        let status = format_goal_status(&goal);
        assert!(status.contains("Goal [active]: ship"));
    }

    #[test]
    fn goal_status_shows_complete_state() {
        let goal = Goal {
            objective: "ship".to_string(),
            is_complete: true,
        };
        let status = format_goal_status(&goal);
        assert!(status.contains("Goal [complete]: ship"));
    }
}
