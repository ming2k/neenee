//! `/btw` side-conversation machinery (ADR-0017): the live side session, the
//! primary-status watcher that streams coarse updates to the side banner, and
//! the active-turn router that directs a prompt at whichever session the user
//! is currently composing into. Extracted verbatim from `main.rs`.
//!
//! The primary turn machinery is intentionally untouched here — a side session
//! peers the primary's per-turn state with its own `Agent` + store + history +
//! cancellation slot, so a side turn runs concurrently with the primary turn
//! without disturbing the primary's token/generation.

use neenee_agent::orchestration::{
    start_interactive_turn, CompactionSettings, InteractiveTurnContext, ProxyProvider, TurnInput,
};
use neenee_agent::skills::SkillRegistry;
use neenee_agent::Agent;
use neenee_core::{AgentResponse, Message, ParentStatus, Provider, Tool};
use neenee_store::config::Config;
use neenee_store::session::SessionStore;
use std::sync::RwLock;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{mpsc, RwLock as AsyncRwLock};
use tokio_util::sync::CancellationToken;

use crate::agent_setup::active_context_window;

/// A live `/btw` side conversation (ADR-0017). Peers the primary session's
/// loose per-turn state with its own [`Agent`], [`SessionStore`], history
/// mutex, and cancellation slot, so a side turn runs concurrently with the
/// primary turn without disturbing the primary's token/generation. The side
/// store is pinned to a self-contained file written by
/// [`SessionStore::fork_to_side`]; only that file is mutated by side turns.
pub struct SideSession {
    pub id: String,
    pub agent: Arc<Agent>,
    pub store: Arc<SessionStore>,
    pub history: Arc<tokio::sync::Mutex<Vec<Message>>>,
    pub token_slot: Arc<AsyncRwLock<Option<CancellationToken>>>,
    pub generation: Arc<AtomicU64>,
}

impl SideSession {
    /// Fork the primary into a self-contained side file and construct a fresh
    /// [`Agent`] + store + history bound to it. The primary's active pointer,
    /// history, and in-flight turn are left untouched. Returns [`None`] when
    /// the fork or side-store open fails; the caller surfaces the error.
    pub async fn build(
        primary: &SessionStore,
        base_tools: &[Arc<dyn Tool>],
        provider_holder: &Arc<RwLock<Arc<dyn Provider>>>,
        skills: SkillRegistry,
        project_root: &std::path::Path,
    ) -> Result<Self, String> {
        let (side_id, _parent_id) = primary.fork_to_side().await?;
        let store = Arc::new(primary.open_side(&side_id).await?);
        let history = Arc::new(tokio::sync::Mutex::new(store.messages().await));

        // Fresh side agent. The provider is shared through the same
        // `ProxyProvider` holder as the primary, which clones the inner
        // `Arc<dyn Provider>` per call and is safe under concurrency
        // (ADR-0017 §2). Tools come from the cached base snapshot (built-in +
        // MCP, no `SubagentTool`) so a side chat recurses no further than the
        // primary — mirroring the subagent profile filter in `SubagentTool`.
        let side_provider: Arc<dyn Provider> =
            Arc::new(ProxyProvider::new(provider_holder.clone()));
        let agent = Arc::new(Agent::new(side_provider, base_tools.to_vec(), skills));
        agent.set_thread_id(&side_id);
        agent.set_project_root(Some(project_root.to_path_buf()));
        // A side conversation is a quick aside; auto-approve its write tools so
        // it never raises a permission modal whose reply could not be routed
        // back to the side `Agent` through the shared permission channel. This
        // mirrors the subagent policy (`subagent_tool.rs` sets `auto_approve`).
        agent.set_auto_approve(true);

        Ok(Self {
            id: side_id,
            agent,
            store,
            history,
            token_slot: Arc::new(AsyncRwLock::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
        })
    }
}

/// Coarse primary-session status, derived from the primary's live token slot
/// for the `/btw` parent-status watcher (ADR-0017 §5). A cancelled or absent
/// token means the primary is idle; a live (uncancelled) token means running.
pub async fn primary_status(
    primary_token_slot: &Arc<AsyncRwLock<Option<CancellationToken>>>,
) -> ParentStatus {
    match primary_token_slot.read().await.as_ref() {
        Some(token) if !token.is_cancelled() => ParentStatus::Running,
        _ => ParentStatus::Idle,
    }
}

/// Watch the primary turn while a `/btw` side session is live and stream
/// coarse [`ParentStatus`] updates to the TUI's side banner (ADR-0017 §5).
/// Self-terminates once the side session is torn down. Emits only on change
/// so a long-running primary turn does not flood the channel.
pub fn spawn_parent_status_watcher(
    side: Arc<AsyncRwLock<Option<SideSession>>>,
    primary_token_slot: Arc<AsyncRwLock<Option<CancellationToken>>>,
    tx: mpsc::UnboundedSender<AgentResponse>,
) {
    tokio::spawn(async move {
        let mut last: Option<ParentStatus> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            if side.read().await.is_none() {
                break;
            }
            let status = primary_status(&primary_token_slot).await;
            if last != Some(status) {
                last = Some(status);
                let _ = tx.send(AgentResponse::ParentStatus(status));
            }
        }
    });
}

