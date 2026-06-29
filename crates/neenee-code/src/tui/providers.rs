//! Snapshot-driven provider/model picker filter & sort logic.
//!
//! The picker renders directly from [`neenee_core::ProviderPickerSnapshot`] — one
//! [`neenee_core::ProviderPickerRow`] per provider the harness knows how to
//! drive, carrying the display name, the served model ids, the active model, and
//! the live per-user signals (favorite, key-ready, last-used). Built-in and
//! user-defined providers share this single path, so a custom provider added via
//! the editor shows up like any built-in (there is no separate static table).
//!
//! The picker is **two-stage**: [`providers_filtered_from`] builds the stage-1
//! provider list; activating a multi-model provider drills into its models via
//! [`provider_models_filtered_from`] (stage 2). Single-model providers activate
//! directly.

use neenee_core::{KNOWN_MODELS, ProviderPickerSnapshot, WireFormat, resolve_model};

use crate::tui::fuzzy;

/// The protocol choices offered by the custom-provider editor, as
/// `(display label, wire id)`. The index is [`crate::tui::App::custom_protocol`];
/// the wire id is sent in `AgentRequest::AddProvider` and mapped to a
/// `UserTransport` by the harness.
pub(crate) const CUSTOM_PROTOCOLS: &[(&str, &str)] = &[
    ("OpenAI-compatible", "openai"),
    ("Anthropic", "anthropic"),
    ("Gemini", "gemini"),
];

/// The registry model ids that match a custom protocol's wire format, used as the
/// candidate list when picking a model for a custom provider (the "list select"
/// half of "list select + custom fallback"). An unknown protocol falls back to
/// the OpenAI-compatible set, which is also the default.
pub(crate) fn protocol_model_candidates(protocol_wire: &str) -> Vec<&'static str> {
    let format = match protocol_wire {
        "anthropic" => WireFormat::AnthropicCompat,
        "gemini" => WireFormat::Gemini,
        _ => WireFormat::OpenAiCompat,
    };
    KNOWN_MODELS
        .iter()
        .filter(|m| m.format == format)
        .map(|m| m.id)
        .collect()
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

/// The context window (in tokens) of a model id, resolved from the registry.
/// Returns `0` for unknown models. Replaces the former `provider_context_window`
/// now that the picker carries the active model id directly.
pub(crate) fn model_context_window(model: &str) -> usize {
    resolve_model(model).context_window
}

/// One selectable row in the **stage-2 model sub-list**: a single
/// (provider, model) pair within one drilled-into provider. Built by
/// [`provider_models_filtered_from`]; the picker browses, searches, and
/// activates these once a multi-model provider is opened.
pub(crate) struct RankedModel {
    /// Canonical id of the provider serving this model (its snapshot row id).
    pub provider_id: String,
    /// Wire model id to activate.
    pub model: String,
    /// The rendered label — the model's display name (stage 2 is already scoped
    /// to one provider, so no provider suffix). The fuzzy match indexes directly
    /// onto these characters.
    pub label: String,
    /// The fuzzy match against `label`, or `None` in browse mode (empty query),
    /// where every row is shown unhighlighted.
    pub m: Option<fuzzy::FuzzyMatch>,
}

/// One selectable row in the **stage-1 provider list**. Carries everything the
/// renderer and input handler need (copied out of the snapshot row), so neither
/// re-indexes the snapshot. The two-stage picker shows providers first (this),
/// then drills into a single provider's models ([`RankedModel`]) on activation.
pub(crate) struct RankedProvider {
    /// Index into [`ProviderPickerSnapshot::rows`] of this provider (stable
    /// across re-filtering, so it identifies the drilled-into provider).
    pub row_idx: usize,
    /// Canonical provider id.
    pub id: String,
    /// Display name (the fuzzy target; mirrors `label`).
    pub name: String,
    /// Active model wire id.
    pub model: String,
    /// Every model id this provider serves.
    pub models: Vec<String>,
    /// `true` for built-in presets, `false` for user-defined providers. Drives
    /// the built-in/custom grouping and whether `e` opens the full meta editor.
    pub builtin: bool,
    /// Whether the provider is favorited (mirrors the snapshot row).
    pub favorite: bool,
    /// The rendered label — the provider's display name.
    pub label: String,
    /// The fuzzy match against `label`, or `None` in browse mode (empty query).
    pub m: Option<fuzzy::FuzzyMatch>,
}

