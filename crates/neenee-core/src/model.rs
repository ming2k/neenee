//! Canonical model registry — the single source of truth for a model's
//! intrinsic, provider-independent properties (context window, capabilities).
//!
//! A [`ProviderEntry`](crate::catalog::ProviderEntry) references a model by its
//! wire id (e.g. `"glm-5.2"`); this module resolves that id to the definitive
//! metadata. This avoids duplicating per-model facts across every provider that
//! serves the same model (official endpoint, relay, local proxy, …).

/// A canonical model definition with its intrinsic properties.
///
/// Provider-independent: whether a model reasons or supports tool calls does
/// not change depending on which endpoint serves it. The [`KNOWN_MODELS`]
/// registry is the authoritative list; [`model_by_id`] is the lookup.
#[derive(Debug, Clone, Copy)]
pub struct Model {
    /// Wire model id sent in API requests, e.g. `"glm-5.2"`.
    pub id: &'static str,
    /// Human-readable display name, e.g. `"GLM-5.2"`.
    pub name: &'static str,
    /// Model family for grouping, e.g. `"glm"`, `"gpt"`, `"gemini"`.
    pub family: &'static str,
    /// Context window in tokens. `0` means unknown.
    pub context_window: usize,
    /// Whether the model emits `reasoning_content` / supports thinking.
    pub reasoning: bool,
    /// Whether the model supports native tool/function calling.
    pub tool_call: bool,
}

/// The canonical registry of known models. Add a model here when it is
/// referenced by any built-in provider preset; user-defined models that are not
/// in this list fall back to [`fallback_model`] at resolution time.
pub const KNOWN_MODELS: &[Model] = &[
    // ── GLM family (Zhipu / Z.AI) ──────────────────────────────────────────
    Model {
        id: "glm-5.2",
        name: "GLM-5.2",
        family: "glm",
        context_window: 1_000_000,
        reasoning: true,
        tool_call: true,
    },
    Model {
        id: "glm-5.1",
        name: "GLM-5.1",
        family: "glm",
        context_window: 200_000,
        reasoning: true,
        tool_call: true,
    },
    Model {
        id: "glm-4.7",
        name: "GLM-4.7",
        family: "glm",
        context_window: 200_000,
        reasoning: true,
        tool_call: true,
    },
    // ── Kimi (Moonshot) ────────────────────────────────────────────────────
    Model {
        id: "kimi-k2.7-code",
        name: "Kimi K2.7 Code",
        family: "kimi",
        context_window: 262_144,
        reasoning: true,
        tool_call: true,
    },
    // ── GPT (OpenAI) ───────────────────────────────────────────────────────
    Model {
        id: "gpt-4o",
        name: "GPT-4o",
        family: "gpt",
        context_window: 128_000,
        reasoning: false,
        tool_call: true,
    },
    Model {
        id: "gpt-4o-mini",
        name: "GPT-4o Mini",
        family: "gpt",
        context_window: 128_000,
        reasoning: false,
        tool_call: true,
    },
    // ── Gemini (Google) ────────────────────────────────────────────────────
    Model {
        id: "gemini-2.5-flash",
        name: "Gemini 2.5 Flash",
        family: "gemini",
        context_window: 1_000_000,
        reasoning: false,
        tool_call: true,
    },
    Model {
        id: "gemini-2.0-flash",
        name: "Gemini 2.0 Flash",
        family: "gemini",
        context_window: 1_000_000,
        reasoning: false,
        tool_call: true,
    },
    // ── DeepSeek ───────────────────────────────────────────────────────────
    Model {
        id: "deepseek-v4-flash",
        name: "DeepSeek V4 Flash",
        family: "deepseek",
        context_window: 1_000_000,
        reasoning: true,
        tool_call: true,
    },
    Model {
        id: "deepseek-v4-pro",
        name: "DeepSeek V4 Pro",
        family: "deepseek",
        context_window: 1_000_000,
        reasoning: true,
        tool_call: true,
    },
];

/// Look up a known model by its wire id. Returns `None` for user-defined or
/// unrecognized model ids; callers should fall back to [`fallback_model`].
pub fn model_by_id(id: &str) -> Option<&'static Model> {
    KNOWN_MODELS.iter().find(|m| m.id == id)
}

/// A conservative fallback for model ids not in [`KNOWN_MODELS`] (local models,
/// user-defined relays, unreleased models). Assumes tool calling (the harness
/// depends on it) and nothing else.
pub fn fallback_model(_id: &str) -> Model {
    Model {
        id: "",
        name: "",
        family: "",
        context_window: 0,
        reasoning: false,
        tool_call: true,
    }
}

/// Resolve any model id to its metadata: the canonical entry when known, or a
/// conservative fallback otherwise. Never returns `None` so callers need not
/// branch on absence.
pub fn resolve(id: &str) -> Model {
    model_by_id(id)
        .copied()
        .unwrap_or_else(|| fallback_model(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_have_unique_ids() {
        let mut ids: Vec<&str> = KNOWN_MODELS.iter().map(|m| m.id).collect();
        ids.sort_unstable();
        let dups: Vec<&str> = ids
            .windows(2)
            .filter(|w| w[0] == w[1])
            .map(|w| w[0])
            .collect();
        assert!(dups.is_empty(), "duplicate model ids: {dups:?}");
    }

    #[test]
    fn resolve_returns_known_model() {
        let m = resolve("glm-5.2");
        assert_eq!(m.name, "GLM-5.2");
        assert_eq!(m.context_window, 1_000_000);
        assert!(m.reasoning);
    }

    #[test]
    fn resolve_falls_back_for_unknown() {
        let m = resolve("some-local-model");
        assert_eq!(m.context_window, 0);
        assert!(!m.reasoning);
        // The harness depends on tool calling, so even the fallback assumes it.
        assert!(m.tool_call);
    }
}
