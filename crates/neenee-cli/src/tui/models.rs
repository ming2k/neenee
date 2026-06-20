//! Static model catalog + the `/models` picker's shared filter/sort logic.
//!
//! `SOLUTIONS` is the TUI-side mirror of the providers the harness knows how
//! to drive: it carries the display name, the default model id, the
//! context-window size (for the header usage indicator), and a one-line
//! description. The live per-user state (favorite, key-ready, last-used)
//! arrives via [`ModelPickerSnapshot`]; [`models_filtered_from`] joins the two
//! to render and navigate the picker.
//!
//! [`ModelPickerSnapshot`]: neenee_core::ModelPickerSnapshot

use neenee_core::ModelPickerRow;

use crate::tui::fuzzy;

#[derive(Clone, Copy)]
pub(crate) struct ModelSolution {
    pub id: &'static str,
    pub name: &'static str,
    pub model: &'static str,
    pub description: &'static str,
    /// Model context window in tokens, used by the header context-usage
    /// indicator. `0` means "unknown" (custom / local / mock), which hides the
    /// indicator rather than showing a meaningless fill level.
    pub context_window: usize,
}

/// Look up the context window (in tokens) for a provider preset id. Returns `0`
/// when the provider is unknown or has no fixed window.
pub(crate) fn model_context_window(provider: &str) -> usize {
    SOLUTIONS
        .iter()
        .find(|s| s.id == provider)
        .map(|s| s.context_window)
        .unwrap_or(0)
}

/// Human-readable name for the hint bar / status surfaces.
///
/// The picker already shows the friendly preset name (e.g. `DeepSeek V4 Pro`);
/// the hint bar used to render the raw model id instead (`deepseek-v4-pro`),
/// which is an opaque internal identifier the user never typed and can't map
/// back to a product. Resolve the preset's display name from the provider id so
/// the always-visible indicator matches the picker, and fall back to the raw
/// model id only for providers that aren't built-in presets (custom / local /
/// mock), where the model id is the only label the user has.
pub(crate) fn model_display_name(provider: &str, model: &str) -> String {
    SOLUTIONS
        .iter()
        .find(|s| s.id == provider)
        .map(|s| s.name.to_string())
        .unwrap_or_else(|| model.to_string())
}

pub(crate) const SOLUTIONS: &[ModelSolution] = &[
    ModelSolution {
        id: "kimi-code",
        name: "Kimi Code",
        model: "kimi-for-coding",
        description: "Moonshot AI coding model",
        context_window: 256_000,
    },
    ModelSolution {
        id: "openai",
        name: "OpenAI GPT-4o",
        model: "gpt-4o",
        description: "OpenAI API",
        context_window: 128_000,
    },
    ModelSolution {
        id: "gemini",
        name: "Gemini 2.5 Flash",
        model: "gemini-2.5-flash",
        description: "Google Gemini 2.5 Flash",
        context_window: 1_000_000,
    },
    ModelSolution {
        id: "deepseek-v4-flash",
        name: "DeepSeek V4 Flash",
        model: "deepseek-v4-flash",
        description: "DeepSeek V4 Flash",
        context_window: 1_000_000,
    },
    ModelSolution {
        id: "deepseek-v4-pro",
        name: "DeepSeek V4 Pro",
        model: "deepseek-v4-pro",
        description: "DeepSeek V4 Pro",
        context_window: 1_000_000,
    },
    ModelSolution {
        id: "qwen",
        name: "Qwen Plus",
        model: "qwen-plus",
        description: "Alibaba DashScope",
        context_window: 131_072,
    },
    ModelSolution {
        id: "glm",
        name: "GLM 4 Plus",
        model: "glm-4-plus",
        description: "Zhipu AI",
        context_window: 128_000,
    },
    ModelSolution {
        id: "llama",
        name: "Llama",
        model: "local-model",
        description: "Local Llama server",
        context_window: 0,
    },
    ModelSolution {
        id: "mock",
        name: "Mock",
        model: "mock-model",
        description: "Test provider",
        context_window: 0,
    },
];

/// Filter and sort the model picker rows (ADR-0002 phase 3). Joins the TUI's
/// static `SOLUTIONS` (display metadata) with the live picker snapshot
/// (favorite / key-ready / last-used), fuzzy-filters by `query`, and sorts by
/// **favorite first → last-used descending → name ascending**.
///
/// The fuzzy query is a *filter*, not a sort key: once a row passes the filter,
/// its position is set by the user's preference and usage signals, not by match
/// quality, so a favorite always wins over a slightly-better-matching
/// non-favorite. Returns `(SOLUTIONS index, picker row)` pairs.
pub(crate) fn models_filtered_from<'a>(
    solutions: &'a [ModelSolution],
    picker: &'a neenee_core::ModelPickerSnapshot,
    query: &str,
) -> Vec<(usize, &'a ModelPickerRow)> {
    let mut rows: Vec<(usize, &ModelPickerRow)> = solutions
        .iter()
        .enumerate()
        .filter_map(|(i, solution)| {
            let row = picker.rows.iter().find(|r| r.id == solution.id)?;
            Some((i, row))
        })
        .filter(|(i, _)| {
            if query.is_empty() {
                return true;
            }
            let solution = &solutions[*i];
            fuzzy::fuzzy_match(solution.name, query).is_some()
                || fuzzy::fuzzy_match(solution.id, query).is_some()
        })
        .collect();
    rows.sort_by(|(ia, ra), (ib, rb)| {
        let name_a = &solutions[*ia].name;
        let name_b = &solutions[*ib].name;
        rb.favorite
            .cmp(&ra.favorite)
            .then_with(|| rb.last_used_ms.cmp(&ra.last_used_ms))
            .then_with(|| name_a.cmp(name_b))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_resolves_preset_from_provider_id() {
        // The provider id maps to the friendly preset name the picker shows,
        // so the hint bar no longer leaks the raw model id.
        assert_eq!(
            model_display_name("deepseek-v4-pro", "deepseek-v4-pro"),
            "DeepSeek V4 Pro"
        );
        assert_eq!(
            model_display_name("kimi-code", "kimi-for-coding"),
            "Kimi Code"
        );
        assert_eq!(
            model_display_name("gemini", "gemini-2.5-flash"),
            "Gemini 2.5 Flash"
        );
    }

    #[test]
    fn display_name_falls_back_to_model_id_for_unknown_providers() {
        // Custom / unknown providers have no preset entry, so the raw model id
        // is the only label available — it must pass through unchanged.
        assert_eq!(
            model_display_name("custom-whatever", "some-model"),
            "some-model"
        );
        assert_eq!(model_display_name("acme-corp", "acme-7b"), "acme-7b");
    }
}
