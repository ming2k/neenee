//! Provider-switch / favorite / default-model handlers, extracted verbatim
//! from the agent background task's `match req { … }` dispatch.
//!
//! Each handler is one match arm, lifted unchanged. Parameters are named to
//! match the original loop locals (`config`, `agent`, `provider_for_task`,
//! `resp_tx`, `provider_usage`) so the body reads exactly as it did inline.

use neenee_agent::Agent;
use neenee_agent::catalog;
use neenee_core::{AgentResponse, Provider};
use neenee_store::{config::Config, provider_usage::ProviderUsage};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;

use crate::agent_setup::{reseed_prune_threshold, reseed_tool_variants};
use crate::session_view::provider_key_status;

/// Whether `id` is a multi-model provider — a built-in that hosts several models
/// behind one key, or a user-defined provider with more than one channel. For
/// these the active model lives in `config.default_model` rather than a
/// per-provider model slot.
fn is_multi_model_provider(config: &Config, id: &str) -> bool {
    if matches!(
        id,
        "openai" | "opencode-go" | "anthropic" | "google" | "deepseek"
    ) {
        return true;
    }
    config
        .providers
        .iter()
        .any(|p| p.id == id && p.channels.len() > 1)
}

/// `AgentRequest::SwitchProvider` — persist the chosen key/url/model/default,
/// rebuild the provider through the catalog so resolution stays shared with
/// startup, swap it into the shared holder, re-seed mid-turn relief, and push
/// the picker + key snapshots.
#[allow(clippy::too_many_arguments)]
pub async fn switch(
    config: &mut Config,
    agent: &Agent,
    provider_for_task: &Arc<RwLock<Arc<dyn Provider>>>,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    provider_usage: &mut ProviderUsage,
    provider_type: String,
    model: String,
    api_key: Option<String>,
    base_url: Option<String>,
) {
    // A key entered in the TUI is persisted and wins over
    // config; environment variables still take precedence.
    if let Some(key) = api_key.clone() {
        match provider_type.as_str() {
            "openai" => config.openai_api_key = Some(key),
            "google" => config.gemini_api_key = Some(key),
            "kimi-code" => config.moonshot_api_key = Some(key),
            "deepseek" => config.deepseek_api_key = Some(key),
            "zai-code" => config.zai_api_key = Some(key),
            "opencode-go" => config.opencode_go_api_key = Some(key),
            "anthropic" => config.anthropic_api_key = Some(key),
            _ => {}
        }
    }
    if let Some(url) = base_url
        && provider_type.as_str() == "anthropic"
    {
        config.anthropic_base_url = Some(url);
    }
    // ADR-0046: reasoning (effort/thinking) is no longer set on provider
    // switch — it is opted in per model via `[model_reasoning]`
    // (`EditModelReasoning`) / a channel's reasoning fields
    // (`EditProviderModel`). Switching just selects the provider + model.
    // Persist the chosen model and default-provider pointer before
    // building so the catalog reads them back. The key/url writes
    // above already landed in `config`.
    config.default_provider = provider_type.clone();
    // Multi-model providers (opencode-go, anthropic, google, deepseek, and any
    // user-defined provider with several channels) carry the active model in the
    // shared `default_model` field — every channel shares one API key and each
    // model's transport is derived from its catalog channel. Single-model
    // built-ins keep their per-provider model slot as before.
    if is_multi_model_provider(config, &provider_type) {
        config.default_model = Some(model.clone());
    } else {
        config.default_model = None;
        match provider_type.as_str() {
            "kimi-code" => config.moonshot_model = Some(model.clone()),
            "zai-code" => config.zai_model = Some(model.clone()),
            _ => {}
        }
    }
    let _ = config.save();
    activate(
        config,
        agent,
        provider_for_task,
        resp_tx,
        provider_usage,
        provider_type,
        model,
    )
    .await;
}

