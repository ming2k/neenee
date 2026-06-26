//! Canonical model registry — the single source of truth for a model's
//! intrinsic, provider-independent properties (context window, capabilities,
//! wire format).
//!
//! A [`ProviderEntry`](crate::catalog::ProviderEntry) references a model by its
//! wire id (e.g. `"glm-5.2"`); this module resolves that id to the definitive
//! metadata. This avoids duplicating per-model facts across every provider that
//! serves the same model (official endpoint, relay, local proxy, …).
//!
//! The [`WireFormat`] on each model records the wire protocol a provider uses to
//! reach it. Most models speak OpenAI chat-completions everywhere they are
//! served; a relay like opencode-go, however, serves MiniMax/Qwen behind an
//! Anthropic `/messages` surface, so those models carry [`WireFormat::AnthropicCompat`].
//! The catalog consults this when building the [`crate::catalog::Transport`] so
//! one provider (`opencode-go`) can host models of mixed formats.

/// The wire protocol a provider uses to reach a model. Determined per model
/// (not per provider): the same model id is served the same way everywhere in
/// practice, and opencode-go's mixed-format catalogue is the reason this field
/// exists. The catalog maps a format to a [`crate::catalog::Transport`] variant
/// and the endpoint suffix (`/chat/completions` vs `/messages`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WireFormat {
    /// OpenAI chat-completions (`/v1/chat/completions`). The common case.
    #[default]
    OpenAiCompat,
    /// Anthropic Messages (`/v1/messages`). Used by opencode-go for
    /// MiniMax/Qwen, and by any Anthropic-compatible relay.
    AnthropicCompat,
    /// Google Gemini native (`generativelanguage.googleapis.com`).
    Gemini,
}

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
    /// Whether the model supports vision (image inputs via `image_url`/
    /// `inline_data`). When `false`, images attached to messages are
    /// silently stripped before the request hits the wire.
    pub vision: bool,
    /// Wire protocol used to reach this model. See [`WireFormat`].
    pub format: WireFormat,
    /// Optional tool-usage guardrails injected into the system prompt as a
    /// [`ModelToolUsageGuidance`] section. Empty for models that need no
    /// extra guidance (Claude, GPT). Non-empty for models (e.g. GLM) that
    /// benefit from explicit anti-loop / one-tool-per-turn instructions.
    /// Stored here so the model entry is the single source of truth; the
    /// prompt engine just renders whatever the resolved model carries.
    pub tool_usage_hint: &'static str,
}

/// The canonical registry of known models. Add a model here when it is
/// referenced by any built-in provider preset; user-defined models that are not
/// in this list fall back to [`fallback_model`] at resolution time.

/// Tool-usage guardrails for the GLM model family. These models can get
/// stuck re-issuing identical tool calls without making progress. The
/// prompt engine injects this hint via [`ModelToolUsageGuidance`] when
/// the resolved model carries a non-empty [`Model::tool_usage_hint`].
pub const GLM_TOOL_USAGE_HINT: &str = "\
# Tool usage policy\n\
- Use exactly one tool per assistant message. After each tool call, wait \
for the result before continuing.\n\
- Avoid repeating the same tool with the same parameters once you have \
useful results. Use the result to take the next step (e.g. pick one match, \
read that file, then act); do not search again in a loop.";

