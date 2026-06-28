//! Permission / interruption handlers, extracted verbatim from the agent
//! background task's `match req { … }` dispatch.
//!
//! Each handler is one match arm, lifted unchanged. Parameters are named to
//! match the original loop locals (`agent`, `session`, `resp_tx`,
//! `ctt_clone`, `side`, `subagent_registry`, …) so the body reads exactly as
//! it did inline.

use neenee_agent::orchestration::send_harness_state;
use neenee_agent::{Agent, SubagentRegistry};
use neenee_core::{AgentResponse, PermissionDecision};
use neenee_store::session::SessionStore;
use std::sync::Arc;
use tokio::sync::{RwLock as AsyncRwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::side::SideSession;

/// `AgentRequest::Interrupt` — reject every pending permission / question,
/// flip the harness to idle eagerly (before the in-flight turn's own terminal
/// idle snapshot, which is gated behind persistence fsyncs), then cancel the
/// live token. The generation counter is deliberately NOT bumped here so the
/// stale turn still emits its own "... \[Interrupted\]" cleanup.
pub async fn interrupt(
    agent: &Agent,
    session: &Arc<SessionStore>,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    ctt_clone: &Arc<AsyncRwLock<Option<CancellationToken>>>,
) {
    agent.reject_pending_permissions();
    agent.reject_pending_user_questions();
    let _ = resp_tx.send(AgentResponse::PermissionsCleared);

    // Flip the harness to idle the instant interrupt is requested — BEFORE
    // the in-flight turn unwinds. The work itself stops the moment the token
    // is cancelled below, but the turn task's own terminal "idle" snapshot is
    // only sent at the very end of its cleanup, which is gated behind
    // persistence fsyncs (`session.replace_messages` inside `execute_turn`,
    // then `set_checkpoint` in `start_pursuit`). Without this eager snapshot
    // the activity bar keeps showing the stale "pursue"/"running" loop_status
    // — and a climbing elapsed timer — for the whole disk-write window, which
    // reads as "still working" when the work is already stopped.
    //
    // This is idempotent with the stale task's later idle send: if no new
    // turn starts, both snapshots are "idle"; if one does, it bumps
    // generation itself and its "running" snapshot supersedes, while the
    // stale task's generation-guarded idle send is skipped
    // (`orchestration.rs` start_pursuit / start_interactive_turn /
    // run_shell_command).
    send_harness_state(resp_tx, &session.id().await, agent, "idle");

    let mut token = ctt_clone.write().await;
    if let Some(t) = token.take() {
        t.cancel();
    }
}

/// `AgentRequest::PermissionReply` — full-duplex routing (ADR-0029): a reply
/// tagged with a `parent_call_id` targets a subagent's parked oneshot via the
/// registry handle; `None` keeps the legacy top-level (/btw side) path. A late
/// reply after the child finished finds no handle and falls through to the
/// "no longer pending" error.
pub async fn reply(
    agent: &Agent,
    subagent_registry: &Arc<SubagentRegistry>,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    request_id: String,
    decision: PermissionDecision,
    parent_call_id: Option<String>,
) {
    let resolved = if let Some(parent) = &parent_call_id {
        subagent_registry
            .get(parent)
            .is_some_and(|handle| handle.reply_permission(&request_id, decision))
    } else {
        agent.reply_permission(&request_id, decision)
    };
    if !resolved {
        let _ = resp_tx.send(AgentResponse::Error(
            "Permission request is no longer pending.".to_string(),
        ));
    }
}

/// `AgentRequest::UserQuestionReply` — mirror the permission arm: a
/// `parent_call_id` targets the subagent; otherwise try the primary, then a
/// `/btw` side agent (ADR-0017).
pub async fn reply_question(
    agent: &Agent,
    subagent_registry: &Arc<SubagentRegistry>,
    side: &Arc<AsyncRwLock<Option<SideSession>>>,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    request_id: String,
    answers: Vec<Vec<String>>,
    parent_call_id: Option<String>,
) {
    let resolved = if let Some(parent) = &parent_call_id {
        subagent_registry
            .get(parent)
            .is_some_and(|handle| handle.reply_user_question(&request_id, answers.clone()))
    } else if agent.reply_user_question(&request_id, answers.clone()) {
        true
    } else if let Some(s) = side.read().await.as_ref() {
        s.agent.reply_user_question(&request_id, answers)
    } else {
        false
    };
    if !resolved {
        let _ = resp_tx.send(AgentResponse::Error(
            "Question request is no longer pending.".to_string(),
        ));
    }
}
