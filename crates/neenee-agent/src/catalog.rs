//! Materializes a `Catalog` from the host crate's [`Config`].
//!
//! This is the single source of truth for the environment-variable-then-config
//! resolution rules that startup and runtime provider switching share. Every
//! [`Channel`] produced here carries fully resolved credentials and model id, so
//! provider construction (`build_provider_for_channel` in `neenee-providers`)
//! never touches the environment or config again.
//!
//! ADR-0002: built-in presets produce one `"default"` channel per entry from
//! the per-provider fields, while user-defined entries may declare several
//! channels (with `default_channel` selecting one). Favorites and recency are
//! layered on top via the provider-usage telemetry.

use neenee_core::catalog::{Channel, ProviderEntry, Transport, builtin_provider_metadata};
use neenee_core::{ProviderPickerRow, ProviderPickerSnapshot, WireFormat};
use neenee_providers::{NEENEE_USER_AGENT, OPENAI_PROVIDER_SPECS, OpenAiProviderSpec};
use neenee_store::config::{Config, UserChannelConfig, UserProviderConfig, UserTransport};
use neenee_store::provider_usage::ProviderUsage;

use crate::modelsdev::{self, ModelsDevProvider};

/// The effective default provider id from `config.default_provider`.
pub fn default_provider_id(config: &Config) -> &str {
    &config.default_provider
}

/// Convert a user-defined channel config into a resolved [`Channel`].
///
/// Resolution rules mirror the built-in path: an `api_key_env` value wins over
/// an inline `api_key` (and empty env values fall through, just like built-ins);
/// the wire `model` falls back to the parent model's id; transport-specific
/// fields (`base_url`, `user_agent`) fall back to localhost defaults so a
/// minimal entry still builds.
fn user_channel_to_channel(uc: &UserChannelConfig, fallback_model: &str) -> Channel {
    let api_key = env_or_config(uc.api_key_env.as_deref(), uc.api_key.clone()).unwrap_or_default();
    let model = uc
        .model
        .clone()
        .unwrap_or_else(|| fallback_model.to_string());
    let transport = match uc.transport {
        UserTransport::GeminiNative => Transport::GeminiNative,
        UserTransport::Llama => Transport::Llama {
            base_url: uc
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:8080".to_string()),
        },
        UserTransport::Anthropic => Transport::Anthropic {
            base_url: uc
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:8080/v1/messages".to_string()),
            user_agent: uc
                .user_agent
                .clone()
                .unwrap_or_else(|| NEENEE_USER_AGENT.to_string()),
        },
        UserTransport::OpenAiCompat => Transport::OpenAiCompat {
            base_url: uc
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:8080/v1/chat/completions".to_string()),
            user_agent: uc
                .user_agent
                .clone()
                .unwrap_or_else(|| NEENEE_USER_AGENT.to_string()),
        },
    };
    Channel {
        id: uc.label.clone(),
        label: uc.label.clone(),
        transport,
        api_key,
        model,
    }
}

/// Convert a user-defined model config into a resolved [`ProviderEntry`]. Reuses
/// built-in display metadata (name / description / context window) when the id
/// matches a built-in, so overriding e.g. `gemini` inherits its friendly name
/// unless the user supplies their own. A model with no channels renders but is
/// not usable until the user supplies one.
fn user_provider_to_entry(um: &UserProviderConfig) -> ProviderEntry {
    let builtin = builtin_provider_metadata(&um.id);
    let name = um
        .name
        .clone()
        .or_else(|| builtin.map(|(n, _)| n.to_string()))
        .unwrap_or_else(|| um.id.clone());
    let description = builtin.map(|(_, d)| d.to_string()).unwrap_or_default();
    let fallback_model = um.id.clone();
    let channels: Vec<Channel> = um
        .channels
        .iter()
        .map(|c| user_channel_to_channel(c, &fallback_model))
        .collect();
    let default_channel = um.default_channel.min(channels.len().saturating_sub(1));
    ProviderEntry {
        id: um.id.clone(),
        name,
        description,
        channels,
        default_channel,
        builtin: false,
    }
}

