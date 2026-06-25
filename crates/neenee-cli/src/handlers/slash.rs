//! The `AgentRequest::SlashCommand` dispatcher, extracted verbatim from the
//! agent background task's `match req { … }` dispatch.
//!
//! This is the largest handler — it fans the parsed command out across every
//! `BuiltinCmd` variant (`/provider`, `/plan`, `/verify`, `/mcp`,
//! `/compact`, `/clear`, `/permissions`, `/auto-approve`, `/review`,
//! `/verify-nudge`, `/search`, `/resume`, `/session`, `/sessions`, `/btw`,
//! `/pursue`, `/repeat`, `/init`, `/skills`, `/skill`, `/export`, `/help`,
//! `/exit`) plus the `None` arm that runs a user-defined project command.
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

use neenee_agent::orchestration::{
    compact_turn_history, emit_pursuit_updated, refresh_agent_pursuit, send_compaction,
    send_harness_state, start_pursuit, turn, CompactionSettings, PursuitContext, TurnInput,
};
use neenee_agent::skills::tools::{ListSkillsTool, ReloadSkillsTool, UseSkillTool};
use neenee_agent::skills::SkillRegistry;
use neenee_agent::Agent;
use neenee_core::{
    AgentRequest, AgentResponse, CronExpr, McpConnectionStatus, Message, Provider, Pursuit,
    PursuitService, RepeatStore, Tool, TurnEvent,
};
use neenee_store::{config::Config, embedding, session::SessionStore};
use neenee_tools::commands::{expand_command, CustomCommand};
use neenee_tools::project::init_neenee_config;

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, RwLock,
};
use tokio::sync::{mpsc, RwLock as AsyncRwLock};
use tokio_util::sync::CancellationToken;

use crate::agent_setup::active_context_window;
use crate::pursuits::format_pursuit_status;
use crate::review::format_review_report;
use crate::session_view::{build_sessions_overview, resume_session, short_session_id};
use crate::side::{spawn_parent_status_watcher, start_active_turn, SideSession};
use crate::startup::{split_custom_command, BuiltinCmd, StartupMode};

