//! Static provider catalog + the picker's shared filter/sort logic.
//!
//! `PROVIDERS` is the TUI-side mirror of the providers the harness knows how
//! to drive: it carries the display name, the default model id, and a one-line
//! description. Context-window size is resolved from the [`neenee_core::model`]
//! registry, not duplicated here. The live per-user state (favorite, key-ready,
//! last-used) arrives via [`ProviderPickerSnapshot`]; [`models_filtered_from`]
//! joins the two into the flat, ranked model list the picker browses.
//!
//! [`ProviderPickerSnapshot`]: neenee_core::ProviderPickerSnapshot

use neenee_core::resolve_model;

use crate::tui::fuzzy;

#[derive(Clone, Copy)]
pub(crate) struct ProviderPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub model: &'static str,
    pub description: &'static str,
    /// Every model this provider serves. Empty for single-model presets (the
    /// one in `model` is the only choice). Non-empty for multi-model providers
    /// (opencode-go): activating such a provider opens the second-stage model
    /// picker listing these. The first entry is the default.
    pub models: &'static [&'static str],
}

/// Look up the context window (in tokens) for a provider preset id by resolving
/// its default model through the model registry. Returns `0` when the provider
/// or its model is unknown.
pub(crate) fn provider_context_window(provider: &str) -> usize {
    PROVIDERS
        .iter()
        .find(|s| s.id == provider)
        .map(|s| resolve_model(s.model).context_window)
        .unwrap_or(0)
}

/// Human-readable model name for the hint bar / status surfaces.
///
/// Resolves the wire model id through the [`neenee_core::model`] registry so
/// the always-visible indicator shows the actual model the user is talking to
/// (e.g. `GLM-5.2`, `Kimi K2.7 Code`), not the provider preset. Falls back to
/// the raw model id for unknown models (custom / local), where the id is the
/// only label available.
pub(crate) fn model_display_name(model: &str) -> String {
    let resolved = resolve_model(model);
    if resolved.name.is_empty() {
        model.to_string()
    } else {
        resolved.name.to_string()
    }
}

pub(crate) const PROVIDERS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "kimi-code",
        name: "Kimi Code",
        model: "kimi-k2.7-code",
        description: "Moonshot AI coding model",
        models: &[],
    },
    ProviderPreset {
        id: "openai",
        name: "OpenAI GPT-4o",
        model: "gpt-4o",
        description: "OpenAI API",
        models: &[],
    },
    ProviderPreset {
        id: "gemini",
        name: "Gemini 2.5 Flash",
        model: "gemini-2.5-flash",
        description: "Google Gemini 2.5 Flash",
        models: &[],
    },
    ProviderPreset {
        id: "deepseek-v4-flash",
        name: "DeepSeek V4 Flash",
        model: "deepseek-v4-flash",
        description: "DeepSeek V4 Flash",
        models: &[],
    },
    ProviderPreset {
        id: "deepseek-v4-pro",
        name: "DeepSeek V4 Pro",
        model: "deepseek-v4-pro",
        description: "DeepSeek V4 Pro",
        models: &[],
    },
    ProviderPreset {
        id: "zai-code",
        name: "ZAI Code",
        model: "glm-5.2",
        description: "Z.AI coding plan (GLM-5.2)",
        models: &[],
    },
    // OpenCode Go — one provider hosting many models. Each model's wire format
    // (OpenAI /chat/completions vs Anthropic /messages) is resolved by the
    // catalog from the model registry, so the picker lists model ids only. The
    // first entry is the default; activating opencode-go opens the second-stage
    // model picker.
    ProviderPreset {
        id: "opencode-go",
        name: "OpenCode Go",
        model: "glm-5.2",
        description: "opencode.ai relay (multi-model)",
        models: &[
            "glm-5.2",
            "glm-5.1",
            "glm-5",
            "kimi-k2.7-code",
            "kimi-k2.6",
            "kimi-k2.5",
            "deepseek-v4-pro",
            "deepseek-v4-flash",
            "mimo-v2.5-pro",
            "mimo-v2.5",
            "mimo-v2-pro",
            "mimo-v2-omni",
            "minimax-m3",
            "minimax-m2.7",
            "minimax-m2.5",
            "qwen3.7-max",
            "qwen3.7-plus",
            "qwen3.6-plus",
            "qwen3.5-plus",
        ],
    },
    ProviderPreset {
        id: "llama",
        name: "Llama",
        model: "local-model",
        description: "Local Llama server",
        models: &[],
    },
];