/// Resolve `env_var` first, then `config_value`. Empty and whitespace-only env
/// values are treated as unset and fall through to config, which unifies the
/// pre-catalog construction and readiness paths on one sensible rule: an empty
/// API key or model is never useful, so an empty env var never silently wins.
fn env_or_config(env_var: Option<&str>, config_value: Option<String>) -> Option<String> {
    env_var
        .and_then(|name| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty())
        .or(config_value)
}

/// The per-provider API key stored in config. Centralized so the catalog is the
/// only place that maps a model id to its config field. Replaces the former
/// `config_api_key` free function in `main.rs`.
fn config_key_for(config: &Config, id: &str) -> Option<String> {
    match id {
        "openai" => config.openai_api_key.clone(),
        "gemini" => config.gemini_api_key.clone(),
        "kimi-code" => config.moonshot_api_key.clone(),
        "deepseek-v4-flash" | "deepseek-v4-pro" => config.deepseek_api_key.clone(),
        "zai-code" => config.zai_api_key.clone(),
        "opencode-go" => config.opencode_go_api_key.clone(),
        _ => None,
    }
}

/// The per-provider model override stored in config. Replaces the former
/// `config_model` free function in `main.rs`.
fn config_model_for(config: &Config, id: &str) -> Option<String> {
    match id {
        "openai" => config.openai_model.clone(),
        "gemini" => config.gemini_model.clone(),
        "llama" => config.llama_model.clone(),
        "kimi-code" => config.moonshot_model.clone(),
        "deepseek-v4-flash" => config.deepseek_flash_model.clone(),
        "deepseek-v4-pro" => config.deepseek_pro_model.clone(),
        "zai-code" => config.zai_model.clone(),
        // opencode-go is multi-model: the active model lives in the shared
        // `default_model` field, not a per-provider slot.
        "opencode-go" => config.default_model.clone(),
        _ => None,
    }
}

/// Attach the built-in display metadata (name, description) to a raw `(id,
/// channels)` pair. Model-level metadata (context window, capabilities) is
/// resolved on demand from the model registry via [`ProviderEntry::context_window`].
/// Falls back to the raw id as the name when no metadata is registered.
fn entry_with_metadata(id: &str, channels: Vec<Channel>, builtin: bool) -> ProviderEntry {
    let (name, description) = builtin_provider_metadata(id)
        .map(|(n, d)| (n.to_string(), d.to_string()))
        .unwrap_or_else(|| (id.to_string(), String::new()));
    ProviderEntry {
        id: id.to_string(),
        name,
        description,
        channels,
        default_channel: 0,
        builtin,
    }
}

/// Build a single-channel entry for an OpenAI-compatible registry preset.
fn openai_compat_entry_from_spec(config: &Config, spec: &OpenAiProviderSpec) -> ProviderEntry {
    let api_key =
        env_or_config(Some(spec.env_api_key), config_key_for(config, spec.id)).unwrap_or_default();
    // A pinned `fixed_model` always wins; otherwise env override, then config,
    // then the spec default.
    let model = if let Some(fixed) = spec.fixed_model {
        fixed.to_string()
    } else {
        env_or_config(Some(spec.env_model), config_model_for(config, spec.id))
            .unwrap_or_else(|| spec.default_model.to_string())
    };
    let user_agent = spec
        .default_user_agent
        .unwrap_or(NEENEE_USER_AGENT)
        .to_string();
    let name = builtin_provider_metadata(spec.id)
        .map(|(n, _)| n.to_string())
        .unwrap_or_else(|| spec.id.to_string());
    let channel = Channel {
        id: "default".to_string(),
        label: name.clone(),
        transport: Transport::OpenAiCompat {
            base_url: spec.base_url.to_string(),
            user_agent,
        },
        api_key,
        model,
    };
    entry_with_metadata(spec.id, vec![channel], true)
}

