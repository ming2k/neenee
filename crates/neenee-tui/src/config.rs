//! TUI display configuration sourced from the `[tui]` table of
//! `config.toml` (XDG: `$XDG_CONFIG_HOME/neenee/config.toml`).
//!
//! Today this owns the per-step-kind default expand state: which tool steps
//! and reasoning traces open expanded when they first appear (live or
//! restored), before the user toggles anything with Ctrl+T. An explicit entry
//! overrides a tool's built-in default; unlisted tools keep their built-in
//! behavior (e.g. `edit_file` expands by default, everything else collapses).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::render::tools::presenter_for;

/// Reserved `[tui.default_expanded]` key that controls reasoning traces.
/// Reasoning isn't a tool, so it has no presenter and is addressed by name.
pub const THINKING_KEY: &str = "thinking";

/// User-tunable TUI presentation, deserialized from the optional `[tui]`
/// table of `config.toml`. All fields default sensibly, so a `config.toml`
/// with no `[tui]` table (or a partially specified one) is valid.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    /// Per-step-kind default expand state. Keys are tool names (`edit_file`,
    /// `bash`, …) or [`THINKING_KEY`] for reasoning traces. A tool that is
    /// unlisted here falls back to its built-in presenter default.
    ///
    /// ```toml
    /// [tui.default_expanded]
    /// edit_file = true
    /// bash = true
    /// thinking = false
    /// ```
    pub default_expanded: HashMap<String, bool>,
}

impl TuiConfig {
    /// Effective default-expand state for a tool step. An explicit config
    /// entry wins; otherwise the presenter's built-in default applies.
    pub fn tool_default_expanded(&self, name: &str) -> bool {
        self.default_expanded
            .get(name)
            .copied()
            .unwrap_or_else(|| presenter_for(name).default_expanded())
    }

    /// Effective default-expand state for a reasoning trace. Defaults to
    /// collapsed (`false`) when not configured.
    pub fn thinking_default_expanded(&self) -> bool {
        self.default_expanded.get(THINKING_KEY).copied().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlisted_tool_falls_back_to_presenter_default() {
        let cfg = TuiConfig::default();
        // edit_file has a built-in default of expanded.
        assert!(cfg.tool_default_expanded("edit_file"));
        // bash collapses by default.
        assert!(!cfg.tool_default_expanded("bash"));
    }

    #[test]
    fn explicit_override_wins_over_presenter_default() {
        let mut map = HashMap::new();
        map.insert("edit_file".to_string(), false);
        map.insert("bash".to_string(), true);
        let cfg = TuiConfig {
            default_expanded: map,
        };
        assert!(!cfg.tool_default_expanded("edit_file"));
        assert!(cfg.tool_default_expanded("bash"));
        // Still falls back for unlisted tools.
        assert!(!cfg.tool_default_expanded("read_file"));
    }

    #[test]
    fn thinking_defaults_collapsed_and_is_overridable() {
        assert!(!TuiConfig::default().thinking_default_expanded());
        let mut map = HashMap::new();
        map.insert(THINKING_KEY.to_string(), true);
        let cfg = TuiConfig {
            default_expanded: map,
        };
        assert!(cfg.thinking_default_expanded());
    }

    #[test]
    fn parses_tui_table_from_toml() {
        // When deserialized directly into TuiConfig, the map is the top-level
        // table. In the full config.toml it is nested under [tui.default_expanded].
        let toml = r#"
[default_expanded]
edit_file = true
bash = true
thinking = true
"#;
        let cfg: TuiConfig = toml::from_str(toml).expect("parses");
        assert!(cfg.tool_default_expanded("edit_file"));
        assert!(cfg.tool_default_expanded("bash"));
        assert!(!cfg.tool_default_expanded("read_file"));
        assert!(cfg.thinking_default_expanded());
    }

    #[test]
    fn empty_config_yields_defaults() {
        let cfg: TuiConfig = toml::from_str("").expect("empty parses");
        assert!(cfg.default_expanded.is_empty());
        assert!(cfg.tool_default_expanded("edit_file"));
        assert!(!cfg.thinking_default_expanded());
    }
}
