//! TUI presentation policy on top of the data-layer `[tui]` table.
//!
//! The serialisable data struct ([`TuiConfig`]) lives in `neenee-store::config`
//! so every frontend (TUI, future GUI) can read the same `config.toml`
//! without cross-frontend dependencies. This module re-exports that struct
//! for TUI-internal convenience and layers the **presenter-aware** policy on
//! top: how a raw `[tui.default_expanded]` entry combines with each tool's
//! built-in presenter default. That lookup touches `crate::tui::render::tools`,
//! so it cannot live below the TUI.

use crate::tui::render::tools::presenter_for;

pub use neenee_store::config::TuiConfig;

/// Effective default-expand state for a tool step. An explicit config entry
/// wins; otherwise the presenter's built-in default applies.
pub fn tool_default_expanded(config: &TuiConfig, name: &str) -> bool {
    config
        .default_expanded
        .get(name)
        .copied()
        .unwrap_or_else(|| presenter_for(name).default_expanded())
}

/// Effective default-expand state for a reasoning trace. Defaults to
/// collapsed (`false`) when not configured.
pub fn thinking_default_expanded(config: &TuiConfig) -> bool {
    config
        .default_expanded
        .get(neenee_store::config::THINKING_KEY)
        .copied()
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(defaults: &[(&str, bool)]) -> TuiConfig {
        let mut map = HashMap::new();
        for (k, v) in defaults {
            map.insert((*k).to_string(), *v);
        }
        TuiConfig {
            default_expanded: map,
        }
    }

    #[test]
    fn unlisted_tool_falls_back_to_presenter_default() {
        let cfg = TuiConfig::default();
        // edit_file has a built-in default of expanded.
        assert!(tool_default_expanded(&cfg, "edit_file"));
        // bash collapses by default.
        assert!(!tool_default_expanded(&cfg, "bash"));
    }

    #[test]
    fn explicit_override_wins_over_presenter_default() {
        let cfg = config(&[("edit_file", false), ("bash", true)]);
        assert!(!tool_default_expanded(&cfg, "edit_file"));
        assert!(tool_default_expanded(&cfg, "bash"));
        // Still falls back for unlisted tools.
        assert!(!tool_default_expanded(&cfg, "read_file"));
    }

    #[test]
    fn thinking_defaults_collapsed_and_is_overridable() {
        assert!(!thinking_default_expanded(&TuiConfig::default()));
        let cfg = config(&[(neenee_store::config::THINKING_KEY, true)]);
        assert!(thinking_default_expanded(&cfg));
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
        assert!(tool_default_expanded(&cfg, "edit_file"));
        assert!(tool_default_expanded(&cfg, "bash"));
        assert!(!tool_default_expanded(&cfg, "read_file"));
        assert!(thinking_default_expanded(&cfg));
    }

    #[test]
    fn empty_config_yields_defaults() {
        let cfg: TuiConfig = toml::from_str("").expect("empty parses");
        assert!(cfg.default_expanded.is_empty());
        assert!(tool_default_expanded(&cfg, "edit_file"));
        assert!(!thinking_default_expanded(&cfg));
    }
}