/// The models.dev provider ids neenee treats as "catalog-driven": their model
/// lists, wire formats, and endpoints come entirely from the models.dev
/// mirror, so adding a model there appears here with zero code changes. Any
/// provider id in this set whose models.dev entry exists and whose API key
/// resolves gets a catalog entry built from the directory.
const CATALOG_DRIVEN_PROVIDERS: &[&str] = &["opencode-go"];

/// Build a catalog entry for a models.dev-driven provider. Every model the
/// directory lists becomes a channel; the transport (OpenAI `/chat/completions`
/// vs Anthropic `/messages`) is derived from the model's wire format, which is
/// itself derived from the model's `provider.npm` override or the provider's
/// `npm`. The API key resolves from the provider's `env` field or the
/// per-provider config slot.
///
/// This is the opencode-style "zero hardcoding" path: models.dev is the source
/// of truth for what models exist and how to reach them. When the cache is
/// absent (first run, offline), the caller falls back to the compiled-in
/// `KNOWN_MODELS` registry via [`fallback_catalog_driven_entry`].
fn catalog_driven_entry(config: &Config, provider: &ModelsDevProvider) -> ProviderEntry {
    let api_key = provider
        .env
        .first()
        .and_then(|env_var| env_or_config(Some(env_var), config_key_for(config, &provider.id)))
        .unwrap_or_default();
    let user_agent = NEENEE_USER_AGENT.to_string();
    let base = if provider.api.is_empty() {
        String::new()
    } else {
        provider.api.clone()
    };
    // Stable display order: sort by model id so the picker list is predictable.
    let mut models: Vec<_> = provider.models.values().collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    let channels: Vec<Channel> = models
        .iter()
        .map(|m| {
            let format = modelsdev::model_wire_format(provider, m);
            let suffix = modelsdev::endpoint_suffix(format);
            let full_url = if base.is_empty() {
                String::new()
            } else {
                format!("{base}{suffix}")
            };
            let transport = match format {
                WireFormat::Anthropic => Transport::Anthropic {
                    base_url: full_url,
                    user_agent: user_agent.clone(),
                },
                WireFormat::Gemini => Transport::GeminiNative,
                _ => Transport::OpenAiCompat {
                    base_url: full_url,
                    user_agent: user_agent.clone(),
                },
            };
            Channel {
                id: m.id.clone(),
                label: m.name.clone(),
                transport,
                api_key: api_key.clone(),
                model: m.id.clone(),
            }
        })
        .collect();
    let default_channel = config
        .default_model
        .as_deref()
        .and_then(|m| channels.iter().position(|c| c.model == m))
        .unwrap_or(0);
    let (name, description) = builtin_provider_metadata(&provider.id)
        .map(|(n, d)| (n.to_string(), d.to_string()))
        .unwrap_or_else(|| (provider.name.clone(), String::new()));
    ProviderEntry {
        id: provider.id.clone(),
        name,
        description,
        channels,
        default_channel,
        builtin: true,
    }
}

