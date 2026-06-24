use crate::tui::start_tui;
use neenee_agent::catalog;
use neenee_agent::orchestration::{
    compact_turn_history, emit_pursuit_updated, refresh_agent_pursuit, send_compaction,
    send_harness_state, start_interactive_turn, start_pursuit, start_repeat_scheduler,
    CompactionSettings, InteractiveTurnContext, MidTurnCompactionGate, ProxyProvider,
    PursuitContext, RelayCompactionHooks, TurnInput,
};
#[cfg(test)]
use neenee_agent::orchestration::{execute_turn, retry_delay_ms, TurnContext};
use neenee_agent::skills::{
    tools::{ListSkillsTool, ReloadSkillsTool, UseSkillTool},
    SkillRegistry,
};
use neenee_agent::Agent;
use neenee_agent::TaskTool;
#[cfg(test)]
use neenee_core::{async_trait, Message, ProviderStreamEvent};
use neenee_core::{
    AgentMode, AgentRequest, AgentResponse, CronExpr, Provider, Pursuit, PursuitService,
    PursuitStore, RepeatStore, Tool, CHARS_PER_TOKEN, EXPLORE, resolve_model,
};
use neenee_providers::MockProvider;
use neenee_store::{
    config::Config,
    embedding, lock, paths, provider_usage,
    search_tool::SearchHistoryTool,
    session::{self, SessionStore},
};
use neenee_tools::commands::{discover_commands, expand_command, CustomCommand};
use neenee_tools::{
    mcp::load_mcp_tools,
    project::{init_neenee_config, CreateProjectTool, InitConfigTool},
    AskUserTool, BashTool, EditFileTool, GlobTool, GrepTool, ListDirTool, ReadFileTool,
    WebFetchTool, WebSearchTool, WriteFileTool,
};
#[allow(dead_code)]
mod tui;

mod pursuits;
mod session_view;
mod startup;

use pursuits::{format_pursuit_status, load_legacy_pursuit_from_config};
use session_view::{
    build_session_context, build_sessions_overview, provider_key_status, resume_session,
    short_session_id,
};
use startup::{init_tracing, parse_args, split_custom_command, BuiltinCmd, StartupMode};

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{mpsc, RwLock as AsyncRwLock};
use tokio_util::sync::CancellationToken;

/// Resolve the active model's context window (tokens) from the live provider.
/// `0` means unknown (a user-defined or local model not in the registry); the
/// compaction policy substitutes a conservative fallback at resolve time.
fn active_context_window(agent: &Agent) -> usize {
    resolve_model(&agent.provider.model()).context_window
}