impl RankedProvider {
    /// Whether the provider hosts more than one model (its activation opens the
    /// stage-2 model picker). Single-model providers activate directly.
    pub(crate) fn is_multi_model(&self) -> bool {
        self.models.len() > 1
    }
}

/// The favorite → last-used-desc → name ordering shared by both picker stages.
/// Pulls each provider's live signals from its snapshot row.
fn provider_order(
    picker: &ProviderPickerSnapshot,
    a_id: &str,
    b_id: &str,
    a_name: &str,
    b_name: &str,
) -> std::cmp::Ordering {
    let signal = |id: &str| {
        picker
            .rows
            .iter()
            .find(|r| r.id == id)
            .map(|r| (r.favorite, r.last_used_ms))
            .unwrap_or((false, None))
    };
    let (a_fav, a_used) = signal(a_id);
    let (b_fav, b_used) = signal(b_id);
    b_fav
        .cmp(&a_fav)
        .then_with(|| b_used.cmp(&a_used))
        .then_with(|| a_name.cmp(b_name))
}

/// Build the **stage-1** provider rows: one per snapshot row, fuzzy-filtered by
/// `query` against the provider name and sorted favorite → last-used → name. An
/// empty `query` (browse mode) keeps every provider with no match positions.
pub(crate) fn providers_filtered_from(
    picker: &ProviderPickerSnapshot,
    query: &str,
) -> Vec<RankedProvider> {
    let mut rows: Vec<RankedProvider> = Vec::new();
    for (row_idx, prow) in picker.rows.iter().enumerate() {
        let label = prow.name.clone();
        let m = if query.is_empty() {
            None
        } else {
            match fuzzy::fuzzy_match(&label, query) {
                Some(m) => Some(m),
                None => continue,
            }
        };
        rows.push(RankedProvider {
            row_idx,
            id: prow.id.clone(),
            name: prow.name.clone(),
            model: prow.model.clone(),
            models: prow.models.clone(),
            builtin: prow.builtin,
            favorite: prow.favorite,
            label,
            m,
        });
    }
    // Built-in providers group first, then user-defined ones; within each group
    // the shared favorite → last-used → name order applies.
    rows.sort_by(|a, b| {
        b.builtin
            .cmp(&a.builtin)
            .then_with(|| provider_order(picker, &a.id, &b.id, &a.name, &b.name))
    });
    rows
}