/// `AgentRequest::AddProvider` — persist a user-defined ("custom") provider to
/// `config.providers`, then activate it. The provider is a single-channel entry
/// carrying its own protocol, endpoint, inline key, and model, so it round-trips
/// through config and is reachable by the same catalog path as a built-in.
#[allow(clippy::too_many_arguments)]
pub async fn add(
    config: &mut Config,
    agent: &Agent,
    provider_for_task: &Arc<RwLock<Arc<dyn Provider>>>,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    provider_usage: &mut ProviderUsage,
    name: String,
    protocol: String,
    base_url: String,
    api_key: String,
    models: Vec<String>,
) {
    use neenee_store::config::{UserChannelConfig, UserProviderConfig, UserTransport};

    let id = custom_provider_id(&name);
    let transport = match protocol.as_str() {
        "anthropic" => UserTransport::Anthropic,
        "gemini" => UserTransport::GeminiNative,
        // Default (and explicit "openai"): the OpenAI-compatible chat surface.
        _ => UserTransport::OpenAiCompat,
    };
    let trimmed_key = api_key.trim();
    let api_key = (!trimmed_key.is_empty()).then(|| trimmed_key.to_string());
    let base_url = {
        let trimmed = base_url.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    // ADR-0046: reasoning is opt-in per model. New channels are created with no
    // effort/thinking — the user opts a model in from the stage-2 model `e`
    // editor (`EditProviderModel`). One channel per seeded model — a template
    // that seeds the whole Claude family lands every model in the picker's
    // stage-2 list, all sharing the provider's transport/endpoint/key. Empty/
    // whitespace model ids are dropped.
    let channels: Vec<UserChannelConfig> = models
        .iter()
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        .map(|model| UserChannelConfig {
            label: model.to_string(),
            transport,
            api_key_env: None,
            api_key: api_key.clone(),
            model: Some(model.to_string()),
            base_url: base_url.clone(),
            user_agent: None,
            effort: None,
            thinking: None,
        })
        .collect();
    // A provider must serve at least one model; a template with no usable model
    // id is a no-op rather than a broken zero-channel entry.
    if channels.is_empty() {
        return;
    }
    let active_model = channels[0].model.clone().unwrap_or_default();
    let entry = UserProviderConfig {
        id: id.clone(),
        name: (!name.trim().is_empty()).then(|| name.trim().to_string()),
        channels,
        default_channel: 0,
    };
    // Replace any existing custom provider with the same derived id, else append.
    if let Some(existing) = config.providers.iter_mut().find(|p| p.id == id) {
        *existing = entry;
    } else {
        config.providers.push(entry);
    }
    config.default_provider = id.clone();
    // Record the first seeded model as the active model so the picker and status
    // surfaces land on it.
    config.default_model = Some(active_model.clone());
    let _ = config.save();
    activate(
        config,
        agent,
        provider_for_task,
        resp_tx,
        provider_usage,
        id,
        active_model,
    )
    .await;
}

/// `AgentRequest::EditProvider` — update a user-defined provider's metadata in
/// place: display name, and every channel's transport/base-URL/key. Each
/// channel's model id is preserved, so a multi-model custom provider keeps all
/// its models. An empty `api_key` leaves the existing key untouched. Persists,
/// then re-activates so the live provider picks up the new endpoint/key.
#[allow(clippy::too_many_arguments)]
pub async fn edit(
    config: &mut Config,
    agent: &Agent,
    provider_for_task: &Arc<RwLock<Arc<dyn Provider>>>,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    provider_usage: &mut ProviderUsage,
    id: String,
    name: String,
    protocol: String,
    base_url: String,
    api_key: String,
) {
    use neenee_store::config::UserTransport;

    let transport = match protocol.as_str() {
        "anthropic" => UserTransport::Anthropic,
        "gemini" => UserTransport::GeminiNative,
        _ => UserTransport::OpenAiCompat,
    };
    let trimmed_url = base_url.trim();
    let trimmed_key = api_key.trim();
    let trimmed_name = name.trim();
    let Some(provider) = config.providers.iter_mut().find(|p| p.id == id) else {
        return;
    };
    if !trimmed_name.is_empty() {
        provider.name = Some(trimmed_name.to_string());
    }
    for channel in &mut provider.channels {
        channel.transport = transport;
        channel.base_url = (!trimmed_url.is_empty()).then(|| trimmed_url.to_string());
        // An empty key keeps whatever the channel already had.
        if !trimmed_key.is_empty() {
            channel.api_key = Some(trimmed_key.to_string());
        }
        // ADR-0046: reasoning (effort/thinking) is no longer edited here — it
        // is per-model, via `EditProviderModel`. Editing provider metadata
        // leaves each channel's reasoning knobs untouched.
    }
    let _ = config.save();
    // Only rebuild the live provider when editing the active one (so a new
    // endpoint/key takes effect); editing an inactive provider just refreshes
    // the persisted config + the picker snapshot without switching.
    if config.default_provider == id {
        let model = catalog::resolved_model_name(config, &id);
        activate(
            config,
            agent,
            provider_for_task,
            resp_tx,
            provider_usage,
            id,
            model,
        )
        .await;
    } else {
        let _ = resp_tx.send(AgentResponse::ProviderKeys(provider_key_status(config)));
        let _ = resp_tx.send(AgentResponse::ProviderPicker(catalog::build_picker_state(
            config,
            provider_usage,
        )));
    }
}

/// `AgentRequest::AddProviderModel` — append a model to a user-defined provider
/// as a new channel that shares the provider's transport/endpoint/key (only the
/// wire model id differs), persist, and push a fresh picker snapshot. No-op for
/// built-in providers (curated model lists) or a model the provider already
/// serves.
pub async fn add_model(
    config: &mut Config,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    provider_usage: &ProviderUsage,
    provider_id: String,
    model: String,
) {
    let model = model.trim().to_string();
    if model.is_empty() {
        return;
    }
    if let Some(provider) = config.providers.iter_mut().find(|p| p.id == provider_id) {
        let already = provider
            .channels
            .iter()
            .any(|c| c.model.as_deref() == Some(model.as_str()));
        // Clone the first channel as a template so transport/base_url/key carry
        // over; only the model id (and label) change.
        if !already && let Some(template) = provider.channels.first().cloned() {
            let mut channel = template;
            channel.label = model.clone();
            channel.model = Some(model.clone());
            provider.channels.push(channel);
        }
    }
    if let Err(error) = config.save() {
        tracing::warn!(?error, "could not persist added provider model");
    }
    let _ = resp_tx.send(AgentResponse::ProviderPicker(catalog::build_picker_state(
        config,
        provider_usage,
    )));
}

/// `AgentRequest::RemoveProviderModel` — drop a model (channel) from a
/// user-defined provider, persist, and push a fresh picker snapshot. The last
/// remaining channel is kept (a provider must serve at least one model). If the
/// removed model was the active `default_model`, it is cleared so the provider
/// falls back to its default channel.
pub async fn remove_model(
    config: &mut Config,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    provider_usage: &ProviderUsage,
    provider_id: String,
    model: String,
) {
    if let Some(provider) = config.providers.iter_mut().find(|p| p.id == provider_id)
        && provider.channels.len() > 1
        && let Some(pos) = provider
            .channels
            .iter()
            .position(|c| c.model.as_deref() == Some(model.as_str()))
    {
        provider.channels.remove(pos);
        if provider.default_channel >= provider.channels.len() {
            provider.default_channel = 0;
        }
    }
    if config.default_model.as_deref() == Some(model.as_str()) {
        config.default_model = None;
    }
    if let Err(error) = config.save() {
        tracing::warn!(?error, "could not persist removed provider model");
    }
    let _ = resp_tx.send(AgentResponse::ProviderPicker(catalog::build_picker_state(
        config,
        provider_usage,
    )));
}

/// `AgentRequest::EditProviderModel` — update settings for one channel of a
/// user-defined provider. Provider metadata (name/base URL/key) is untouched.
#[allow(clippy::too_many_arguments)]
pub async fn edit_model(
    config: &mut Config,
    agent: &Agent,
    provider_for_task: &Arc<RwLock<Arc<dyn Provider>>>,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    provider_usage: &mut ProviderUsage,
    provider_id: String,
    model: String,
    effort: Option<String>,
    thinking: Option<bool>,
) {
    let valid_effort = effort.and_then(|e| {
        let t = e.trim();
        (!t.is_empty())
            .then(|| t.to_ascii_lowercase())
            .filter(|s| neenee_core::effort::Effort::parse(s).is_some())
    });

    let Some(provider) = config.providers.iter_mut().find(|p| p.id == provider_id) else {
        return;
    };
    let Some(channel) = provider
        .channels
        .iter_mut()
        .find(|c| c.model.as_deref() == Some(model.as_str()))
    else {
        return;
    };

    if matches!(
        channel.transport,
        neenee_store::config::UserTransport::Anthropic
    ) {
        channel.effort = valid_effort;
        channel.thinking = thinking;
    }

    if let Err(error) = config.save() {
        tracing::warn!(?error, "could not persist provider model settings");
    }

    let active_model = catalog::resolved_model_name(config, &provider_id);
    if config.default_provider == provider_id && active_model == model {
        activate(
            config,
            agent,
            provider_for_task,
            resp_tx,
            provider_usage,
            provider_id,
            model,
        )
        .await;
    } else {
        let _ = resp_tx.send(AgentResponse::ProviderPicker(catalog::build_picker_state(
            config,
            provider_usage,
        )));
    }
}

/// `AgentRequest::EditModelReasoning` — update the per-model reasoning
/// settings (Anthropic effort/thinking) persisted in the
/// `[model_reasoning."<model-id>"]` table. This serves the **built-in**
/// `anthropic` provider (and any built-in Anthropic-format model), which has
/// no user-editable channels: its per-model knobs live in this shared table
/// keyed by model id (ADR-0045). If the edited model is the active one, the
/// live provider is re-activated so the new settings take effect at once.
#[allow(clippy::too_many_arguments)]
pub async fn edit_model_reasoning(
    config: &mut Config,
    agent: &Agent,
    provider_for_task: &Arc<RwLock<Arc<dyn Provider>>>,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    provider_usage: &mut ProviderUsage,
    model: String,
    effort: Option<String>,
    thinking: Option<bool>,
) {
    let valid_effort = effort.and_then(|e| {
        let t = e.trim();
        (!t.is_empty())
            .then(|| t.to_ascii_lowercase())
            .filter(|s| neenee_core::effort::Effort::parse(s).is_some())
    });

    let settings = config.model_reasoning.for_model_mut(&model);
    settings.effort = valid_effort;
    settings.thinking = thinking;

    if let Err(error) = config.save() {
        tracing::warn!(?error, "could not persist per-model reasoning settings");
    }

    // Re-activate if this model is the live one so the change applies now.
    let provider_id = &config.default_provider;
    let active_model = catalog::resolved_model_name(config, provider_id);
    if active_model == model {
        activate(
            config,
            agent,
            provider_for_task,
            resp_tx,
            provider_usage,
            provider_id.clone(),
            model,
        )
        .await;
    } else {
        let _ = resp_tx.send(AgentResponse::ProviderPicker(catalog::build_picker_state(
            config,
            provider_usage,
        )));
    }
}

/// `AgentRequest::DeleteProvider` — remove a user-defined provider entry
/// entirely. Drops it from `config.providers` (a no-op for built-ins or an
/// unknown id), prunes it from `favorites`, and persists. When the deleted
/// provider was the active one (`config.default_provider`), it falls back to
/// the default built-in provider (`"kimi-code"`) and re-activates so the live
/// provider never points at a removed entry. Otherwise (deleting an inactive
/// provider) it only refreshes the picker snapshot.
#[allow(clippy::too_many_arguments)]
pub async fn delete(
    config: &mut Config,
    agent: &Agent,
    provider_for_task: &Arc<RwLock<Arc<dyn Provider>>>,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    provider_usage: &mut ProviderUsage,
    id: String,
) {
    // Drop the user-defined entry. `retain` is a no-op when the id is unknown,
    // and built-in ids are never present in `config.providers`, so this is
    // safely a built-in guard.
    let before = config.providers.len();
    config.providers.retain(|p| p.id != id);
    // Nothing to do — the id was not a user-defined provider.
    if config.providers.len() == before {
        return;
    }
    // Prune the removed id from favorites so the picker never references it.
    config.favorites.retain(|fav| *fav != id);

    let was_active = config.default_provider == id;
    if was_active {
        // Fall back to the catalog's default built-in provider (kimi-code),
        // clear any model pointer that belonged to the deleted provider, then
        // activate so the live provider is rebuilt from a valid entry.
        config.default_provider = catalog::default_provider_id(&Config::default()).to_string();
        config.default_model = None;
    }
    if let Err(error) = config.save() {
        tracing::warn!(?error, "could not persist deleted provider");
    }

    if was_active {
        let fallback = config.default_provider.clone();
        let model = catalog::resolved_model_name(config, &fallback);
        activate(
            config,
            agent,
            provider_for_task,
            resp_tx,
            provider_usage,
            fallback,
            model,
        )
        .await;
    } else {
        // Deleting an inactive provider: refresh the picker + key snapshots
        // without switching the live provider.
        let _ = resp_tx.send(AgentResponse::ProviderKeys(provider_key_status(config)));
        let _ = resp_tx.send(AgentResponse::ProviderPicker(catalog::build_picker_state(
            config,
            provider_usage,
        )));
    }
}

/// Derive a stable provider id from a user-supplied display name: lowercase,
/// non-alphanumeric runs collapsed to single hyphens, trimmed. Falls back to
/// `"custom"` for an empty/symbol-only name so the id is always non-empty.
fn custom_provider_id(name: &str) -> String {
    let mut id = String::new();
    let mut prev_hyphen = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            id.push(ch.to_ascii_lowercase());
            prev_hyphen = false;
        } else if !prev_hyphen && !id.is_empty() {
            id.push('-');
            prev_hyphen = true;
        }
    }
    let id = id.trim_end_matches('-').to_string();
    if id.is_empty() {
        "custom".to_string()
    } else {
        id
    }
}