/// Start an interactive turn against whichever session the user is currently
/// composing into — the primary, or the live `/btw` side session when the
/// active-view flag is set (ADR-0017). A stale flag (side torn down
/// concurrently) falls back to the primary so the prompt is never silently
/// dropped. Compaction/retry knobs are resolved once from the primary agent +
/// config, which is correct because the side shares the same provider/model.
#[allow(clippy::too_many_arguments)]
pub async fn start_active_turn(
    active_view_side: &AtomicBool,
    side: &Arc<AsyncRwLock<Option<SideSession>>>,
    primary_agent: &Arc<Agent>,
    primary_history: &Arc<tokio::sync::Mutex<Vec<Message>>>,
    primary_session: &Arc<SessionStore>,
    primary_token_slot: &Arc<AsyncRwLock<Option<CancellationToken>>>,
    primary_generation: &Arc<AtomicU64>,
    tx: &mpsc::UnboundedSender<AgentResponse>,
    config: &Config,
    input: TurnInput,
) {
    let compaction = CompactionSettings::from_config(config, active_context_window(primary_agent));
    let retry_max_attempts = config.provider_retry_max_attempts;
    let retry_base_ms = config.provider_retry_base_ms;
    let retry_max_ms = config.provider_retry_max_ms;

    // Resolve which live session this turn belongs to, cloning the per-session
    // Arcs out of the registry under a short-lived read lock. The guard drops
    // at the end of this statement, before the turn starts.
    let (agent, history, session, token_slot, generation, session_id) =
        if active_view_side.load(Ordering::SeqCst) {
            let guard = side.read().await;
            if let Some(s) = guard.as_ref() {
                (
                    s.agent.clone(),
                    s.history.clone(),
                    s.store.clone(),
                    s.token_slot.clone(),
                    s.generation.clone(),
                    s.id.clone(),
                )
            } else {
                (
                    primary_agent.clone(),
                    primary_history.clone(),
                    primary_session.clone(),
                    primary_token_slot.clone(),
                    primary_generation.clone(),
                    primary_session.id().await,
                )
            }
        } else {
            (
                primary_agent.clone(),
                primary_history.clone(),
                primary_session.clone(),
                primary_token_slot.clone(),
                primary_generation.clone(),
                primary_session.id().await,
            )
        };

    start_interactive_turn(
        InteractiveTurnContext {
            agent,
            history,
            tx: tx.clone(),
            token_slot,
            generation_counter: generation,
            session,
            session_id,
            compaction,
            retry_max_attempts,
            retry_base_ms,
            retry_max_ms,
        },
        input,
    )
    .await;
}
