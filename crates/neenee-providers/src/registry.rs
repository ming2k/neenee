//! OpenAI-compatible provider registry and the `Channel` → concrete `Provider`
//! factory consumed by the orchestration layer.

use neenee_core::Provider;
use neenee_core::catalog::{Channel, Transport};
use std::sync::Arc;

use crate::{AnthropicMessagesProvider, GeminiProvider, NEENEE_USER_AGENT, OpenAiCompatProvider};

// ═════════════════════════════════════════════════════════════════════════════
// OpenAI-compatible provider wrappers for popular Chinese & global services
// ═════════════════════════════════════════════════════════════════════════════

/// Per-model `max_tokens` for the Anthropic `/messages` surface. The Messages
/// API requires `max_tokens`; capping the response at the model's registered
/// output limit (rather than a flat 8192) lets long agent turns from
/// high-output models (MiniMax M3: 131072) run untruncated. Values mirror
/// models.dev's opencode-go entries. Unknown models fall back to the default
/// inside [`AnthropicMessagesProvider`].
const ANTHROPIC_MODEL_MAX_TOKENS: &[(&str, u32)] = &[
    ("minimax-m3", 131072),
    ("minimax-m2.7", 131072),
    ("minimax-m2.5", 65536),
    ("qwen3.7-max", 65536),
    ("qwen3.7-plus", 65536),
    ("qwen3.6-plus", 65536),
    ("qwen3.5-plus", 65536),
];

/// Look up the `max_tokens` for an Anthropic-format model id. `None` lets the
/// provider fall back to its built-in default.
fn anthropic_model_max_tokens(model_id: &str) -> Option<u32> {
    ANTHROPIC_MODEL_MAX_TOKENS
        .iter()
        .find(|(id, _)| *id == model_id)
        .map(|(_, tokens)| *tokens)
}

/// Specification for an OpenAI-compatible provider.
///
/// Every provider in [`OPENAI_PROVIDER_SPECS`] speaks the OpenAI
/// chat-completions wire format and differs only in endpoint, default model,
/// the environment variables consulted, and (rarely) a pinned model or a
/// required user agent. Modelling them as *data* rather than one delegating
/// newtype per vendor means adding a provider is a single table entry instead
/// of ~30 lines of boilerplate trait delegation.
pub struct OpenAiProviderSpec {
    /// Stable identifier used in config (`default_provider`) and the TUI.
    pub id: &'static str,
    /// Full chat-completions endpoint URL.
    pub base_url: &'static str,
    /// Model used when neither config nor environment specifies one.
    pub default_model: &'static str,
    /// Environment variable consulted for the API key.
    pub env_api_key: &'static str,
    /// Environment variable consulted for a model override.
    pub env_model: &'static str,
    /// When set, the endpoint pins this model and ignores any override
    /// (e.g. the Kimi coding endpoint).
    pub fixed_model: Option<&'static str>,
    /// When set, the endpoint requires this user agent unless overridden.
    pub default_user_agent: Option<&'static str>,
}

