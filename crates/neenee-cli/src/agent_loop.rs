//! The agent background task: a single async loop that owns every live piece
//! of harness state (the `Agent`, the session store, config, the
//! provider-usage telemetry, the `/btw` side registry, the cancellation
//! slot, …) and dispatches [`AgentRequest`]s coming from the TUI.
//!
//! Extracted from the `tokio::spawn` block that used to live inline in
//! `main.rs`. The dozens of `Arc` handles the block closed over are collected
//! into [`Harness`], which `main` constructs and hands to [`run`]. Each
//! `AgentRequest` variant is routed to its handler in [`handlers`](crate::handlers);
//! the loop body itself is just the prologue (initial snapshots/telemetry) +
//! the thin dispatch `match`.
//!
//! The destructuring-into-locals at the top of [`run`] preserves the original
//! inline-block identifiers (`resp_tx`, `agent`, `session`, …) so the
//! transplant was pure code motion.

use neenee_agent::catalog;
use neenee_agent::orchestration::send_harness_state;
use neenee_agent::skills::SkillRegistry;
use neenee_agent::{Agent, SubagentRegistry};
use neenee_core::{AgentRequest, AgentResponse, McpConnectionStatus, Message, Provider, Tool};
use neenee_store::{
    config::Config, embedding, provider_usage::ProviderUsage, session::SessionStore, RepeatStore,
};
use neenee_tools::commands::CustomCommand;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64},
    Arc, RwLock,
};
use tokio::sync::{mpsc, RwLock as AsyncRwLock};
use tokio_util::sync::CancellationToken;

use crate::session_view::{build_sessions_overview, provider_key_status};
use crate::side::SideSession;
use crate::startup::StartupMode;

/// Every piece of long-lived state the agent background task owns. Built by
/// `main` after startup wiring and moved into [`run`].
///
/// The field set is exactly the set of locals the old inline
/// `tokio::spawn(async move { … })` block closed over; nothing has been added
/// or removed, only named. Fields are `pub(crate)` because the binary is the
/// only consumer.
#[allow(clippy::type_complexity)]
pub(crate) struct Harness {
    /// Responses bound for the TUI (`resp_tx` in the old code).
    pub tx: mpsc::UnboundedSender<AgentResponse>,
    /// Inbound request channel, cloned so `/repeat` can self-fire a `Chat`
    /// (`req_tx_for_commands` in the old code).
    pub req_tx: mpsc::UnboundedSender<AgentRequest>,
    /// The primary agent.
    pub agent: Arc<Agent>,
    /// The primary session store.
    pub session: Arc<SessionStore>,
    /// Primary turn history mirror.
    pub history: Arc<tokio::sync::Mutex<Vec<Message>>>,
    /// Live config; mutated by provider/favorite/default switches and saved.
    pub config: Config,
    /// Per-model usage telemetry; mutated by activations and switches.
    pub provider_usage: ProviderUsage,
    /// The shared provider holder backing the `ProxyProvider`
    /// (`provider_for_task` in the old code).
    pub provider_holder: Arc<RwLock<Arc<dyn Provider>>>,
    /// Shared skills registry.
    pub skills_registry: Arc<SkillRegistry>,
    /// Full-duplex subagent registry (ADR-0029): maps the parent tool-call
    /// id to the live child handle so a permission / ask_user reply can be
    /// routed back down into the specific subagent that surfaced it.
    pub subagent_registry: Arc<SubagentRegistry>,
    /// MCP server name → connection status snapshot.
    pub mcp_statuses: Vec<(String, McpConnectionStatus)>,
    /// User-defined `/<name>` commands (`commands_for_task` in the old code).
    pub commands: Arc<HashMap<String, CustomCommand>>,
    /// Project embedding index for `/search` (`embedding_store_for_commands`).
    pub embedding_store: Arc<AsyncRwLock<embedding::EmbeddingStore>>,
    /// Durable store for `/repeat` cron jobs (`repeat_store_for_commands`).
    pub repeat_store: RepeatStore,
    /// Primary turn cancellation slot (`ctt_clone` in the old code).
    pub current_task_token: Arc<AsyncRwLock<Option<CancellationToken>>>,
    /// Primary turn generation counter (`generation_clone` in the old code).
    pub task_generation: Arc<AtomicU64>,
    /// Live `/btw` side session registry (ADR-0017).
    pub side: Arc<AsyncRwLock<Option<SideSession>>>,
    /// Whether the user is composing into the side view right now.
    pub active_view_side: Arc<AtomicBool>,
    /// Cached base toolset snapshot for side-session construction
    /// (`base_tools_for_side`).
    pub base_tools: Arc<Vec<Arc<dyn Tool>>>,
    /// Project root for side-session pinning (`project_root_for_side`).
    pub project_root: PathBuf,
    /// Startup mode — read by the misplaced SessionStart-hooks block inside
    /// `/pursue status` (preserved verbatim; see note in [`run`]).
    pub startup: StartupMode,
    /// Whether the sessions picker should open on launch.
    pub open_picker_on_start: bool,
}