/// Build the **stage-2** model rows for a single provider: one [`RankedModel`]
/// per model the provider serves (in snapshot order — a curated, predictable
/// list), fuzzy-filtered by `query` against the model display name. `row_idx`
/// indexes into `picker.rows`; an out-of-range index yields no rows.
pub(crate) fn provider_models_filtered_from(
    picker: &ProviderPickerSnapshot,
    row_idx: usize,
    query: &str,
) -> Vec<RankedModel> {
    let Some(prow) = picker.rows.get(row_idx) else {
        return Vec::new();
    };
    let mut rows: Vec<RankedModel> = Vec::new();
    for model in &prow.models {
        // Stage 2 is already scoped to one provider, so the label is just the
        // model name — no provider suffix to disambiguate.
        let label = model_display_name(model);
        let m = if query.is_empty() {
            None
        } else {
            match fuzzy::fuzzy_match(&label, query) {
                Some(m) => Some(m),
                None => continue,
            }
        };
        rows.push(RankedModel {
            provider_id: prow.id.clone(),
            model: model.clone(),
            label,
            m,
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use neenee_core::ProviderPickerRow;

    fn row(id: &str, name: &str, models: &[&str], builtin: bool) -> ProviderPickerRow {
        ProviderPickerRow {
            id: id.to_string(),
            name: name.to_string(),
            model: models.first().copied().unwrap_or("").to_string(),
            models: models.iter().map(|m| m.to_string()).collect(),
            builtin,
            protocol: String::new(),
            base_url: String::new(),
            key_ready: true,
            favorite: false,
            last_used_ms: None,
        }
    }

    fn sample() -> ProviderPickerSnapshot {
        ProviderPickerSnapshot {
            default_id: "openai".to_string(),
            rows: vec![
                row("kimi-code", "Kimi Code", &["kimi-k2.7-code"], true),
                row("openai", "OpenAI", &["gpt-4o", "gpt-4o-mini"], true),
                row(
                    "anthropic",
                    "Anthropic",
                    &["claude-opus-4-8", "claude-sonnet-4-6"],
                    true,
                ),
                row("my-relay", "My Relay", &["glm-5.2", "glm-5.1"], false),
            ],
        }
    }

    #[test]
    fn display_name_resolves_from_model_registry() {
        assert_eq!(model_display_name("glm-5.2"), "GLM-5.2");
        assert_eq!(model_display_name("gpt-4o"), "GPT-4o");
    }

    #[test]
    fn display_name_falls_back_to_raw_id_for_unknown_models() {
        assert_eq!(model_display_name("acme-7b"), "acme-7b");
    }

    #[test]
    fn protocol_candidates_filter_by_wire_format() {
        let openai = protocol_model_candidates("openai");
        assert!(openai.contains(&"gpt-4o"));
        // Anthropic-format models are excluded from the OpenAI candidate list.
        assert!(!openai.contains(&"claude-opus-4-8"));
        let anthropic = protocol_model_candidates("anthropic");
        assert!(anthropic.contains(&"claude-opus-4-8"));
        assert!(!anthropic.contains(&"gpt-4o"));
    }

    #[test]
    fn stage1_lists_one_row_per_provider_including_custom() {
        let snapshot = sample();
        let rows = providers_filtered_from(&snapshot, "");
        assert_eq!(rows.len(), snapshot.rows.len());
        // The user-defined provider shows up like any built-in.
        assert!(rows.iter().any(|r| r.id == "my-relay"));
    }

    #[test]
    fn stage1_fuzzy_filters_by_provider_name() {
        let snapshot = sample();
        let rows = providers_filtered_from(&snapshot, "anthro");
        assert!(rows.iter().all(|r| r.id == "anthropic"));
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn stage1_sorts_favorites_first_within_group() {
        let mut snapshot = sample();
        // Favorite a built-in: it sorts to the top of the built-in group (which
        // itself precedes the custom group).
        for r in &mut snapshot.rows {
            r.favorite = r.id == "anthropic";
        }
        let rows = providers_filtered_from(&snapshot, "");
        assert_eq!(rows[0].id, "anthropic");
        assert!(rows[0].favorite);
        // Built-ins group before the custom provider regardless of favorites.
        let custom_pos = rows.iter().position(|r| r.id == "my-relay").unwrap();
        assert!(rows[..custom_pos].iter().all(|r| r.builtin));
    }

    #[test]
    fn stage1_groups_builtins_before_custom() {
        let snapshot = sample();
        let rows = providers_filtered_from(&snapshot, "");
        // Every built-in precedes every custom provider.
        let first_custom = rows.iter().position(|r| !r.builtin).unwrap();
        assert!(rows[..first_custom].iter().all(|r| r.builtin));
        assert!(rows[first_custom..].iter().all(|r| !r.builtin));
    }

    #[test]
    fn is_multi_model_tracks_model_count() {
        let snapshot = sample();
        let rows = providers_filtered_from(&snapshot, "");
        let kimi = rows.iter().find(|r| r.id == "kimi-code").unwrap();
        assert!(!kimi.is_multi_model());
        let openai = rows.iter().find(|r| r.id == "openai").unwrap();
        assert!(openai.is_multi_model());
    }

    #[test]
    fn stage2_lists_a_single_providers_models() {
        let snapshot = sample();
        let idx = snapshot.rows.iter().position(|r| r.id == "openai").unwrap();
        let rows = provider_models_filtered_from(&snapshot, idx, "");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.provider_id == "openai"));
        assert!(rows.iter().any(|r| r.model == "gpt-4o"));
    }

    #[test]
    fn stage2_single_model_provider_yields_one_row() {
        let snapshot = sample();
        let idx = snapshot
            .rows
            .iter()
            .position(|r| r.id == "kimi-code")
            .unwrap();
        let rows = provider_models_filtered_from(&snapshot, idx, "");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model, "kimi-k2.7-code");
    }
}