/// The single registry of OpenAI-compatible providers — the source of truth for
/// their endpoints, default models, and environment variables.
pub const OPENAI_PROVIDER_SPECS: &[OpenAiProviderSpec] = &[
    // Kimi Code — Moonshot AI's coding model, served via the Kimi Code
    // membership platform (api.kimi.com/coding/v1). The platform requires a
    // recognized coding-agent User-Agent and pins the model id to the fixed
    // `kimi-k2.7-code` alias. API key env still uses the MOONSHOT_API_KEY
    // legacy name for config compatibility.
    OpenAiProviderSpec {
        id: "kimi-code",
        base_url: "https://api.kimi.com/coding/v1/chat/completions",
        default_model: "kimi-k2.7-code",
        env_api_key: "MOONSHOT_API_KEY",
        env_model: "MOONSHOT_MODEL",
        fixed_model: Some("kimi-k2.7-code"),
        default_user_agent: Some("opencode/0.1.0"),
    },
    // DeepSeek V4 Flash — versatile model with thinking + non-thinking modes.
    OpenAiProviderSpec {
        id: "deepseek-v4-flash",
        base_url: "https://api.deepseek.com/v1/chat/completions",
        default_model: "deepseek-v4-flash",
        env_api_key: "DEEPSEEK_API_KEY",
        env_model: "DEEPSEEK_FLASH_MODEL",
        fixed_model: None,
        default_user_agent: None,
    },
    // DeepSeek V4 Pro — most capable model.
    OpenAiProviderSpec {
        id: "deepseek-v4-pro",
        base_url: "https://api.deepseek.com/v1/chat/completions",
        default_model: "deepseek-v4-pro",
        env_api_key: "DEEPSEEK_API_KEY",
        env_model: "DEEPSEEK_PRO_MODEL",
        fixed_model: None,
        default_user_agent: None,
    },
    // ZAI Code — Z.AI (Zhipu) coding-plan platform
    // (api.z.ai/api/coding/paas/v4). A coding-agent membership endpoint that
    // serves the GLM-5 family; glm-5.2 is the current flagship. Like the Kimi
    // Code platform, it expects a recognized coding-agent User-Agent. Shares
    // the ZHIPU_API_KEY legacy name for key compatibility with the broader
    // Zhipu ecosystem, while ZAI_API_KEY is the preferred alias.
    OpenAiProviderSpec {
        id: "zai-code",
        base_url: "https://api.z.ai/api/coding/paas/v4/chat/completions",
        default_model: "glm-5.2",
        env_api_key: "ZAI_API_KEY",
        env_model: "ZAI_MODEL",
        fixed_model: None,
        default_user_agent: Some("opencode/1.17.10"),
    },
];

/// Look up an OpenAI-compatible provider spec by its identifier. Exact match
/// only; preset ids are unique and do not have alias mappings.
pub fn openai_provider_spec(id: &str) -> Option<&'static OpenAiProviderSpec> {
    OPENAI_PROVIDER_SPECS.iter().find(|spec| spec.id == id)
}

impl OpenAiProviderSpec {
    /// Resolve the model to use: a pinned `fixed_model` always wins, otherwise
    /// the caller's override, otherwise the provider default.
    pub fn resolve_model(&self, override_model: Option<String>) -> String {
        if let Some(fixed) = self.fixed_model {
            return fixed.to_string();
        }
        override_model.unwrap_or_else(|| self.default_model.to_string())
    }

    /// Build a concrete [`OpenAiCompatProvider`] for this spec. `user_agent` overrides
    /// the spec default (used by the Kimi coding endpoint).
    pub fn build(
        &self,
        api_key: String,
        override_model: Option<String>,
        user_agent: Option<String>,
    ) -> OpenAiCompatProvider {
        let model = self.resolve_model(override_model);
        let agent = user_agent
            .or_else(|| self.default_user_agent.map(str::to_string))
            .unwrap_or_else(|| NEENEE_USER_AGENT.to_string());
        let mut provider = OpenAiCompatProvider::with_base_url_and_user_agent(
            api_key,
            model,
            self.base_url,
            &agent,
        );
        provider.id = self.id.to_string();
        provider
    }
}

