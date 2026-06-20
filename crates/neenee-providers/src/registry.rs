//! OpenAI-compatible provider registry and the `Channel` → concrete `Provider`
//! factory consumed by the orchestration layer.

use neenee_core::catalog::{Channel, Transport};
use neenee_core::Provider;
use std::sync::Arc;

use crate::{
    GeminiProvider, LlamaServerProvider, MockProvider, OpenAiCompatProvider, NEENEE_USER_AGENT,
};

// ═════════════════════════════════════════════════════════════════════════════
// OpenAI-compatible provider wrappers for popular Chinese & global services
// ═════════════════════════════════════════════════════════════════════════════

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
    // Kimi K2.7 Code — Moonshot AI's strongest coding model. Official API at
    // api.moonshot.ai, OpenAI-compatible. Model can be overridden (e.g. to
    // kimi-k2.7-code-highspeed) via MOONSHOT_MODEL env or config.
    OpenAiProviderSpec {
        id: "kimi-k2.7-code",
        base_url: "https://api.moonshot.ai/v1/chat/completions",
        default_model: "kimi-k2.7-code",
        env_api_key: "MOONSHOT_API_KEY",
        env_model: "MOONSHOT_MODEL",
        fixed_model: None,
        default_user_agent: None,
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
    // Qwen (Tongyi / Alibaba DashScope). Models: qwen-plus, qwen-max, qwen-coder-plus.
    OpenAiProviderSpec {
        id: "qwen",
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
        default_model: "qwen-plus",
        env_api_key: "DASHSCOPE_API_KEY",
        env_model: "QWEN_MODEL",
        fixed_model: None,
        default_user_agent: None,
    },
    // GLM (Zhipu AI / 智谱). Models: glm-4-plus, glm-4, glm-4-air, glm-4-flash.
    OpenAiProviderSpec {
        id: "glm",
        base_url: "https://open.bigmodel.cn/api/paas/v4/chat/completions",
        default_model: "glm-4-plus",
        env_api_key: "GLM_API_KEY",
        env_model: "GLM_MODEL",
        fixed_model: None,
        default_user_agent: None,
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
        Transport::Mock => Arc::new(MockProvider),
        Transport::GeminiNative => Arc::new(GeminiProvider {
            api_key: channel.api_key.clone(),
            model: channel.model.clone(),
            id: entry_id.to_string(),
        }),
        Transport::Llama { base_url } => Arc::new(LlamaServerProvider {
            base_url: base_url.clone(),
            model: channel.model.clone(),
            id: entry_id.to_string(),
        }),
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
    fn kimi_k27_code_uses_official_endpoint_and_model() {
        let spec = openai_provider_spec("kimi-k2.7-code").expect("kimi-k2.7-code spec");
        // Model defaults to kimi-k2.7-code but can be overridden (e.g. highspeed).
        assert_eq!(spec.resolve_model(None), "kimi-k2.7-code");
        assert_eq!(
            spec.resolve_model(Some("kimi-k2.7-code-highspeed".to_string())),
            "kimi-k2.7-code-highspeed"
        );

        let provider = spec.build("test-key".to_string(), None, None);
        assert_eq!(provider.base_url, "https://api.moonshot.ai/v1/chat/completions");
        assert_eq!(provider.model, "kimi-k2.7-code");
        // No special user agent — uses the shared neenee default.
        assert_eq!(provider.user_agent, NEENEE_USER_AGENT);
        // The registry stamps the preset id onto the concrete provider so
        // assistant responses can be attributed to "kimi-k2.7-code".
        assert_eq!(provider.id, "kimi-k2.7-code");
        assert_eq!(provider.provider_id(), "kimi-k2.7-code");
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
    fn build_provider_returns_mock_for_mock_transport() {
        let channel = Channel {
            id: "default".to_string(),
            label: "Mock".to_string(),
            transport: Transport::Mock,
            api_key: String::new(),
            model: "mock".to_string(),
        };
        let provider = build_provider_for_channel(&channel, "mock");
        assert_eq!(provider.provider_id(), "mock");
    }
}
