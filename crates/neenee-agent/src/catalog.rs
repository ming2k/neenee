//! Materializes a [`Catalog`] from the host crate's [`Config`].
//!
//! This is the single source of truth for the environment-variable-then-config
//! resolution rules that startup and runtime provider switching share. Every
//! [`Channel`] produced here carries fully resolved credentials and model id, so
//! [`Channel::build`](neenee_core::catalog::Channel::build) never touches the
//! environment or config again.
//!
//! Phase 1 of ADR-0002: produces one channel per entry from the legacy
//! per-provider fields. The on-disk schema is unchanged; later phases add
//! multi-channel entries, favorites, and recency.

use neenee_core::catalog::{builtin_metadata, Catalog, Channel, ModelEntry, Transport};
use neenee_core::{ModelPickerRow, ModelPickerSnapshot};
use neenee_providers::{OpenAiProviderSpec, NEENEE_USER_AGENT, OPENAI_PROVIDER_SPECS};
use neenee_store::config::{Config, UserChannelConfig, UserModelConfig, UserTransport};
use neenee_store::model_usage::ModelUsage;

/// Resolve the effective default-model id: `config.default_model` when set,
/// otherwise the legacy `config.default_provider`. Canonicalized by the caller
/// where it matters (catalog lookup already canonicalizes).
pub fn default_model_id(config: &Config) -> &str {
    config
        .default_model
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&config.default_provider)
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
        UserTransport::Mock => Transport::Mock,
        UserTransport::GeminiNative => Transport::GeminiNative,
        UserTransport::Llama => Transport::Llama {
            base_url: uc
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:8080".to_string()),
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

/// Convert a user-defined model config into a resolved [`ModelEntry`]. Reuses
/// built-in display metadata (name / description / context window) when the id
/// matches a built-in, so overriding e.g. `gemini` inherits its friendly name
/// unless the user supplies their own. A model with no channels is degenerate;
/// it gets a single mock channel so it still renders and selects safely.
fn user_model_to_entry(um: &UserModelConfig) -> ModelEntry {
    let builtin = builtin_metadata(&um.id);
    let name = um
        .name
        .clone()
        .or_else(|| builtin.map(|(n, _, _)| n.to_string()))
        .unwrap_or_else(|| um.id.clone());
    let (description, context_window) = builtin
        .map(|(_, d, c)| (d.to_string(), c))
        .unwrap_or_default();
    let fallback_model = um.id.clone();
    let channels: Vec<Channel> = if um.channels.is_empty() {
        vec![Channel {
            id: "default".to_string(),
            label: name.clone(),
            transport: Transport::Mock,
            api_key: String::new(),
            model: fallback_model,
        }]
    } else {
        um.channels
            .iter()
            .map(|c| user_channel_to_channel(c, &fallback_model))
            .collect()
    };
    let default_channel = um.default_channel.min(channels.len().saturating_sub(1));
    ModelEntry {
        id: um.id.clone(),
        name,
        description,
        context_window,
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
        "kimi-k2.7-code" => config.moonshot_api_key.clone(),
        "deepseek-v4-flash" | "deepseek-v4-pro" => config.deepseek_api_key.clone(),
        "qwen" => config.qwen_api_key.clone(),
        "glm" => config.glm_api_key.clone(),
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
        "kimi-k2.7-code" => config.moonshot_model.clone(),
        "deepseek-v4-flash" => config.deepseek_flash_model.clone(),
        "deepseek-v4-pro" => config.deepseek_pro_model.clone(),
        "qwen" => config.qwen_model.clone(),
        "glm" => config.glm_model.clone(),
        _ => None,
    }
}

/// Attach the built-in display metadata (name, description, context window) to
/// a raw `(id, channels)` pair. Falls back to the raw id as the name when no
/// metadata is registered, so user-defined entries still render.
fn entry_with_metadata(id: &str, channels: Vec<Channel>, builtin: bool) -> ModelEntry {
    let (name, description, context_window) = builtin_metadata(id)
        .map(|(n, d, c)| {
            let owned_name: String = n.to_string();
            let owned_desc: String = d.to_string();
            (owned_name, owned_desc, c)
        })
        .unwrap_or_else(|| (id.to_string(), String::new(), 0));
    ModelEntry {
        id: id.to_string(),
        name,
        description,
        context_window,
        channels,
        default_channel: 0,
        builtin,
    }
}

/// Build a single-channel entry for an OpenAI-compatible registry preset.
fn openai_compat_entry_from_spec(config: &Config, spec: &OpenAiProviderSpec) -> ModelEntry {
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
    let user_agent = NEENEE_USER_AGENT.to_string();
    let (name, _, _) = builtin_metadata(spec.id)
        .map(|(n, d, c)| (n.to_string(), d.to_string(), c))
        .unwrap_or_else(|| (spec.id.to_string(), String::new(), 0));
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

/// Build the catalog by materializing every known model from `config`.
///
/// Order is registry presets first, then bespoke providers, then the mock
/// fixture. Order does not affect behavior — all lookups are by id — but a
/// stable order makes the catalog readable in debug output and (later) the
/// picker's default pre-search listing.
pub fn build_catalog(config: &Config) -> Catalog {
    let mut entries: Vec<ModelEntry> = Vec::new();

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

    // Mock fixture — always last, always keyless.
    entries.push(entry_with_metadata(
        "mock",
        vec![Channel {
            id: "default".to_string(),
            label: "Mock".to_string(),
            transport: Transport::Mock,
            api_key: String::new(),
            model: "mock".to_string(),
        }],
        true,
    ));

    // User-defined models (ADR-0002 phase 5): override built-ins by id, or
    // append new models. A user entry may carry several channels, finally
    // enabling multi-channel delivery (e.g. Gemini via Studio and Vertex).
    for user_entry in config.models.iter().map(user_model_to_entry) {
        if let Some(existing) = entries.iter_mut().find(|e| e.id == user_entry.id) {
            *existing = user_entry;
        } else {
            entries.push(user_entry);
        }
    }

    Catalog { entries }
}

/// Resolve the active provider for a given model id from `config`. Returns the
/// mock provider when the id is unknown or the entry has no default channel, so
/// callers never have to branch on absence. This is the single replacement for
/// the resolution logic that used to be duplicated at startup and in the
/// `SwitchProvider` handler.
pub fn build_provider_for(config: &Config, id: &str) -> std::sync::Arc<dyn neenee_core::Provider> {
    let catalog = build_catalog(config);
    match catalog.get(id) {
        Some(entry) => match entry.default_channel() {
            Some(channel) => neenee_providers::build_provider_for_channel(channel, &entry.id),
            None => std::sync::Arc::new(neenee_providers::MockProvider),
        },
        None => std::sync::Arc::new(neenee_providers::MockProvider),
    }
}

/// The display model name for a given model id, as resolved from `config`.
/// Falls back to `"mock-model"` when the id is unknown. Replaces the former
/// `initial_m_name` block in `main.rs`.
pub fn resolved_model_name(config: &Config, id: &str) -> String {
    build_catalog(config)
        .get(id)
        .and_then(|entry| entry.default_channel())
        .map(|channel| channel.model.clone())
        .unwrap_or_else(|| "mock-model".to_string())
}

/// Build the full model-picker snapshot: the canonical default id plus one row
/// per catalog entry carrying the dynamic signals the picker renders and sorts
/// by (key readiness, favorite flag, last-used timestamp). Sent to the TUI on
/// startup and after any mutation so the picker always shows a consistent
/// picture (ADR-0002 phase 3).
pub fn build_picker_state(config: &Config, usage: &ModelUsage) -> ModelPickerSnapshot {
    let catalog = build_catalog(config);
    let default_id = default_model_id(config).to_string();
    let rows = catalog
        .entries
        .iter()
        .map(|entry| ModelPickerRow {
            id: entry.id.clone(),
            key_ready: entry.key_ready(),
            favorite: config.favorites.iter().any(|fav| fav == &entry.id),
            last_used_ms: usage.last_used_ms(&entry.id),
        })
        .collect();
    ModelPickerSnapshot { default_id, rows }
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
        let catalog = build_catalog(&bare_config());
        let ids: Vec<&str> = catalog.entries.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"kimi-k2.7-code"), "missing kimi-k2.7-code: {ids:?}");
        assert!(ids.contains(&"openai"));
        assert!(ids.contains(&"gemini"));
        assert!(ids.contains(&"llama"));
        assert!(ids.contains(&"mock"));
        // Every registry preset is present.
        for spec in OPENAI_PROVIDER_SPECS {
            assert!(
                catalog.get(spec.id).is_some(),
                "registry preset {} missing",
                spec.id
            );
        }
    }

    #[test]
    fn kimi_k27_code_uses_official_endpoint_and_default_model() {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("MOONSHOT_MODEL");
        let config = bare_config();
        let catalog = build_catalog(&config);
        let entry = catalog.get("kimi-k2.7-code").expect("kimi-k2.7-code entry");
        let channel = entry.default_channel().expect("default channel");
        assert_eq!(
            channel.model, "kimi-k2.7-code",
            "default model must be kimi-k2.7-code"
        );
        let (base_url, user_agent) = match &channel.transport {
            Transport::OpenAiCompat {
                base_url,
                user_agent,
            } => (base_url.clone(), user_agent.clone()),
            other => panic!("kimi-k2.7-code must be OpenAiCompat, got {other:?}"),
        };
        assert_eq!(base_url, "https://api.moonshot.ai/v1/chat/completions");
        assert_eq!(user_agent, NEENEE_USER_AGENT);
    }

    #[test]
    fn config_model_override_wins_over_default() {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("GEMINI_MODEL");
        let mut config = bare_config();
        config.gemini_model = Some("gemini-2.0-flash".to_string());
        let catalog = build_catalog(&config);
        let entry = catalog.get("gemini").expect("gemini entry");
        assert_eq!(entry.default_channel().unwrap().model, "gemini-2.0-flash");
    }

    #[test]
    fn resolved_model_name_falls_back_for_unknown_id() {
        assert_eq!(resolved_model_name(&bare_config(), "nope"), "mock-model");
    }

    #[test]
    fn resolved_model_name_returns_default_channel_model() {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("DEEPSEEK_PRO_MODEL");
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
        std::env::remove_var("DEEPSEEK_FLASH_MODEL");
        // No alias mapping: stale ids no longer resolve and fall back to mock.
        let provider = build_provider_for(&bare_config(), "deepseek");
        assert_eq!(provider.provider_id(), "mock");
    }

    #[test]
    fn keyless_providers_report_ready_without_keys() {
        let catalog = build_catalog(&bare_config());
        let llama = catalog.get("llama").expect("llama entry");
        let mock = catalog.get("mock").expect("mock entry");
        assert!(llama.key_ready(), "llama must be keyless-ready");
        assert!(mock.key_ready(), "mock must be keyless-ready");
    }

    #[test]
    fn cloud_providers_report_not_ready_without_key() {
        let _guard = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("OPENAI_API_KEY");
        let catalog = build_catalog(&bare_config());
        let openai = catalog.get("openai").expect("openai entry");
        assert!(
            !openai.key_ready(),
            "openai without a key must not be ready"
        );
    }

    /// Build a user model override on `gemini` with two channels.
    fn gemini_two_channel_config() -> Config {
        let mut config = bare_config();
        config.models = vec![UserModelConfig {
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
        let catalog = build_catalog(&gemini_two_channel_config());
        let gemini = catalog.get("gemini").expect("overridden gemini entry");
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
        std::env::set_var("GEMINI_STUDIO_KEY", "env-key");
        let catalog = build_catalog(&gemini_two_channel_config());
        let entry = catalog.get("gemini").unwrap();
        // Studio names an env var → the env value wins.
        let studio = entry.channels.iter().find(|c| c.label == "Studio").unwrap();
        assert_eq!(studio.api_key, "env-key");
        // Relay uses an inline key (no env var named) → inline wins.
        let relay = entry.channels.iter().find(|c| c.label == "Relay").unwrap();
        assert_eq!(relay.api_key, "inline-key");
        std::env::remove_var("GEMINI_STUDIO_KEY");
    }

    #[test]
    fn user_model_appends_when_id_is_new() {
        let mut config = bare_config();
        config.models = vec![UserModelConfig {
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
        let catalog = build_catalog(&config);
        let relay = catalog.get("my-relay").expect("appended user model");
        assert_eq!(relay.name, "My Relay");
        assert_eq!(relay.default_channel().unwrap().model, "my-model");
    }

    #[test]
    fn default_model_pointer_preferred_over_default_provider() {
        let mut config = bare_config();
        config.default_provider = "mock".to_string();
        config.default_model = Some("gemini".to_string());
        assert_eq!(default_model_id(&config), "gemini");
    }

    #[test]
    fn default_model_falls_back_to_default_provider() {
        let mut config = bare_config();
        config.default_provider = "glm".to_string();
        config.default_model = None;
        assert_eq!(default_model_id(&config), "glm");
    }

    #[test]
    fn empty_default_model_falls_back_to_default_provider() {
        let mut config = bare_config();
        config.default_provider = "qwen".to_string();
        config.default_model = Some(String::new());
        assert_eq!(
            default_model_id(&config),
            "qwen",
            "an empty default_model must not shadow default_provider"
        );
    }

    #[test]
    fn picker_state_reflects_user_default_and_channels() {
        let mut config = gemini_two_channel_config();
        config.default_model = Some("gemini".to_string());
        let usage = ModelUsage::default();
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