/// Shared tail of [`switch`] and [`add`]: rebuild the active provider through the
/// catalog (so api-key / endpoint / user-agent resolution matches startup), swap
/// it into the shared holder, re-seed mid-turn relief, and push the key + picker
/// snapshots. `config` must already be persisted with the chosen pointers.
async fn activate(
    config: &Config,
    agent: &Agent,
    provider_for_task: &Arc<RwLock<Arc<dyn Provider>>>,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    provider_usage: &mut ProviderUsage,
    provider_type: String,
    model: String,
) {
    // For multi-model providers the explicit model selects the channel (and thus
    // the per-model transport); build_provider_for_model reads `default_model` as
    // a fallback.
    let new_p: Arc<dyn Provider> =
        match catalog::build_provider_for_model(config, &provider_type, Some(&model)) {
            provider if provider.provider_id() != "mock" => provider,
            // Fall back to the catalog default if explicit-model resolution hit
            // the mock sentinel (e.g. an unknown model id).
            _ => catalog::build_provider_for(config, &provider_type),
        };
    *provider_for_task
        .write()
        .unwrap_or_else(|error| error.into_inner()) = new_p;

    // The new model may have a different context window; re-seed
    // the mid-turn prune threshold so relief tracks it.
    reseed_prune_threshold(agent, config);
    // Tool-description overrides are keyed by model id, so they must
    // re-track the live model too.
    reseed_tool_variants(agent, config);

    let _ = resp_tx.send(AgentResponse::ProviderKeys(provider_key_status(config)));
    // Record the switch as an activation so the picker's recency
    // ordering tracks it. Best-effort: telemetry is rebuildable.
    provider_usage.record(&provider_type);
    if let Err(error) = provider_usage.save() {
        tracing::warn!(?error, "could not persist model usage telemetry");
    }
    let _ = resp_tx.send(AgentResponse::ProviderSwitched {
        provider: provider_type,
        model,
    });
    let _ = resp_tx.send(AgentResponse::ProviderPicker(catalog::build_picker_state(
        config,
        provider_usage,
    )));
}