/// Construct the concrete `Provider` for a [`neenee_core::catalog::Channel`].
///
/// This is the construction layer that knows about every concrete `Provider`
/// implementation; it lives in `neenee-providers` (not `neenee-core`) so the
/// domain crate stays free of HTTP I/O. `entry_id` becomes the provider's
/// attribution id (`Provider::provider_id`) so assistant responses are
/// attributed to the logical model even after a mid-session switch.
pub fn build_provider_for_channel(channel: &Channel, entry_id: &str) -> Arc<dyn Provider> {
    match &channel.transport {
        Transport::GeminiNative => Arc::new(GeminiProvider {
            api_key: channel.api_key.clone(),
            model: channel.model.clone(),
            id: entry_id.to_string(),
        }),
        Transport::Llama { base_url } => {
            // `llama-server --jinja` speaks the full OpenAI chat-completions
            // surface — including native tool calls and streaming tool-call
            // deltas — so the local server is reached through the same
            // `OpenAiCompatProvider` as any cloud endpoint. The channel is
            // keyless (`Transport::Llama` resolves no API key), so the empty
            // key suppresses the `Authorization` header entirely.
            let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
            let mut provider = OpenAiCompatProvider::with_base_url_and_user_agent(
                channel.api_key.clone(),
                channel.model.clone(),
                &url,
                NEENEE_USER_AGENT,
            );
            provider.id = entry_id.to_string();
            Arc::new(provider)
        }
        Transport::Anthropic {
            base_url,
            user_agent,
        } => {
            let mut provider = AnthropicMessagesProvider::with_user_agent(
                channel.api_key.clone(),
                channel.model.clone(),
                base_url,
                user_agent,
            );
            provider.id = entry_id.to_string();
            // Cap the response length at the model's registered output limit so
            // high-output models (MiniMax M3) are not truncated by the default.
            if let Some(max_tokens) = anthropic_model_max_tokens(&channel.model) {
                provider = provider.with_max_tokens(max_tokens);
            }
            Arc::new(provider)
        }
        Transport::OpenAiCompat {
            base_url,
            user_agent,
        } => {
            let mut provider = OpenAiCompatProvider::with_base_url_and_user_agent(
                channel.api_key.clone(),
                channel.model.clone(),
                base_url,
                user_agent,
            );
            provider.id = entry_id.to_string();
            Arc::new(provider)
        }
    }
}

#[cfg(test)]
mod spec_tests {
    use super::*;

    #[test]
    fn kimi_code_uses_kimi_code_platform() {
        let spec = openai_provider_spec("kimi-code").expect("kimi-code spec");
        // The Kimi Code platform pins the model id — overrides are ignored.
        assert_eq!(spec.resolve_model(None), "kimi-k2.7-code");
        assert_eq!(
            spec.resolve_model(Some("kimi-k2.7-code-highspeed".to_string())),
            "kimi-k2.7-code"
        );

        let provider = spec.build("test-key".to_string(), None, None);
        assert_eq!(
            provider.base_url,
            "https://api.kimi.com/coding/v1/chat/completions"
        );
        assert_eq!(provider.model, "kimi-k2.7-code");
        // The Kimi Code platform requires a recognized coding-agent UA.
        assert_eq!(provider.user_agent, "opencode/0.1.0");
        // The registry stamps the preset id onto the concrete provider so
        // assistant responses can be attributed to "kimi-code".
        assert_eq!(provider.id, "kimi-code");
        assert_eq!(provider.provider_id(), "kimi-code");
        assert_eq!(provider.model(), "kimi-k2.7-code");
    }

    #[test]
    fn openai_compat_spec_resolves_model_override_and_default() {
        let spec = openai_provider_spec("deepseek-v4-flash").expect("deepseek-v4-flash spec");
        assert_eq!(spec.resolve_model(None), "deepseek-v4-flash");
        assert_eq!(
            spec.resolve_model(Some("deepseek-v4-pro".to_string())),
            "deepseek-v4-pro"
        );
        // Non-coding providers fall back to the shared neenee user agent.
        let provider = spec.build("k".to_string(), None, None);
        assert_eq!(provider.user_agent, NEENEE_USER_AGENT);
    }

    #[test]
    fn deepseek_pro_defaults_to_pro_model() {
        let spec = openai_provider_spec("deepseek-v4-pro").expect("deepseek-v4-pro spec");
        assert_eq!(spec.resolve_model(None), "deepseek-v4-pro");
        assert_eq!(
            spec.base_url,
            "https://api.deepseek.com/v1/chat/completions"
        );
    }

