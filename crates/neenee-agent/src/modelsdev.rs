//! models.dev catalog integration — dynamic provider/model discovery.
//!
//! neenee mirrors `https://models.dev/api.json` (the same catalog opencode uses)
//! so adding a provider or model on the models.dev side appears in neenee with
//! **zero code changes**: the catalog's `build_catalog` reads the cached mirror
//! and derives provider endpoints, model lists, context windows, and wire
//! formats (`OpenAI /chat/completions` vs `Anthropic /messages` vs `Gemini`)
//! from it.
//!
//! ## Lifecycle
//!
//! - **Refresh** (`refresh()`): async, called at startup and every 60 minutes.
//!   Fetches the catalog over HTTP and atomically writes it to
//!   [`paths::Dirs::models_dev_cache`]. Failures are logged and swallowed — a
//!   stale or absent cache never blocks startup.
//! - **Load** (`load()`): sync, called by `build_catalog`. Reads the cache file
//!   and deserializes it. Returns `None` when the file is missing or corrupt;
//!   the catalog then falls back to the compiled-in [`KNOWN_MODELS`] registry.
//!
//! ## Wire format
//!
//! models.dev records the wire protocol as an `npm` package name (the AI SDK
//! package a provider uses). [`wire_format_from_npm`] maps that to neenee's
//! [`WireFormat`], selecting the transport and endpoint suffix. A model may
//! override its provider's format via a model-level `provider.npm` (e.g.
//! opencode-go serves MiniMax via `@ai-sdk/anthropic` while the provider's
//! default is `@ai-sdk/openai-compatible`).
//!
//! [`KNOWN_MODELS`]: neenee_core::model::KNOWN_MODELS
//! [`WireFormat`]: neenee_core::WireFormat

use std::collections::HashMap;
use std::time::Duration;

use neenee_core::DynamicCatalog;
use neenee_core::WireFormat;
use neenee_store::cache::CachedResource;
use neenee_store::paths;
use serde::Deserialize;

/// The models.dev catalog URL. Pinned to HTTPS; a future env override could
/// point at a mirror for offline/air-gapped setups.
const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// How often the background loop refreshes the models.dev catalog.
const REFRESH_PERIOD: Duration = Duration::from_secs(60 * 60);

/// A [`DynamicCatalog`] implementation for the models.dev provider/model
/// directory. Refreshes from the public API, caches to
/// [`paths::Dirs::models_dev_cache`], and falls back to the compiled-in
/// `KNOWN_MODELS` registry when the cache is absent. Construct one and pass it
/// to [`spawn_refresh`](crate::dynamic::spawn_refresh) to drive periodic
/// refresh.
pub struct ModelsDevCatalog;

impl DynamicCatalog for ModelsDevCatalog {
    fn id(&self) -> &'static str {
        "models-dev"
    }

    async fn refresh(&self) -> Result<(), String> {
        refresh().await
    }

    fn refresh_period(&self) -> Duration {
        REFRESH_PERIOD
    }
}

/// A provider entry in the models.dev catalog. Fields are a subset of what
/// models.dev publishes — only what neenee's catalog construction needs.
#[derive(Debug, Deserialize)]
pub struct ModelsDevProvider {
    pub id: String,
    pub name: String,
    /// Environment variable name(s) consulted for the API key. The first
    /// non-empty one resolves the key.
    #[serde(default)]
    pub env: Vec<String>,
    /// The AI SDK npm package — the primary wire-format signal. Maps to a
    /// [`WireFormat`] via [`wire_format_from_npm`].
    #[serde(default)]
    pub npm: String,
    /// API base URL (e.g. `https://opencode.ai/zen/go/v1`). The endpoint suffix
    /// (`/chat/completions`, `/messages`) is appended based on the wire format.
    #[serde(default)]
    pub api: String,
    /// Models served by this provider. A dict keyed by model id.
    pub models: HashMap<String, ModelsDevModel>,
}