/// A compiled-in fallback for a catalog-driven provider when the models.dev
/// cache is absent (first run, offline). Uses the `KNOWN_MODELS` registry to
/// produce a best-effort entry so the provider is still selectable. Once the
/// cache is refreshed the dynamic entry replaces this on the next catalog
/// rebuild.
fn fallback_catalog_driven_entry(config: &Config, provider_id: &str) -> ProviderEntry {
    let api_key = env_or_config(Some("OPENCODE_API_KEY"), config.opencode_go_api_key.clone())
        .unwrap_or_default();
    let user_agent = NEENEE_USER_AGENT.to_string();
    // Derive the endpoint root from the known provider id. This is the only
    // hardcoding left, and only on the offline fallback path.
    let base = match provider_id {
        "opencode-go" => "https://opencode.ai/zen/go/v1",
        _ => "",
    };
    // Every known model whose format resolves to a served model gets a channel.
    // This is a subset (only models in KNOWN_MODELS), but it keeps the provider
    // usable offline.
    let channels: Vec<Channel> = neenee_core::KNOWN_MODELS
        .iter()
        .filter(|m| {
            // Only include models relevant to this provider. opencode-go serves
            // the open coding models; a precise filter isn't possible offline,
            // so include models that are commonly served by relays.
            matches!(
                m.family,
                "glm" | "kimi" | "deepseek" | "mimo" | "minimax" | "qwen"
            )
        })
        .map(|m| {
            let suffix = modelsdev::endpoint_suffix(m.format);
            let full_url = format!("{base}{suffix}");
            let transport = match m.format {
                WireFormat::Anthropic => Transport::Anthropic {
                    base_url: full_url,
                    user_agent: user_agent.clone(),
                },
                _ => Transport::OpenAiCompat {
                    base_url: full_url,
                    user_agent: user_agent.clone(),
                },
            };
            Channel {
                id: m.id.to_string(),
                label: m.name.to_string(),
                transport,
                api_key: api_key.clone(),
                model: m.id.to_string(),
            }
        })
        .collect();
    let default_channel = config
        .default_model
        .as_deref()
        .and_then(|m| channels.iter().position(|c| c.model == m))
        .unwrap_or(0);
    let (name, description) = builtin_provider_metadata(provider_id)
        .map(|(n, d)| (n.to_string(), d.to_string()))
        .unwrap_or_else(|| (provider_id.to_string(), String::new()));
    ProviderEntry {
        id: provider_id.to_string(),
        name,
        description,
        channels,
        default_channel,
        builtin: true,
    }
}

/// Build the catalog by materializing every known provider from `config`.
///
/// Order is registry presets first, then bespoke providers, then the mock
/// fixture. Order does not affect behavior — all lookups are by id — but a
/// stable order makes the catalog readable in debug output and (later) the
/// picker's default pre-search listing.
pub fn build_catalog(config: &Config) -> Vec<ProviderEntry> {
    let mut entries: Vec<ProviderEntry> = Vec::new();

    // OpenAI-compatible registry presets.
    for spec in OPENAI_PROVIDER_SPECS {
        entries.push(openai_compat_entry_from_spec(config, spec));
    }

    // Bespoke OpenAI (not in the registry, but same transport).
    let openai_api_key =
        env_or_config(Some("OPENAI_API_KEY"), config.openai_api_key.clone()).unwrap_or_default();
    let openai_model = env_or_config(Some("OPENAI_MODEL"), config.openai_model.clone())
        .unwrap_or_else(|| "gpt-4o".to_string());
    entries.push(entry_with_metadata(
        "openai",
        vec![Channel {
            id: "default".to_string(),
            label: "OpenAI".to_string(),
            transport: Transport::OpenAiCompat {
                base_url: "https://api.openai.com/v1/chat/completions".to_string(),
                user_agent: NEENEE_USER_AGENT.to_string(),
            },
            api_key: openai_api_key,
            model: openai_model,
        }],
        true,
    ));

    // Bespoke Gemini (native API, not OpenAI-compatible).
    let gemini_api_key =
        env_or_config(Some("GEMINI_API_KEY"), config.gemini_api_key.clone()).unwrap_or_default();
    let gemini_model = env_or_config(Some("GEMINI_MODEL"), config.gemini_model.clone())
        .unwrap_or_else(|| "gemini-2.5-flash".to_string());
    entries.push(entry_with_metadata(
        "gemini",
        vec![Channel {
            id: "default".to_string(),
            label: "Gemini 2.5 Flash".to_string(),
            transport: Transport::GeminiNative,
            api_key: gemini_api_key,
            model: gemini_model,
        }],
        true,
    ));

    // Local llama.cpp / compatible server. Keyless: no API key resolution.
    let llama_model = env_or_config(Some("LLAMA_MODEL"), config.llama_model.clone())
        .unwrap_or_else(|| "local-model".to_string());
    let llama_base_url = env_or_config(Some("LLAMA_BASE_URL"), config.llama_base_url.clone())
        .unwrap_or_else(|| "http://localhost:8080".to_string());
    entries.push(entry_with_metadata(
        "llama",
        vec![Channel {
            id: "default".to_string(),
            label: "Llama".to_string(),
            transport: Transport::Llama {
                base_url: llama_base_url,
            },
            api_key: String::new(),
            model: llama_model,
        }],
        true,
    ));

    // Catalog-driven providers (opencode-go): model lists, wire formats, and
    // endpoints come from the models.dev mirror — zero hardcoding. When the
    // cache is present each provider gets a dynamic entry built from the
    // directory; when absent (first run, offline) a compiled-in fallback keeps
    // the provider selectable.
    let models_dev = modelsdev::load();
    for &pid in CATALOG_DRIVEN_PROVIDERS {
        let entry = match models_dev.as_ref().and_then(|c| c.get(pid)) {
            Some(provider) => catalog_driven_entry(config, provider),
            None => fallback_catalog_driven_entry(config, pid),
        };
        entries.push(entry);
    }

    // User-defined models: override built-ins by id, or
    // append new models. A user entry may carry several channels, finally
    // enabling multi-channel delivery (e.g. Gemini via Studio and Vertex).
    for user_entry in config.providers.iter().map(user_provider_to_entry) {
        if let Some(existing) = entries.iter_mut().find(|e| e.id == user_entry.id) {
            *existing = user_entry;
        } else {
            entries.push(user_entry);
        }
    }

    entries
}

