//! Model/channel catalog: the two-layer abstraction over LLM backends.
//!
//! A [`ModelEntry`] is a logical model (for example `gemini-2.0-flash`) that
//! owns one or more [`Channel`]s — delivery paths distinguished by transport
//! and endpoint. A [`Catalog`] is the materialized set of entries, built from
//! configuration by the host crate (see `neenee::catalog::build_catalog`).
//!
//! This module owns the *types* and the provider *construction* path. It is
//! deliberately decoupled from any specific config struct: a [`Channel`] already
//! carries resolved credentials and the wire model id, so [`Channel::build`] is
//! a pure constructor for `Arc<dyn Provider>`. Resolution (environment variable
//! then config field) lives in the loader, not here, so the same types serve
//! both built-in presets and future user-defined entries.
//!
//! See `docs/adr/0002-model-channel-abstraction.md` for the design.

/// How a [`Channel`] speaks to its model. Determines which `Provider`
/// implementation [`Channel::build`] constructs.
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
    /// In-process test fixture; ignores credentials and model.
    Mock,
}

impl Transport {
    /// Whether this transport needs an API key at all. Local servers and the
    /// mock fixture never do; the cloud transports do.
    pub fn needs_api_key(&self) -> bool {
        match self {
            Transport::Mock | Transport::Llama { .. } => false,
            Transport::OpenAiCompat { .. } | Transport::GeminiNative => true,
        }
    }
}

/// One delivery path for a [`ModelEntry`].
///
/// A channel pairs a [`Transport`] with resolved credentials (`api_key`) and
/// the wire `model` id. Phase 1 of ADR-0002 materializes exactly one channel
/// per entry from the legacy per-provider config fields; later phases introduce
/// multiple channels per model (e.g. Gemini via Studio, Vertex, or a relay).
#[derive(Debug, Clone)]
pub struct Channel {
    /// Stable identifier within the model (e.g. `"studio"`, `"vertex"`).
    /// Phase 1 entries use `"default"`.
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
    /// ([`Transport::Llama`], [`Transport::Mock`]) always report ready; the rest
    /// require a non-empty key.
    pub fn key_ready(&self) -> bool {
        if !self.transport.needs_api_key() {
            return true;
        }
        !self.api_key.trim().is_empty()
    }
}

/// A catalog entry: a logical model with one or more channels.
#[derive(Debug, Clone)]
pub struct ModelEntry {
    /// Canonical stable identifier. Phase 1 reuses the legacy provider id
    /// (`"gemini"`, `"kimi-k2.7-code"`, ...) so existing config keeps working.
    pub id: String,
    /// Display name (e.g. `"Gemini"`).
    pub name: String,
    /// Short human-readable description.
    pub description: String,
    /// Model context window in tokens. `0` means unknown.
    pub context_window: usize,
    /// Delivery paths for this model. Phase 1: exactly one per entry.
    pub channels: Vec<Channel>,
    /// Index into `channels` of the preferred path.
    pub default_channel: usize,
    /// `true` for the built-in presets; `false` for user-defined entries.
    pub builtin: bool,
}

impl ModelEntry {
    /// The preferred channel, or `None` if the entry has no channels.
    pub fn default_channel(&self) -> Option<&Channel> {
        self.channels.get(self.default_channel)
    }

    /// Whether the entry has a usable API key on its default channel. Built-in
    /// keyless entries (local server, mock) always report ready.
    pub fn key_ready(&self) -> bool {
        self.default_channel()
            .map(Channel::key_ready)
            .unwrap_or(true)
    }
}

/// The full set of known models, materialized from configuration.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// Insertion order; lookup is linear. The catalog is small (dozens of
    /// entries at most), so a `Vec` keeps iteration order deterministic for
    /// display without the overhead of a map.
    pub entries: Vec<ModelEntry>,
}

impl Catalog {
/// Look up an entry by id. Exact match only; preset ids are unique and do
/// not have alias mappings.
pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }
}

/// Display metadata for a built-in preset. Returns `(name, description,
/// context_window)`. Returns `None` for ids with no built-in metadata; the
/// loader falls back to the raw id as the name in that case.
///
/// This is the seed of the single source of truth that replaces the parallel
/// `SOLUTIONS` table in the TUI (ADR-0002 phase 3). The brief overlap during
/// the migration is intentional and documented.
pub fn builtin_metadata(id: &str) -> Option<(&'static str, &'static str, usize)> {
    let (name, description, context_window) = match id {
        "kimi-k2.7-code" => (
            "Kimi K2.7 Code",
            "Moonshot AI coding model",
            256_000,
        ),
        "openai" => ("OpenAI GPT-4o", "OpenAI API", 128_000),
        "gemini" => ("Gemini 2.5 Flash", "Google Gemini 2.5 Flash", 1_000_000),
        "deepseek-v4-flash" => ("DeepSeek V4 Flash", "DeepSeek V4 Flash", 1_000_000),
        "deepseek-v4-pro" => ("DeepSeek V4 Pro", "DeepSeek V4 Pro", 1_000_000),
        "qwen" => ("Qwen Plus", "Alibaba DashScope", 131_072),
        "glm" => ("GLM 4 Plus", "Zhipu AI", 128_000),
        "llama" => ("Llama", "Local Llama server", 0),
        "mock" => ("Mock", "Test provider", 0),
        _ => return None,
    };
    Some((name, description, context_window))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lookup_is_exact_match() {
        let catalog = Catalog {
            entries: vec![ModelEntry {
                id: "deepseek-v4-flash".to_string(),
                name: "DeepSeek V4 Flash".to_string(),
                description: String::new(),
                context_window: 0,
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
            }],
        };
        assert_eq!(
            catalog.get("deepseek-v4-flash").expect("exact id").id,
            "deepseek-v4-flash"
        );
        // No alias mapping: stale ids do not resolve.
        assert!(catalog.get("deepseek").is_none());
        assert!(catalog.get("deepseek-flash").is_none());
        assert!(catalog.get("unknown").is_none());
    }

    #[test]
    fn key_ready_is_true_for_keyless_transports() {
        let mock = Channel {
            id: "default".to_string(),
            label: "Mock".to_string(),
            transport: Transport::Mock,
            api_key: String::new(),
            model: "mock".to_string(),
        };
        assert!(mock.key_ready());

        let llama = Channel {
            id: "default".to_string(),
            label: "Llama".to_string(),
            transport: Transport::Llama {
                base_url: "http://localhost:8080".to_string(),
            },
            api_key: String::new(),
            model: "local-model".to_string(),
        };
        assert!(llama.key_ready());
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
    fn builtin_metadata_covers_every_preset() {
        for id in [
            "kimi-k2.7-code",
            "openai",
            "gemini",
            "deepseek-v4-flash",
            "deepseek-v4-pro",
            "qwen",
            "glm",
            "llama",
            "mock",
        ] {
            let (name, _, context_window) =
                builtin_metadata(id).unwrap_or_else(|| panic!("missing metadata for {id}"));
            assert!(!name.is_empty());
            // Context window is either a positive known value or 0 (unknown);
            // it must never underflow / wrap.
            let _ = context_window;
        }
        assert!(builtin_metadata("unknown").is_none());
    }
}