/// A model entry in the models.dev catalog.
#[derive(Debug, Deserialize)]
pub struct ModelsDevModel {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub tool_call: bool,
    #[serde(default)]
    pub attachment: bool,
    pub limit: ModelsDevLimit,
    /// Model-level override of the wire format. When present, `npm` selects the
    /// transport for this model regardless of the provider's default (e.g.
    /// opencode-go's MiniMax models carry `@ai-sdk/anthropic`).
    #[serde(default)]
    pub provider: Option<ModelsDevModelProvider>,
}

/// A model's `provider` sub-object (the model-level override).
#[derive(Debug, Deserialize)]
pub struct ModelsDevModelProvider {
    /// Overrides the wire format for this model.
    #[serde(default)]
    pub npm: String,
}

#[derive(Debug, Deserialize)]
pub struct ModelsDevLimit {
    #[serde(default)]
    pub context: u64,
    #[serde(default)]
    pub output: u64,
}

/// Map an AI SDK `npm` package name to neenee's [`WireFormat`]. This is the
/// single place that translates models.dev's wire-format signal into a
/// transport selection. Unknown packages default to OpenAI-compatible (the
/// common case for OpenAI-format relays).
pub fn wire_format_from_npm(npm: &str) -> WireFormat {
    if npm.contains("anthropic") {
        WireFormat::Anthropic
    } else if npm.contains("google") || npm.contains("vertex") {
        WireFormat::Gemini
    } else {
        // @ai-sdk/openai, @ai-sdk/openai-compatible, and anything unknown
        // speak the OpenAI chat-completions wire format.
        WireFormat::OpenAiCompat
    }
}

/// The endpoint suffix appended to a provider's `api` base for a given wire
/// format. OpenAI-compatible → `/chat/completions`; Anthropic → `/messages`.
/// Gemini uses a different URL shape entirely and is handled by its native
/// transport (no suffix here).
pub fn endpoint_suffix(format: WireFormat) -> &'static str {
    match format {
        WireFormat::Anthropic => "/messages",
        WireFormat::OpenAiCompat => "/chat/completions",
        WireFormat::Gemini => "",
        WireFormat::Llama => "/chat/completions",
    }
}

/// Resolve the wire format for a specific model under a provider: the model's
/// own `provider.npm` override wins, otherwise the provider's `npm`.
pub fn model_wire_format(provider: &ModelsDevProvider, model: &ModelsDevModel) -> WireFormat {
    if let Some(mp) = &model.provider {
        if !mp.npm.is_empty() {
            return wire_format_from_npm(&mp.npm);
        }
    }
    wire_format_from_npm(&provider.npm)
}

/// Refresh the models.dev cache: fetch the catalog over HTTP and atomically
/// write it to the cache path. Best-effort — errors are logged and returned so
/// the caller (startup) can continue with a stale cache. Safe to call
/// concurrently: the atomic write makes the final file consistent; a concurrent
/// refresh just overwrites with a fresh copy.
pub async fn refresh() -> Result<(), String> {
    let cache = CachedResource::new(paths::get().models_dev_cache());
    tracing::debug!(url = MODELS_DEV_URL, path = ?cache.path(), "refreshing models.dev catalog");
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("models.dev: build client: {e}"))?
        .get(MODELS_DEV_URL)
        .header("User-Agent", "neenee/models-dev")
        .send()
        .await
        .map_err(|e| format!("models.dev: fetch: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("models.dev: HTTP {}", response.status()));
    }
    let text = response
        .text()
        .await
        .map_err(|e| format!("models.dev: read body: {e}"))?;
    // Validate it parses as JSON before writing, so a corrupt download never
    // replaces a good cache.
    serde_json::from_str::<HashMap<String, ModelsDevProvider>>(&text)
        .map_err(|e| format!("models.dev: parse: {e}"))?;
    cache.store(&text)?;
    tracing::info!("models.dev catalog refreshed");
    Ok(())
}