pub const KNOWN_MODELS: &[Model] = &[
    // ── GLM family (Zhipu / Z.AI / opencode-go) ───────────────────────────
    Model {
        id: "glm-5.2",
        name: "GLM-5.2",
        family: "glm",
        context_window: 1_000_000,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: GLM_TOOL_USAGE_HINT,
    },
    Model {
        id: "glm-5.1",
        name: "GLM-5.1",
        family: "glm",
        context_window: 200_000,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: GLM_TOOL_USAGE_HINT,
    },
    Model {
        id: "glm-5",
        name: "GLM-5",
        family: "glm",
        context_window: 200_000,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: GLM_TOOL_USAGE_HINT,
    },
    Model {
        id: "glm-4.7",
        name: "GLM-4.7",
        family: "glm",
        context_window: 200_000,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: GLM_TOOL_USAGE_HINT,
    },
    // ── Kimi (Moonshot / opencode-go) ─────────────────────────────────────
    Model {
        id: "kimi-k2.7-code",
        name: "Kimi K2.7 Code",
        family: "kimi",
        context_window: 262_144,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
    },
    Model {
        id: "kimi-k2.6",
        name: "Kimi K2.6",
        family: "kimi",
        context_window: 262_144,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
    },
    Model {
        id: "kimi-k2.5",
        name: "Kimi K2.5",
        family: "kimi",
        context_window: 262_144,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
    },
    // ── GPT (OpenAI) ───────────────────────────────────────────────────────
    Model {
        id: "gpt-4o",
        name: "GPT-4o",
        family: "gpt",
        context_window: 128_000,
        reasoning: false,
        tool_call: true,
        vision: true,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
    },
    Model {
        id: "gpt-4o-mini",
        name: "GPT-4o Mini",
        family: "gpt",
        context_window: 128_000,
        reasoning: false,
        tool_call: true,
        vision: true,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
    },
    // ── Gemini (Google) ────────────────────────────────────────────────────
    Model {
        id: "gemini-2.5-flash",
        name: "Gemini 2.5 Flash",
        family: "gemini",
        context_window: 1_000_000,
        reasoning: false,
        tool_call: true,
        vision: true,
        format: WireFormat::Gemini,
        tool_usage_hint: "",
    },
    Model {
        id: "gemini-2.0-flash",
        name: "Gemini 2.0 Flash",
        family: "gemini",
        context_window: 1_000_000,
        reasoning: false,
        tool_call: true,
        vision: true,
        format: WireFormat::Gemini,
        tool_usage_hint: "",
    },
    // ── DeepSeek (opencode-go / direct) ────────────────────────────────────
    Model {
        id: "deepseek-v4-flash",
        name: "DeepSeek V4 Flash",
        family: "deepseek",
        context_window: 1_000_000,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
    },
    Model {
        id: "deepseek-v4-pro",
        name: "DeepSeek V4 Pro",
        family: "deepseek",
        context_window: 1_000_000,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
    },
    // ── MiMo (Xiaomi / opencode-go, OpenAI format) ─────────────────────────
    Model {
        id: "mimo-v2.5",
        name: "MiMo V2.5",
        family: "mimo",
        context_window: 1_000_000,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
    },
    Model {
        id: "mimo-v2.5-pro",
        name: "MiMo V2.5 Pro",
        family: "mimo",
        context_window: 1_048_576,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
    },
    Model {
        id: "mimo-v2-pro",
        name: "MiMo V2 Pro",
        family: "mimo",
        context_window: 1_048_576,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
    },
    Model {
        id: "mimo-v2-omni",
        name: "MiMo V2 Omni",
        family: "mimo",
        context_window: 262_144,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
    },
    // ── MiniMax (opencode-go, Anthropic /messages format) ──────────────────
    Model {
        id: "minimax-m3",
        name: "MiniMax M3",
        family: "minimax",
        context_window: 512_000,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::AnthropicCompat,
        tool_usage_hint: "",
    },
    Model {
        id: "minimax-m2.7",
        name: "MiniMax M2.7",
        family: "minimax",
        context_window: 204_800,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::AnthropicCompat,
        tool_usage_hint: "",
    },
    Model {
        id: "minimax-m2.5",
        name: "MiniMax M2.5",
        family: "minimax",
        context_window: 204_800,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::AnthropicCompat,
        tool_usage_hint: "",
    },
    // ── Qwen (opencode-go, OpenAI /chat/completions format) ────────────────
    // models.dev records qwen3.* as `@ai-sdk/openai-compatible` under
    // opencode-go; the KNOWN_MODELS fallback mirrors that so the offline
    // fallback path matches the live catalog.
    Model {
        id: "qwen3.7-max",
        name: "Qwen3.7 Max",
        family: "qwen",
        context_window: 1_000_000,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
    },
    Model {
        id: "qwen3.7-plus",
        name: "Qwen3.7 Plus",
        family: "qwen",
        context_window: 1_000_000,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
    },
    Model {
        id: "qwen3.6-plus",
        name: "Qwen3.6 Plus",
        family: "qwen",
        context_window: 1_000_000,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
    },
    Model {
        id: "qwen3.5-plus",
        name: "Qwen3.5 Plus",
        family: "qwen",
        context_window: 262_144,
        reasoning: true,
        tool_call: true,
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
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
        vision: false,
        format: WireFormat::OpenAiCompat,
        tool_usage_hint: "",
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

    #[test]
    fn opencode_go_models_carry_their_wire_format() {
        // OpenAI-format models served by opencode-go.
        assert_eq!(resolve("glm-5.2").format, WireFormat::OpenAiCompat);
        assert_eq!(resolve("kimi-k2.6").format, WireFormat::OpenAiCompat);
        assert_eq!(
            resolve("deepseek-v4-flash").format,
            WireFormat::OpenAiCompat
        );
        assert_eq!(resolve("mimo-v2.5-pro").format, WireFormat::OpenAiCompat);
        // Anthropic-/messages-format models served by opencode-go.
        assert_eq!(resolve("minimax-m3").format, WireFormat::AnthropicCompat);
        // models.dev records qwen3.* as openai-compatible under opencode-go.
        assert_eq!(resolve("qwen3.7-max").format, WireFormat::OpenAiCompat);
    }

    #[test]
    fn fallback_format_is_openai_compat() {
        assert_eq!(fallback_model("anything").format, WireFormat::OpenAiCompat);
    }
}