    #[test]
    fn stale_deepseek_ids_do_not_resolve() {
        // No alias mapping: the pre-rename ids are gone and must not resolve.
        assert!(openai_provider_spec("deepseek").is_none());
        assert!(openai_provider_spec("deepseek-flash").is_none());
        assert!(openai_provider_spec("deepseek-pro").is_none());
        // Qwen was removed from the registry and must not resolve.
        assert!(openai_provider_spec("qwen").is_none());
    }
}

#[cfg(test)]
mod build_tests {
    use super::*;

    #[test]
    fn build_provider_stamps_entry_id_on_openai_compat() {
        let channel = Channel {
            id: "default".to_string(),
            label: "OpenAI".to_string(),
            transport: Transport::OpenAiCompat {
                base_url: "https://api.openai.com/v1/chat/completions".to_string(),
                user_agent: "agent".to_string(),
            },
            api_key: "k".to_string(),
            model: "gpt-4o".to_string(),
        };
        let provider = build_provider_for_channel(&channel, "openai");
        assert_eq!(provider.provider_id(), "openai");
        assert_eq!(provider.model(), "gpt-4o");
    }

    #[test]
    fn build_provider_dispatches_anthropic_transport() {
        // opencode-go's MiniMax/Qwen models reach an Anthropic /messages
        // endpoint; the catalog builds an Anthropic transport for them, and
        // build_provider_for_channel must dispatch it to the messages provider.
        let channel = Channel {
            id: "minimax-m3".to_string(),
            label: "MiniMax M3".to_string(),
            transport: Transport::Anthropic {
                base_url: "https://opencode.ai/zen/go/v1/messages".to_string(),
                user_agent: "agent".to_string(),
            },
            api_key: "go-key".to_string(),
            model: "minimax-m3".to_string(),
        };
        let provider = build_provider_for_channel(&channel, "opencode-go");
        assert_eq!(provider.provider_id(), "opencode-go");
        assert_eq!(provider.model(), "minimax-m3");
    }

    #[test]
    fn build_provider_routes_llama_transport_through_openai_compat() {
        // `llama-server --jinja` speaks the full OpenAI chat-completions
        // surface (native tool calls + streaming tool-call deltas), so the local
        // server is reached through `OpenAiCompatProvider` rather than a limited
        // local provider. The channel is keyless: the request body builder keeps
        // the tool-capable machinery, and the empty key suppresses auth.
        let channel = Channel {
            id: "default".to_string(),
            label: "Llama".to_string(),
            transport: Transport::Llama {
                base_url: "http://localhost:8080".to_string(),
            },
            api_key: String::new(),
            model: "gemma-4-E4B-it-GGUF".to_string(),
        };
        let provider = build_provider_for_channel(&channel, "llama");
        assert_eq!(provider.provider_id(), "llama");
        assert_eq!(provider.model(), "gemma-4-E4B-it-GGUF");
        // The concrete provider is the OpenAI-compatible one (downcast is not
        // available on `dyn Provider`, so verify the identity indirectly: a
        // provider with native tool support exposes the same id/model surface,
        // and the trait default for `prepare_tools` would be a no-op). The
        // load-bearing assertion is that construction succeeds and attributes
        // correctly; tool wiring is exercised by the agent harness.
    }

    #[test]
    fn anthropic_max_tokens_derives_from_model_output_limit() {
        // minimax-m3's registered output limit (131072) must cap the request's
        // max_tokens, not the provider's flat 8192 default. Construct directly
        // so the typed field is readable (the trait object returned by
        // build_provider_for_channel is not downcastable).
        let provider = AnthropicMessagesProvider::with_user_agent(
            "k".to_string(),
            "minimax-m3".to_string(),
            "https://opencode.ai/zen/go/v1/messages",
            "agent",
        )
        .with_max_tokens(anthropic_model_max_tokens("minimax-m3").unwrap());
        assert_eq!(provider.max_tokens, 131072);
        // An unknown model id falls back to None (the provider keeps its
        // default), proving the lookup does not invent a limit.
        assert!(anthropic_model_max_tokens("not-a-model").is_none());
    }
}
