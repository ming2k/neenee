//! The `AgentRequest::SlashCommand` dispatcher, extracted verbatim from the
//! agent background task's `match req { … }` dispatch.
//!
//! This is the largest handler — it fans the parsed command out across every
//! `BuiltinCmd` variant (`/provider`, `/mcp`, `/compact`, `/clear`,
//! `/permissions`, `/unattended`, `/review`, `/search`, `/resume`,
//! `/session`, `/sessions`, `/btw`, `/pursue`, `/repeat`, `/init`,
//! `/skills`, `/skill`, `/export`, `/debug`, `/help`, `/exit`) plus the
//! `None` arm that runs a user-defined project command.
//!
//! The body is the original inline match arm, lifted unchanged except that
//! every loop-level `continue` is now a function-level `return` (semantically
//! identical: the caller's `while let` proceeds to the next request either
//! way). Parameters are named to match the original loop locals so the body
//! reads exactly as it did inline.
//!
//! NOTE: a `refresh_agent_pursuit` + SessionStart-hooks block inside the
//! `/pursue status` branch has inconsistent indentation and looks misplaced —
//! it fires session-start hooks every time `/pursue status` runs. Preserved
//! verbatim; not this refactor's job to fix.

use neenee_agent::Agent;
use neenee_agent::AgentIdentity;
use neenee_agent::orchestration::{
    ContextProjectionSettings, PursuitContext, RoundInput, compact_round_history,
    emit_pursuit_updated, refresh_agent_pursuit, send_compaction, send_harness_state,
    start_pursuit, turn,
};
use neenee_agent::skills::SkillRegistry;
use neenee_agent::skills::tools::{ListSkillsTool, UseSkillTool};
use neenee_core::{
    AgentNotice, AgentRequest, AgentResponse, CronExpr, DebugMessageInfo, DebugSnapshot,
    DebugToolInfo, Message, NoticeKind, NoticeSeverity, NoticeSource, NoticeSurface, Provider,
    Pursuit, Tool, RoundEvent, estimate_bytes, estimate_message_tokens, estimate_tokens,
};
use neenee_store::{RepeatStore, config::Config, embedding, session::SessionStore};
use neenee_tools::commands::{CustomCommand, expand_command};
use neenee_tools::project::init_neenee_config;