/// Load the cached models.dev catalog synchronously. Returns `None` when the
/// cache is missing or corrupt — the caller falls back to the compiled-in
/// `KNOWN_MODELS` registry. Never panics; a missing cache is a normal
/// first-run condition.
pub fn load() -> Option<HashMap<String, ModelsDevProvider>> {
    CachedResource::new(paths::get().models_dev_cache()).load_json()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npm_mapping_covers_known_packages() {
        assert_eq!(
            wire_format_from_npm("@ai-sdk/anthropic"),
            WireFormat::Anthropic
        );
        assert_eq!(
            wire_format_from_npm("@ai-sdk/openai-compatible"),
            WireFormat::OpenAiCompat
        );
        assert_eq!(
            wire_format_from_npm("@ai-sdk/openai"),
            WireFormat::OpenAiCompat
        );
        assert_eq!(wire_format_from_npm("@ai-sdk/google"), WireFormat::Gemini);
        assert_eq!(
            wire_format_from_npm("@ai-sdk/google-vertex"),
            WireFormat::Gemini
        );
        // Unknown packages default to OpenAI-compatible.
        assert_eq!(
            wire_format_from_npm("@ai-sdk/unknown-future"),
            WireFormat::OpenAiCompat
        );
    }

    #[test]
    fn endpoint_suffix_matches_format() {
        assert_eq!(endpoint_suffix(WireFormat::Anthropic), "/messages");
        assert_eq!(
            endpoint_suffix(WireFormat::OpenAiCompat),
            "/chat/completions"
        );
    }

    #[test]
    fn model_override_beats_provider_default() {
        // opencode-go's provider npm is openai-compatible, but its MiniMax
        // models override to anthropic — the model-level override must win.
        let provider = ModelsDevProvider {
            id: "opencode-go".to_string(),
            name: "OpenCode Go".to_string(),
            env: vec!["OPENCODE_API_KEY".to_string()],
            npm: "@ai-sdk/openai-compatible".to_string(),
            api: "https://opencode.ai/zen/go/v1".to_string(),
            models: HashMap::new(),
        };
        let minimax = ModelsDevModel {
            id: "minimax-m3".to_string(),
            name: "MiniMax M3".to_string(),
            family: "minimax".to_string(),
            reasoning: true,
            tool_call: true,
            attachment: false,
            limit: ModelsDevLimit {
                context: 512000,
                output: 131072,
            },
            provider: Some(ModelsDevModelProvider {
                npm: "@ai-sdk/anthropic".to_string(),
            }),
        };
        let glm = ModelsDevModel {
            id: "glm-5.2".to_string(),
            name: "GLM-5.2".to_string(),
            family: "glm".to_string(),
            reasoning: true,
            tool_call: true,
            attachment: false,
            limit: ModelsDevLimit {
                context: 1000000,
                output: 131072,
            },
            provider: None,
        };
        assert_eq!(
            model_wire_format(&provider, &minimax),
            WireFormat::Anthropic
        );
        assert_eq!(model_wire_format(&provider, &glm), WireFormat::OpenAiCompat);
    }

    #[test]
    fn parses_minimal_catalog() {
        // A minimal but valid catalog fragment exercises the deserialization.
        let json = r#"{
            "opencode-go": {
                "id": "opencode-go",
                "name": "OpenCode Go",
                "env": ["OPENCODE_API_KEY"],
                "npm": "@ai-sdk/openai-compatible",
                "api": "https://opencode.ai/zen/go/v1",
                "models": {
                    "glm-5.2": {
                        "id": "glm-5.2",
                        "name": "GLM-5.2",
                        "family": "glm",
                        "reasoning": true,
                        "tool_call": true,
                        "attachment": false,
                        "limit": {"context": 1000000, "output": 131072}
                    },
                    "minimax-m3": {
                        "id": "minimax-m3",
                        "name": "MiniMax M3",
                        "reasoning": true,
                        "tool_call": true,
                        "attachment": false,
                        "limit": {"context": 512000, "output": 131072},
                        "provider": {"npm": "@ai-sdk/anthropic"}
                    }
                }
            }
        }"#;
        let catalog: HashMap<String, ModelsDevProvider> =
            serde_json::from_str(json).expect("parses");
        let og = &catalog["opencode-go"];
        assert_eq!(og.models.len(), 2);
        assert_eq!(
            model_wire_format(og, &og.models["glm-5.2"]),
            WireFormat::OpenAiCompat
        );
        assert_eq!(
            model_wire_format(og, &og.models["minimax-m3"]),
            WireFormat::Anthropic
        );
    }
}