/// Resolve the active provider for a given provider id from `config`. Returns
/// the mock provider when the id is unknown or the entry has no usable channel,
/// so callers never have to branch on absence.
///
/// Channel selection honors `config.default_model`: for a multi-model provider
/// like opencode-go, the channel carrying that model (and thus the matching
/// transport) is chosen; otherwise the entry's default channel is used. This is
/// the single replacement for the resolution logic that used to be duplicated
/// at startup and in the `SwitchProvider` handler.
pub fn build_provider_for(config: &Config, id: &str) -> std::sync::Arc<dyn neenee_core::Provider> {
    build_provider_for_model(config, id, config.default_model.as_deref())
}

/// Resolve the provider for `provider_id`, selecting the channel that carries
/// `model_id` when given (falling back to `config.default_model`, then the
/// entry's default channel). Runtime switches that carry an explicit model
/// (e.g. selecting `minimax-m3` under opencode-go) route through here so the
/// per-model transport is picked correctly.
pub fn build_provider_for_model(
    config: &Config,
    provider_id: &str,
    model_id: Option<&str>,
) -> std::sync::Arc<dyn neenee_core::Provider> {
    let entries = build_catalog(config);
    let Some(entry) = entries.iter().find(|e| e.id == provider_id) else {
        return std::sync::Arc::new(neenee_providers::MockProvider);
    };
    let wanted = model_id.or(config.default_model.as_deref());
    let channel = wanted
        .and_then(|m| entry.channel_for_model(m))
        .or_else(|| entry.default_channel());
    match channel {
        Some(channel) => neenee_providers::build_provider_for_channel(channel, &entry.id),
        None => std::sync::Arc::new(neenee_providers::MockProvider),
    }
}

/// The display model name for a given provider id, as resolved from `config`.
/// Falls back to `"mock-model"` when the id is unknown. Replaces the former
/// `initial_m_name` block in `main.rs`.
///
/// For multi-model providers, the active model is `config.default_model` when
/// set (and served by the provider); otherwise the entry's default-channel
/// model.
pub fn resolved_model_name(config: &Config, id: &str) -> String {
    build_catalog(config)
        .iter()
        .find(|e| e.id == id)
        .and_then(|entry| {
            config
                .default_model
                .as_deref()
                .filter(|m| entry.offers_model(m))
                .map(|m| m.to_string())
                .or_else(|| entry.default_channel().map(|channel| channel.model.clone()))
        })
        .unwrap_or_else(|| "mock-model".to_string())
}

