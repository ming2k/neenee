//! Skill-related configuration that can be shared between `neenee-core` and
//! the main `neenee` crate.

use serde::{Deserialize, Serialize};

/// Skill configuration stored under `[skills]` in `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SkillsConfig {
    /// Additional local directories to scan for skills.
    pub paths: Vec<String>,
    /// Remote skill repositories to fetch and cache.
    pub urls: Vec<String>,
    /// Skill names to disable (case-sensitive).
    pub disabled: Vec<String>,
    /// Whether bundled system skills are enabled.
    pub bundled: bool,
}

impl SkillsConfig {
    /// True when no skill configuration is present.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.urls.is_empty() && self.disabled.is_empty() && !self.bundled
    }

    /// True when the given skill name is disabled.
    pub fn is_disabled(&self, name: &str) -> bool {
        self.disabled.iter().any(|n| n == name)
    }
}
