//! Chat-turn and `!`-prefix shell-command handlers, extracted verbatim from
//! the agent background task's `match req { … }` dispatch.
//!
//! Each handler is one match arm, lifted unchanged. Parameters are named to
//! match the original loop locals (`active_view_side`, `side`, `agent`,
//! `history`, `session`, `ctt_clone`, `generation_clone`, `resp_tx`,
//! `pursuit_service`, `config`, …) so the body reads exactly as it did inline.

use neenee_agent::orchestration::TurnInput;
use neenee_agent::Agent;
use neenee_core::{AgentResponse, Message, PursuitService};
use neenee_store::{config::Config, session::SessionStore};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock as AsyncRwLock};
use tokio_util::sync::CancellationToken;

use crate::shell::run_shell_command;
use crate::side::{start_active_turn, SideSession};

/// `AgentRequest::Chat` — start an interactive turn against whichever session
/// the user is currently composing into (primary or `/btw` side).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn chat(
    active_view_side: &AtomicBool,
    side: &Arc<AsyncRwLock<Option<SideSession>>>,
    agent: &Arc<Agent>,
    history: &Arc<tokio::sync::Mutex<Vec<Message>>>,
    session: &Arc<SessionStore>,
    ctt_clone: &Arc<AsyncRwLock<Option<CancellationToken>>>,
    generation_clone: &Arc<AtomicU64>,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    pursuit_service: PursuitService,
    config: &Config,
    text: String,
    images: Vec<neenee_core::ImagePart>,
) {
    start_active_turn(
        active_view_side,
        side,
        agent,
        history,
        session,
        ctt_clone,
        generation_clone,
        resp_tx,
        pursuit_service,
        config,
        TurnInput {
            prompt: text,
            hidden: false,
            display_prompt: None,
            images,
        },
    )
    .await;
}

/// `AgentRequest::ShellCommand` — the `!` prefix path: run the command
/// directly through the `bash` tool, bypassing the LLM. The lifecycle mirrors
/// a normal tool step (`ToolCall` → live `ToolStream` → `ToolResult`) so the
/// existing render path picks it up with no special-casing. Spawned onto its
/// own task so it runs concurrently with the dispatch loop.
pub(crate) async fn shell(
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    ctt_clone: &Arc<AsyncRwLock<Option<CancellationToken>>>,
    generation_clone: &Arc<AtomicU64>,
    agent: &Arc<Agent>,
    session: &Arc<SessionStore>,
    command: String,
) {
    let shell_tx = resp_tx.clone();
    let shell_token_slot = ctt_clone.clone();
    let shell_generation = generation_clone.clone();
    let shell_agent = agent.clone();
    let shell_session_id = session.id().await;
    tokio::spawn(async move {
        run_shell_command(
            command,
            shell_tx,
            shell_session_id,
            shell_token_slot,
            shell_generation,
            shell_agent,
        )
        .await;
    });
}