/// The model ids a provider serves, in catalog order. Used by the picker to
/// render the second-stage model list for multi-model providers (opencode-go).
/// Empty for providers with no channels.
pub fn models_for_provider(config: &Config, provider_id: &str) -> Vec<String> {
    build_catalog(config)
        .iter()
        .find(|e| e.id == provider_id)
        .map(|entry| entry.channels.iter().map(|c| c.model.clone()).collect())
        .unwrap_or_default()
}

/// Build the full model-picker snapshot: the canonical default id plus one row
/// per catalog entry carrying the dynamic signals the picker renders and sorts
/// by (key readiness, favorite flag, last-used timestamp). Sent to the TUI on
/// startup and after any mutation so the picker always shows a consistent
/// picture
pub fn build_picker_state(config: &Config, usage: &ProviderUsage) -> ProviderPickerSnapshot {
    let entries = build_catalog(config);
    let default_id = default_provider_id(config).to_string();
    let rows = entries
        .iter()
        .map(|entry| ProviderPickerRow {
            id: entry.id.clone(),
            key_ready: entry.key_ready(),
            favorite: config.favorites.iter().any(|fav| fav == &entry.id),
            last_used_ms: usage.last_used_ms(&entry.id),
        })
        .collect();
    ProviderPickerSnapshot { default_id, rows }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that mutate process-wide env vars (`*_API_KEY`, `*_MODEL`)
    /// must serialize against each other so the parallel test runner never
    /// observes a half-set environment. Mirrors the `ENV_GUARD` pattern in
    /// `paths.rs`.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    /// A config with no keys or model overrides set beyond the built-in
    /// defaults, so every field resolves predictably.
    fn bare_config() -> Config {
        Config::default()
    }

    #[test]
    fn catalog_contains_every_builtin_preset() {
        let entries = build_catalog(&bare_config());
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"kimi-code"), "missing kimi-code: {ids:?}");
        assert!(ids.contains(&"openai"));
        assert!(ids.contains(&"gemini"));
        assert!(ids.contains(&"llama"));
        assert!(ids.contains(&"opencode-go"), "missing opencode-go: {ids:?}");
        // Every registry preset is present.
        for spec in OPENAI_PROVIDER_SPECS {
            assert!(
                entries.iter().find(|e| e.id == spec.id).is_some(),
                "registry preset {} missing",
                spec.id
            );
        }
    }

    #[test]
    fn opencode_go_hosts_both_wire_formats() {
        let entries = build_catalog(&bare_config());
        let entry = entries
            .iter()
            .find(|e| e.id == "opencode-go")
            .expect("opencode-go entry");
        // Every served model has its own channel.
        assert!(!entry.channels.is_empty());
        // An OpenAI-format model routes through the OpenAiCompat transport.
        let glm = entry
            .channel_for_model("glm-5.2")
            .expect("glm-5.2 served by opencode-go");
        assert!(
            matches!(
                glm.transport,
                neenee_core::catalog::Transport::OpenAiCompat { .. }
            ),
            "glm-5.2 must use OpenAiCompat"
        );
        // An Anthropic-format model routes through the Anthropic transport —
        // the load-bearing detail: one provider, two wire formats.
        let mm = entry
            .channel_for_model("minimax-m3")
            .expect("minimax-m3 served by opencode-go");
        assert!(
            matches!(
                mm.transport,
                neenee_core::catalog::Transport::Anthropic { .. }
            ),
            "minimax-m3 must use Anthropic /messages"
        );
    }

    #[test]
    fn opencode_go_default_model_selects_its_channel() {
        let mut config = bare_config();
        config.default_model = Some("minimax-m3".to_string());
        // resolved_model_name honors default_model when the provider serves it.
        assert_eq!(resolved_model_name(&config, "opencode-go"), "minimax-m3");
        // models_for_provider lists every served model for the picker.
        let models = models_for_provider(&config, "opencode-go");
        assert!(models.contains(&"glm-5.2".to_string()));
        assert!(models.contains(&"minimax-m3".to_string()));
    }

    #[test]
    fn build_provider_for_model_picks_anthropic_transport_for_minimax() {
        // Selecting minimax-m3 under opencode-go must build a provider whose
        // model id is minimax-m3 (the Anthropic /messages path), proving the
        // per-model transport routing reaches construction.
        let config = bare_config();
        let provider = build_provider_for_model(&config, "opencode-go", Some("minimax-m3"));
        assert_eq!(provider.model(), "minimax-m3");
        assert_eq!(provider.provider_id(), "opencode-go");
    }

    #[test]
    fn kimi_code_uses_kimi_code_platform() {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("MOONSHOT_MODEL");
        }
        let config = bare_config();
        let entries = build_catalog(&config);
        let entry = entries
            .iter()
            .find(|e| e.id == "kimi-code")
            .expect("kimi-code entry");
        let channel = entry.default_channel().expect("default channel");
        // The Kimi Code platform pins the model id to kimi-k2.7-code.
        assert_eq!(
            channel.model, "kimi-k2.7-code",
            "model must be the pinned kimi-k2.7-code alias"
        );
        let (base_url, user_agent) = match &channel.transport {
            Transport::OpenAiCompat {
                base_url,
                user_agent,
            } => (base_url.clone(), user_agent.clone()),
            other => panic!("kimi-code must be OpenAiCompat, got {other:?}"),
        };
        assert_eq!(base_url, "https://api.kimi.com/coding/v1/chat/completions");
        // The Kimi Code platform requires a recognized coding-agent UA.
        assert_eq!(user_agent, "opencode/0.1.0");
    }

    #[test]
    fn config_model_override_wins_over_default() {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("GEMINI_MODEL");
        }
        let mut config = bare_config();
        config.gemini_model = Some("gemini-2.0-flash".to_string());
        let entries = build_catalog(&config);
        let entry = entries
            .iter()
            .find(|e| e.id == "gemini")
            .expect("gemini entry");
        assert_eq!(entry.default_channel().unwrap().model, "gemini-2.0-flash");
    }

    #[test]
    fn resolved_model_name_falls_back_for_unknown_id() {
        assert_eq!(resolved_model_name(&bare_config(), "nope"), "mock-model");
    }

    #[test]
    fn resolved_model_name_returns_default_channel_model() {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("DEEPSEEK_PRO_MODEL");
        }
        assert_eq!(
            resolved_model_name(&bare_config(), "deepseek-v4-pro"),
            "deepseek-v4-pro"
        );
    }

    #[test]
    fn build_provider_for_unknown_id_returns_mock() {
        let provider = build_provider_for(&bare_config(), "does-not-exist");
        assert_eq!(provider.provider_id(), "mock");
    }

    #[test]
    fn stale_deepseek_ids_fall_back_to_mock() {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("DEEPSEEK_FLASH_MODEL");
        } // No alias mapping: stale ids no longer resolve and fall back to mock.
        let provider = build_provider_for(&bare_config(), "deepseek");
        assert_eq!(provider.provider_id(), "mock");
    }

    #[test]
    fn keyless_providers_report_ready_without_keys() {
        let entries = build_catalog(&bare_config());
        let llama = entries
            .iter()
            .find(|e| e.id == "llama")
            .expect("llama entry");
        assert!(llama.key_ready(), "llama must be keyless-ready");
    }

    #[test]
    fn cloud_providers_report_not_ready_without_key() {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
        let entries = build_catalog(&bare_config());
        let openai = entries
            .iter()
            .find(|e| e.id == "openai")
            .expect("openai entry");
        assert!(
            !openai.key_ready(),
            "openai without a key must not be ready"
        );
    }

    /// Build a user model override on `gemini` with two channels.
    fn gemini_two_channel_config() -> Config {
        let mut config = bare_config();
        config.providers = vec![UserProviderConfig {
            id: "gemini".to_string(),
            name: Some("Gemini (custom)".to_string()),
            channels: vec![
                UserChannelConfig {
                    label: "Studio".to_string(),
                    transport: UserTransport::GeminiNative,
                    api_key_env: Some("GEMINI_STUDIO_KEY".to_string()),
                    model: Some("gemini-2.5-flash".to_string()),
                    ..Default::default()
                },
                UserChannelConfig {
                    label: "Relay".to_string(),
                    transport: UserTransport::OpenAiCompat,
                    base_url: Some("https://relay.example.com/v1/chat/completions".to_string()),
                    api_key: Some("inline-key".to_string()),
                    model: Some("gemini-2.5-flash".to_string()),
                    ..Default::default()
                },
            ],
            default_channel: 1,
        }];
        config
    }

    #[test]
    fn user_model_overrides_builtin_by_id() {
        let entries = build_catalog(&gemini_two_channel_config());
        let gemini = entries
            .iter()
            .find(|e| e.id == "gemini")
            .expect("overridden gemini entry");
        // The user-supplied name wins over the built-in "Gemini 2.5 Flash".
        assert_eq!(gemini.name, "Gemini (custom)");
        assert!(!gemini.builtin, "an override is user-owned, not read-only");
        // Two channels, with the user's default index honored.
        assert_eq!(gemini.channels.len(), 2);
        assert_eq!(gemini.default_channel, 1);
        assert_eq!(gemini.default_channel().unwrap().label, "Relay");
    }

    #[test]
    fn user_channel_resolves_env_key_over_inline() {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("GEMINI_STUDIO_KEY", "env-key");
        }
        let entries = build_catalog(&gemini_two_channel_config());
        let entry = entries.iter().find(|e| e.id == "gemini").unwrap();
        // Studio names an env var → the env value wins.
        let studio = entry.channels.iter().find(|c| c.label == "Studio").unwrap();
        assert_eq!(studio.api_key, "env-key");
        // Relay uses an inline key (no env var named) → inline wins.
        let relay = entry.channels.iter().find(|c| c.label == "Relay").unwrap();
        assert_eq!(relay.api_key, "inline-key");
        unsafe {
            std::env::remove_var("GEMINI_STUDIO_KEY");
        }
    }

    #[test]
    fn user_model_appends_when_id_is_new() {
        let mut config = bare_config();
        config.providers = vec![UserProviderConfig {
            id: "my-relay".to_string(),
            name: Some("My Relay".to_string()),
            channels: vec![UserChannelConfig {
                label: "default".to_string(),
                transport: UserTransport::OpenAiCompat,
                base_url: Some("https://my.example.com/v1/chat/completions".to_string()),
                api_key: Some("k".to_string()),
                model: Some("my-model".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        }];
        let entries = build_catalog(&config);
        let relay = entries
            .iter()
            .find(|e| e.id == "my-relay")
            .expect("appended user model");
        assert_eq!(relay.name, "My Relay");
        assert_eq!(relay.default_channel().unwrap().model, "my-model");
    }

    #[test]
    fn default_provider_id_reads_config() {
        let mut config = bare_config();
        config.default_provider = "zai-code".to_string();
        assert_eq!(default_provider_id(&config), "zai-code");
    }

    #[test]
    fn picker_state_reflects_user_default_and_channels() {
        let mut config = gemini_two_channel_config();
        config.default_provider = "gemini".to_string();
        let usage = ProviderUsage::default();
        let snapshot = build_picker_state(&config, &usage);
        assert_eq!(snapshot.default_id, "gemini");
        let gemini_row = snapshot
            .rows
            .iter()
            .find(|r| r.id == "gemini")
            .expect("gemini row present");
        assert!(gemini_row.key_ready, "Relay channel has an inline key");
    }
}
