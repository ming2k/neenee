//! Provider/channel catalog: the two-layer abstraction over LLM backends.
//!
//! A [`ProviderEntry`] is a configured provider preset (e.g. `zai-code`,
//! `kimi-code`) that owns one or more [`Channel`]s — delivery paths
//! distinguished by transport and endpoint. Each channel references a model by
//! its wire id; intrinsic model metadata (context window, capabilities) is
//! resolved from the [`crate::model`] registry, not duplicated per provider.
//!
//! This module owns the *types* and the provider *construction* path. It is
//! deliberately decoupled from any specific config struct: a [`Channel`] already
//! carries resolved credentials and the wire model id, so constructing a
//! provider from it (see `build_provider_for_channel` in `neenee-providers`)
//! is a pure operation. Resolution (environment variable then config field)
//! lives in the loader, not here, so the same types serve both built-in
//! presets and future user-defined entries.
//!
//! See `docs/adr/0002-model-channel-abstraction.md` for the design.

/// How a [`Channel`] speaks to its model. Determines which `Provider`
/// implementation is constructed for it (in `neenee-providers`).
///
/// Variants carry only the endpoint shape intrinsic to the transport.
/// Per-call credentials and the wire model id live on the [`Channel`] itself,
/// so the same transport serves a built-in preset and a user-defined relay.
#[derive(Debug, Clone)]
pub enum Transport {
    /// OpenAI-compatible chat-completions endpoint at `base_url`. The
    /// `user_agent` is sent verbatim on every request.
    OpenAiCompat {
        base_url: String,
        user_agent: String,
    },
    /// Google Gemini native API (`generativelanguage.googleapis.com`). The model
    /// id and API key are read from the owning [`Channel`].
    GeminiNative,
    /// A local llama.cpp / compatible server at `${base_url}/v1/chat/completions`.
    Llama { base_url: String },
}

impl Transport {
    /// Whether this transport needs an API key at all. Local servers never do;
    /// the cloud transports do.
    pub fn needs_api_key(&self) -> bool {
        match self {
            Transport::Llama { .. } => false,
            Transport::OpenAiCompat { .. } | Transport::GeminiNative => true,
        }
    }
}

/// One delivery path for a [`ProviderEntry`].
///
/// A channel pairs a [`Transport`] with resolved credentials (`api_key`) and
/// the wire `model` id. Built-in presets materialize exactly one channel per
/// entry (id `"default"`); user-defined entries may declare several channels
/// per model (e.g. Gemini via Studio, Vertex, or a relay), with the entry's
/// `default_channel` selecting one. See ADR-0002.
#[derive(Debug, Clone)]
pub struct Channel {
    /// Stable identifier within the model (e.g. `"studio"`, `"vertex"`).
    /// Built-in presets use `"default"`.
    pub id: String,
    /// Display label shown in the picker (e.g. `"Google Studio"`).
    pub label: String,
    /// Endpoint shape and provider implementation selector.
    pub transport: Transport,
    /// Resolved API key (env var first, then config field). Empty for keyless
    /// channels; never absent so construction never branches on `Option`.
    pub api_key: String,
    /// Resolved wire model id sent to the provider.
    pub model: String,
}

impl Channel {
    /// Whether this channel has a usable API key. Keyless transports
    /// ([`Transport::Llama`], the in-memory mock) always report ready; the rest
    /// require a non-empty key.
    pub fn key_ready(&self) -> bool {
        if !self.transport.needs_api_key() {
            return true;
        }
        !self.api_key.trim().is_empty()
    }
}

/// A catalog entry: a provider preset with one or more channels. Each channel
/// references a model by wire id; model metadata (context window, capabilities)
/// is resolved from the [`crate::model`] registry.
#[derive(Debug, Clone)]
pub struct ProviderEntry {
    /// Canonical stable identifier — the provider/preset id
    /// (`"zai-code"`, `"kimi-code"`, ...).
    pub id: String,
    /// Display name (e.g. `"ZAI Code"`).
    pub name: String,
    /// Short human-readable description.
    pub description: String,
    /// Delivery paths for this provider. Phase 1: exactly one per entry.
    pub channels: Vec<Channel>,
    /// Index into `channels` of the preferred path.
    pub default_channel: usize,
    /// `true` for the built-in presets; `false` for user-defined entries.
    pub builtin: bool,
}

impl ProviderEntry {
    /// The preferred channel, or `None` if the entry has no channels.
    pub fn default_channel(&self) -> Option<&Channel> {
        self.channels.get(self.default_channel)
    }

    /// Whether the entry has a usable API key on its default channel. Built-in
    /// keyless entries (local server) always report ready.
    pub fn key_ready(&self) -> bool {
        self.default_channel()
            .map(Channel::key_ready)
            .unwrap_or(true)
    }