/// One selectable row in the flat model picker: a single (provider, model)
/// pair. Multi-model providers (opencode-go) contribute one `RankedModel` per
/// model they serve; single-model presets contribute exactly one (their
/// default `model`). This is the unit the picker browses, searches, and
/// activates — there is no separate provider→model two-step.
pub(crate) struct RankedModel {
    /// Index into [`PROVIDERS`] of the provider serving this model.
    pub provider_idx: usize,
    /// Wire model id to activate.
    pub model: &'static str,
    /// Whether the provider is favorited (mirrors the snapshot row).
    pub favorite: bool,
    /// The rendered label, `"<model display>  <provider name>"`. The fuzzy
    /// match is computed against this exact string so `m.positions` index
    /// directly onto the characters the renderer draws.
    pub label: String,
    /// Char count of the model-display prefix of `label`; the renderer styles
    /// `[0, model_w)` as the model name and the remainder as the provider name.
    pub model_w: usize,
    /// The fuzzy match against `label`, or `None` in browse mode (empty query),
    /// where every row is shown unhighlighted.
    pub m: Option<fuzzy::FuzzyMatch>,
}

/// Build the flat, ranked model list for the picker. Expands every provider
/// into its model rows (multi-model providers fan out; single-model presets
/// yield one), joins the live favorite / last-used signals from `picker`,
/// fuzzy-filters by `query` against each row's `"<model>  <provider>"` label,
/// and sorts **favorite first → last-used desc → provider name → model name**.
///
/// As in the provider picker, the fuzzy query is a *filter*, not a sort key:
/// a favorite always outranks a slightly-better-matching non-favorite. An empty
/// `query` (browse mode) keeps every row with no match positions.
pub(crate) fn models_filtered_from(
    solutions: &[ProviderPreset],
    picker: &neenee_core::ProviderPickerSnapshot,
    query: &str,
) -> Vec<RankedModel> {
    let mut rows: Vec<RankedModel> = Vec::new();
    for (provider_idx, solution) in solutions.iter().enumerate() {
        let Some(prow) = picker.rows.iter().find(|r| r.id == solution.id) else {
            continue;
        };
        // Single-model presets list their one default; multi-model providers
        // fan out into one row per served model.
        let models: &[&str] = if solution.models.is_empty() {
            std::slice::from_ref(&solution.model)
        } else {
            solution.models
        };
        for &model in models {
            let display = model_display_name(model);
            let model_w = display.chars().count();
            let label = format!("{display}  {}", solution.name);
            let m = if query.is_empty() {
                None
            } else {
                match fuzzy::fuzzy_match(&label, query) {
                    Some(m) => Some(m),
                    None => continue,
                }
            };
            rows.push(RankedModel {
                provider_idx,
                model,
                favorite: prow.favorite,
                label,
                model_w,
                m,
            });
        }
    }
    rows.sort_by(|a, b| {
        let ra = &picker.rows[picker
            .rows
            .iter()
            .position(|r| r.id == solutions[a.provider_idx].id)
            .expect("row built from an existing snapshot entry")];
        let rb = &picker.rows[picker
            .rows
            .iter()
            .position(|r| r.id == solutions[b.provider_idx].id)
            .expect("row built from an existing snapshot entry")];
        rb.favorite
            .cmp(&ra.favorite)
            .then_with(|| rb.last_used_ms.cmp(&ra.last_used_ms))
            .then_with(|| {
                solutions[a.provider_idx]
                    .name
                    .cmp(solutions[b.provider_idx].name)
            })
            .then_with(|| a.label.cmp(&b.label))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_resolves_from_model_registry() {
        // The hint bar shows the model's display name from the registry,
        // not the provider preset name.
        assert_eq!(model_display_name("glm-5.2"), "GLM-5.2");
        assert_eq!(model_display_name("kimi-k2.7-code"), "Kimi K2.7 Code");
        assert_eq!(model_display_name("gemini-2.5-flash"), "Gemini 2.5 Flash");
        assert_eq!(model_display_name("gpt-4o"), "GPT-4o");
    }

    #[test]
    fn display_name_falls_back_to_raw_id_for_unknown_models() {
        // Custom / local models not in the registry pass through unchanged.
        assert_eq!(model_display_name("some-model"), "some-model");
        assert_eq!(model_display_name("acme-7b"), "acme-7b");
    }
}