/// Run the agent background task to completion (i.e. until the TUI drops the
/// request channel and `req_rx` closes).
///
/// The [`Harness`] is destructured into locals with the original inline-block
/// names so the transplanted dispatch body reads unchanged. This is
/// deliberate: the move from `main.rs` is pure code motion, and any rewrite
/// of the ~1.5k-line match is deferred to the `handlers::*` split.
//
// NOTE: a `refresh_agent_pursuit` + SessionStart-hooks block inside the
// `/pursue status` branch has inconsistent indentation and looks misplaced —
// it fires session-start hooks every time `/pursue status` runs. Preserved
// verbatim here; not this refactor's job to fix.
pub(crate) async fn run(mut req_rx: mpsc::UnboundedReceiver<AgentRequest>, h: Harness) {
    let Harness {
        tx: resp_tx,
        req_tx: req_tx_for_commands,
        agent,
        session,
        history,
        mut config,
        mut provider_usage,
        provider_holder: provider_for_task,
        skills_registry,
        subagent_registry,
        mcp_statuses,
        commands: commands_for_task,
        embedding_store: embedding_store_for_commands,
        repeat_store: repeat_store_for_commands,
        current_task_token: ctt_clone,
        task_generation: generation_clone,
        side,
        active_view_side,
        base_tools: base_tools_for_side,
        project_root: project_root_for_side,
        startup,
        open_picker_on_start,
    } = h;
    // The old inline block captured two clones of the skills registry —
    // `skills_registry` (read for the session-context snapshot) and
    // `skills_registry_for_commands` (handed to the `/skills` / `/skill`
    // tools). One Harness field backs both; re-create the alias here.
    let skills_registry_for_commands = skills_registry.clone();

    send_harness_state(&resp_tx, &session.id().await, &agent, "idle");
    let _ = resp_tx.send(AgentResponse::ProviderKeys(provider_key_status(&config)));
    // Record that the default model was activated on startup, so the
    // picker's recency ordering reflects "last used = now". Best-effort:
    // usage tracking is rebuildable state and must never block startup.
    {
        let initial_id = catalog::default_provider_id(&config).to_string();
        provider_usage.record(&initial_id);
        if let Err(error) = provider_usage.save() {
            tracing::warn!(?error, "could not persist model usage telemetry");
        }
    }
    // Push the initial model-picker snapshot (default id + per-model
    // favorite / key-ready / last-used) so the picker is ready the moment
    // the user opens it.
    let _ = resp_tx.send(AgentResponse::ProviderPicker(catalog::build_picker_state(
        &config,
        &provider_usage,
    )));
    if open_picker_on_start {
        let _ = resp_tx.send(AgentResponse::SessionsOverview(
            build_sessions_overview(&session).await,
        ));
    }
    while let Some(req) = req_rx.recv().await {
        match req {
            AgentRequest::Interrupt => {
                crate::handlers::permission::interrupt(&agent, &session, &resp_tx, &ctt_clone)
                    .await;
            }
            AgentRequest::PermissionReply {
                request_id,
                decision,
                parent_call_id,
            } => {
                crate::handlers::permission::reply(
                    &agent,
                    &subagent_registry,
                    &resp_tx,
                    request_id,
                    decision,
                    parent_call_id,
                )
                .await;
            }
            AgentRequest::UserQuestionReply {
                request_id,
                answers,
                parent_call_id,
            } => {
                crate::handlers::permission::reply_question(
                    &agent,
                    &subagent_registry,
                    &side,
                    &resp_tx,
                    request_id,
                    answers,
                    parent_call_id,
                )
                .await;
            }
            AgentRequest::SwitchProvider {
                provider_type,
                model,
                api_key,
                base_url,
            } => {
                crate::handlers::provider::switch(
                    &mut config,
                    &agent,
                    &provider_for_task,
                    &resp_tx,
                    &mut provider_usage,
                    provider_type,
                    model,
                    api_key,
                    base_url,
                )
                .await;
            }
            AgentRequest::ToggleFavorite { id } => {
                crate::handlers::provider::toggle_favorite(
                    &mut config,
                    &resp_tx,
                    &provider_usage,
                    id,
                )
                .await;
            }
            AgentRequest::SetDefaultModel { id } => {
                crate::handlers::provider::set_default_model(
                    &mut config,
                    &agent,
                    &provider_for_task,
                    &resp_tx,
                    &mut provider_usage,
                    id,
                )
                .await;
            }
            AgentRequest::DeleteSession { id } => {
                crate::handlers::session::delete(&session, &resp_tx, id).await;
            }
            AgentRequest::QuerySessionContext => {
                crate::handlers::session::query_context(
                    &agent,
                    &skills_registry,
                    &mcp_statuses,
                    &config,
                    &resp_tx,
                );
            }
            AgentRequest::RevokePermission { tool, scope } => {
                crate::handlers::session::revoke_permission(
                    &agent,
                    &skills_registry,
                    &mcp_statuses,
                    &config,
                    &resp_tx,
                    tool,
                    scope,
                );
            }
            AgentRequest::ToggleTool { name, enabled } => {
                crate::handlers::session::toggle_tool(
                    &agent,
                    &skills_registry,
                    &mcp_statuses,
                    &config,
                    &resp_tx,
                    name,
                    enabled,
                );
            }
            AgentRequest::SlashCommand(cmd) => {
                crate::handlers::slash::dispatch(
                    cmd,
                    &config,
                    &agent,
                    &resp_tx,
                    &session,
                    &history,
                    &ctt_clone,
                    &generation_clone,
                    &side,
                    &active_view_side,
                    &base_tools_for_side,
                    &provider_for_task,
                    skills_registry.clone(),
                    &skills_registry_for_commands,
                    &mcp_statuses,
                    &commands_for_task,
                    &embedding_store_for_commands,
                    &repeat_store_for_commands,
                    &req_tx_for_commands,
                    &project_root_for_side,
                    &startup,
                )
                .await;
            }
            AgentRequest::Chat { text, images } => {
                crate::handlers::chat::chat(
                    &active_view_side,
                    &side,
                    &agent,
                    &history,
                    &session,
                    &ctt_clone,
                    &generation_clone,
                    &resp_tx,
                    &config,
                    text,
                    images,
                )
                .await;
            }
            AgentRequest::ShellCommand { command } => {
                crate::handlers::chat::shell(
                    &resp_tx,
                    &ctt_clone,
                    &generation_clone,
                    &agent,
                    &session,
                    command,
                )
                .await;
            }
            AgentRequest::ExitSideView => {
                crate::handlers::session::exit_side_view(&side, &active_view_side, &resp_tx).await;
            }
        }
    }
}