/// Re-seed the mid-turn prune threshold from the active model's context window.
/// Called at startup and after every provider/model switch so mid-turn relief
/// tracks the live model instead of a frozen, model-agnostic budget. A no-op
/// when pruning is disabled (no gate is installed in that case).
fn reseed_prune_threshold(agent: &Agent, config: &Config) {
    if !config.compaction_prune {
        return;
    }
    let window = active_context_window(agent);
    agent.set_context_prune_threshold(config.compaction.resolve(window).prune_threshold_tokens);
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _tracing_guard = init_tracing();
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<AgentRequest>();
    let (resp_tx, resp_rx) = mpsc::unbounded_channel::<AgentResponse>();
    let custom_commands = discover_commands()
        .into_iter()
        .filter(|command| {
            !BuiltinCmd::ALL
                .iter()
                .any(|(name, _)| *name == command.name)
        })
        .map(|command| (command.name.clone(), command))
        .collect::<HashMap<String, CustomCommand>>();
    let custom_command_suggestions = {
        let mut suggestions = custom_commands
            .values()
            .map(|command| {
                (
                    format!("/{}", command.name),
                    command
                        .description
                        .clone()
                        .unwrap_or_else(|| "Run project command".to_string()),
                )
            })
            .collect::<Vec<_>>();
        suggestions.sort_by(|left, right| left.0.cmp(&right.0));
        suggestions
    };

    let mut config = Config::load();
    let pursuit_store = PursuitStore::open(paths::get().pursuits_db()).await?;
    let pursuit_service = PursuitService::new(pursuit_store);

    // Durable store for `/repeat` cron jobs. Opened once; cloned for the
    // command handler and the background scheduler.
    let repeat_store = RepeatStore::open(paths::get().repeat_db()).await?;
    // Background scheduler: every 30s prune expired jobs and fire any that are
    // due, dispatching each prompt as a normal chat turn.
    start_repeat_scheduler(
        repeat_store.clone(),
        req_tx.clone(),
        std::time::Duration::from_secs(30),
    );

    // Initialize Agent logic. The provider is resolved through the model
    // catalog (`build_provider_for`), the single source of truth for the
    // env-var-then-config resolution rules shared with runtime switching. See
    // `docs/adr/0002-model-channel-abstraction.md`.
    let initial_provider: Arc<dyn Provider> =
        catalog::build_provider_for(&config, catalog::default_provider_id(&config));

    let provider_holder = Arc::new(RwLock::new(initial_provider));
    let provider_for_task = provider_holder.clone();

    let agent_provider = Arc::new(ProxyProvider {
        holder: provider_holder,
    });

    // Shared skills registry for the skill tools.
    let skills_registry = Arc::new(SkillRegistry::load(&config.skills).await);

    let mcp = load_mcp_tools(&config.mcp).await;
    let mcp_statuses = mcp.statuses;

    // CLI: `neenee` -> fresh session; `neenee resume [id]` -> resume a session;
    // `neenee doctor` -> verify stored session integrity.
    let (startup, project_override, auto_approve_at_start, single_instance) =
        parse_args(std::env::args().skip(1).collect());
    let project_root = project_override.clone().unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    });

    // ADR-0018: the per-project advisory lock is opt-in. The default is
    // unlocked so multiple `neenee` instances can run in one project — each
    // pins its own `sessions/<id>.{json,jsonl}` and never shares a mutable
    // file. `--single-instance` restores the pre-0018 exclusive lock for users
    // who want it. Doctor always skips the lock so it can inspect stores while
    // another instance is running.
    let _lock = if single_instance && !matches!(startup, StartupMode::Doctor) {
        Some(lock::ProcessLock::acquire(
            &paths::get().project_lock_file(&project_root),
        )?)
    } else {
        None
    };

    if matches!(startup, StartupMode::Doctor) {
        session::run_doctor(project_override.as_deref()).await?;
        return Ok(());
    }

    // Session loading honors the startup mode. Under ADR-0018
    // `load_for_project` pins a fresh `sessions/<id>.{json,jsonl}`, so a bare
    // `neenee` always starts a new session; prior sessions stay on disk and
    // are reachable through the picker or `resume`. Resume opens an existing
    // session file by id.
    let session = Arc::new(SessionStore::load_for_project(project_root.clone()));
    let open_picker_on_start = match &startup {
        StartupMode::Fresh => false,
        StartupMode::Picker => true,
        StartupMode::Resume(id) => {
            if let Err(error) = session.resume(id.as_deref()).await {
                eprintln!("resume failed: {error}; starting a fresh session.");
            }
            false
        }
        StartupMode::Doctor => unreachable!("doctor returns before this match"),
    };

    // C12: lightweight semantic-search index for this project. The provider is
    // a deterministic mock; swap it for a local model or cloud API to get real
    // semantic similarity.
    let embedding_store: Arc<AsyncRwLock<embedding::EmbeddingStore>> = Arc::new(AsyncRwLock::new(
        embedding::EmbeddingStore::open(
            paths::get().project_embeddings(&project_root),
            Arc::new(embedding::MockEmbeddingProvider::new(384)),
        )
        .await?,
    ));

    let mut tools: Vec<Arc<dyn neenee_core::Tool>> = vec![
        Arc::new(BashTool),
        Arc::new(ReadFileTool),
        Arc::new(WriteFileTool),
        Arc::new(AskUserTool),
        Arc::new(EditFileTool),
        Arc::new(GrepTool),
        Arc::new(GlobTool),
        Arc::new(ListDirTool),
        Arc::new(WebFetchTool::with_config(config.websearch.clone())),
        Arc::new(WebSearchTool::with_config(config.websearch.clone())),
        Arc::new(CreateProjectTool),
        Arc::new(InitConfigTool),
        Arc::new(UseSkillTool {
            registry: skills_registry.clone(),
        }),
        Arc::new(ListSkillsTool {
            registry: skills_registry.clone(),
        }),
        Arc::new(ReloadSkillsTool {
            registry: skills_registry.clone(),
        }),
    ];
    tools.extend(mcp.tools);
    // TaskTool gets a snapshot of the toolset (excluding itself) so spawned
    // sub-agents cannot recurse and inherit the live provider. It binds the
    // EXPLORE profile (read-only / non-interactive / non-recursive).
    let task_tool = Arc::new(TaskTool::new(
        agent_provider.clone(),
        tools.clone(),
        &EXPLORE,
    ));
    tools.push(task_tool);
    tools.push(Arc::new(SearchHistoryTool::new(
        embedding_store.clone(),
        session.clone(),
    )));
    let agent = Arc::new(Agent::new(
        agent_provider,
        tools,
        AgentMode::Build,
        pursuit_service.clone(),
        (*skills_registry).clone(),
    ));
    // Wire the per-project "always allow" allowlist so prior `Always`
    // approvals survive across sessions in this project. Best-effort: a
    // missing or unreadable permissions.json just means we re-prompt.
    agent.set_project_root(Some(project_root.clone()));
    if auto_approve_at_start {
        agent.set_auto_approve(true);
        let _ = resp_tx.send(AgentResponse::Text(
            "Auto-approve ON: write tools will execute without permission prompts.".to_string(),
        ));
    }

    let active_messages = session.messages().await;
    let restored_messages = session.transcript().await;
    let history = Arc::new(tokio::sync::Mutex::new(active_messages));

    // Mid-turn context relief: when pruning is enabled, install a gate that
    // clears old tool results between tool rounds once pressure crosses the
    // prune threshold. The threshold is derived from the active model's context
    // window and re-seeded whenever the provider switches (see
    // `reseed_prune_threshold`), so it tracks the live model rather than a
    // fixed character budget.
    if config.compaction_prune {
        agent.set_compaction_gate(Some(Arc::new(MidTurnCompactionGate {
            session: session.clone(),
            prune_protect_chars: config.compaction_prune_protect_tokens * CHARS_PER_TOKEN,
        })));
        reseed_prune_threshold(&agent, &config);
    }

    // Wire the `[agent]` config table: the opt-in hard-stop budget and the
    // verify hard-nudge toggle. (Session review is on-demand via `/review`,
    // so it has no config to seed.) All default to sensible values when the
    // table is absent, so this is a no-op for the common case.
    agent.set_hard_stop_rounds(config.agent.hard_stop_rounds);
    agent.set_verify_nudge_enabled(config.agent.verify_nudge_enabled);

    // Tie the agent and its pursuit persistence to this session/thread.
    let thread_id = session.id().await;
    agent.set_thread_id(&thread_id);
    if pursuit_service.get_pursuit(&thread_id).await?.is_none() {
        if let Some(pursuit) = load_legacy_pursuit_from_config() {
            let _ = pursuit_service
                .set_pursuit(&thread_id, &pursuit.objective)
                .await;
        }
    }
    refresh_agent_pursuit(&agent, &pursuit_service, &thread_id).await;

    // Restore the active plan path from the persisted session so resume
    // re-enters Build mode with the "you are implementing X" hint intact.
    // If the session was in Plan mode when last saved this will be None
    // (plan_enter clears it), so there is nothing to restore.
    if let Some(plan_path) = session.active_plan_path().await {
        agent.set_active_plan_path(Some(plan_path));
    }

    // Restore the unified task list so resume re-shows the sticky panel with
    // the same items (and identity) the model last persisted. An empty list
    // is the "no active task list" state and needs no restore.
    let persisted_todos = session.todos().await;
    if !persisted_todos.is_empty() {
        agent.set_todos(persisted_todos);
    }

    // Load history
    let input_history = Config::load_history();

    // Load per-model usage telemetry (recency signal for the picker,
    // ADR-0002 phase 2). Moved into the agent task so both the startup
    // activation and runtime switches record through one instance.
    let provider_usage = provider_usage::ProviderUsage::load();

    let current_task_token = Arc::new(AsyncRwLock::new(None::<CancellationToken>));
    let task_generation = Arc::new(AtomicU64::new(0));
    let ctt_clone = current_task_token.clone();
    let generation_clone = task_generation.clone();
    let commands_for_task = Arc::new(custom_commands);
    let embedding_store_for_commands = embedding_store.clone();
    let repeat_store_for_commands = repeat_store.clone();
    let req_tx_for_commands = req_tx.clone();

    // Initial values for TUI
    let initial_p_name = catalog::default_provider_id(&config).to_string();
    let initial_m_name = catalog::resolved_model_name(&config, &initial_p_name);

    // Spawn Agent Background Task
    let mcp_statuses_for_tui = mcp_statuses.clone();
    let skills_registry_for_commands = skills_registry.clone();
    // The agent background task takes ownership of `config`; pull the TUI
    // presentation config out first so it can be handed to the TUI later.
    let tui_config = config.tui.clone();
    tokio::spawn(async move {
        send_harness_state(&resp_tx, &agent, "idle");
        let _ = resp_tx.send(AgentResponse::ProviderKeys(provider_key_status(&config)));
        // Record that the default model was activated on startup, so the
        // picker's recency ordering reflects "last used = now". Best-effort:
        // usage tracking is rebuildable state and must never block startup.
        let mut provider_usage = provider_usage;
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
                    // Cancellation is driven by the token below; we deliberately
                    // do NOT bump the generation counter here. Bumping would make
                    // the in-flight turn's `is_current` check false, so its own
                    // cleanup (the "... [Interrupted]" message and the transition
                    // back to "idle") would be skipped — leaving the UI stuck in
                    // the "running" state with no interruption feedback. A later
                    // turn bumps the generation itself and supersedes this one.
                    agent.reject_pending_permissions();
                    agent.reject_pending_user_questions();
                    let _ = resp_tx.send(AgentResponse::PermissionsCleared);

                    // Flip the harness to idle the instant interrupt is
                    // requested — BEFORE the in-flight turn unwinds. The work
                    // itself stops the moment the token is cancelled below, but
                    // the turn task's own terminal "idle" snapshot is only sent
                    // at the very end of its cleanup, which is gated behind
                    // persistence fsyncs (`session.replace_messages` inside
                    // `execute_turn`, then `set_checkpoint` in `start_pursuit`).
                    // Without this eager snapshot the activity bar keeps showing
                    // the stale "pursue"/"running" loop_status — and a climbing
                    // elapsed timer — for the whole disk-write window, which
                    // reads as "still working" when the work is already stopped.
                    //
                    // This is idempotent with the stale task's later idle send:
                    // if no new turn starts, both snapshots are "idle"; if one
                    // does, it bumps generation itself and its "running" snapshot
                    // supersedes, while the stale task's generation-guarded idle
                    // send is skipped (`orchestration.rs` start_pursuit /
                    // start_interactive_turn / run_shell_command).
                    send_harness_state(&resp_tx, &agent, "idle");

                    let mut token = ctt_clone.write().await;
                    if let Some(t) = token.take() {
                        t.cancel();
                    }
                }
                AgentRequest::PermissionReply {
                    request_id,
                    decision,
                } => {
                    if !agent.reply_permission(&request_id, decision) {
                        let _ = resp_tx.send(AgentResponse::Error(
                            "Permission request is no longer pending.".to_string(),
                        ));
                    }
                }
                AgentRequest::UserQuestionReply {
                    request_id,
                    answers,
                } => {
                    if !agent.reply_user_question(&request_id, answers) {
                        let _ = resp_tx.send(AgentResponse::Error(
                            "Question request is no longer pending.".to_string(),
                        ));
                    }
                }
                AgentRequest::SwitchProvider {
                    provider_type,
                    model,
                    api_key,
                    base_url,
                } => {
                    // A key entered in the TUI is persisted and wins over
                    // config; environment variables still take precedence.
                    if let Some(key) = api_key.clone() {
                        match provider_type.as_str() {
                            "openai" => config.openai_api_key = Some(key),
                            "gemini" => config.gemini_api_key = Some(key),
                            "kimi-code" => config.moonshot_api_key = Some(key),
                            "deepseek-v4-flash" | "deepseek-v4-pro" => {
                                config.deepseek_api_key = Some(key)
                            }
                            "zai-code" => config.zai_api_key = Some(key),
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
                    let _ = config.save();

                    // Build through the catalog so api-key / user-agent / base-url
                    // resolution is shared with startup. The TUI-supplied model
                    // still wins over any ambient env var, preserving the
                    // pre-catalog switch semantics.
                    let new_p: Arc<dyn Provider> = match catalog::build_catalog(&config)
                        .iter()
                        .find(|e| e.id == provider_type)
                    {
                        Some(entry) => match entry.default_channel() {
                            Some(channel) => {
                                let mut channel = channel.clone();
                                channel.model = model.clone();
                                neenee_providers::build_provider_for_channel(&channel, &entry.id)
                            }
                            None => Arc::new(MockProvider),
                        },
                        None => Arc::new(MockProvider),
                    };
                    *provider_for_task
                        .write()
                        .unwrap_or_else(|error| error.into_inner()) = new_p;

                    // The new model may have a different context window; re-seed
                    // the mid-turn prune threshold so relief tracks it.
                    reseed_prune_threshold(&agent, &config);

                    let _ = resp_tx.send(AgentResponse::ProviderKeys(provider_key_status(&config)));
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
                    let _ = resp_tx.send(AgentResponse::ProviderPicker(
                        catalog::build_picker_state(&config, &provider_usage),
                    ));
                }
                AgentRequest::ToggleFavorite { id } => {
                    // Toggle the id in the favorites list, persist, and push a
                    // fresh picker snapshot so the ★ flips at once.
                    if let Some(pos) = config.favorites.iter().position(|fav| *fav == id) {
                        config.favorites.remove(pos);
                    } else {
                        config.favorites.push(id.clone());
                    }
                    if let Err(error) = config.save() {
                        tracing::warn!(?error, "could not persist favorites");
                    }
                    let _ = resp_tx.send(AgentResponse::ProviderPicker(
                        catalog::build_picker_state(&config, &provider_usage),
                    ));
                }
                AgentRequest::SetDefaultModel { id } => {
                    // `d` in the picker: make `id` the default AND activate it,
                    // reusing the catalog so resolution rules stay shared. No
                    // new key/model comes from the TUI — the provider's existing
                    // resolved config is used as-is.
                    config.default_provider = id.clone();
                    if let Err(error) = config.save() {
                        tracing::warn!(?error, "could not persist default model");
                    }
                    let new_p = catalog::build_provider_for(&config, &id);
                    *provider_for_task
                        .write()
                        .unwrap_or_else(|error| error.into_inner()) = new_p;
                    // Re-seed mid-turn relief for the newly activated model's
                    // context window.
                    reseed_prune_threshold(&agent, &config);
                    provider_usage.record(&id);
                    if let Err(error) = provider_usage.save() {
                        tracing::warn!(?error, "could not persist model usage telemetry");
                    }
                    let model_name = catalog::resolved_model_name(&config, &id);
                    let _ = resp_tx.send(AgentResponse::ProviderSwitched {
                        provider: id.clone(),
                        model: model_name,
                    });
                    let _ = resp_tx.send(AgentResponse::ProviderKeys(provider_key_status(&config)));
                    let _ = resp_tx.send(AgentResponse::ProviderPicker(
                        catalog::build_picker_state(&config, &provider_usage),
                    ));
                }
                AgentRequest::DeleteSession { id } => match session.delete(&id).await {
                    Ok(()) => {
                        let _ = resp_tx.send(AgentResponse::SessionsOverview(
                            build_sessions_overview(&session).await,
                        ));
                    }
                    Err(error) => {
                        let _ = resp_tx.send(AgentResponse::Error(error));
                    }
                },
                AgentRequest::QuerySessionContext => {
                    let snapshot =
                        build_session_context(&agent, &skills_registry, &mcp_statuses, &config);
                    let _ = resp_tx.send(AgentResponse::SessionContext(snapshot));
                }
                AgentRequest::RevokePermission { tool, scope } => {
                    let removed = agent.revoke_allowed_tool(&tool, &scope);
                    if removed {
                        let snapshot =
                            build_session_context(&agent, &skills_registry, &mcp_statuses, &config);
                        let _ = resp_tx.send(AgentResponse::SessionContext(snapshot));
                    } else {
                        let _ = resp_tx.send(AgentResponse::Error(format!(
                            "No cached always-allow rule for {} {}.",
                            tool, scope
                        )));
                    }
                }
                AgentRequest::ToggleTool { name, enabled } => {
                    let changed = agent.set_tool_enabled(&name, enabled);
                    let snapshot =
                        build_session_context(&agent, &skills_registry, &mcp_statuses, &config);
                    if !changed {
                        // Even a no-op (unknown tool, or already in the target
                        // state) refreshes the snapshot so the modal settles
                        // rather than leaving the row looking stale.
                        let _ = resp_tx.send(AgentResponse::Error(format!(
                            "Tool '{}' is unknown or already {}.",
                            name,
                            if enabled { "enabled" } else { "disabled" }
                        )));
                    }
                    let _ = resp_tx.send(AgentResponse::SessionContext(snapshot));
                }
                AgentRequest::SlashCommand(cmd) => {
                    let parts: Vec<&str> = cmd.split_whitespace().collect();
                    if parts.is_empty() {
                        continue;
                    }
                    match BuiltinCmd::from_slash(parts[0]) {
                        Some(BuiltinCmd::Provider) => {
                            // Handled in TUI
                        }
                        Some(BuiltinCmd::Mode) => {
                            if parts.len() > 1 {
                                let new_mode = match parts[1].to_lowercase().as_str() {
                                    "build" => AgentMode::Build,
                                    "plan" => AgentMode::Plan,
                                    _ => {
                                        let _ = resp_tx.send(AgentResponse::Error(format!(
                                            "Unknown mode '{}'. Use 'build' or 'plan'.",
                                            parts[1]
                                        )));
                                        continue;
                                    }
                                };
                                agent.set_mode(new_mode);
                                // `set_mode(Plan)` clears the active plan
                                // `set_mode(Plan)` clears the active plan
                                // path in-memory; mirror that to the session
                                // so a resume after a manual mode switch does
                                // not resurrect a stale plan hint.
                                let agent_plan = agent.active_plan_path();
                                let stored_plan = session.active_plan_path().await;
                                if agent_plan != stored_plan {
                                    if let Err(err) = session.set_active_plan_path(agent_plan).await
                                    {
                                        let _ = resp_tx.send(AgentResponse::Error(format!(
                                            "could not persist plan path: {err}"
                                        )));
                                    }
                                }
                                // Mirror the task list too, since entering
                                // Plan mode clears it.
                                let agent_todos = agent.todos();
                                let stored_todos = session.todos().await;
                                if agent_todos != stored_todos {
                                    if let Err(err) = session.set_todos(agent_todos).await {
                                        let _ = resp_tx.send(AgentResponse::Error(format!(
                                            "could not persist todos: {err}"
                                        )));
                                    }
                                }
                                let _ = resp_tx.send(AgentResponse::Text(format!(
                                    "Mode changed to: {:?}",
                                    new_mode
                                )));
                                send_harness_state(&resp_tx, &agent, "idle");
                            } else {
                                let _ = resp_tx.send(AgentResponse::Text(format!(
                                    "Current mode: {:?}",
                                    agent.get_mode()
                                )));
                            }
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
                            let _ = resp_tx.send(AgentResponse::Text(message));
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
                                    let _ = resp_tx.send(AgentResponse::Text(
                                        "No active plan to preview.".to_string(),
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
                                    let _ = resp_tx.send(AgentResponse::Text(
                                        "No active plan to verify.".to_string(),
                                    ));
                                }
                            }
                        }
                        Some(BuiltinCmd::Permissions) => {
                            if parts.get(1) == Some(&"clear") {
                                agent.clear_allowed_tools();
                                let _ = resp_tx.send(AgentResponse::Text(
                                    "Always-allowed tool rules cleared.".to_string(),
                                ));
                            } else {
                                let allowed = agent.allowed_tools();
                                let message = if allowed.is_empty() {
                                    "No tools are always allowed for this process.".to_string()
                                } else {
                                    format!("Always-allowed tools:\n- {}", allowed.join("\n- "))
                                };
                                let _ = resp_tx.send(AgentResponse::Text(message));
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
                                    continue;
                                }
                                None => None,
                            };
                            let enabled = next.unwrap_or_else(|| !agent.get_auto_approve());
                            agent.set_auto_approve(enabled);
                            let _ = resp_tx.send(AgentResponse::Text(format!(
                                "Auto-approve {}: write tools {} run without permission prompts.",
                                if enabled { "ON" } else { "OFF" },
                                if enabled { "will" } else { "won't" },
                            )));
                            let _ = resp_tx.send(AgentResponse::AutoApproveChanged(enabled));
                            send_harness_state(&resp_tx, &agent, "idle");
                        }
                        Some(BuiltinCmd::Review) => {
                            // /review — on-demand session review (ADR-0018,
                            // superseding the periodic ADR-0016 design).
                            // Runs the bounded read-only REVIEW sub-agent
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
                                continue;
                            }
                            let transcript = session.transcript().await;
                            let rounds = Agent::estimate_tool_rounds(&transcript);
                            if rounds == 0 {
                                let _ = resp_tx.send(AgentResponse::Text(
                                    "Nothing to review yet — no tool rounds in the current \
                                     transcript."
                                        .to_string(),
                                ));
                                continue;
                            }
                            let _ = resp_tx.send(AgentResponse::Activity(
                                "running session review…".to_string(),
                            ));
                            let verdicts = agent.review_now(&transcript).await;
                            // Mirror the worst verdict into the activity-bar
                            // banner (empty alert clears it when healthy).
                            let alert = Agent::render_review_alert(&verdicts, rounds);
                            let _ = resp_tx.send(AgentResponse::SessionReview { alert });
                            let _ = resp_tx
                                .send(AgentResponse::Text(format_review_report(&verdicts, rounds)));
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
                                    continue;
                                }
                                None => None,
                            };
                            let enabled = next.unwrap_or_else(|| !agent.get_verify_nudge_enabled());
                            agent.set_verify_nudge_enabled(enabled);
                            let _ = resp_tx.send(AgentResponse::Text(format!(
                                "Verify hard nudge {}: the harness {} inject a reminder when \
                                 the model ends a turn with an approved plan but no \
                                 verify_plan_execution call.",
                                if enabled { "ON" } else { "OFF" },
                                if enabled { "will" } else { "won't" },
                            )));
                        }
                        Some(BuiltinCmd::Search) => {
                            let query = cmd.strip_prefix("/search").unwrap_or("").trim();
                            if query.is_empty() {
                                let _ = resp_tx.send(AgentResponse::Text(
                                    "Usage: /search <query>".to_string(),
                                ));
                            } else {
                                let messages = session.transcript().await;
                                {
                                    let mut store = embedding_store_for_commands.write().await;
                                    let session_id = session.id().await;
                                    if let Err(error) = store.index(&messages, &session_id).await {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                        continue;
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
                                            let _ = resp_tx.send(AgentResponse::Text(
                                                "No relevant history found.".to_string(),
                                            ));
                                        } else {
                                            let mut lines =
                                                vec!["Relevant history (most similar first):"
                                                    .to_string()];
                                            for (i, (text, score)) in results.iter().enumerate() {
                                                lines.push(format!(
                                                    "{}. [score={:.3}]\n{}",
                                                    i + 1,
                                                    score,
                                                    text
                                                ));
                                            }
                                            let _ = resp_tx
                                                .send(AgentResponse::Text(lines.join("\n\n")));
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
                            match resume_session(&session, &history, parts.get(1).copied()).await {
                                Ok((id, transcript)) => {
                                    let _ = resp_tx
                                        .send(AgentResponse::ConversationReplaced(transcript));
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "Resumed session {}.",
                                        short_session_id(&id)
                                    )));
                                    send_harness_state(&resp_tx, &agent, "idle");
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
                                let compaction = session.compaction().await;
                                let checkpoint_text = checkpoint
                                    .map(|item| {
                                        format!(
                                            "{} {}/{} ({})",
                                            item.pursuit,
                                            item.iteration,
                                            item.max_iterations,
                                            item.status
                                        )
                                    })
                                    .unwrap_or_else(|| "none".to_string());
                                let _ = resp_tx.send(AgentResponse::Text(format!(
                                    "Session: {}\nForked from: {}\nActive messages: {}\nArchived messages: {}\nLoop checkpoint: {}\nLast compaction: {}",
                                    id,
                                    parent_id,
                                    message_count,
                                    archived_count,
                                    checkpoint_text,
                                    compaction
                                        .map(|item| format!(
                                            "{} -> {} chars",
                                            item.before_chars, item.after_chars
                                        ))
                                        .unwrap_or_else(|| "none".to_string())
                                )));
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
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "Sessions:\n{}",
                                        lines.join("\n")
                                    )));
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
                                        let _ = resp_tx.send(AgentResponse::Text(format!(
                                            "Forked session {} from {}.",
                                            id, parent_id
                                        )));
                                        send_harness_state(&resp_tx, &agent, "idle");
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
                                    continue;
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
                                        let _ = resp_tx
                                            .send(AgentResponse::ConversationReplaced(transcript));
                                        let _ = resp_tx.send(AgentResponse::Text(format!(
                                            "Opened session {}.",
                                            id
                                        )));
                                        send_harness_state(&resp_tx, &agent, "idle");
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
                                match resume_session(&session, &history, parts.get(2).copied())
                                    .await
                                {
                                    Ok((id, transcript)) => {
                                        let _ = resp_tx
                                            .send(AgentResponse::ConversationReplaced(transcript));
                                        let _ = resp_tx.send(AgentResponse::Text(format!(
                                            "Resumed session {}.",
                                            short_session_id(&id)
                                        )));
                                        send_harness_state(&resp_tx, &agent, "idle");
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
                                        let _ = resp_tx.send(AgentResponse::Text(format!(
                                            "Started new session: {}",
                                            id
                                        )));
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
                                build_sessions_overview(&session).await,
                            ));
                        }
                        Some(BuiltinCmd::Compact) => {
                            let mut current = history.lock().await.clone();
                            let settings = CompactionSettings::from_config(&config, active_context_window(&agent));
                            let hooks = RelayCompactionHooks {
                                tx: resp_tx.clone(),
                            };
                            match compact_turn_history(
                                &mut current,
                                &session,
                                &settings,
                                Some(agent.provider.clone()),
                                &hooks,
                            )
                            .await
                            {
                                Ok(Some(checkpoint)) => {
                                    *history.lock().await = current;
                                    send_compaction(&resp_tx, &checkpoint);
                                }
                                Ok(None) => {
                                    let _ = resp_tx.send(AgentResponse::Text(
                                        "Not enough complete turns to compact.".to_string(),
                                    ));
                                }
                                Err(error) => {
                                    let _ = resp_tx.send(AgentResponse::Error(error));
                                }
                            }
                        }
                        Some(BuiltinCmd::Pursue) => {
                            let thread_id = session.id().await;
                            let argument = cmd.strip_prefix("/pursue").unwrap_or("").trim();
                            let rest = argument;

                            async fn report_pursuit_result(
                                tx: &mpsc::UnboundedSender<AgentResponse>,
                                agent: &Agent,
                                result: Result<Option<Pursuit>, String>,
                                success: impl FnOnce(&Pursuit) -> String,
                                empty: impl Into<String>,
                            ) {
                                match result {
                                    Ok(Some(pursuit)) => {
                                        agent.set_pursuit(pursuit.clone());
                                        emit_pursuit_updated(tx, &pursuit);
                                        let _ = tx.send(AgentResponse::Text(success(&pursuit)));
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
                                    let _ = resp_tx.send(AgentResponse::Text(
                                        "Pursuit stop requested.".to_string(),
                                    ));
                                } else {
                                    let _ = resp_tx.send(AgentResponse::Text(
                                        "No pursuit is running.".to_string(),
                                    ));
                                }
                                send_harness_state(&resp_tx, &agent, "idle");
                                continue;
                            }

                            if rest == "status" {
                                refresh_agent_pursuit(&agent, &pursuit_service, &thread_id).await;
                                let armed = agent.is_pursuit_armed();
                                let iterations = agent.pursuit_iterations();
                                let message = match agent.get_pursuit() {
                                    Some(pursuit) => {
                                        let mut m = format_pursuit_status(&pursuit);
                                        if armed {
                                            m.push_str(&format!(
                                                "\nPursuit active · gate iteration {iterations}"
                                            ));
                                        }
                                        m
                                    }
                                    None => {
                                        "No active pursuit. Start one with /pursue <condition>."
                                            .to_string()
                                    }
                                };
                                let _ = resp_tx.send(AgentResponse::Text(message));
                            } else if rest == "clear" {
                                agent.disarm_pursuit();
                                match pursuit_service.clear_pursuit(&thread_id).await {
                                    Ok(true) => {
                                        agent.clear_pursuit();
                                        let _ = resp_tx.send(AgentResponse::Text(
                                            "Pursuit cleared.".to_string(),
                                        ));
                                    }
                                    Ok(false) => {
                                        let _ = resp_tx.send(AgentResponse::Text(
                                            "No pursuit to clear.".to_string(),
                                        ));
                                    }
                                    Err(error) => {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                    }
                                }
                            } else if rest == "done" {
                                agent.disarm_pursuit();
                                report_pursuit_result(
                                    &resp_tx,
                                    &agent,
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
                                            emit_pursuit_updated(&resp_tx, &pursuit);
                                            let _ = resp_tx.send(AgentResponse::Text(format!(
                                                "Pursuit updated: {}",
                                                pursuit.objective
                                            )));
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
                            } else if rest == "pause"
                                || rest == "resume"
                                || rest.starts_with("budget ")
                            {
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
                                            let _ = resp_tx.send(AgentResponse::Text(format!(
                                                "Resuming pursuit on existing pursuit: {}",
                                                pursuit.objective
                                            )));
                                            pursuit.objective
                                        }
                                        Ok(None) => {
                                            let _ = resp_tx.send(AgentResponse::Error(
                                                "No active pursuit. Start one with /pursue <condition>."
                                                    .to_string(),
                                            ));
                                            send_harness_state(&resp_tx, &agent, "idle");
                                            continue;
                                        }
                                        Err(error) => {
                                            let _ = resp_tx.send(AgentResponse::Error(error));
                                            send_harness_state(&resp_tx, &agent, "idle");
                                            continue;
                                        }
                                    }
                                } else {
                                    match pursuit_service.set_pursuit(&thread_id, rest).await {
                                        Ok(pursuit) => {
                                            agent.set_pursuit(pursuit.clone());
                                            emit_pursuit_updated(&resp_tx, &pursuit);
                                            pursuit.objective
                                        }
                                        Err(error) => {
                                            let _ = resp_tx.send(AgentResponse::Error(error));
                                            send_harness_state(&resp_tx, &agent, "idle");
                                            continue;
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
                                        pursuit_service: pursuit_service.clone(),
                                        compaction: CompactionSettings::from_config(&config, active_context_window(&agent)),
                                        retry_max_attempts: config.provider_retry_max_attempts,
                                        retry_base_ms: config.provider_retry_base_ms,
                                        retry_max_ms: config.provider_retry_max_ms,
                                    },
                                    condition,
                                )
                                .await;
                                continue;
                            }
                            send_harness_state(&resp_tx, &agent, "idle");
                        }
                        Some(BuiltinCmd::Repeat) => {
                            let rest = cmd.strip_prefix("/repeat").unwrap_or("").trim();
                            if rest.is_empty() || rest == "help" {
                                let _ = resp_tx.send(AgentResponse::Text(
                                    "Usage: /repeat <cron> <prompt>\n\
                                     cron is five fields: minute hour day month weekday \
                                     (e.g. `*/5 * * * *` = every 5 min, `0 9 * * 1-5` = 09:00 weekdays).\n\
                                     Also: /repeat list, /repeat cancel <id>."
                                        .to_string(),
                                ));
                                continue;
                            }
                            if rest == "list" {
                                let jobs =
                                    repeat_store_for_commands.list().await.unwrap_or_default();
                                if jobs.is_empty() {
                                    let _ = resp_tx.send(AgentResponse::Text(
                                        "No /repeat jobs scheduled.".to_string(),
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
                                    let _ = resp_tx.send(AgentResponse::Text(lines.join("\n")));
                                }
                                continue;
                            }
                            if let Some(id) = rest.strip_prefix("cancel ") {
                                let id = id.trim();
                                match repeat_store_for_commands.delete(id).await {
                                    Ok(true) => {
                                        let _ = resp_tx.send(AgentResponse::Text(format!(
                                            "Cancelled repeat job {id}."
                                        )));
                                    }
                                    Ok(false) => {
                                        let _ = resp_tx.send(AgentResponse::Text(format!(
                                            "No repeat job with id {id}."
                                        )));
                                    }
                                    Err(error) => {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                    }
                                }
                                continue;
                            }
                            // `/repeat <5-field cron> <prompt>`
                            let tokens: Vec<&str> = rest.split_whitespace().collect();
                            if tokens.len() < 6 {
                                let _ = resp_tx.send(AgentResponse::Error(
                                    "Usage: /repeat <5-field cron> <prompt>. \
                                      Example: /repeat */5 * * * * check the deploy"
                                        .to_string(),
                                ));
                                continue;
                            }
                            let cron = tokens[0..5].join(" ");
                            let prompt = tokens[5..].join(" ");
                            let parsed = match CronExpr::parse(&cron) {
                                Ok(p) => p,
                                Err(error) => {
                                    let _ = resp_tx.send(AgentResponse::Error(format!(
                                        "Invalid cron: {error}"
                                    )));
                                    continue;
                                }
                            };
                            let now = chrono::Utc::now();
                            let next = match parsed.next_fire(now) {
                                Some(n) => n,
                                None => {
                                    let _ = resp_tx.send(AgentResponse::Error(
                                        "That cron expression never fires within the next year."
                                            .to_string(),
                                    ));
                                    continue;
                                }
                            };
                            match repeat_store_for_commands.add(&cron, &prompt, next).await {
                                Ok(job) => {
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "Scheduled repeat job {} (`{}`), next {}. Running now.",
                                        &job.id[..8.min(job.id.len())],
                                        cron,
                                        next.format("%Y-%m-%d %H:%M"),
                                    )));
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
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "neenee is already configured in '{}'. Nothing to do.",
                                        target
                                    )));
                                }
                                Ok(created) => {
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "Initialized neenee configuration in '{}'.\nCreated:\n{}",
                                        target,
                                        created
                                            .iter()
                                            .map(|path| format!("- {}", path))
                                            .collect::<Vec<_>>()
                                            .join("\n")
                                    )));
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
                                            let _ = resp_tx.send(AgentResponse::Text(output));
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
                                            let _ = resp_tx.send(AgentResponse::Text(output));
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
                                let _ = resp_tx
                                    .send(AgentResponse::Error("Usage: /skill <name>".to_string()));
                            } else {
                                let args = serde_json::json!({ "name": name }).to_string();
                                let tool = UseSkillTool {
                                    registry: skills_registry_for_commands.clone(),
                                };
                                match tool.call(&args).await {
                                    Ok(output) => {
                                        let _ = resp_tx.send(AgentResponse::Text(output));
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
                            let _ = resp_tx.send(AgentResponse::Text(
                                "Conversation history cleared.".to_string(),
                            ));
                        }
                        Some(BuiltinCmd::Export) => {
                            let messages = history.lock().await.clone();
                            let session_id = session.id().await;
                            let provider_id = agent.provider.provider_id();
                            let model_name = agent.provider.model();
                            let mode = match agent.get_mode() {
                                AgentMode::Build => "build",
                                AgentMode::Plan => "plan",
                            };
                            let pursuit = agent.get_pursuit();
                            let plan_path = agent.active_plan_path();
                            let markdown = crate::tui::export::format_export_markdown(
                                crate::tui::export::ExportContext {
                                    session_id: &session_id,
                                    provider: &provider_id,
                                    model: &model_name,
                                    mode,
                                    pursuit: pursuit.as_ref(),
                                    active_plan_path: plan_path.as_deref(),
                                },
                                &messages,
                            );
                            let char_count = markdown.chars().count();
                            match crate::tui::clipboard::copy(&markdown).await {
                                Ok(crate::tui::clipboard::CopyOutcome::Native) => {
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "Session exported to clipboard ({} messages, {} chars). \
                                         Paste it into another agent to continue this work.",
                                        messages.len(),
                                        char_count
                                    )));
                                }
                                Ok(crate::tui::clipboard::CopyOutcome::Osc52) => {
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "Session exported via OSC52 ({} messages, {} chars). \
                                         If your terminal did not capture it, run neenee in a \
                                         clipboard-capable environment.",
                                        messages.len(),
                                        char_count
                                    )));
                                }
                                Err(error) => {
                                    let _ = resp_tx.send(AgentResponse::Error(format!(
                                        "Export built ({} chars) but clipboard copy failed: {}",
                                        char_count, error
                                    )));
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
                            let _ = resp_tx
                                .send(AgentResponse::Text(format!("{}
{custom_help}", lines.join("
"))));
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
                                continue;
                            };
                            start_interactive_turn(
                                InteractiveTurnContext {
                                    agent: agent.clone(),
                                    history: history.clone(),
                                    tx: resp_tx.clone(),
                                    token_slot: ctt_clone.clone(),
                                    generation_counter: generation_clone.clone(),
                                    session: session.clone(),
                                    pursuit_service: pursuit_service.clone(),
                                    compaction: CompactionSettings::from_config(&config, active_context_window(&agent)),
                                    retry_max_attempts: config.provider_retry_max_attempts,
                                    retry_base_ms: config.provider_retry_base_ms,
                                    retry_max_ms: config.provider_retry_max_ms,
                                },
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
                AgentRequest::Chat { text, images } => {
                    start_interactive_turn(
                        InteractiveTurnContext {
                            agent: agent.clone(),
                            history: history.clone(),
                            tx: resp_tx.clone(),
                            token_slot: ctt_clone.clone(),
                            generation_counter: generation_clone.clone(),
                            session: session.clone(),
                            pursuit_service: pursuit_service.clone(),
                            compaction: CompactionSettings::from_config(&config, active_context_window(&agent)),
                            retry_max_attempts: config.provider_retry_max_attempts,
                            retry_base_ms: config.provider_retry_base_ms,
                            retry_max_ms: config.provider_retry_max_ms,
                        },
                        TurnInput {
                            prompt: text,
                            hidden: false,
                            display_prompt: None,
                            images,
                        },
                    )
                    .await;
                }
                AgentRequest::ShellCommand { command } => {
                    // The `!` prefix path: run the command directly through
                    // the `bash` tool, bypassing the LLM. The lifecycle
                    // mirrors a normal tool step — `ToolCall` → live
                    // `ToolStream` → `ToolResult` — so the existing render
                    // path picks it up with no special-casing.
                    let shell_tx = resp_tx.clone();
                    let shell_token_slot = ctt_clone.clone();
                    let shell_generation = generation_clone.clone();
                    let shell_agent = agent.clone();
                    tokio::spawn(async move {
                        run_shell_command(
                            command,
                            shell_tx,
                            shell_token_slot,
                            shell_generation,
                            shell_agent,
                        )
                        .await;
                    });
                }
            }
        }
    });

    // Start TUI in the main thread
    match start_tui(
        req_tx,
        resp_rx,
        initial_p_name,
        initial_m_name,
        input_history,
        restored_messages,
        custom_command_suggestions,
        mcp_statuses_for_tui,
        tui_config,
    )
    .await
    {
        Ok(history) => {
            let _ = Config::save_history(&history);
        }
        Err(e) => return Err(e),
    }

    Ok(())
}