use std::collections::HashMap;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::{RwLock as AsyncRwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::agent_setup::active_context_window;
use crate::pursuits::format_pursuit_status;
use crate::review::format_review_report;
use crate::session_view::{build_sessions_overview, resume_session, short_session_id};
use crate::side::{SideSession, spawn_parent_status_watcher, start_active_turn};
use crate::startup::{BuiltinCmd, StartupMode, split_custom_command};

/// `AgentRequest::SlashCommand` — parse the command, dispatch to the matching
/// built-in handler, or fall through to the user-defined project-command path.
#[allow(clippy::too_many_arguments)]
pub async fn dispatch(
    cmd: String,
    config: &Config,
    agent: &Arc<Agent>,
    resp_tx: &mpsc::UnboundedSender<AgentResponse>,
    session: &Arc<SessionStore>,
    history: &Arc<tokio::sync::Mutex<Vec<Message>>>,
    ctt_clone: &Arc<AsyncRwLock<Option<CancellationToken>>>,
    generation_clone: &Arc<AtomicU64>,
    side: &Arc<AsyncRwLock<Option<SideSession>>>,
    active_view_side: &AtomicBool,
    base_tools_for_side: &Arc<Vec<Arc<dyn Tool>>>,
    provider_for_task: &Arc<RwLock<Arc<dyn Provider>>>,
    skills_registry: Arc<SkillRegistry>,
    skills_registry_for_commands: &Arc<SkillRegistry>,
    commands_for_task: &HashMap<String, CustomCommand>,
    embedding_store_for_commands: &Arc<AsyncRwLock<embedding::EmbeddingStore>>,
    repeat_store_for_commands: &RepeatStore,
    req_tx_for_commands: &mpsc::UnboundedSender<AgentRequest>,
    project_root_for_side: &std::path::Path,
    startup: &StartupMode,
    ui: &dyn crate::UiBridge,
) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return;
    }
    match BuiltinCmd::from_slash(parts[0]) {
        Some(BuiltinCmd::Provider) => {
            // Handled in TUI
        }
        Some(BuiltinCmd::Config) => {
            // Handled in the TUI: `/config` opens the config manager modal
            // locally (intercepted in input.rs as `InputAction::OpenConfig`),
            // so it is never forwarded here as a SlashCommand. The modal
            // reads the live `nudge_config` snapshot and sends
            // `AgentRequest::UpdateNudgeConfig` to mutate settings.
        }
        Some(BuiltinCmd::Tools) => {
            // Handled in TUI (`/tools` opens the tools manager modal
            // locally; it is never forwarded here as a SlashCommand).
        }
        Some(BuiltinCmd::Mcp) => {
            // Handled in TUI: `/mcp` opens the MCP manager modal locally
            // (intercepted in input.rs as `InputAction::OpenMcp`) and is never
            // forwarded here as a SlashCommand. The modal reads the live
            // session-context snapshot, whose MCP pane the harness keeps current
            // via the shared `McpRuntime`.
        }
        Some(BuiltinCmd::Permissions) => {
            if parts.get(1) == Some(&"clear") {
                agent.clear_allowed_tools();
                let _ = resp_tx.send(turn(
                    &session.id().await,
                    RoundEvent::Text("Always-allowed tool rules cleared.".to_string()),
                ));
            } else {
                let allowed = agent.allowed_tools();
                let message = if allowed.is_empty() {
                    "No tools are always allowed for this process.".to_string()
                } else {
                    format!("Always-allowed tools:\n- {}", allowed.join("\n- "))
                };
                let _ = resp_tx.send(turn(&session.id().await, RoundEvent::Text(message)));
            }
        }
        Some(BuiltinCmd::Unattended) => {
            let next = match parts.get(1).map(|s| s.to_lowercase()).as_deref() {
                Some("on") | Some("true") | Some("1") => Some(true),
                Some("off") | Some("false") | Some("0") => Some(false),
                Some(other) => {
                    let _ = resp_tx.send(AgentResponse::Error(format!(
                        "Unknown value '{}'. Use `/unattended on|off`.",
                        other
                    )));
                    return;
                }
                None => None,
            };
            let enabled = next.unwrap_or_else(|| !agent.get_unattended());
            agent.set_unattended(enabled);
            let _ = resp_tx.send(turn(
                &session.id().await,
                RoundEvent::Text(format!(
                    "Unattended {}: write tools {} run without permission prompts.",
                    if enabled { "ON" } else { "OFF" },
                    if enabled { "will" } else { "won't" },
                )),
            ));
            let _ = resp_tx.send(turn(
                &session.id().await,
                RoundEvent::UnattendedChanged(enabled),
            ));
            // No `send_harness_state` here: toggling unattended is not a
            // turn lifecycle transition, so emitting a `HarnessState("idle")`
            // would make the HarnessState handler clear the live activity
            // cell (`activity_status`) and momentarily hide the activity bar
            // mid-turn. The `UnattendedChanged` event above already mirrors
            // the new value into the TUI snapshot without that side effect.
        }
        Some(BuiltinCmd::Review) => {
            // /review — on-demand session review (ADR-0018,
            // superseding the periodic ADR-0016 design).
            // Runs the bounded read-only REVIEW envoy
            // against the current transcript and reports the
            // verdict(s). Review no longer fires on a round
            // schedule; it only runs when asked. Takes no
            // arguments.
            if parts.iter().skip(1).any(|t| !t.trim().is_empty()) {
                let _ = resp_tx.send(AgentResponse::Error(
                    "`/review` takes no arguments. Usage: `/review` runs an \
                                     on-demand diagnostic of the current turn."
                        .to_string(),
                ));
                return;
            }
            let transcript = session.full_transcript().await;
            let rounds = Agent::estimate_tool_rounds(&transcript);
            if rounds == 0 {
                let _ = resp_tx.send(turn(
                    &session.id().await,
                    RoundEvent::Text(
                        "Nothing to review yet — no tool rounds in the current \
                                         transcript."
                            .to_string(),
                    ),
                ));
                return;
            }
            let _ = resp_tx.send(turn(
                &session.id().await,
                RoundEvent::Activity("running session review…".to_string()),
            ));
            let verdicts = agent.review_now(&transcript).await;
            // Mirror the worst verdict into the activity-bar
            // banner (empty alert clears it when healthy).
            let alert = Agent::render_review_alert(&verdicts, rounds);
            if !alert.trim().is_empty() {
                let _ = resp_tx.send(turn(
                    &session.id().await,
                    RoundEvent::Notice(
                        AgentNotice::new(
                            NoticeKind::ReviewAlert,
                            NoticeSeverity::Warning,
                            "Session review needs attention",
                            NoticeSource::Review,
                        )
                        .with_body(alert.clone())
                        .with_surface(NoticeSurface::Banner),
                    ),
                ));
            }
            let _ = resp_tx.send(turn(
                &session.id().await,
                RoundEvent::SessionReview { alert },
            ));
            let _ = resp_tx.send(turn(
                &session.id().await,
                RoundEvent::Text(format_review_report(&verdicts, rounds)),
            ));
        }
        Some(BuiltinCmd::Search) => {
            let query = cmd.strip_prefix("/search").unwrap_or("").trim();
            if query.is_empty() {
                let _ = resp_tx.send(turn(
                    &session.id().await,
                    RoundEvent::Text("Usage: /search <query>".to_string()),
                ));
            } else {
                let messages = session.full_transcript().await;
                {
                    let mut store = embedding_store_for_commands.write().await;
                    let session_id = session.id().await;
                    if let Err(error) = store.index(&messages, &session_id).await {
                        let _ = resp_tx.send(AgentResponse::Error(error));
                        return;
                    }
                }
                match embedding_store_for_commands
                    .read()
                    .await
                    .search(query, 5)
                    .await
                {
                    Ok(results) => {
                        if results.is_empty() {
                            let _ = resp_tx.send(turn(
                                &session.id().await,
                                RoundEvent::Text("No relevant history found.".to_string()),
                            ));
                        } else {
                            let mut lines =
                                vec!["Relevant history (most similar first):".to_string()];
                            for (i, (text, score)) in results.iter().enumerate() {
                                lines.push(format!("{}. [score={:.3}]\n{}", i + 1, score, text));
                            }
                            let _ = resp_tx.send(turn(
                                &session.id().await,
                                RoundEvent::Text(lines.join("\n\n")),
                            ));
                        }
                    }
                    Err(error) => {
                        let _ = resp_tx.send(AgentResponse::Error(error));
                    }
                }
            }
        }
        Some(BuiltinCmd::Resume) => {
            generation_clone.fetch_add(1, Ordering::SeqCst);
            agent.reject_pending_permissions();
            let _ = resp_tx.send(AgentResponse::PermissionsCleared);
            if let Some(token) = ctt_clone.write().await.take() {
                token.cancel();
            }
            match resume_session(session, history, parts.get(1).copied()).await {
                Ok((id, transcript)) => {
                    let _ = resp_tx.send(AgentResponse::ConversationReplaced(transcript));
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        RoundEvent::Text(format!("Resumed session {}.", short_session_id(&id))),
                    ));
                    send_harness_state(resp_tx, &session.id().await, agent, "idle");
                }
                Err(error) => {
                    let _ = resp_tx.send(AgentResponse::Error(error));
                }
            }
        }
        Some(BuiltinCmd::Session) => match parts.get(1).copied().unwrap_or("status") {
            "status" => {
                let id = session.id().await;
                let parent_id = session
                    .parent_id()
                    .await
                    .unwrap_or_else(|| "none".to_string());
                let message_count = history.lock().await.len();
                let archived_count = session.archived_transcript_count().await;
                let checkpoint = session.checkpoint().await;
                let last_projection = session.last_projection().await;
                let checkpoint_text = checkpoint
                    .map(|item| {
                        format!(
                            "{} {}/{} ({})",
                            item.pursuit, item.iteration, item.max_iterations, item.status
                        )
                    })
                    .unwrap_or_else(|| "none".to_string());
                let _ = resp_tx.send(turn(
                                        &session.id().await,
                                        RoundEvent::Text(format!(
                                    "Session: {}\nForked from: {}\nModel-window messages: {}\nArchived transcript messages: {}\nLoop checkpoint: {}\nLast context projection: {}",
                                    id,
                                    parent_id,
                                    message_count,
                                    archived_count,
                                    checkpoint_text,
                                    last_projection
                                        .map(|item| format!(
                                            "{:?}: {} -> {} chars",
                                            item.operation, item.before_chars, item.after_chars
                                        ))
                                        .unwrap_or_else(|| "none".to_string())
                                )),
                                    ));
            }
            "list" => match session.list().await {
                Ok(sessions) => {
                    let lines = sessions
                        .into_iter()
                        .map(|item| {
                            format!(
                                "- {}{}  messages={}  parent={}",
                                short_session_id(&item.id),
                                if item.active { " [active]" } else { "" },
                                item.message_count,
                                item.parent_id
                                    .map(|id| short_session_id(&id).to_string())
                                    .unwrap_or_else(|| "none".to_string())
                            )
                        })
                        .collect::<Vec<_>>();
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        RoundEvent::Text(format!("Sessions:\n{}", lines.join("\n"))),
                    ));
                }
                Err(error) => {
                    let _ = resp_tx.send(AgentResponse::Error(error));
                }
            },
            "fork" => {
                generation_clone.fetch_add(1, Ordering::SeqCst);
                agent.reject_pending_permissions();
                let _ = resp_tx.send(AgentResponse::PermissionsCleared);
                if let Some(token) = ctt_clone.write().await.take() {
                    token.cancel();
                }
                match session.fork().await {
                    Ok((id, parent_id)) => {
                        let _ = resp_tx.send(turn(
                            &session.id().await,
                            RoundEvent::Text(format!("Forked session {} from {}.", id, parent_id)),
                        ));
                        send_harness_state(resp_tx, &session.id().await, agent, "idle");
                    }
                    Err(error) => {
                        let _ = resp_tx.send(AgentResponse::Error(error));
                    }
                }
            }
            "open" => {
                let Some(id) = parts.get(2) else {
                    let _ = resp_tx.send(AgentResponse::Error(
                        "Usage: /session open <session-id>".to_string(),
                    ));
                    return;
                };
                generation_clone.fetch_add(1, Ordering::SeqCst);
                agent.reject_pending_permissions();
                let _ = resp_tx.send(AgentResponse::PermissionsCleared);
                if let Some(token) = ctt_clone.write().await.take() {
                    token.cancel();
                }
                match session.open(id).await {
                    Ok(()) => {
                        *history.lock().await = session.model_window().await;
                        let transcript = session.full_transcript().await;
                        let _ = resp_tx.send(AgentResponse::ConversationReplaced(transcript));
                        let _ = resp_tx.send(turn(
                            &session.id().await,
                            RoundEvent::Text(format!("Opened session {}.", id)),
                        ));
                        send_harness_state(resp_tx, &session.id().await, agent, "idle");
                    }
                    Err(error) => {
                        let _ = resp_tx.send(AgentResponse::Error(error));
                    }
                }
            }
            "resume" => {
                generation_clone.fetch_add(1, Ordering::SeqCst);
                agent.reject_pending_permissions();
                let _ = resp_tx.send(AgentResponse::PermissionsCleared);
                if let Some(token) = ctt_clone.write().await.take() {
                    token.cancel();
                }
                match resume_session(session, history, parts.get(2).copied()).await {
                    Ok((id, transcript)) => {
                        let _ = resp_tx.send(AgentResponse::ConversationReplaced(transcript));
                        let _ = resp_tx.send(turn(
                            &session.id().await,
                            RoundEvent::Text(format!("Resumed session {}.", short_session_id(&id))),
                        ));
                        send_harness_state(resp_tx, &session.id().await, agent, "idle");
                    }
                    Err(error) => {
                        let _ = resp_tx.send(AgentResponse::Error(error));
                    }
                }
            }
            "new" => {
                generation_clone.fetch_add(1, Ordering::SeqCst);
                agent.reject_pending_permissions();
                let _ = resp_tx.send(AgentResponse::PermissionsCleared);
                if let Some(token) = ctt_clone.write().await.take() {
                    token.cancel();
                }
                history.lock().await.clear();
                agent.clear_todos();
                match session.reset().await {
                    Ok(id) => {
                        let _ = resp_tx.send(turn(
                            &session.id().await,
                            RoundEvent::TodosUpdated(neenee_core::TodoList::default()),
                        ));
                        let _ = resp_tx.send(AgentResponse::ConversationCleared);
                        let _ = resp_tx.send(turn(
                            &session.id().await,
                            RoundEvent::Text(format!("Started new session: {}", id)),
                        ));
                    }
                    Err(error) => {
                        let _ = resp_tx.send(AgentResponse::Error(error));
                    }
                }
            }
            other => {
                let _ = resp_tx.send(AgentResponse::Error(format!(
                    "Unknown session command '{}'. Use status, list, resume, fork, open, or new.",
                    other
                )));
            }
        },
        Some(BuiltinCmd::Sessions) => {
            let _ = resp_tx.send(AgentResponse::SessionsOverview(
                build_sessions_overview(session).await,
            ));
        }
        Some(BuiltinCmd::Btw) => {
            // `/btw [prompt]` opens a side conversation
            // (ADR-0017): fork the primary into a
            // self-contained side file, build a fresh side
            // `Agent` + store + history, and switch the view.
            // The primary turn keeps running untouched —
            // unlike `/session open`, we deliberately do NOT
            // bump the generation counter, reject permissions,
            // or cancel the primary token.
            let prompt = cmd.strip_prefix("/btw").unwrap_or("").trim();
            if side.read().await.is_some() {
                let _ = resp_tx.send(AgentResponse::Error(
                    "A side conversation is already open. \
                                     Leave it with Esc first."
                        .to_string(),
                ));
                return;
            }
            let primary_id = session.id().await;
            let side_session = match SideSession::build(
                session,
                base_tools_for_side,
                provider_for_task,
                (*skills_registry).clone(),
                project_root_for_side,
                AgentIdentity::new(crate::NEENEE_NAME, crate::NEENEE_MISSION),
            )
            .await
            {
                Ok(s) => s,
                Err(error) => {
                    let _ = resp_tx.send(AgentResponse::Error(error));
                    return;
                }
            };
            let side_id = side_session.id.clone();
            *side.write().await = Some(side_session);
            active_view_side.store(true, Ordering::SeqCst);
            // Tell the TUI to enter the side view (seeds the
            // side buffer + records the routing keys) before
            // the first side turn starts streaming.
            let _ = resp_tx.send(AgentResponse::SideViewOpened {
                side_id: side_id.clone(),
                primary_id,
            });
            // Stream coarse primary-status updates to the
            // side banner while the side is live. Emits an
            // immediate baseline so the banner is never
            // empty, then self-terminates on side teardown.
            spawn_parent_status_watcher(side.clone(), ctt_clone.clone(), resp_tx.clone());
            if !prompt.is_empty() {
                start_active_turn(
                    active_view_side,
                    side,
                    agent,
                    history,
                    session,
                    ctt_clone,
                    generation_clone,
                    resp_tx,
                    config,
                    RoundInput {
                        prompt: prompt.to_string(),
                        hidden: false,
                        display_prompt: None,
                        images: Vec::new(),
                    },
                )
                .await;
            }
        }
        Some(BuiltinCmd::Compact) => {
            let mut current = history.lock().await.clone();
            let settings =
                ContextProjectionSettings::from_config(config, active_context_window(agent));
            let _ = resp_tx.send(turn(
                &session.id().await,
                RoundEvent::Activity("compacting context".to_string()),
            ));
            let extra = agent.fire_pre_compact().await;
            match compact_round_history(
                &mut current,
                session,
                &settings,
                Some(agent.provider.clone()),
                extra,
            )
            .await
            {
                Ok(Some(checkpoint)) => {
                    *history.lock().await = current;
                    send_compaction(resp_tx, &session.id().await, &checkpoint);
                }
                Ok(None) => {
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        RoundEvent::Text("Not enough complete turns to compact.".to_string()),
                    ));
                }
                Err(error) => {
                    let _ = resp_tx.send(AgentResponse::Error(error));
                }
            }
            agent.fire_post_compact().await;
        }
        Some(BuiltinCmd::Pursue) => {
            let thread_id = session.id().await;
            let argument = cmd.strip_prefix("/pursue").unwrap_or("").trim();
            let rest = argument;

            async fn report_pursuit_result(
                tx: &mpsc::UnboundedSender<AgentResponse>,
                session_id: &str,
                agent: &Agent,
                result: Result<Option<Pursuit>, String>,
                success: impl FnOnce(&Pursuit) -> String,
                empty: impl Into<String>,
            ) {
                match result {
                    Ok(Some(pursuit)) => {
                        agent.set_pursuit(pursuit.clone());
                        emit_pursuit_updated(tx, session_id, &pursuit);
                        let _ = tx.send(turn(session_id, RoundEvent::Text(success(&pursuit))));
                    }
                    Ok(None) => {
                        let _ = tx.send(AgentResponse::Error(empty.into()));
                    }
                    Err(error) => {
                        let _ = tx.send(AgentResponse::Error(error));
                    }
                }
            }

            if rest == "stop" {
                let mut current = ctt_clone.write().await;
                if let Some(token) = current.take() {
                    token.cancel();
                    let _ = resp_tx.send(turn(
                        &thread_id,
                        RoundEvent::Text("Pursuit stop requested.".to_string()),
                    ));
                } else {
                    let _ = resp_tx.send(turn(
                        &thread_id,
                        RoundEvent::Text("No pursuit is running.".to_string()),
                    ));
                }
                // Genuine lifecycle transition (mirrors `interrupt`): flip the
                // harness to idle eagerly so the activity bar reflects the
                // stopped work before the cancelled task's own terminal idle,
                // which is gated behind persistence fsyncs.
                send_harness_state(resp_tx, &session.id().await, agent, "idle");
                return;
            }

            if rest == "status" {
                refresh_agent_pursuit(agent, session).await;

                // SessionStart hooks (ADR-0025): inject setup context before the first
                // turn. Resume vs fresh start is surfaced so a hook can branch.
                {
                    let source = match &startup {
                        StartupMode::Resume(_) => neenee_core::SessionSource::Resume,
                        _ => neenee_core::SessionSource::Startup,
                    };
                    let mut history = history.lock().await;
                    agent.fire_session_start(source, &mut history).await;
                }
                let armed = agent.is_pursuit_armed();
                let iterations = agent.pursuit_iterations();
                let message = match agent.get_pursuit() {
                    Some(pursuit) => {
                        let mut m = format_pursuit_status(&pursuit);
                        if armed {
                            m.push_str(&format!("\nPursuit active · gate iteration {iterations}"));
                        }
                        m
                    }
                    None => "No active pursuit. Start one with /pursue <condition>.".to_string(),
                };
                let _ = resp_tx.send(turn(&thread_id, RoundEvent::Text(message)));
            } else if rest == "clear" {
                agent.disarm_pursuit();
                match session.set_pursuit(None).await {
                    Ok(_) => {
                        if agent.get_pursuit().is_some() {
                            agent.clear_pursuit();
                            // Mirror the cleared pursuit into the TUI snapshot
                            // via the non-gated channel so the activity bar's
                            // `⟴` badge updates without flushing the live
                            // activity cell (which a `HarnessState("idle")`
                            // would do, flickering the bar mid-turn).
                            let _ = resp_tx.send(turn(&thread_id, RoundEvent::PursuitCleared));
                            let _ = resp_tx.send(turn(
                                &thread_id,
                                RoundEvent::Text("Pursuit cleared.".to_string()),
                            ));
                        } else {
                            let _ = resp_tx.send(turn(
                                &thread_id,
                                RoundEvent::Text("No pursuit to clear.".to_string()),
                            ));
                        }
                    }
                    Err(error) => {
                        let _ = resp_tx.send(AgentResponse::Error(error));
                    }
                }
            } else if rest == "done" {
                agent.disarm_pursuit();
                report_pursuit_result(
                    resp_tx,
                    &thread_id,
                    agent,
                    session.mark_pursuit_complete().await,
                    |_| "Pursuit marked completed.".to_string(),
                    "No pursuit to complete.",
                )
                .await;
            } else if rest.starts_with("edit ") {
                let new_objective = rest.strip_prefix("edit ").unwrap_or("").trim();
                if new_objective.is_empty() {
                    let _ = resp_tx.send(AgentResponse::Error(
                        "Usage: /pursue edit <new condition>".to_string(),
                    ));
                } else {
                    match session.update_pursuit_objective(new_objective).await {
                        Ok(Some(pursuit)) => {
                            agent.set_pursuit(pursuit.clone());
                            {
                                let mut messages = history.lock().await;
                                agent.inject_objective_updated(&mut messages);
                                let updated = messages.clone();
                                drop(messages);
                                let _ = session.replace_messages(updated).await;
                            }
                            emit_pursuit_updated(resp_tx, &thread_id, &pursuit);
                            let _ = resp_tx.send(turn(
                                &thread_id,
                                RoundEvent::Text(format!("Pursuit updated: {}", pursuit.objective)),
                            ));
                        }
                        Ok(None) => {
                            let _ = resp_tx.send(AgentResponse::Error(
                                "No pursuit to edit. Start one with /pursue <condition>."
                                    .to_string(),
                            ));
                        }
                        Err(error) => {
                            let _ = resp_tx.send(AgentResponse::Error(error));
                        }
                    }
                }
            } else if rest == "pause" || rest == "resume" || rest.starts_with("budget ") {
                let _ = resp_tx.send(AgentResponse::Error(
                    "/pursue pause, /pursue resume, and /pursue budget are not \
                                     supported. Use /pursue <condition>, /pursue edit, /pursue \
                                     done, /pursue clear, /pursue status, or /pursue stop."
                        .to_string(),
                ));
            } else {
                // `/pursue <condition>` sets a fresh condition and drives it;
                // `/pursue` (empty) re-arms and drives the existing pursuit.
                let condition = if rest.is_empty() {
                    match session.pursuit().await {
                        Some(pursuit) if !pursuit.is_complete => {
                            let _ = resp_tx.send(turn(
                                &thread_id,
                                RoundEvent::Text(format!(
                                    "Resuming pursuit on existing pursuit: {}",
                                    pursuit.objective
                                )),
                            ));
                            pursuit.objective
                        }
                        _ => {
                            let _ = resp_tx.send(AgentResponse::Error(
                                "No active pursuit. Start one with /pursue <condition>."
                                    .to_string(),
                            ));
                            return;
                        }
                    }
                } else {
                    let pursuit = Pursuit {
                        objective: rest.to_string(),
                        is_complete: false,
                    };
                    match session.set_pursuit(Some(pursuit.clone())).await {
                        Ok(_) => {
                            agent.set_pursuit(pursuit.clone());
                            emit_pursuit_updated(resp_tx, &thread_id, &pursuit);
                            pursuit.objective
                        }
                        Err(error) => {
                            let _ = resp_tx.send(AgentResponse::Error(error));
                            return;
                        }
                    }
                };
                start_pursuit(
                    PursuitContext {
                        agent: agent.clone(),
                        history: history.clone(),
                        tx: resp_tx.clone(),
                        token_slot: ctt_clone.clone(),
                        generation_counter: generation_clone.clone(),
                        session: session.clone(),
                        session_id: session.id().await,
                        projection: ContextProjectionSettings::from_config(
                            config,
                            active_context_window(agent),
                        ),
                        retry_max_attempts: config.provider_retry_max_attempts,
                        retry_base_ms: config.provider_retry_base_ms,
                        retry_max_ms: config.provider_retry_max_ms,
                    },
                    condition,
                )
                .await;
            }
            // `/pursue status` / unsupported-subcommand paths reach here. None
            // of them mutate harness state, so there is nothing to mirror and
            // no turn boundary to signal — a `HarnessState("idle")` here would
            // only flicker the activity bar.
        }
        Some(BuiltinCmd::Repeat) => {
            let rest = cmd.strip_prefix("/repeat").unwrap_or("").trim();
            if rest.is_empty() || rest == "help" {
                let _ = resp_tx.send(turn(
                                    &session.id().await,
                                    RoundEvent::Text(
                                        "Usage: /repeat <cron> <prompt>\n\
                                         cron is five fields: minute hour day month weekday \
                                         (e.g. `*/5 * * * *` = every 5 min, `0 9 * * 1-5` = 09:00 weekdays).\n\
                                         Also: /repeat list, /repeat cancel <id>."
                                            .to_string(),
                                    ),
                                ));
                return;
            }
            if rest == "list" {
                let jobs = repeat_store_for_commands.list().await.unwrap_or_default();
                if jobs.is_empty() {
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        RoundEvent::Text("No /repeat jobs scheduled.".to_string()),
                    ));
                } else {
                    let mut lines = vec!["Scheduled /repeat jobs:".to_string()];
                    for j in &jobs {
                        lines.push(format!(
                            "  {} · `{}` · next {} · {}",
                            &j.id[..8.min(j.id.len())],
                            j.cron,
                            j.next_fire.format("%Y-%m-%d %H:%M"),
                            j.prompt,
                        ));
                    }
                    let _ =
                        resp_tx.send(turn(&session.id().await, RoundEvent::Text(lines.join("\n"))));
                }
                return;
            }
            if let Some(id) = rest.strip_prefix("cancel ") {
                let id = id.trim();
                match repeat_store_for_commands.delete(id).await {
                    Ok(true) => {
                        let _ = resp_tx.send(turn(
                            &session.id().await,
                            RoundEvent::Text(format!("Cancelled repeat job {id}.")),
                        ));
                    }
                    Ok(false) => {
                        let _ = resp_tx.send(turn(
                            &session.id().await,
                            RoundEvent::Text(format!("No repeat job with id {id}.")),
                        ));
                    }
                    Err(error) => {
                        let _ = resp_tx.send(AgentResponse::Error(error));
                    }
                }
                return;
            }
            // `/repeat <5-field cron> <prompt>`
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            if tokens.len() < 6 {
                let _ = resp_tx.send(AgentResponse::Error(
                    "Usage: /repeat <5-field cron> <prompt>. \
                                      Example: /repeat */5 * * * * check the deploy"
                        .to_string(),
                ));
                return;
            }
            let cron = tokens[0..5].join(" ");
            let prompt = tokens[5..].join(" ");
            let parsed = match CronExpr::parse(&cron) {
                Ok(p) => p,
                Err(error) => {
                    let _ = resp_tx.send(AgentResponse::Error(format!("Invalid cron: {error}")));
                    return;
                }
            };
            let now = chrono::Utc::now();
            let next = match parsed.next_fire(now) {
                Some(n) => n,
                None => {
                    let _ = resp_tx.send(AgentResponse::Error(
                        "That cron expression never fires within the next year.".to_string(),
                    ));
                    return;
                }
            };
            match repeat_store_for_commands.add(&cron, &prompt, next).await {
                Ok(job) => {
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        RoundEvent::Text(format!(
                            "Scheduled repeat job {} (`{}`), next {}. Running now.",
                            &job.id[..8.min(job.id.len())],
                            cron,
                            next.format("%Y-%m-%d %H:%M"),
                        )),
                    ));
                    // Fire the first run immediately (cron handles the rest).
                    let _ = req_tx_for_commands.send(AgentRequest::Chat {
                        text: prompt,
                        images: Vec::new(),
                    });
                }
                Err(error) => {
                    let _ = resp_tx.send(AgentResponse::Error(error));
                }
            }
        }
        Some(BuiltinCmd::Init) => {
            let target = parts.get(1).copied().unwrap_or(".");
            match init_neenee_config(std::path::Path::new(target)) {
                Ok(created) if created.is_empty() => {
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        RoundEvent::Text(format!(
                            "neenee is already configured in '{}'. Nothing to do.",
                            target
                        )),
                    ));
                }
                Ok(created) => {
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        RoundEvent::Text(format!(
                            "Initialized neenee configuration in '{}'.\nCreated:\n{}",
                            target,
                            created
                                .iter()
                                .map(|path| format!("- {}", path))
                                .collect::<Vec<_>>()
                                .join("\n")
                        )),
                    ));
                }
                Err(error) => {
                    let _ = resp_tx.send(AgentResponse::Error(error));
                }
            }
        }
        Some(BuiltinCmd::Skills) => {
            let sub = parts.get(1).copied().unwrap_or("list");
            match sub {
                "list" => {
                    let tool = ListSkillsTool {
                        registry: skills_registry_for_commands.clone(),
                    };
                    match tool.call("{}").await {
                        Ok(output) => {
                            let _ =
                                resp_tx.send(turn(&session.id().await, RoundEvent::Text(output)));
                        }
                        Err(error) => {
                            let _ = resp_tx.send(AgentResponse::Error(error));
                        }
                    }
                }
                "reload" => {
                    skills_registry_for_commands.reload().await;
                    let count = skills_registry_for_commands.lock().list().len();
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        RoundEvent::Text(format!("Skills reloaded. {} skill(s) available.", count)),
                    ));
                }
                other => {
                    let _ = resp_tx.send(AgentResponse::Error(format!(
                        "Unknown skills command '{}'. Use 'list' or 'reload'.",
                        other
                    )));
                }
            }
        }
        Some(BuiltinCmd::Skill) => {
            let name = cmd.strip_prefix("/skill").unwrap_or("").trim();
            if name.is_empty() {
                let _ = resp_tx.send(AgentResponse::Error("Usage: /skill <name>".to_string()));
            } else {
                let args = serde_json::json!({ "name": name }).to_string();
                let tool = UseSkillTool {
                    registry: skills_registry_for_commands.clone(),
                };
                match tool.call(&args).await {
                    Ok(output) => {
                        let _ = resp_tx.send(turn(&session.id().await, RoundEvent::Text(output)));
                    }
                    Err(error) => {
                        let _ = resp_tx.send(AgentResponse::Error(error));
                    }
                }
            }
        }
        Some(BuiltinCmd::Clear) => {
            history.lock().await.clear();
            let _ = session.replace_messages(Vec::new()).await;
            agent.clear_todos();
            let _ = session.set_todos(neenee_core::TodoList::default()).await;
            let _ = resp_tx.send(AgentResponse::ConversationCleared);
            let _ = resp_tx.send(turn(
                &session.id().await,
                RoundEvent::TodosUpdated(neenee_core::TodoList::default()),
            ));
            let _ = resp_tx.send(turn(
                &session.id().await,
                RoundEvent::Text("Conversation history cleared.".to_string()),
            ));
        }
        Some(BuiltinCmd::Export) => {
            let messages = history.lock().await.clone();
            let session_id = session.id().await;
            let provider_id = agent.provider.provider_id();
            let model_name = agent.provider.model();
            let pursuit = agent.get_pursuit();
            let markdown = crate::export::format_export_markdown(
                crate::export::ExportContext {
                    session_id: &session_id,
                    provider: &provider_id,
                    model: &model_name,
                    pursuit: pursuit.as_ref(),
                },
                &messages,
            );
            let char_count = markdown.chars().count();
            match ui.copy_to_clipboard(&markdown).await {
                Ok(crate::CopyOutcome::Native) => {
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        RoundEvent::Text(format!(
                            "Session exported to clipboard ({} messages, {} chars). \
                                             Paste it into another agent to continue this work.",
                            messages.len(),
                            char_count
                        )),
                    ));
                }
                Ok(crate::CopyOutcome::Osc52) => {
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        RoundEvent::Text(format!(
                            "Session exported via OSC52 ({} messages, {} chars). \
                                             If your terminal did not capture it, run neenee in a \
                                             clipboard-capable environment.",
                            messages.len(),
                            char_count
                        )),
                    ));
                }
                Err(error) => {
                    let _ = resp_tx.send(AgentResponse::Error(format!(
                        "Export built ({} chars) but clipboard copy failed: {}",
                        char_count, error
                    )));
                }
            }
        }
        Some(BuiltinCmd::Debug) => {
            // /debug network on|off — arm/disarm semantic network capture at
            // the ProxyProvider layer. Each provider round-trip (request
            // messages + streamed/returned response) is then written as one
            // JSON file under the per-project `network/` directory. Captures
            // the `Vec<Message>` exchange — not raw HTTP bytes — so API keys
            // in headers/query strings never land on disk.
            match parts.get(1).copied() {
                Some("network") => {
                    let next = match parts.get(2).map(|s| s.to_lowercase()).as_deref() {
                        Some("on") | Some("true") | Some("1") => Some(true),
                        Some("off") | Some("false") | Some("0") => Some(false),
                        Some(other) => {
                            let _ = resp_tx.send(AgentResponse::Error(format!(
                                "Unknown value '{other}'. Use `/debug network on|off`."
                            )));
                            return;
                        }
                        None => None,
                    };
                    let enabled = next.unwrap_or_else(|| !agent.provider.debug_capture_enabled());
                    let dir = neenee_store::paths::get().project_network_dir(project_root_for_side);
                    agent.provider.set_debug_capture(enabled, dir.clone());
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        RoundEvent::Text(format!(
                            "Network capture {}: each provider round-trip {} written to\n  {}",
                            if enabled { "ON" } else { "OFF" },
                            if enabled { "is" } else { "will no longer be" },
                            dir.display(),
                        )),
                    ));
                }
                Some("context") => {
                    // /debug context — dev-only dry run. Snapshots the exact
                    // request the next turn would send: the model-visible
                    // message list (rebuilt head system message + auto-loaded
                    // skills, mirroring `prepare_turn_messages`), the
                    // model-visible tool schemas, provider/model identity,
                    // context window, estimated token pressure, and the active
                    // pursuit. NO provider call is made; nothing is mutated.
                    // The full JSON record is persisted to the per-project
                    // `debug/` dir, and a typed snapshot is shipped to the TUI
                    // as `AgentResponse::DebugSnapshot` so it renders in a
                    // dedicated inspector modal — not a flat transcript line.
                    let messages = {
                        let mut snapshot = history.lock().await.clone();
                        agent.prepare_turn_messages_debug(&mut snapshot);
                        snapshot
                    };
                    let provider_id = agent.provider.provider_id();
                    let model_name = agent.provider.model();
                    let window = active_context_window(agent);
                    let tokens = estimate_tokens(&messages);
                    let tools: Vec<DebugToolInfo> = agent
                        .installed_tools()
                        .iter()
                        .map(|tool| DebugToolInfo {
                            name: tool.name().to_string(),
                            description: tool.description().to_string(),
                            variant: tool.variant().to_string(),
                        })
                        .collect();
                    let pursuit = agent.get_pursuit();
                    let session_id = session.id().await;
                    let timestamp = chrono::Utc::now();
                    let pressure_pct = if window > 0 {
                        (tokens as f64 / window as f64 * 100.0).round() as u64
                    } else {
                        0
                    };

                    // Per-message flattened rows for the inspector's Messages
                    // section: index, role, hidden flag, per-message token
                    // estimate, and a one-line summary.
                    let message_infos: Vec<DebugMessageInfo> = messages
                        .iter()
                        .enumerate()
                        .map(|(index, m)| DebugMessageInfo {
                            index,
                            role: format!("{:?}", m.role).to_lowercase(),
                            hidden: m.hidden || m.origin.is_some(),
                            tokens: estimate_message_tokens(m).max(0) as usize,
                            summary: one_line_summary(&m.content),
                        })
                        .collect();

                    let snapshot = DebugSnapshot {
                        timestamp: timestamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        file_path: String::new(), // filled after the file write below
                        session_id: session_id.clone(),
                        provider: provider_id.clone(),
                        model: model_name.clone(),
                        context_window_tokens: window,
                        estimated_tokens: tokens,
                        estimated_bytes: estimate_bytes(&messages),
                        pressure_pct,
                        pursuit: pursuit.clone(),
                        tools: tools.clone(),
                        messages: message_infos,
                    };

                    // Persist the full record (with the raw messages + tool
                    // schemas) for offline inspection.
                    let dir = neenee_store::paths::get().project_debug_dir(project_root_for_side);
                    let stamp = timestamp.format("%Y%m%d-%H%M%S%.3f");
                    let file = dir.join(format!("{stamp}_context.json"));
                    let record = serde_json::json!({
                        "timestamp": snapshot.timestamp,
                        "session_id": snapshot.session_id,
                        "provider": snapshot.provider,
                        "model": snapshot.model,
                        "context_window_tokens": window,
                        "estimated_tokens": tokens,
                        "estimated_bytes": snapshot.estimated_bytes,
                        "pressure_pct": pressure_pct,
                        "pursuit": pursuit,
                        "tools": agent
                            .installed_tools()
                            .iter()
                            .map(|tool| tool.to_openai_function())
                            .collect::<Vec<_>>(),
                        "messages": messages,
                    });
                    let file_path = file.display().to_string();
                    match serde_json::to_vec_pretty(&record) {
                        Ok(bytes) => {
                            if let Err(error) =
                                neenee_store::fsutil::atomic_write_bytes(&file, &bytes)
                            {
                                let _ = resp_tx.send(AgentResponse::Error(format!(
                                    "Context snapshot write failed: {error}"
                                )));
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = resp_tx.send(AgentResponse::Error(format!(
                                "Context snapshot serialize failed: {error}"
                            )));
                            return;
                        }
                    }

                    // Ship the typed snapshot (with the resolved path) to the
                    // TUI so it opens the inspector modal.
                    let snapshot = DebugSnapshot { file_path: file_path.clone(), ..snapshot };
                    let _ = resp_tx.send(AgentResponse::DebugSnapshot(snapshot));
                    let _ = resp_tx.send(turn(
                        &session_id,
                        RoundEvent::Text(format!(
                            "Context snapshot (dry run) — inspector opened. \
                             ~{tokens} tokens, {n_msgs} message(s), {n_tools} tool(s). \
                             Full JSON: {file_path}",
                            n_msgs = messages.len(),
                            n_tools = tools.len(),
                        )),
                    ));
                }
                Some(other) => {
                    let _ = resp_tx.send(AgentResponse::Error(format!(
                        "Unknown debug target '{other}'. Available: network, context. \
                         Usage: `/debug network on|off` or `/debug context`."
                    )));
                }
                None => {
                    let network_on = agent.provider.debug_capture_enabled();
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        RoundEvent::Text(format!(
                            "Debug status:\n- network: {}\n\nUsage:\n\
                             - `/debug network on|off` — capture each provider round-trip\n\
                             - `/debug context` — dry-run the next request to disk",
                            if network_on { "ON" } else { "OFF" },
                        )),
                    ));
                }
            }
        }
        Some(BuiltinCmd::Help) => {
            let custom_help = if commands_for_task.is_empty() {
                String::new()
            } else {
                let mut commands = commands_for_task.values().collect::<Vec<_>>();
                commands.sort_by(|left, right| left.name.cmp(&right.name));
                format!(
                    "\n\nProject commands:\n{}",
                    commands
                        .into_iter()
                        .map(|command| format!(
                            "/{} — {}",
                            command.name,
                            command
                                .description
                                .as_deref()
                                .unwrap_or("Run project command")
                        ))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            };
            let mut lines = vec!["Slash commands:".to_string()];
            for (name, desc) in BuiltinCmd::ALL {
                lines.push(format!("{name:<13} — {desc}"));
            }
            let _ = resp_tx.send(turn(
                &session.id().await,
                RoundEvent::Text(format!(
                    "{}
{custom_help}",
                    lines.join(
                        "
"
                    )
                )),
            ));
        }
        Some(BuiltinCmd::Exit) => {
            let _ = resp_tx.send(AgentResponse::Exit);
        }
        None => {
            let (name, arguments) = split_custom_command(&cmd);
            let Some(command) = commands_for_task.get(name) else {
                let _ = resp_tx.send(AgentResponse::Error(format!(
                    "Unknown command: {}",
                    parts[0]
                )));
                return;
            };
            start_active_turn(
                active_view_side,
                side,
                agent,
                history,
                session,
                ctt_clone,
                generation_clone,
                resp_tx,
                config,
                RoundInput {
                    prompt: expand_command(command, arguments),
                    hidden: false,
                    display_prompt: Some(cmd),
                    images: Vec::new(),
                },
            )
            .await;
        }
    }
}

/// One-line, length-capped summary of a message's content for the debug
/// inspector's Messages list. Collapses control chars to spaces and truncates
/// to `max_chars`, appending an ellipsis when truncated. Mirrors the
/// `common::one_line` flattening the TUI uses for row text.
fn one_line_summary(content: &str) -> String {
    const MAX_CHARS: usize = 120;
    let flat: String = content
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= MAX_CHARS {
        flat.to_string()
    } else {
        let mut out: String = flat.chars().take(MAX_CHARS).collect();
        out.push('…');
        out
    }
}