    /// The context window (in tokens) of the model on the default channel,
    /// resolved from the model registry. Returns `0` when the entry has no
    /// default channel or the model is not in the registry.
    pub fn context_window(&self) -> usize {
        self.default_channel()
            .map(|ch| crate::model::resolve(&ch.model).context_window)
            .unwrap_or(0)
    }
}

/// Display metadata for a built-in provider preset. Returns `(name,
/// description)`. Model-level metadata (context window, capabilities) lives in
/// the [`crate::model`] registry and is resolved separately. Returns `None` for
/// ids with no built-in metadata; the loader falls back to the raw id as the
/// name in that case.
pub fn builtin_provider_metadata(id: &str) -> Option<(&'static str, &'static str)> {
    let (name, description) = match id {
        "kimi-code" => ("Kimi Code", "Moonshot AI coding model"),
        "openai" => ("OpenAI GPT-4o", "OpenAI API"),
        "gemini" => ("Gemini 2.5 Flash", "Google Gemini 2.5 Flash"),
        "deepseek-v4-flash" => ("DeepSeek V4 Flash", "DeepSeek V4 Flash"),
        "deepseek-v4-pro" => ("DeepSeek V4 Pro", "DeepSeek V4 Pro"),
        "zai-code" => ("ZAI Code", "Z.AI coding plan (GLM-5.2)"),
        "llama" => ("Llama", "Local Llama server"),
        _ => return None,
    };
    Some((name, description))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lookup_is_exact_match() {
        let entries = [ProviderEntry {
            id: "deepseek-v4-flash".to_string(),
            name: "DeepSeek V4 Flash".to_string(),
            description: String::new(),
            channels: vec![Channel {
                id: "default".to_string(),
                label: "DeepSeek V4 Flash".to_string(),
                transport: Transport::OpenAiCompat {
                    base_url: "https://api.deepseek.com/v1/chat/completions".to_string(),
                    user_agent: "agent".to_string(),
                },
                api_key: "k".to_string(),
                model: "deepseek-v4-flash".to_string(),
            }],
            default_channel: 0,
            builtin: true,
        }];
        assert_eq!(
            entries
                .iter()
                .find(|e| e.id == "deepseek-v4-flash")
                .expect("exact id")
                .id,
            "deepseek-v4-flash"
        );
        // No alias mapping: stale ids do not resolve.
        assert!(entries.iter().find(|e| e.id == "deepseek").is_none());
        assert!(entries.iter().find(|e| e.id == "deepseek-flash").is_none());
        assert!(entries.iter().find(|e| e.id == "unknown").is_none());
    }

    #[test]
    fn key_ready_is_true_for_keyless_transports() {
        let llama = Channel {
            id: "default".to_string(),
            label: "Llama".to_string(),
            transport: Transport::Llama {
                base_url: "http://localhost:8080".to_string(),
            },
            api_key: String::new(),
            model: "local-model".to_string(),
        };
        assert!(llama.key_ready(), "llama must be keyless-ready");
    }

    #[test]
    fn key_ready_is_false_for_empty_cloud_key() {
        let channel = Channel {
            id: "default".to_string(),
            label: "OpenAI".to_string(),
            transport: Transport::OpenAiCompat {
                base_url: "https://api.openai.com/v1/chat/completions".to_string(),
                user_agent: "agent".to_string(),
            },
            api_key: "   ".to_string(),
            model: "gpt-4o".to_string(),
        };
        assert!(!channel.key_ready());
    }

    #[test]
    fn builtin_provider_metadata_covers_every_preset() {
        for id in [
            "kimi-code",
            "openai",
            "gemini",
            "deepseek-v4-flash",
            "deepseek-v4-pro",
            "zai-code",
            "llama",
        ] {
            let (name, _) = builtin_provider_metadata(id)
                .unwrap_or_else(|| panic!("missing metadata for {id}"));
            assert!(!name.is_empty());
        }
        assert!(builtin_provider_metadata("unknown").is_none());
    }

    #[test]
    fn context_window_resolves_from_model_registry() {
        let entry = ProviderEntry {
            id: "zai-code".to_string(),
            name: "ZAI Code".to_string(),
            description: String::new(),
            channels: vec![Channel {
                id: "default".to_string(),
                label: "ZAI Code".to_string(),
                transport: Transport::OpenAiCompat {
                    base_url: "https://api.z.ai/api/coding/paas/v4/chat/completions".to_string(),
                    user_agent: "agent".to_string(),
                },
                api_key: "k".to_string(),
                model: "glm-5.2".to_string(),
            }],
            default_channel: 0,
            builtin: true,
        };
        assert_eq!(entry.context_window(), 1_000_000);
    }
}