/// Render the verdicts of an on-demand `/review` as a durable text report for
/// the conversation stream. Complements the transient activity-bar alert
/// (which carries only the worst status + details) by listing every dimension
/// with its status label and the reviewer's detail sentence.
fn format_review_report(verdicts: &[neenee_core::ReviewVerdict], rounds: usize) -> String {
    use neenee_core::ReviewStatus;
    let worst = verdicts.iter().map(|v| v.status).max();
    let headline = match worst {
        None => {
            return format!(
                "Session review (~{rounds} tool rounds): no review dimensions registered."
            );
        }
        Some(ReviewStatus::Healthy) => {
            format!("Session review (~{rounds} tool rounds): no concerns found.")
        }
        Some(status) => {
            format!(
                "Session review (~{rounds} tool rounds) — verdict: {}.",
                status.label()
            )
        }
    };
    let mut lines = vec![headline];
    for verdict in verdicts {
        let detail = verdict.detail.trim();
        if detail.is_empty() {
            lines.push(format!(
                "  • {} — {}",
                verdict.dimension,
                verdict.status.label()
            ));
        } else {
            lines.push(format!(
                "  • {} — {}: {}",
                verdict.dimension,
                verdict.status.label(),
                detail
            ));
        }
    }
    lines.push("Interrupt the turn with Esc if it looks stuck.".to_string());
    lines.join("\n")
}

