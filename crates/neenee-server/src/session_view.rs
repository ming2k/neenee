//! Snapshotting and lookup helpers for the session-context modal and the
//! session picker. Pure reads — no mutation of agent or session state.
//!
//! Extracted verbatim from `main.rs` to keep the binary entry-point focused on
//! wiring rather than presentation shaping.

use neenee_agent::Agent;
use neenee_agent::catalog;
use neenee_agent::skills::SkillRegistry;
use neenee_core::{
    McpConnectionStatus, McpServerInfo, Message, ModelInfo, SessionContextSnapshot, SessionOverview,
};
use neenee_store::{config::Config, session::SessionStore};

/// Resume a session by id (or the active one when `id` is `None`), refreshing
/// the in-memory `history` to match. Returns the resumed id plus the transcript
/// for any caller that wants to display it.
pub async fn resume_session(
    session: &SessionStore,
    history: &tokio::sync::Mutex<Vec<Message>>,
    id: Option<&str>,
) -> Result<(String, Vec<Message>), String> {
    let id = session.resume(id).await?;
    *history.lock().await = session.model_window().await;
    Ok((id, session.full_transcript().await))
}

/// First 8 characters of a session id — short enough for picker rows while
/// still disambiguating in practice.
pub fn short_session_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Whether each provider has a usable API key (env var or config).
/// Keyless providers (local llama, mock) always report `true`.
///
/// Derived from the provider catalog so the readiness signal and the actual
/// provider construction share one resolution path.
pub fn provider_key_status(config: &Config) -> Vec<(String, bool)> {
    catalog::build_catalog(config)
        .iter()
        .map(|entry| (entry.id.clone(), entry.key_ready()))
        .collect()
}

/// Build a render-ready snapshot of the live session for the session-context
/// modal. Pulls model info from the catalog, tools/permissions/skills from the
/// agent, and MCP per-server tool names by matching the `mcp__<server>__*`
/// naming convention against the agent's installed tools.
///
/// Sent in reply to [`neenee_core::AgentRequest::QuerySessionContext`] and re-sent
/// after any mutation ([`neenee_core::AgentRequest::RevokePermission`] /
/// [`neenee_core::AgentRequest::ToggleTool`]) so the modal always reflects the
/// post-change state.
pub fn build_session_context(
    agent: &Agent,
    _skills_registry: &SkillRegistry,
    mcp_statuses: &[(String, McpConnectionStatus)],
    config: &Config,
) -> SessionContextSnapshot {
    let provider_id = catalog::default_provider_id(config).to_string();
    let model = catalog::resolved_model_name(config, &provider_id);

    // Catalog entry carries the authoritative display metadata; fall back to
    // the raw model id / empty when the provider isn't a known catalog entry.
    let entry = catalog::build_catalog(config)
        .into_iter()
        .find(|e| e.id == provider_id);
    let display_name = entry
        .as_ref()
        .map(|e| e.name.clone())
        .unwrap_or_else(|| model.clone());
    let description = entry
        .as_ref()
        .map(|e| e.description.clone())
        .unwrap_or_default();
    let context_window = entry.as_ref().map(|e| e.context_window()).unwrap_or(0);
    let api_key_ready = entry.as_ref().map(|e| e.key_ready()).unwrap_or(false);

    let model_info = ModelInfo {
        provider: provider_id,
        capabilities: derive_capabilities(&model),
        display_name,
        model,
        context_window,
        api_key_ready,
        description,
    };

    let tools = agent.snapshot_tools();
    let permissions = agent.allowed_tools_structured();
    let skills = agent.snapshot_skills();

    // Per-server tool names: match the agent's installed tools by their
    // `mcp__<server>__<tool>` naming convention. The status enum only carries a
    // count, so this is where the per-server list is reconstructed.
    let mcp = mcp_statuses
        .iter()
        .map(|(name, status)| {
            let prefix = format!("mcp:{}", name);
            let tool_names: Vec<String> = tools
                .iter()
                .filter(|t| t.source == prefix)
                .map(|t| t.name.clone())
                .collect();
            let (connected, disabled, failure) = match status {
                McpConnectionStatus::Connected { .. } => (true, false, None),
                McpConnectionStatus::Disabled => (false, true, None),
                McpConnectionStatus::Failed(reason) => (false, false, Some(reason.clone())),
            };
            McpServerInfo {
                name: name.clone(),
                connected,
                disabled,
                failure,
                tool_names,
            }
        })
        .collect();

    SessionContextSnapshot {
        model: model_info,
        tools,
        permissions,
        skills,
        mcp,
    }
}

/// Heuristic model-capability hints for the session modal. Per-model capability
/// data is resolved from the [`neenee_core::model`] registry; the harness
/// depends on tool calling for every provider, so it is always advertised.
pub fn derive_capabilities(model: &str) -> Vec<String> {
    let mut caps = vec!["tool calling".to_string()];
    if neenee_core::resolve_model(model).reasoning {
        caps.push("reasoning".to_string());
    }
    caps
}

/// Render-ready list of past sessions for the picker. Failures (unreadable
/// store, corrupt index) degrade to an empty list rather than surfacing an
/// error: the picker is non-modal and a missing list is recoverable.
pub async fn build_sessions_overview(session: &SessionStore) -> Vec<SessionOverview> {
    match session.list().await {
        Ok(items) => items
            .into_iter()
            .map(|item| SessionOverview {
                id: item.id,
                overview: item.overview,
                created_at: item.created_at,
                updated_at: item.updated_at,
                message_count: item.message_count,
                active: item.active,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}