/// `AgentRequest::ToggleFavorite` — flip the id in the favorites list,
/// persist, and push a fresh picker snapshot so the ★ flips at once.
pub async fn toggle_favorite(
    config: &mut Config,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    provider_usage: &ProviderUsage,
    id: String,
) {
    if let Some(pos) = config.favorites.iter().position(|fav| *fav == id) {
        config.favorites.remove(pos);
    } else {
        config.favorites.push(id.clone());
    }
    if let Err(error) = config.save() {
        tracing::warn!(?error, "could not persist favorites");
    }
    let _ = resp_tx.send(AgentResponse::ProviderPicker(catalog::build_picker_state(
        config,
        provider_usage,
    )));
}

/// `AgentRequest::SetDefaultModel` — make `id` the default AND activate it,
/// reusing the catalog so resolution rules stay shared. No new key/model
/// comes from the TUI — the provider's existing resolved config is used as-is.
pub async fn set_default_model(
    config: &mut Config,
    agent: &Agent,
    provider_for_task: &Arc<RwLock<Arc<dyn Provider>>>,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    provider_usage: &mut ProviderUsage,
    id: String,
) {
    config.default_provider = id.clone();
    if let Err(error) = config.save() {
        tracing::warn!(?error, "could not persist default model");
    }
    let new_p = catalog::build_provider_for(config, &id);
    *provider_for_task
        .write()
        .unwrap_or_else(|error| error.into_inner()) = new_p;
    // Re-seed mid-turn relief for the newly activated model's
    // context window.
    reseed_prune_threshold(agent, config);
    // Tool-description overrides track the live model id.
    reseed_tool_variants(agent, config);
    provider_usage.record(&id);
    if let Err(error) = provider_usage.save() {
        tracing::warn!(?error, "could not persist model usage telemetry");
    }
    let model_name = catalog::resolved_model_name(config, &id);
    let _ = resp_tx.send(AgentResponse::ProviderSwitched {
        provider: id.clone(),
        model: model_name,
    });
    let _ = resp_tx.send(AgentResponse::ProviderKeys(provider_key_status(config)));
    let _ = resp_tx.send(AgentResponse::ProviderPicker(catalog::build_picker_state(
        config,
        provider_usage,
    )));
}

