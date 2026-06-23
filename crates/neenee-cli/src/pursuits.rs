//! Pursuit-related display and parsing helpers.
//!
//! `load_legacy_pursuit_from_config` reads the pre-ADR-0010 single-pursuit config
//! shape so an upgrade never silently drops a user's pinned pursuit.
//! `format_pursuit_status` is the textual form surfaced in the TUI for `/pursuit`.

use neenee_core::Pursuit;
use neenee_store::config::Config;

/// Read the pre-ADR-0010 `harness_goal*` keys from the config file, if any.
/// Used once at startup to migrate a pinned pursuit into the new pursuit store.
pub fn load_legacy_pursuit_from_config() -> Option<Pursuit> {
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
    Some(Pursuit {
        objective,
        is_complete: legacy.harness_goal_completed,
    })
}

/// Single textual rendering of a [`Pursuit`] for `/pursuit` and exports: state
/// label and objective.
pub fn format_pursuit_status(pursuit: &Pursuit) -> String {
    format!(
        "Pursuit [{}]: {}",
        if pursuit.is_complete { "complete" } else { "active" },
        pursuit.objective
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_status_shows_active_state() {
        let pursuit = Pursuit {
            objective: "ship".to_string(),
            is_complete: false,
        };
        let status = format_pursuit_status(&pursuit);
        assert!(status.contains("Pursuit [active]: ship"));
    }

    #[test]
    fn goal_status_shows_complete_state() {
        let pursuit = Pursuit {
            objective: "ship".to_string(),
            is_complete: true,
        };
        let status = format_pursuit_status(&pursuit);
        assert!(status.contains("Pursuit [complete]: ship"));
    }
}