/// Execute a `!`-prefixed shell command directly through the `bash` tool,
/// bypassing the LLM. Emits the same lifecycle events as a normal tool step
/// (`ToolCall` → live `ToolStream` → `ToolResult` or `ToolCancelled`) so the
/// existing render path picks it up unchanged.
///
/// Cancellation mirrors [`start_interactive_turn`]: a fresh
/// [`CancellationToken`] is installed (any previous token is cancelled) and
/// the generation counter is bumped so a later turn supersedes a still-running
/// shell command and its tail-end events do not race with the new turn.
async fn run_shell_command(
    command: String,
    tx: mpsc::UnboundedSender<AgentResponse>,
    token_slot: Arc<AsyncRwLock<Option<CancellationToken>>>,
    generation_counter: Arc<AtomicU64>,
    agent: Arc<Agent>,
) {
    use neenee_core::ToolStream;

    let call_id = format!("shell_{}", uuid::Uuid::new_v4());
    let arguments = serde_json::json!({ "command": command }).to_string();

    // Mirror start_interactive_turn: install a fresh cancellation token,
    // cancelling any in-flight predecessor, and bump the generation so we
    // can tell on exit whether we are still the active turn.
    let token = CancellationToken::new();
    if let Some(previous) = token_slot.write().await.replace(token.clone()) {
        agent.reject_pending_permissions();
        let _ = tx.send(AgentResponse::PermissionsCleared);
        previous.cancel();
    }
    let generation = generation_counter.fetch_add(1, Ordering::SeqCst) + 1;
    let is_current = || generation_counter.load(Ordering::SeqCst) == generation;

    // Surface the synthetic tool step starting. The response listener maps
    // `name: "bash"` to the "running command" activity status.
    let _ = tx.send(AgentResponse::ToolCall {
        id: call_id.clone(),
        name: "bash".to_string(),
        arguments: arguments.clone(),
    });

    let bash = BashTool;
    let tx_for_stream = tx.clone();
    let call_id_for_stream = call_id.clone();
    let mut on_stream = move |stream: ToolStream| {
        if !is_current() {
            return;
        }
        let _ = tx_for_stream.send(AgentResponse::ToolStream {
            id: call_id_for_stream.clone(),
            stream,
        });
    };

    let run = bash.call_structured_with_events("", &arguments, Box::new(|_| {}), &mut on_stream);

    tokio::select! {
        biased;
        _ = token.cancelled() => {
            // Ctrl+C (or a newer turn replacing us): dropping `run` kills
            // the child via `kill_on_drop`. Only emit the cancellation
            // event if we are still the active turn — a newer turn's
            // ToolCall events must not be flattened by our exit.
            if is_current() {
                let _ = tx.send(AgentResponse::ToolCancelled {
                    id: call_id,
                    name: "bash".to_string(),
                });
            }
        }
        result = run => if is_current() {
            match result {
                Ok(structured) => {
                    let output = structured.to_text();
                    let _ = tx.send(AgentResponse::ToolResult {
                        id: call_id,
                        name: "bash".to_string(),
                        output,
                        structured,
                        duration_ms: 0,
                    });
                }
                Err(error) => {
                    let structured = neenee_core::ToolOutput::Text(error.clone());
                    let _ = tx.send(AgentResponse::ToolResult {
                        id: call_id,
                        name: "bash".to_string(),
                        output: error,
                        structured,
                        duration_ms: 0,
                    });
                }
            }
        },
    }

    // Release the slot and flip the harness to idle, matching
    // start_interactive_turn's cleanup. Guarded by the generation check so
    // a newer turn is not reset by our exit.
    let mut slot = token_slot.write().await;
    if is_current() {
        slot.take();
        drop(slot);
        send_harness_state(&tx, &agent, "idle");
        let _ = tx.send(AgentResponse::Activity(String::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use std::sync::atomic::AtomicUsize;

    struct RetryOnceProvider(AtomicUsize);
    struct ToolThenRetryProvider(AtomicUsize);
    struct RetryReadTool;

    #[async_trait]
    impl Provider for RetryOnceProvider {
        async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
            Err("non-streaming path should not be used".to_string())
        }

        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<futures::stream::BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(stream::empty()))
        }

        async fn stream_chat_events(
            &self,
            _messages: Vec<Message>,
        ) -> Result<futures::stream::BoxStream<'static, Result<ProviderStreamEvent, String>>, String>
        {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Box::pin(stream::iter(vec![
                    Ok(ProviderStreamEvent::TextDelta("partial".to_string())),
                    Err(neenee_core::retryable_error("rate limited", Some(1))),
                ])))
            } else {
                Ok(Box::pin(stream::iter(vec![Ok(
                    ProviderStreamEvent::TextDelta("done".to_string()),
                )])))
            }
        }
    }

    #[async_trait]
    impl Provider for ToolThenRetryProvider {
        async fn chat(&self, _messages: Vec<Message>) -> Result<Message, String> {
            Err("non-streaming path should not be used".to_string())
        }

        async fn stream_chat(
            &self,
            _messages: Vec<Message>,
        ) -> Result<futures::stream::BoxStream<'static, Result<String, String>>, String> {
            Ok(Box::pin(stream::empty()))
        }

        async fn stream_chat_events(
            &self,
            _messages: Vec<Message>,
        ) -> Result<futures::stream::BoxStream<'static, Result<ProviderStreamEvent, String>>, String>
        {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Box::pin(stream::iter(vec![Ok(
                    ProviderStreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call".to_string()),
                        name: Some("retry_read".to_string()),
                        arguments: "{}".to_string(),
                    },
                )])))
            } else {
                Ok(Box::pin(stream::iter(vec![Err(
                    neenee_core::retryable_error("upstream unavailable", None),
                )])))
            }
        }
    }

    #[async_trait]
    impl neenee_core::Tool for RetryReadTool {
        fn name(&self) -> &str {
            "retry_read"
        }

        fn description(&self) -> &str {
            "retry safety test"
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn access(&self) -> neenee_core::ToolAccess {
            neenee_core::ToolAccess::Read
        }

        async fn call(&self, _arguments: &str) -> Result<String, String> {
            Ok("read".to_string())
        }
    }

    #[tokio::test]
    async fn proxy_provider_does_not_block_the_async_runtime() {
        let holder: Arc<RwLock<Arc<dyn Provider>>> = Arc::new(RwLock::new(Arc::new(MockProvider)));
        let proxy = ProxyProvider { holder };

        proxy.prepare_tools(&[]);
        let response = proxy.chat(Vec::new()).await.unwrap();

        assert!(response.content.contains("mock AI"));
    }

    #[test]
    fn context_overflow_detection_is_conservative() {
        assert!(neenee_core::is_context_overflow(
            "maximum context length exceeded for this model"
        ));
        assert!(neenee_core::is_context_overflow(
            "too many tokens in request"
        ));
        assert!(!neenee_core::is_context_overflow(
            "network connection reset"
        ));
    }

    #[tokio::test]
    async fn turn_retries_transient_provider_failure_before_tool_activity() {
        let directory =
            std::env::temp_dir().join(format!("neenee-retry-test-{}", uuid::Uuid::new_v4()));
        let session = Arc::new(SessionStore::for_path(directory.join("session.json")));
        let history = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let pursuit_service = PursuitService::new(
            PursuitStore::open_in_memory_blocking().expect("in-memory pursuit store"),
        );
        let agent = Arc::new(Agent::new(
            Arc::new(RetryOnceProvider(AtomicUsize::new(0))),
            Vec::new(),
            AgentMode::Build,
            pursuit_service.clone(),
            SkillRegistry::empty(),
        ));
        let (tx, mut rx) = mpsc::unbounded_channel();

        let completed = execute_turn(
            TurnContext {
                agent,
                history: history.clone(),
                tx,
                token: CancellationToken::new(),
                session,
                pursuit_service,
                compaction: CompactionSettings {
                    budget: neenee_core::CompactionPolicy::default().resolve(100_000),
                    preserve_turns: 6,
                    summarize: false,
                    prune: false,
                    prune_protect_chars: 0,
                },
                retry_max_attempts: 3,
                retry_base_ms: 1,
                retry_max_ms: 10,
            },
            TurnInput {
                prompt: "work".to_string(),
                hidden: false,
                display_prompt: None,
                images: Vec::new(),
            },
        )
        .await
        .unwrap();

        assert!(!completed);
        assert!(history
            .lock()
            .await
            .iter()
            .any(|message| message.content == "done"));
        let responses = std::iter::from_fn(|| rx.try_recv().ok()).collect::<Vec<_>>();
        let activities = responses
            .iter()
            .filter_map(|response| match response {
                AgentResponse::Activity(status) => Some(status.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(activities.starts_with(&["saving request", "preparing context"]));
        assert_eq!(
            activities
                .iter()
                .filter(|status| **status == "waiting for model")
                .count(),
            2
        );
        assert_eq!(activities.last(), Some(&"saving response"));
        assert!(responses.iter().any(|response| matches!(
            response,
            AgentResponse::RetryScheduled {
                attempt: 2,
                max_attempts: 3,
                ..
            }
        )));
        assert!(responses
            .iter()
            .any(|response| matches!(response, AgentResponse::StreamDiscard)));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn turn_does_not_retry_after_tool_activity() {
        let directory =
            std::env::temp_dir().join(format!("neenee-retry-tool-{}", uuid::Uuid::new_v4()));
        let session = Arc::new(SessionStore::for_path(directory.join("session.json")));
        let pursuit_service = PursuitService::new(
            PursuitStore::open_in_memory_blocking().expect("in-memory pursuit store"),
        );
        let agent = Arc::new(Agent::new(
            Arc::new(ToolThenRetryProvider(AtomicUsize::new(0))),
            vec![Arc::new(RetryReadTool)],
            AgentMode::Build,
            pursuit_service.clone(),
            SkillRegistry::empty(),
        ));
        let (tx, mut rx) = mpsc::unbounded_channel();

        let error = execute_turn(
            TurnContext {
                agent,
                history: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                tx,
                token: CancellationToken::new(),
                session,
                pursuit_service,
                compaction: CompactionSettings {
                    budget: neenee_core::CompactionPolicy::default().resolve(100_000),
                    preserve_turns: 6,
                    summarize: false,
                    prune: false,
                    prune_protect_chars: 0,
                },
                retry_max_attempts: 4,
                retry_base_ms: 1,
                retry_max_ms: 10,
            },
            TurnInput {
                prompt: "work".to_string(),
                hidden: false,
                display_prompt: None,
                images: Vec::new(),
            },
        )
        .await
        .unwrap_err();

        assert_eq!(error.to_string(), "upstream unavailable");
        assert!(!std::iter::from_fn(|| rx.try_recv().ok())
            .any(|response| matches!(response, AgentResponse::RetryScheduled { .. })));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn retry_delay_honors_headers_and_exponential_bounds() {
        assert_eq!(retry_delay_ms(1, None, 1_000, 30_000), 1_000);
        assert_eq!(retry_delay_ms(3, None, 1_000, 30_000), 4_000);
        assert_eq!(retry_delay_ms(2, Some(45_000), 1_000, 30_000), 30_000);
    }
}