#[cfg(test)]
mod tests {
    use super::*;
    use neenee_store::config::UserProviderConfig;

    #[test]
    fn custom_provider_id_slugifies_names() {
        assert_eq!(custom_provider_id("My Relay"), "my-relay");
        assert_eq!(custom_provider_id("  Acme  AI  "), "acme-ai");
        assert_eq!(custom_provider_id("relay.example.com"), "relay-example-com");
        assert_eq!(custom_provider_id("OpenAI!!!"), "openai");
        // Symbol-only / empty names fall back to a usable id.
        assert_eq!(custom_provider_id("***"), "custom");
        assert_eq!(custom_provider_id(""), "custom");
    }

    #[test]
    fn multi_model_provider_covers_builtins_and_multichannel_user_entries() {
        let mut config = Config::default();
        for id in ["openai", "opencode-go", "anthropic", "google", "deepseek"] {
            assert!(is_multi_model_provider(&config, id), "{id} is multi-model");
        }
        // Single-model built-ins are not multi-model.
        assert!(!is_multi_model_provider(&config, "kimi-code"));
        assert!(!is_multi_model_provider(&config, "zai-code"));
        // A user provider counts as multi-model only with >1 channel.
        config.providers.push(UserProviderConfig {
            id: "my-relay".to_string(),
            channels: vec![Default::default(), Default::default()],
            ..Default::default()
        });
        assert!(is_multi_model_provider(&config, "my-relay"));
    }
}