/// `AgentRequest::SlashCommand` — parse the command, dispatch to the matching
/// built-in handler, or fall through to the user-defined project-command path.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch(
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
    pursuit_service: &PursuitService,
    skills_registry: Arc<SkillRegistry>,
    skills_registry_for_commands: &Arc<SkillRegistry>,
    mcp_statuses: &[(String, McpConnectionStatus)],
    commands_for_task: &HashMap<String, CustomCommand>,
    embedding_store_for_commands: &Arc<AsyncRwLock<embedding::EmbeddingStore>>,
    repeat_store_for_commands: &RepeatStore,
    req_tx_for_commands: &mpsc::UnboundedSender<AgentRequest>,
    project_root_for_side: &std::path::Path,
    startup: &StartupMode,
) {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return;
    }
    match BuiltinCmd::from_slash(parts[0]) {
        Some(BuiltinCmd::Provider) => {
            // Handled in TUI
        }
        Some(BuiltinCmd::Mcp) => {
            let message = if mcp_statuses.is_empty() {
                "No MCP servers configured.".to_string()
            } else {
                format!(
                    "MCP servers:\n{}",
                    mcp_statuses
                        .iter()
                        .map(|(name, status)| format!("- {}: {}", name, status))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            };
            let _ = resp_tx.send(turn(&session.id().await, TurnEvent::Text(message)));
        }
        Some(BuiltinCmd::Plan) => {
            // Open the plan preview modal. The TUI loads
            // the file content from disk on its side; the
            // harness just signals that the modal should
            // open. No-op (with a user-visible message)
            // when no plan is active.
            match agent.active_plan_path() {
                Some(path) => {
                    let _ = resp_tx.send(AgentResponse::OpenPlanPreview(path));
                }
                None => {
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        TurnEvent::Text("No active plan to preview.".to_string()),
                    ));
                }
            }
        }
        Some(BuiltinCmd::Verify) => {
            // Trigger plan verification by submitting a
            // hidden prompt that calls the
            // verify_plan_execution tool. The turn runs
            // through the normal pipeline so the verifier
            // result lands in the transcript and the model
            // can act on it.
            match agent.active_plan_path() {
                Some(_) => {
                    let _ = resp_tx.send(AgentResponse::TriggerVerification);
                }
                None => {
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        TurnEvent::Text("No active plan to verify.".to_string()),
                    ));
                }
            }
        }
        Some(BuiltinCmd::Permissions) => {
            if parts.get(1) == Some(&"clear") {
                agent.clear_allowed_tools();
                let _ = resp_tx.send(turn(
                    &session.id().await,
                    TurnEvent::Text("Always-allowed tool rules cleared.".to_string()),
                ));
            } else {
                let allowed = agent.allowed_tools();
                let message = if allowed.is_empty() {
                    "No tools are always allowed for this process.".to_string()
                } else {
                    format!("Always-allowed tools:\n- {}", allowed.join("\n- "))
                };
                let _ = resp_tx.send(turn(&session.id().await, TurnEvent::Text(message)));
            }
        }
        Some(BuiltinCmd::AutoApprove) => {
            let next = match parts.get(1).map(|s| s.to_lowercase()).as_deref() {
                Some("on") | Some("true") | Some("1") => Some(true),
                Some("off") | Some("false") | Some("0") => Some(false),
                Some(other) => {
                    let _ = resp_tx.send(AgentResponse::Error(format!(
                        "Unknown value '{}'. Use `/auto-approve on|off`.",
                        other
                    )));
                    return;
                }
                None => None,
            };
            let enabled = next.unwrap_or_else(|| !agent.get_auto_approve());
            agent.set_auto_approve(enabled);
            let _ = resp_tx.send(turn(
                &session.id().await,
                TurnEvent::Text(format!(
                    "Auto-approve {}: write tools {} run without permission prompts.",
                    if enabled { "ON" } else { "OFF" },
                    if enabled { "will" } else { "won't" },
                )),
            ));
            let _ = resp_tx.send(turn(
                &session.id().await,
                TurnEvent::AutoApproveChanged(enabled),
            ));
            send_harness_state(resp_tx, &session.id().await, agent, "idle");
        }
        Some(BuiltinCmd::Review) => {
            // /review — on-demand session review (ADR-0018,
            // superseding the periodic ADR-0016 design).
            // Runs the bounded read-only REVIEW subagent
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
            let transcript = session.transcript().await;
            let rounds = Agent::estimate_tool_rounds(&transcript);
            if rounds == 0 {
                let _ = resp_tx.send(turn(
                    &session.id().await,
                    TurnEvent::Text(
                        "Nothing to review yet — no tool rounds in the current \
                                         transcript."
                            .to_string(),
                    ),
                ));
                return;
            }
            let _ = resp_tx.send(turn(
                &session.id().await,
                TurnEvent::Activity("running session review…".to_string()),
            ));
            let verdicts = agent.review_now(&transcript).await;
            // Mirror the worst verdict into the activity-bar
            // banner (empty alert clears it when healthy).
            let alert = Agent::render_review_alert(&verdicts, rounds);
            let _ = resp_tx.send(turn(
                &session.id().await,
                TurnEvent::SessionReview { alert },
            ));
            let _ = resp_tx.send(turn(
                &session.id().await,
                TurnEvent::Text(format_review_report(&verdicts, rounds)),
            ));
        }
        Some(BuiltinCmd::VerifyNudge) => {
            // /verify-nudge        → show current state
            // /verify-nudge on|off → set explicitly
            let next = match parts.get(1).map(|s| s.to_lowercase()).as_deref() {
                Some("on") | Some("true") | Some("1") => Some(true),
                Some("off") | Some("false") | Some("0") => Some(false),
                Some(other) => {
                    let _ = resp_tx.send(AgentResponse::Error(format!(
                        "Unknown value '{other}'. Use `/verify-nudge on|off`."
                    )));
                    return;
                }
                None => None,
            };
            let enabled = next.unwrap_or_else(|| !agent.get_verify_nudge_enabled());
            agent.set_verify_nudge_enabled(enabled);
            let _ = resp_tx.send(turn(
                &session.id().await,
                TurnEvent::Text(format!(
                    "Verify hard nudge {}: the harness {} inject a reminder when \
                                     the model ends a turn with an approved plan but no \
                                     verify_plan_execution call.",
                    if enabled { "ON" } else { "OFF" },
                    if enabled { "will" } else { "won't" },
                )),
            ));
        }
        Some(BuiltinCmd::Search) => {
            let query = cmd.strip_prefix("/search").unwrap_or("").trim();
            if query.is_empty() {
                let _ = resp_tx.send(turn(
                    &session.id().await,
                    TurnEvent::Text("Usage: /search <query>".to_string()),
                ));
            } else {
                let messages = session.transcript().await;
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
                                TurnEvent::Text("No relevant history found.".to_string()),
                            ));
                        } else {
                            let mut lines =
                                vec!["Relevant history (most similar first):".to_string()];
                            for (i, (text, score)) in results.iter().enumerate() {
                                lines.push(format!("{}. [score={:.3}]\n{}", i + 1, score, text));
                            }
                            let _ = resp_tx.send(turn(
                                &session.id().await,
                                TurnEvent::Text(lines.join("\n\n")),
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
                        TurnEvent::Text(format!("Resumed session {}.", short_session_id(&id))),
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
                let archived_count = session.archived_count().await;
                let checkpoint = session.checkpoint().await;
                let last_relief = session.last_relief().await;
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
                                        TurnEvent::Text(format!(
                                    "Session: {}\nForked from: {}\nActive messages: {}\nArchived messages: {}\nLoop checkpoint: {}\nLast context relief: {}",
                                    id,
                                    parent_id,
                                    message_count,
                                    archived_count,
                                    checkpoint_text,
                                    last_relief
                                        .map(|item| format!(
                                            "{} -> {} chars",
                                            item.before_chars, item.after_chars
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
                        TurnEvent::Text(format!("Sessions:\n{}", lines.join("\n"))),
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
                            TurnEvent::Text(format!("Forked session {} from {}.", id, parent_id)),
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
                        *history.lock().await = session.messages().await;
                        let transcript = session.transcript().await;
                        let _ = resp_tx.send(AgentResponse::ConversationReplaced(transcript));
                        let _ = resp_tx.send(turn(
                            &session.id().await,
                            TurnEvent::Text(format!("Opened session {}.", id)),
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
                            TurnEvent::Text(format!("Resumed session {}.", short_session_id(&id))),
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
                match session.reset().await {
                    Ok(id) => {
                        let _ = resp_tx.send(AgentResponse::ConversationCleared);
                        let _ = resp_tx.send(turn(
                            &session.id().await,
                            TurnEvent::Text(format!("Started new session: {}", id)),
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
                pursuit_service.clone(),
                (*skills_registry).clone(),
                project_root_for_side,
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
                    pursuit_service.clone(),
                    config,
                    TurnInput {
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
            let settings = CompactionSettings::from_config(config, active_context_window(agent));
            let _ = resp_tx.send(turn(
                &session.id().await,
                TurnEvent::Activity("compacting context".to_string()),
            ));
            let extra = agent.fire_pre_compact().await;
            match compact_turn_history(
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
                        TurnEvent::Text("Not enough complete turns to compact.".to_string()),
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
                        let _ = tx.send(turn(session_id, TurnEvent::Text(success(&pursuit))));
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
                        TurnEvent::Text("Pursuit stop requested.".to_string()),
                    ));
                } else {
                    let _ = resp_tx.send(turn(
                        &thread_id,
                        TurnEvent::Text("No pursuit is running.".to_string()),
                    ));
                }
                send_harness_state(resp_tx, &session.id().await, agent, "idle");
                return;
            }

            if rest == "status" {
                refresh_agent_pursuit(agent, pursuit_service, &thread_id).await;

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
                let _ = resp_tx.send(turn(&thread_id, TurnEvent::Text(message)));
            } else if rest == "clear" {
                agent.disarm_pursuit();
                match pursuit_service.clear_pursuit(&thread_id).await {
                    Ok(true) => {
                        agent.clear_pursuit();
                        let _ = resp_tx.send(turn(
                            &thread_id,
                            TurnEvent::Text("Pursuit cleared.".to_string()),
                        ));
                    }
                    Ok(false) => {
                        let _ = resp_tx.send(turn(
                            &thread_id,
                            TurnEvent::Text("No pursuit to clear.".to_string()),
                        ));
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
                    pursuit_service.mark_complete(&thread_id).await,
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
                    match pursuit_service
                        .update_objective(&thread_id, new_objective)
                        .await
                    {
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
                                TurnEvent::Text(format!("Pursuit updated: {}", pursuit.objective)),
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
                    match pursuit_service.active_pursuit(&thread_id).await {
                        Ok(Some(pursuit)) => {
                            let _ = resp_tx.send(turn(
                                &thread_id,
                                TurnEvent::Text(format!(
                                    "Resuming pursuit on existing pursuit: {}",
                                    pursuit.objective
                                )),
                            ));
                            pursuit.objective
                        }
                        Ok(None) => {
                            let _ = resp_tx.send(AgentResponse::Error(
                                "No active pursuit. Start one with /pursue <condition>."
                                    .to_string(),
                            ));
                            send_harness_state(resp_tx, &session.id().await, agent, "idle");
                            return;
                        }
                        Err(error) => {
                            let _ = resp_tx.send(AgentResponse::Error(error));
                            send_harness_state(resp_tx, &session.id().await, agent, "idle");
                            return;
                        }
                    }
                } else {
                    match pursuit_service.set_pursuit(&thread_id, rest).await {
                        Ok(pursuit) => {
                            agent.set_pursuit(pursuit.clone());
                            emit_pursuit_updated(resp_tx, &thread_id, &pursuit);
                            pursuit.objective
                        }
                        Err(error) => {
                            let _ = resp_tx.send(AgentResponse::Error(error));
                            send_harness_state(resp_tx, &session.id().await, agent, "idle");
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
                        pursuit_service: pursuit_service.clone(),
                        compaction: CompactionSettings::from_config(
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
                return;
            }
            send_harness_state(resp_tx, &session.id().await, agent, "idle");
        }
        Some(BuiltinCmd::Repeat) => {
            let rest = cmd.strip_prefix("/repeat").unwrap_or("").trim();
            if rest.is_empty() || rest == "help" {
                let _ = resp_tx.send(turn(
                                    &session.id().await,
                                    TurnEvent::Text(
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
                        TurnEvent::Text("No /repeat jobs scheduled.".to_string()),
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
                        resp_tx.send(turn(&session.id().await, TurnEvent::Text(lines.join("\n"))));
                }
                return;
            }
            if let Some(id) = rest.strip_prefix("cancel ") {
                let id = id.trim();
                match repeat_store_for_commands.delete(id).await {
                    Ok(true) => {
                        let _ = resp_tx.send(turn(
                            &session.id().await,
                            TurnEvent::Text(format!("Cancelled repeat job {id}.")),
                        ));
                    }
                    Ok(false) => {
                        let _ = resp_tx.send(turn(
                            &session.id().await,
                            TurnEvent::Text(format!("No repeat job with id {id}.")),
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
                        TurnEvent::Text(format!(
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
                        TurnEvent::Text(format!(
                            "neenee is already configured in '{}'. Nothing to do.",
                            target
                        )),
                    ));
                }
                Ok(created) => {
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        TurnEvent::Text(format!(
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
                                resp_tx.send(turn(&session.id().await, TurnEvent::Text(output)));
                        }
                        Err(error) => {
                            let _ = resp_tx.send(AgentResponse::Error(error));
                        }
                    }
                }
                "reload" => {
                    let tool = ReloadSkillsTool {
                        registry: skills_registry_for_commands.clone(),
                    };
                    match tool.call("{}").await {
                        Ok(output) => {
                            let _ =
                                resp_tx.send(turn(&session.id().await, TurnEvent::Text(output)));
                        }
                        Err(error) => {
                            let _ = resp_tx.send(AgentResponse::Error(error));
                        }
                    }
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
                        let _ = resp_tx.send(turn(&session.id().await, TurnEvent::Text(output)));
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
            let _ = resp_tx.send(AgentResponse::ConversationCleared);
            let _ = resp_tx.send(turn(
                &session.id().await,
                TurnEvent::Text("Conversation history cleared.".to_string()),
            ));
        }
        Some(BuiltinCmd::Export) => {
            let messages = history.lock().await.clone();
            let session_id = session.id().await;
            let provider_id = agent.provider.provider_id();
            let model_name = agent.provider.model();
            let pursuit = agent.get_pursuit();
            let plan_path = agent.active_plan_path();
            let markdown = crate::tui::export::format_export_markdown(
                crate::tui::export::ExportContext {
                    session_id: &session_id,
                    provider: &provider_id,
                    model: &model_name,
                    pursuit: pursuit.as_ref(),
                    active_plan_path: plan_path.as_deref(),
                },
                &messages,
            );
            let char_count = markdown.chars().count();
            match crate::tui::clipboard::copy(&markdown).await {
                Ok(crate::tui::clipboard::CopyOutcome::Native) => {
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        TurnEvent::Text(format!(
                            "Session exported to clipboard ({} messages, {} chars). \
                                             Paste it into another agent to continue this work.",
                            messages.len(),
                            char_count
                        )),
                    ));
                }
                Ok(crate::tui::clipboard::CopyOutcome::Osc52) => {
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        TurnEvent::Text(format!(
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
                        TurnEvent::Text(format!(
                            "Network capture {}: each provider round-trip {} written to\n  {}",
                            if enabled { "ON" } else { "OFF" },
                            if enabled { "is" } else { "will no longer be" },
                            dir.display(),
                        )),
                    ));
                }
                Some(other) => {
                    let _ = resp_tx.send(AgentResponse::Error(format!(
                        "Unknown debug target '{other}'. Available: network. \
                         Usage: `/debug network on|off`."
                    )));
                }
                None => {
                    let network_on = agent.provider.debug_capture_enabled();
                    let _ = resp_tx.send(turn(
                        &session.id().await,
                        TurnEvent::Text(format!(
                            "Debug status:\n- network: {}\n\nUsage: `/debug network on|off`",
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
                TurnEvent::Text(format!(
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
                pursuit_service.clone(),
                config,
                TurnInput {
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
