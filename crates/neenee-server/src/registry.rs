//! The session registry: the map of live sessions and the per-session handle.
//!
//! Each [`SessionHandle`] is an independent `(Agent, SessionStore, channels)`
//! triple — the per-session half of `neenee-code`'s `main.rs:184-408`. A session
//! owns its own `Agent` (provider/tools/pursuit/thread_id/permissions cannot be
//! shared across sessions), plus a request channel for inbound
//! [`AgentRequest`]s and a **broadcast** response channel so multiple clients
//! can subscribe to the same session's event stream.

#![allow(unused_imports)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use neenee_agent::Agent;
use neenee_core::{AgentRequest, AgentResponse};
use neenee_store::session::SessionStore;
use tokio::sync::{Mutex as AsyncMutex, RwLock as AsyncRwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::SharedState;

/// Capacity of the per-session response broadcast channel. Each subscriber
/// gets its own lag buffer; slow subscribers see `RecvError::Lagged` rather
/// than blocking the agent task. Generous because a single turn can emit
/// hundreds of stream deltas, and the TUI/renderer path is the latency-
/// sensitive consumer a browser frontend must not starve.
//
// Used by `SessionRegistry::create_session` once the driver task is populated.
#[allow(dead_code)]
const RESPONSE_BROADCAST_CAPACITY: usize = 1024;

/// One live session: its id, its inbound request channel sender, and a
/// broadcast sender fanning its [`AgentResponse`] stream out to N subscribers.
///
/// The owning [`Agent`] and [`SessionStore`] live inside the driver task
/// spawned by [`SessionRegistry::create_session`]; the handle is the cheap
/// reference clients hold to send requests and subscribe to events without
/// touching the agent directly.
pub struct SessionHandle {
    /// The session id (also the `SessionStore` id / thread id the agent is
    /// bound to).
    pub id: String,
    /// Send an [`AgentRequest`] into the session's driver task. Cloned cheaply
    /// for each client.
    pub req_tx: mpsc::UnboundedSender<AgentRequest>,
    /// Subscribe to this session's response stream. Each client gets its own
    /// receiver; the driver task is the single sender.
    pub events: tokio::sync::broadcast::Sender<AgentResponse>,
    /// Wall-clock creation instant, for session listings and ordering.
    pub created_at: Instant,
}

impl SessionHandle {
    /// Subscribe to this session's response stream. A convenience over
    /// `self.events.subscribe()` so callers don't import `broadcast` directly.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentResponse> {
        self.events.subscribe()
    }
}

/// The live-session map. One of these lives in the server process; the TUI (in
/// single-session embedded mode) constructs a one-element registry for
/// compatibility.
///
/// Built from [`SharedState`]; each [`Self::create_session`] call assembles a
/// fresh `Agent` + `SessionStore` from the shared singletons, spawns its driver
/// task, and registers the handle.
pub struct SessionRegistry {
    /// The process-level singletons every session borrows from.
    shared: Arc<SharedState>,
    /// `session_id → handle`. Write-locked only on create/delete/exit; the
    /// driver tasks themselves never touch this map (they communicate purely
    /// over channels), so a session turn never contends with lookups.
    sessions: AsyncRwLock<HashMap<String, Arc<SessionHandle>>>,
}

impl SessionRegistry {
    /// Wrap a [`SharedState`] in an empty registry.
    pub fn new(shared: Arc<SharedState>) -> Self {
        Self {
            shared,
            sessions: AsyncRwLock::new(HashMap::new()),
        }
    }

    /// A snapshot of the live session ids, oldest first. For session pickers
    /// and the `/sessions` listing.
    pub async fn list(&self) -> Vec<String> {
        let mut ids: Vec<(String, Instant)> = self
            .sessions
            .read()
            .await
            .iter()
            .map(|(id, h)| (id.clone(), h.created_at))
            .collect();
        ids.sort_by_key(|(_, t)| *t);
        ids.into_iter().map(|(id, _)| id).collect()
    }

    /// Look up a live session by id.
    pub async fn get(&self, id: &str) -> Option<Arc<SessionHandle>> {
        self.sessions.read().await.get(id).cloned()
    }

    /// Create and register a fresh session.
    ///
    /// This is the `main.rs:184-408` per-session construction sequence: load
    /// (or resume) the `SessionStore`, assemble the tool context from the
    /// shared singletons, build the `Agent`, bind pursuit/thread/todos, then
    /// spawn the driver task that drains `AgentRequest`s and broadcasts
    /// `AgentResponse`s.
    ///
    /// # Not yet implemented
    ///
    /// The body is the next step of the migration: it moves
    /// `neenee-code`'s `agent_loop::Harness` construction + `agent_loop::run`
    /// loop in here, with the single change that `resp_tx` becomes a
    /// `broadcast::Sender` (multi-subscriber) rather than an `mpsc::Sender`
    /// (single-consumer). The harness fields and dispatch match arms are
    /// already TUI-free.
    pub async fn create_session(
        &self,
        _resume: Option<&str>,
    ) -> Result<Arc<SessionHandle>, String> {
        // TODO(server-move): port agent_loop::Harness + run into this crate.
        // The shape is:
        //   1. let session = Arc::new(SessionStore::load_for_project(...));
        //   2. let (req_tx, req_rx) = mpsc::unbounded_channel();
        //   3. let (events_tx, _) = broadcast::channel(RESPONSE_BROADCAST_CAPACITY);
        //   4. assemble the toolset via neenee_core::collect_toolset(&tool_ctx);
        //   5. let agent = Agent::new(self.shared.agent_provider.clone(), ...);
        //   6. bind pursuit / thread_id / todos / hooks / permissions;
        //   7. spawn a driver task: drain req_rx, dispatch to handlers, and
        //      broadcast each AgentResponse on events_tx (the mpsc→broadcast
        //      bridge is the only behavioral change vs the current loop).
        //   8. insert Arc<SessionHandle> into the map and return it.
        //
        // Until the handler modules move, callers should keep driving the
        // agent through `neenee-code`'s existing in-process path.
        Err("SessionRegistry::create_session is not yet populated — see TODO".into())
    }

    /// Tear down a session: cancel its in-flight turn (if any), let its driver
    /// task drain, fire SessionEnd hooks, and remove it from the map.
    pub async fn close_session(&self, _id: &str) -> Result<(), String> {
        // TODO(server-move): send AgentRequest::Shutdown, await the driver
        // task's join (held in a side table), fire session-end hooks, remove.
        Err("SessionRegistry::close_session is not yet populated — see TODO".into())
    }

    /// Shared state accessor (for the transport layer and admin commands).
    pub fn shared(&self) -> &Arc<SharedState> {
        &self.shared
    }
}

/// The per-turn cancellation slot + generation counter a driver task owns.
/// Mirrors `agent_loop::Harness`'s `current_task_token` / `task_generation` so
/// the server-side driver can be interrupted the same way the TUI's is.
///
/// Held inside the driver task; a clone of the `CancellationToken` is kept by
/// the registry so [`SessionRegistry::close_session`] can cancel without
/// routing through the request channel.
#[allow(dead_code)]
pub(crate) struct TurnControl {
    pub cancel: CancellationToken,
    pub generation: Arc<std::sync::atomic::AtomicU64>,
    /// Joined when the session closes. `None` until the driver task is spawned.
    pub join: AsyncMutex<Option<tokio::task::JoinHandle<()>>>,
}
