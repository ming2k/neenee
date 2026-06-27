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

use crate::agent_setup::{reseed_prune_threshold, reseed_tool_overrides};
use crate::session_view::provider_key_status;

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
            "gemini" => config.gemini_api_key = Some(key),
            "kimi-code" => config.moonshot_api_key = Some(key),
            "deepseek-v4-flash" | "deepseek-v4-pro" => config.deepseek_api_key = Some(key),
            "zai-code" => config.zai_api_key = Some(key),
            "opencode-go" => config.opencode_go_api_key = Some(key),
            _ => {}
        }
    }
    if let Some(url) = base_url {
        if provider_type.as_str() == "llama" {
            config.llama_base_url = Some(url);
        }
    }
    // Persist the chosen model and default-provider pointer before
    // building so the catalog reads them back. The key/url writes
    // above already landed in `config`.
    config.default_provider = provider_type.clone();
    // opencode-go is multi-model: the active model lives in the shared
    // `default_model` field (every channel shares one API key, and each
    // model's transport is derived from its WireFormat). Single-model
    // providers keep their per-provider model slot as before.
    if provider_type.as_str() == "opencode-go" {
        config.default_model = Some(model.clone());
    } else {
        config.default_model = None;
        match provider_type.as_str() {
            "openai" => config.openai_model = Some(model.clone()),
            "gemini" => config.gemini_model = Some(model.clone()),
            "kimi-code" => config.moonshot_model = Some(model.clone()),
            "llama" => config.llama_model = Some(model.clone()),
            "deepseek-v4-flash" => config.deepseek_flash_model = Some(model.clone()),
            "deepseek-v4-pro" => config.deepseek_pro_model = Some(model.clone()),
            "zai-code" => config.zai_model = Some(model.clone()),
            _ => {}
        }
    }
    let _ = config.save();

    // Build through the catalog so api-key / user-agent / base-url
    // resolution is shared with startup. For multi-model providers the
    // explicit model selects the channel (and thus the per-model transport);
    // build_provider_for_model reads `default_model` set above as a fallback.
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
    reseed_tool_overrides(agent, config);

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
    reseed_tool_overrides(agent, config);
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
