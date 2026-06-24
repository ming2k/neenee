//! Session-management, tool-toggle, and `/btw` side-view handlers, extracted
//! verbatim from the agent background task's `match req { … }` dispatch.
//!
//! Each handler is one match arm, lifted unchanged. Parameters are named to
//! match the original loop locals (`session`, `agent`, `resp_tx`, `side`,
//! `active_view_side`, …) so the body reads exactly as it did inline.

use neenee_agent::skills::SkillRegistry;
use neenee_agent::Agent;
use neenee_core::{AgentResponse, McpConnectionStatus};
use neenee_store::{config::Config, session::SessionStore};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock as AsyncRwLock};

use crate::session_view::{build_session_context, build_sessions_overview};
use crate::side::SideSession;

/// `AgentRequest::DeleteSession` — delete by id (or short-id prefix) and push
/// a fresh sessions-overview snapshot, or surface the storage error.
pub(crate) async fn delete(
    session: &Arc<SessionStore>,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    id: String,
) {
    match session.delete(&id).await {
        Ok(()) => {
            let _ = resp_tx.send(AgentResponse::SessionsOverview(
                build_sessions_overview(session).await,
            ));
        }
        Err(error) => {
            let _ = resp_tx.send(AgentResponse::Error(error));
        }
    }
}

/// `AgentRequest::QuerySessionContext` — build and push the
/// model/tools/permissions/skills/mcp snapshot for the session modal.
pub(crate) fn query_context(
    agent: &Agent,
    skills_registry: &Arc<SkillRegistry>,
    mcp_statuses: &[(String, McpConnectionStatus)],
    config: &Config,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
) {
    let snapshot = build_session_context(agent, skills_registry, mcp_statuses, config);
    let _ = resp_tx.send(AgentResponse::SessionContext(snapshot));
}

/// `AgentRequest::RevokePermission` — drop one cached always-allow rule and
/// push a refreshed snapshot, or report there was nothing matching.
pub(crate) fn revoke_permission(
    agent: &Agent,
    skills_registry: &Arc<SkillRegistry>,
    mcp_statuses: &[(String, McpConnectionStatus)],
    config: &Config,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    tool: String,
    scope: String,
) {
    let removed = agent.revoke_allowed_tool(&tool, &scope);
    if removed {
        let snapshot = build_session_context(agent, skills_registry, mcp_statuses, config);
        let _ = resp_tx.send(AgentResponse::SessionContext(snapshot));
    } else {
        let _ = resp_tx.send(AgentResponse::Error(format!(
            "No cached always-allow rule for {} {}.",
            tool, scope
        )));
    }
}

/// `AgentRequest::ToggleTool` — enable/disable a tool for the session and
/// push a refreshed snapshot. A no-op (unknown tool, or already in the target
/// state) still refreshes the snapshot so the modal settles rather than
/// leaving the row looking stale, plus surfaces a soft error.
pub(crate) fn toggle_tool(
    agent: &Agent,
    skills_registry: &Arc<SkillRegistry>,
    mcp_statuses: &[(String, McpConnectionStatus)],
    config: &Config,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    name: String,
    enabled: bool,
) {
    let changed = agent.set_tool_enabled(&name, enabled);
    let snapshot = build_session_context(agent, skills_registry, mcp_statuses, config);
    if !changed {
        let _ = resp_tx.send(AgentResponse::Error(format!(
            "Tool '{}' is unknown or already {}.",
            name,
            if enabled { "enabled" } else { "disabled" }
        )));
    }
    let _ = resp_tx.send(AgentResponse::SessionContext(snapshot));
}

/// `AgentRequest::ExitSideView` — tear down the live `/btw` side session
/// (ADR-0017). Any in-flight side turn is cancelled; the side file stays on
/// disk, recoverable via `/sessions`. The primary turn — if running — is
/// untouched.
pub(crate) async fn exit_side_view(
    side: &Arc<AsyncRwLock<Option<SideSession>>>,
    active_view_side: &AtomicBool,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
) {
    if let Some(s) = side.write().await.take() {
        if let Some(token) = s.token_slot.write().await.take() {
            s.agent.reject_pending_permissions();
            token.cancel();
        }
    }
    active_view_side.store(false, Ordering::SeqCst);
    let _ = resp_tx.send(AgentResponse::SideViewClosed);
}
