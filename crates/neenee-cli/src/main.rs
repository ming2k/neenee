use neenee_tools::commands::{discover_commands, expand_command, CustomCommand};
use neenee_harness::skills::{
    tools::{ListSkillsTool, ReloadSkillsTool, UseSkillTool},
    SkillRegistry,
};
use neenee_harness::TaskTool;
use neenee_tools::{
    mcp::load_mcp_tools,
    project::{init_neenee_config, CreateProjectTool, InitConfigTool},
    AskUserTool, BashTool, EditFileTool, GlobTool, GrepTool, ListDirTool, ReadFileTool,
    TodoWriteTool, WebFetchTool, WebSearchTool, WriteFileTool,
};
use neenee_core::{
    AgentMode, AgentRequest, AgentResponse, Goal, GoalService, GoalStatus, GoalStore, Message,
    Provider, SessionOverview, Tool,
};
use neenee_harness::Agent;
use neenee_harness::catalog;
use neenee_harness::orchestration::{
    compact_turn_history, emit_goal_updated, refresh_agent_goal, send_compaction,
    send_harness_state, start_goal_loop, start_interactive_turn, CompactionSettings,
    InteractiveTurnContext, LoopRunContext, MidTurnCompactionGate, ProxyProvider,
    RelayCompactionHooks, TurnInput,
};
#[cfg(test)]
use neenee_core::{
    async_trait, AgentEvent, GoalAccountingResult, HarnessError, HarnessSnapshot,
    ProviderStreamEvent, TurnTimer, GOAL_COMPLETE_MARKER,
};
#[cfg(test)]
use neenee_harness::orchestration::{self, TurnContext, execute_turn, retry_delay_ms};
use neenee_providers::MockProvider;
use neenee_app::{
    embedding, lock, model_usage, paths, config::Config, search_tool::SearchHistoryTool,
    session::{self, discard_trailing_loop_prompts, SessionStore},
};
use crate::tui::start_tui;
#[allow(dead_code)]
mod tui;
use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{mpsc, RwLock as AsyncRwLock};
use tokio_util::sync::CancellationToken;

fn load_legacy_goal_from_config() -> Option<Goal> {
    #[derive(serde::Deserialize)]
    struct LegacyGoal {
        harness_goal: Option<String>,
        #[serde(default)]
        harness_goal_completed: bool,
        #[serde(default)]
        harness_goal_checklist: Vec<neenee_core::GoalChecklistItem>,
    }

    let path = Config::config_file_path();
    let content = std::fs::read_to_string(path).ok()?;
    let legacy: LegacyGoal = toml::from_str(&content).ok()?;
    let objective = legacy.harness_goal?;
    Some(Goal {
        objective,
        status: if legacy.harness_goal_completed {
            GoalStatus::Complete
        } else {
            GoalStatus::Active
        },
        checklist: legacy.harness_goal_checklist,
        tokens_used: 0,
        token_budget: None,
        time_used_seconds: 0,
    })
}

const BUILTIN_COMMANDS: &[&str] = &[
    "models",
    "mode",
    "mcp",
    "permissions",
    "session",
    "sessions",
    "resume",
    "compact",
    "goal",
    "loop",
    "init",
    "skills",
    "skill",
    "clear",
    "help",
    "exit",
];

fn split_custom_command(input: &str) -> (&str, &str) {
    let input = input.trim();
    let split_at = input.find(char::is_whitespace).unwrap_or(input.len());
    let (name, arguments) = input.split_at(split_at);
    (name.trim_start_matches('/'), arguments.trim())
}

async fn resume_session(
    session: &SessionStore,
    history: &tokio::sync::Mutex<Vec<Message>>,
    id: Option<&str>,
) -> Result<(String, Vec<Message>), String> {
    let id = session.resume(id).await?;
    *history.lock().await = session.messages().await;
    Ok((id, session.transcript().await))
}

fn short_session_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Whether each provider has a usable API key (env var or config).
/// Keyless providers (local llama, mock) always report `true`.
///
/// Derived from the model catalog so the readiness signal and the actual
/// provider construction share one resolution path.
fn provider_key_status(config: &Config) -> Vec<(String, bool)> {
    catalog::build_catalog(config)
        .entries
        .iter()
        .map(|entry| (entry.id.clone(), entry.key_ready()))
        .collect()
}

#[derive(Debug)]
enum StartupMode {
    Fresh,
    Resume(Option<String>),
    Picker,
    Doctor,
}

fn parse_args(args: Vec<String>) -> (StartupMode, Option<std::path::PathBuf>) {
    let mut iter = args.into_iter().peekable();
    let mut project: Option<std::path::PathBuf> = None;
    let mut rest = Vec::new();
    while let Some(arg) = iter.next() {
        if arg == "--project" {
            project = iter.next().map(std::path::PathBuf::from);
        } else if let Some(value) = arg.strip_prefix("--project=") {
            project = Some(std::path::PathBuf::from(value));
        } else {
            rest.push(arg);
        }
    }

    let mode = match rest.as_slice() {
        [] => StartupMode::Fresh,
        [cmd] if cmd == "resume" => StartupMode::Picker,
        [cmd, id] if cmd == "resume" => StartupMode::Resume(Some(id.clone())),
        [cmd, ..] if cmd == "doctor" => StartupMode::Doctor,
        [cmd, ..] => {
            eprintln!(
                "Unknown command '{}'. Usage:\n  neenee              start a fresh session\n  neenee resume [id]  resume a session (picker when no id)\n  neenee doctor       verify stored session integrity",
                cmd
            );
            std::process::exit(2);
        }
    };
    (mode, project)
}

async fn build_sessions_overview(session: &SessionStore) -> Vec<SessionOverview> {
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

/// Initialise file-based tracing when `NEENEE_LOG` names a log file.
///
/// A TUI cannot log to stdout (it would corrupt the display), so tracing is
/// opt-in and always writes to a file. Verbosity comes from `RUST_LOG`,
/// defaulting to `info` for the neenee crates. The returned guard flushes the
/// non-blocking writer on drop and must live for the whole process.
fn init_tracing() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let path = std::path::PathBuf::from(std::env::var_os("NEENEE_LOG")?);
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let file_name = path.file_name()?.to_owned();
    let (writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(dir, file_name));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("neenee=info,neenee_core=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();
    tracing::info!("neenee tracing initialised");
    Some(guard)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _tracing_guard = init_tracing();
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<AgentRequest>();
    let (resp_tx, resp_rx) = mpsc::unbounded_channel::<AgentResponse>();
    let custom_commands = discover_commands()
        .into_iter()
        .filter(|command| !BUILTIN_COMMANDS.contains(&command.name.as_str()))
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
    let goal_store = GoalStore::open(paths::get().goals_db()).await?;
    let goal_service = GoalService::new(goal_store);

    // Initialize Agent logic. The provider is resolved through the model
    // catalog (`build_provider_for`), the single source of truth for the
    // env-var-then-config resolution rules shared with runtime switching. See
    // `docs/adr/0002-model-channel-abstraction.md`.
    let initial_provider: Arc<dyn Provider> =
        catalog::build_provider_for(&config, catalog::default_model_id(&config));

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
    let (startup, project_override) = parse_args(std::env::args().skip(1).collect());
    let project_root = project_override.clone().unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    });

    // C9: per-project advisory lock. Doctor intentionally skips it so it can
    // inspect stores while another instance is running.
    let _lock = if matches!(startup, StartupMode::Doctor) {
        None
    } else {
        Some(lock::ProcessLock::acquire(
            &paths::get().project_lock_file(&project_root),
        )?)
    };

    if matches!(startup, StartupMode::Doctor) {
        session::run_doctor(project_override.as_deref()).await?;
        return Ok(());
    }

    // Session loading honors the startup mode. The previous active session is
    // archived and remains available through /resume or /session resume.
    let session = Arc::new(SessionStore::load_for_project(project_root.clone()));
    let open_picker_on_start = match &startup {
        StartupMode::Fresh => {
            session.reset().await.map_err(std::io::Error::other)?;
            false
        }
        StartupMode::Picker => {
            session.reset().await.map_err(std::io::Error::other)?;
            true
        }
        StartupMode::Resume(id) => {
            if let Err(error) = session.resume(id.as_deref()).await {
                eprintln!("resume failed: {error}; starting a fresh session.");
                session.reset().await.map_err(std::io::Error::other)?;
            }
            false
        }
        StartupMode::Doctor => unreachable!("doctor returns before this match"),
    };

    // C12: lightweight semantic-search index for this project. The provider is
    // a deterministic mock; swap it for a local model or cloud API to get real
    // semantic similarity.
    let embedding_store: Arc<AsyncRwLock<embedding::EmbeddingStore>> =
        Arc::new(AsyncRwLock::new(embedding::EmbeddingStore::open(
            paths::get().project_embeddings(&project_root),
            Arc::new(embedding::MockEmbeddingProvider::new(384)),
        )
        .await?));

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
        Arc::new(TodoWriteTool::new()),
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
    // sub-agents cannot recurse and inherit the live provider.
    let task_tool = Arc::new(TaskTool::new(agent_provider.clone(), tools.clone()));
    tools.push(task_tool);
    tools.push(Arc::new(SearchHistoryTool::new(
        embedding_store.clone(),
        session.clone(),
    )));
    let agent = Arc::new(Agent::new(
        agent_provider,
        tools,
        AgentMode::Build,
        goal_service.clone(),
        (*skills_registry).clone(),
    ));

    let active_messages = session.messages().await;
    let restored_messages = session.transcript().await;
    let history = Arc::new(tokio::sync::Mutex::new(active_messages));

    // Mid-turn context relief: when pruning is enabled, install a gate that
    // clears old tool results between tool rounds once pressure crosses the
    // mid-turn threshold (before the full pre-turn compaction threshold).
    if config.compaction_prune {
        let mid_turn_budget = config.compaction_max_chars
            * CompactionSettings::MID_TURN_TRIGGER_NUM
            / CompactionSettings::MID_TURN_TRIGGER_DEN;
        agent.set_context_budget_chars(mid_turn_budget);
        agent.set_compaction_gate(Some(Arc::new(MidTurnCompactionGate {
            session: session.clone(),
            prune_protect_chars: config.compaction_prune_protect_chars,
        })));
    }

    // Tie the agent and its goal persistence to this session/thread.
    let thread_id = session.id().await;
    agent.set_thread_id(&thread_id);
    if goal_service.get_goal(&thread_id).await?.is_none() {
        if let Some(goal) = load_legacy_goal_from_config() {
            let _ = goal_service
                .set_goal(&thread_id, &goal.objective, goal.status, goal.token_budget)
                .await;
        }
    }
    refresh_agent_goal(&agent, &goal_service, &thread_id).await;

    // Load history
    let input_history = Config::load_history();

    // Load per-model usage telemetry (recency signal for the picker,
    // ADR-0002 phase 2). Moved into the agent task so both the startup
    // activation and runtime switches record through one instance.
    let model_usage = model_usage::ModelUsage::load();

    let current_task_token = Arc::new(AsyncRwLock::new(None::<CancellationToken>));
    let task_generation = Arc::new(AtomicU64::new(0));
    let ctt_clone = current_task_token.clone();
    let generation_clone = task_generation.clone();
    let commands_for_task = Arc::new(custom_commands);
    let embedding_store_for_commands = embedding_store.clone();

    // Initial values for TUI
    let initial_p_name = catalog::default_model_id(&config).to_string();
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
        let mut model_usage = model_usage;
        {
            let initial_id = catalog::default_model_id(&config).to_string();
            model_usage.record(&initial_id);
            if let Err(error) = model_usage.save() {
                tracing::warn!(?error, "could not persist model usage telemetry");
            }
        }
        // Push the initial model-picker snapshot (default id + per-model
        // favorite / key-ready / last-used) so the picker is ready the moment
        // the user opens it.
        let _ = resp_tx.send(AgentResponse::ModelPicker(catalog::build_picker_state(
            &config,
            &model_usage,
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
                            "kimi-code" => config.kimi_code_api_key = Some(key),
                            "deepseek" | "deepseek-flash" | "deepseek-pro" => {
                                config.deepseek_api_key = Some(key)
                            }
                            "qwen" => config.qwen_api_key = Some(key),
                            "glm" => config.glm_api_key = Some(key),
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
                    // above already landed in `config`. Keep both the canonical
                    // `default_model` pointer and the legacy `default_provider`
                    // in sync so they never diverge.
                    config.default_model = Some(provider_type.clone());
                    config.default_provider = provider_type.clone();
                    match provider_type.as_str() {
                        "openai" => config.openai_model = Some(model.clone()),
                        "gemini" => config.gemini_model = Some(model.clone()),
                        "kimi-code" => {}
                        "llama" => config.llama_model = Some(model.clone()),
                        "deepseek" | "deepseek-flash" => {
                            config.deepseek_flash_model = Some(model.clone())
                        }
                        "deepseek-pro" => config.deepseek_pro_model = Some(model.clone()),
                        "qwen" => config.qwen_model = Some(model.clone()),
                        "glm" => config.glm_model = Some(model.clone()),
                        _ => {}
                    }
                    let _ = config.save();

                    // Build through the catalog so api-key / user-agent / base-url
                    // resolution is shared with startup. The TUI-supplied model
                    // still wins over any ambient env var, preserving the
                    // pre-catalog switch semantics.
                    let new_p: Arc<dyn Provider> =
                        match catalog::build_catalog(&config).get(provider_type.as_str()) {
                            Some(entry) => match entry.default_channel() {
                                Some(channel) => {
                                    let mut channel = channel.clone();
                                    channel.model = model.clone();
                                    neenee_providers::build_provider_for_channel(
                                        &channel,
                                        &entry.id,
                                    )
                                }
                                None => Arc::new(MockProvider),
                            },
                            None => Arc::new(MockProvider),
                        };
                    *provider_for_task
                        .write()
                        .unwrap_or_else(|error| error.into_inner()) = new_p;

                    let _ = resp_tx.send(AgentResponse::ProviderKeys(provider_key_status(&config)));
                    // Record the switch as an activation so the picker's recency
                    // ordering tracks it. Best-effort: telemetry is rebuildable.
                    model_usage.record(&provider_type);
                    if let Err(error) = model_usage.save() {
                        tracing::warn!(?error, "could not persist model usage telemetry");
                    }
                    let _ = resp_tx.send(AgentResponse::ProviderSwitched {
                        provider: provider_type,
                        model,
                    });
                    let _ = resp_tx.send(AgentResponse::ModelPicker(catalog::build_picker_state(
                        &config,
                        &model_usage,
                    )));
                }
                AgentRequest::ToggleFavorite { id } => {
                    // Toggle the canonical id in the favorites list, persist,
                    // and push a fresh picker snapshot so the ★ flips at once.
                    let canonical = neenee_core::catalog::canonical_id(&id).to_string();
                    if let Some(pos) = config.favorites.iter().position(|fav| *fav == canonical) {
                        config.favorites.remove(pos);
                    } else {
                        config.favorites.push(canonical);
                    }
                    if let Err(error) = config.save() {
                        tracing::warn!(?error, "could not persist favorites");
                    }
                    let _ = resp_tx.send(AgentResponse::ModelPicker(catalog::build_picker_state(
                        &config,
                        &model_usage,
                    )));
                }
                AgentRequest::SetDefaultModel { id } => {
                    // `d` in the picker: make `id` the default AND activate it,
                    // reusing the catalog so resolution rules stay shared. No
                    // new key/model comes from the TUI — the model's existing
                    // resolved config is used as-is.
                    let canonical = neenee_core::catalog::canonical_id(&id).to_string();
                    config.default_model = Some(canonical.clone());
                    config.default_provider = canonical.clone();
                    if let Err(error) = config.save() {
                        tracing::warn!(?error, "could not persist default model");
                    }
                    let new_p = catalog::build_provider_for(&config, &canonical);
                    *provider_for_task
                        .write()
                        .unwrap_or_else(|error| error.into_inner()) = new_p;
                    model_usage.record(&canonical);
                    if let Err(error) = model_usage.save() {
                        tracing::warn!(?error, "could not persist model usage telemetry");
                    }
                    let model_name = catalog::resolved_model_name(&config, &canonical);
                    let _ = resp_tx.send(AgentResponse::ProviderSwitched {
                        provider: canonical.clone(),
                        model: model_name,
                    });
                    let _ = resp_tx.send(AgentResponse::ProviderKeys(provider_key_status(&config)));
                    let _ = resp_tx.send(AgentResponse::ModelPicker(catalog::build_picker_state(
                        &config,
                        &model_usage,
                    )));
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
                AgentRequest::SlashCommand(cmd) => {
                    let parts: Vec<&str> = cmd.split_whitespace().collect();
                    if parts.is_empty() {
                        continue;
                    }
                    match parts[0] {
                        "/models" => {
                            // Handled in TUI
                        }
                        "/mode" => {
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
                        "/mcp" => {
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
                        "/permissions" => {
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
                        "/search" => {
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
                        "/resume" => {
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
                        "/session" => match parts.get(1).copied().unwrap_or("status") {
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
                                            item.goal,
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
                        "/sessions" => {
                            let _ = resp_tx.send(AgentResponse::SessionsOverview(
                                build_sessions_overview(&session).await,
                            ));
                        }
                        "/compact" => {
                            let mut current = history.lock().await.clone();
                            let settings = CompactionSettings::from(&config);
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
                        "/goal" => {
                            let thread_id = session.id().await;
                            let argument = cmd.strip_prefix("/goal").unwrap_or("").trim();
                            let rest = argument;

                            async fn report_goal_result(
                                tx: &mpsc::UnboundedSender<AgentResponse>,
                                agent: &Agent,
                                result: Result<Option<Goal>, String>,
                                success: impl FnOnce(&Goal) -> String,
                                empty: impl Into<String>,
                            ) {
                                match result {
                                    Ok(Some(goal)) => {
                                        agent.set_goal(goal.clone());
                                        emit_goal_updated(tx, &goal);
                                        let _ = tx.send(AgentResponse::Text(success(&goal)));
                                    }
                                    Ok(None) => {
                                        let _ = tx.send(AgentResponse::Error(empty.into()));
                                    }
                                    Err(error) => {
                                        let _ = tx.send(AgentResponse::Error(error));
                                    }
                                }
                            }

                            if rest.is_empty() || rest == "status" {
                                refresh_agent_goal(&agent, &goal_service, &thread_id).await;
                                let message = match agent.get_goal() {
                                    Some(goal) => format_goal_status(&goal),
                                    None => "No active goal. Set one with /goal <objective>."
                                        .to_string(),
                                };
                                let _ = resp_tx.send(AgentResponse::Text(message));
                            } else if rest == "clear" {
                                match goal_service.clear_goal(&thread_id).await {
                                    Ok(true) => {
                                        agent.clear_goal();
                                        let _ = resp_tx
                                            .send(AgentResponse::Text("Goal cleared.".to_string()));
                                    }
                                    Ok(false) => {
                                        let _ = resp_tx.send(AgentResponse::Text(
                                            "No goal to clear.".to_string(),
                                        ));
                                    }
                                    Err(error) => {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                    }
                                }
                            } else if rest == "done" {
                                report_goal_result(
                                    &resp_tx,
                                    &agent,
                                    goal_service.mark_complete(&thread_id).await,
                                    |_| "Goal marked completed.".to_string(),
                                    "No goal to complete.",
                                )
                                .await;
                            } else if rest == "pause" {
                                report_goal_result(
                                    &resp_tx,
                                    &agent,
                                    goal_service.pause(&thread_id).await,
                                    |_| "Goal paused.".to_string(),
                                    "No active goal to pause.",
                                )
                                .await;
                            } else if rest == "resume" {
                                report_goal_result(
                                    &resp_tx,
                                    &agent,
                                    goal_service.resume(&thread_id).await,
                                    |_| "Goal resumed.".to_string(),
                                    "No goal to resume.",
                                )
                                .await;
                            } else if rest.starts_with("edit ") {
                                let new_objective = rest.strip_prefix("edit ").unwrap_or("").trim();
                                if new_objective.is_empty() {
                                    let _ = resp_tx.send(AgentResponse::Error(
                                        "Usage: /goal edit <new objective>".to_string(),
                                    ));
                                } else {
                                    match goal_service
                                        .update_goal(&thread_id, Some(new_objective), None, None)
                                        .await
                                    {
                                        Ok(Some(goal)) => {
                                            agent.set_goal(goal.clone());
                                            {
                                                let mut messages = history.lock().await;
                                                agent.inject_objective_updated(&mut messages);
                                                let updated = messages.clone();
                                                drop(messages);
                                                let _ = session.replace_messages(updated).await;
                                            }
                                            emit_goal_updated(&resp_tx, &goal);
                                            let _ = resp_tx.send(AgentResponse::Text(format!(
                                                "Goal updated: {}",
                                                goal.objective
                                            )));
                                        }
                                        Ok(None) => {
                                            let _ = resp_tx.send(AgentResponse::Error(
                                                "No goal to edit. Set one first with /goal <objective>."
                                                    .to_string(),
                                            ));
                                        }
                                        Err(error) => {
                                            let _ = resp_tx.send(AgentResponse::Error(error));
                                        }
                                    }
                                }
                            } else if rest.starts_with("budget ") {
                                let budget_arg = rest.strip_prefix("budget ").unwrap_or("").trim();
                                if budget_arg == "clear" {
                                    report_goal_result(
                                        &resp_tx,
                                        &agent,
                                        goal_service
                                            .update_goal(&thread_id, None, None, Some(None))
                                            .await,
                                        |_| "Goal token budget cleared.".to_string(),
                                        "No goal to update.",
                                    )
                                    .await;
                                } else {
                                    match budget_arg.parse::<i64>() {
                                        Ok(budget) if budget > 0 => {
                                            report_goal_result(
                                                &resp_tx,
                                                &agent,
                                                goal_service
                                                    .update_goal(
                                                        &thread_id,
                                                        None,
                                                        None,
                                                        Some(Some(budget)),
                                                    )
                                                    .await,
                                                |_| {
                                                    format!(
                                                        "Goal token budget set to {} tokens.",
                                                        budget
                                                    )
                                                },
                                                "No goal to update.",
                                            )
                                            .await;
                                        }
                                        _ => {
                                            let _ = resp_tx.send(AgentResponse::Error(
                                                "Usage: /goal budget <tokens> | /goal budget clear"
                                                    .to_string(),
                                            ));
                                        }
                                    }
                                }
                            } else {
                                // Set a new goal.
                                match goal_service
                                    .set_goal(&thread_id, rest, GoalStatus::Active, None)
                                    .await
                                {
                                    Ok(goal) => {
                                        agent.set_goal(goal.clone());
                                        emit_goal_updated(&resp_tx, &goal);
                                        let _ = resp_tx.send(AgentResponse::Text(format!(
                                            "Goal set: {}",
                                            goal.objective
                                        )));
                                    }
                                    Err(error) => {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                    }
                                }
                            }
                            send_harness_state(&resp_tx, &agent, "idle");
                        }
                        "/loop" => {
                            let thread_id = session.id().await;
                            let argument = parts.get(1).copied().unwrap_or("8");
                            if argument == "stop" {
                                let mut current = ctt_clone.write().await;
                                if let Some(token) = current.take() {
                                    token.cancel();
                                    let _ = resp_tx.send(AgentResponse::Text(
                                        "Loop stop requested.".to_string(),
                                    ));
                                } else {
                                    let _ = resp_tx.send(AgentResponse::Text(
                                        "No loop is running.".to_string(),
                                    ));
                                }
                                send_harness_state(&resp_tx, &agent, "idle");
                                continue;
                            }
                            if argument == "status" {
                                let running = ctt_clone.read().await.is_some();
                                let status = if running { "running" } else { "idle" };
                                let checkpoint = session.checkpoint().await;
                                let detail = checkpoint
                                    .map(|checkpoint| {
                                        format!(
                                            "{} {}/{} for {}",
                                            checkpoint.status,
                                            checkpoint.iteration,
                                            checkpoint.max_iterations,
                                            checkpoint.goal
                                        )
                                    })
                                    .unwrap_or_else(|| "no checkpoint".to_string());
                                let _ = resp_tx.send(AgentResponse::Text(format!(
                                    "Loop status: {}\nCheckpoint: {}",
                                    status, detail
                                )));
                                send_harness_state(&resp_tx, &agent, status);
                                continue;
                            }
                            if argument == "resume" {
                                if ctt_clone.read().await.is_some() {
                                    let _ = resp_tx.send(AgentResponse::Error(
                                        "A chat or loop task is already running.".to_string(),
                                    ));
                                    continue;
                                }
                                let checkpoint = match session.checkpoint().await {
                                    Some(checkpoint) => checkpoint,
                                    None => {
                                        let _ = resp_tx.send(AgentResponse::Error(
                                            "No loop checkpoint is available to resume."
                                                .to_string(),
                                        ));
                                        continue;
                                    }
                                };
                                let start_iteration = match checkpoint.resume_iteration() {
                                    Ok(iteration) => iteration,
                                    Err(error) => {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                        continue;
                                    }
                                };

                                let mut current = history.lock().await.clone();
                                let discarded = discard_trailing_loop_prompts(&mut current);
                                if discarded > 0 {
                                    *history.lock().await = current.clone();
                                    if let Err(error) = session.replace_messages(current).await {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                        continue;
                                    }
                                }

                                match goal_service
                                    .set_goal(
                                        &thread_id,
                                        &checkpoint.goal,
                                        GoalStatus::Active,
                                        None,
                                    )
                                    .await
                                {
                                    Ok(goal) => {
                                        agent.set_goal(goal.clone());
                                        emit_goal_updated(&resp_tx, &goal);
                                    }
                                    Err(error) => {
                                        let _ = resp_tx.send(AgentResponse::Error(format!(
                                            "Failed to restore loop goal: {error}"
                                        )));
                                        continue;
                                    }
                                }
                                let _ = resp_tx.send(AgentResponse::Text(format!(
                                    "Resuming goal loop at iteration {}/{}{}.",
                                    start_iteration,
                                    checkpoint.max_iterations,
                                    if discarded > 0 {
                                        " after removing an incomplete control prompt"
                                    } else {
                                        ""
                                    }
                                )));
                                start_goal_loop(
                                    LoopRunContext {
                                        agent: agent.clone(),
                                        history: history.clone(),
                                        tx: resp_tx.clone(),
                                        token_slot: ctt_clone.clone(),
                                        generation_counter: generation_clone.clone(),
                                        session: session.clone(),
                                        goal_service: goal_service.clone(),
                                        compaction: CompactionSettings::from(&config),
                                        retry_max_attempts: config.provider_retry_max_attempts,
                                        retry_base_ms: config.provider_retry_base_ms,
                                        retry_max_ms: config.provider_retry_max_ms,
                                    },
                                    checkpoint.goal,
                                    start_iteration,
                                    checkpoint.max_iterations,
                                )
                                .await;
                                continue;
                            }

                            let max_iterations = match argument.parse::<usize>() {
                                Ok(value) if (1..=50).contains(&value) => value,
                                _ => {
                                    let _ = resp_tx.send(AgentResponse::Error(
                                        "Usage: /loop <1-50> | /loop resume | /loop stop | /loop status".to_string(),
                                    ));
                                    continue;
                                }
                            };
                            let goal = match goal_service.active_goal(&thread_id).await {
                                Ok(Some(goal)) => goal,
                                Ok(None) => {
                                    let _ = resp_tx.send(AgentResponse::Error(
                                        "Set an active goal with /goal <objective> before starting /loop.".to_string()
                                    ));
                                    continue;
                                }
                                Err(error) => {
                                    let _ = resp_tx.send(AgentResponse::Error(error));
                                    continue;
                                }
                            };
                            start_goal_loop(
                                LoopRunContext {
                                    agent: agent.clone(),
                                    history: history.clone(),
                                    tx: resp_tx.clone(),
                                    token_slot: ctt_clone.clone(),
                                    generation_counter: generation_clone.clone(),
                                    session: session.clone(),
                                    goal_service: goal_service.clone(),
                                    compaction: CompactionSettings::from(&config),
                                    retry_max_attempts: config.provider_retry_max_attempts,
                                    retry_base_ms: config.provider_retry_base_ms,
                                    retry_max_ms: config.provider_retry_max_ms,
                                },
                                goal.objective,
                                1,
                                max_iterations,
                            )
                            .await;
                        }
                        "/init" => {
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
                        "/skills" => {
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
                        "/skill" => {
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
                        "/clear" => {
                            history.lock().await.clear();
                            let _ = session.replace_messages(Vec::new()).await;
                            let _ = resp_tx.send(AgentResponse::ConversationCleared);
                            let _ = resp_tx.send(AgentResponse::Text(
                                "Conversation history cleared.".to_string(),
                            ));
                        }
                        "/help" => {
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
                            let _ = resp_tx.send(AgentResponse::Text(
                                format!("Slash commands:\n\
                                /models   — Select an LLM provider\n\
                                /mode     — Show or switch mode (build, plan)\n\
                                /mcp      — Show configured MCP server status\n\
                                /compact  — Compact older complete turns now\n\
                                /clear    — Clear the conversation history\n\
                                                                /permissions [clear] — Show or clear always-allowed tool rules
                                /search <query> — Semantic search over the project's session history
\n\
                                /session [status|list|resume|fork|open|new] — Manage durable sessions\n\
                                /sessions — Browse past sessions\n\
                                /resume [id] — Resume the most recent or selected session\n\
                                /goal     — Set, inspect, complete, or clear the active goal\n\
                                /loop [N|resume|status|stop] — Run or resume bounded autonomous goal work\n\
                                /init [path] — Initialize a .neenee/ config tree\n\
                                /skills [list|reload] — List or reload available skills\n\
                                /skill <name> — Load a skill by name\n\
                                /help     — Show available commands and keybindings\n\
                                /exit     — Exit the program{}", custom_help)
                            ));
                        }
                        "/exit" => {
                            let _ = resp_tx.send(AgentResponse::Exit);
                        }
                        _ => {
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
                                    goal_service: goal_service.clone(),
                                    compaction: CompactionSettings::from(&config),
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
                            goal_service: goal_service.clone(),
                            compaction: CompactionSettings::from(&config),
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

fn format_goal_status(goal: &Goal) -> String {
    use neenee_core::GoalChecklistStatus;

    let mut lines = Vec::new();
    lines.push(format!(
        "Goal [{}]: {}",
        goal.status.as_str(),
        goal.objective
    ));

    if !goal.checklist.is_empty() {
        let total = goal.checklist.len();
        let done = goal
            .checklist
            .iter()
            .filter(|item| {
                matches!(
                    item.status,
                    GoalChecklistStatus::Completed | GoalChecklistStatus::Cancelled
                )
            })
            .count();
        lines.push(String::new());
        lines.push(format!("Plans ({done}/{total}):"));
        for item in &goal.checklist {
            let (glyph, label) = match item.status {
                GoalChecklistStatus::Completed => ("✓", "done"),
                GoalChecklistStatus::Cancelled => ("✗", "cancelled"),
                GoalChecklistStatus::InProgress => ("◎", "in progress"),
                GoalChecklistStatus::Pending => ("○", "pending"),
            };
            lines.push(format!(
                "  {glyph} {content}  ({label})",
                content = item.content
            ));
        }
    }

    let has_token_budget = goal.token_budget.is_some_and(|b| b > 0);
    let has_time = goal.time_used_seconds > 0;
    if has_token_budget || has_time {
        lines.push(String::new());
        lines.push("Budget:".to_string());
        if let Some(budget) = goal.token_budget.filter(|b| *b > 0) {
            let used = goal.tokens_used;
            let pct = ((used as f64) / (budget as f64) * 100.0).clamp(0.0, 100.0) as u8;
            lines.push(format!(
                "  tokens  {used} / {budget}  {}",
                budget_bar(pct, 24)
            ));
        }
        if has_time {
            lines.push(format!(
                "  time    {}",
                humanize_seconds(goal.time_used_seconds)
            ));
        }
    }

    lines.join("\n")
}

/// Render a fixed-width ASCII progress bar for `pct` (0..=100). The bar fills
/// with `#` and empties with `-`, bracketed by `[` / `]` for a total width of
/// `width` characters (including the brackets).
fn budget_bar(pct: u8, width: usize) -> String {
    if width < 4 {
        return format!("[{pct}%]");
    }
    let bar_width = width - 2;
    let filled = ((pct as usize) * bar_width / 100).min(bar_width);
    let empty = bar_width - filled;
    format!("[{}{}]", "#".repeat(filled), "-".repeat(empty))
}

/// Format a duration in seconds as a compact `Xh YYm` / `Xm YYs` / `Xs` string.
fn humanize_seconds(secs: i64) -> String {
    let secs = secs.max(0);
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
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

    #[test]
    fn goal_status_includes_structured_checklist() {
        let goal = Goal {
            objective: "ship".to_string(),
            status: GoalStatus::Active,
            checklist: vec![neenee_core::GoalChecklistItem {
                content: "verify".to_string(),
                status: neenee_core::GoalChecklistStatus::InProgress,
            }],
            tokens_used: 0,
            token_budget: None,
            time_used_seconds: 0,
        };

        let status = format_goal_status(&goal);
        assert!(status.contains("Goal [active]: ship"));
        assert!(status.contains("Plans (0/1):"));
        assert!(status.contains("◎ verify  (in progress)"));
    }

    #[test]
    fn goal_status_renders_budget_bar_and_time() {
        let goal = Goal {
            objective: "ship".to_string(),
            status: GoalStatus::Active,
            checklist: Vec::new(),
            tokens_used: 40_000,
            token_budget: Some(50_000),
            time_used_seconds: 125,
        };

        let status = format_goal_status(&goal);
        // 80% of a 24-char bar (bar_width=22) → 17 filled, 5 empty.
        let expected_bar = budget_bar(80, 24);
        assert_eq!(expected_bar.len(), 24);
        assert_eq!(expected_bar.matches('#').count(), 17);
        assert!(status.contains(&format!("tokens  40000 / 50000  {expected_bar}")));
        assert!(status.contains("time    2m05s"));
    }

    #[tokio::test]
    async fn turn_retries_transient_provider_failure_before_tool_activity() {
        let directory =
            std::env::temp_dir().join(format!("neenee-retry-test-{}", uuid::Uuid::new_v4()));
        let session = Arc::new(SessionStore::for_path(directory.join("session.json")));
        let history = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let goal_service =
            GoalService::new(GoalStore::open_in_memory_blocking().expect("in-memory goal store"));
        let agent = Arc::new(Agent::new(
            Arc::new(RetryOnceProvider(AtomicUsize::new(0))),
            Vec::new(),
            AgentMode::Build,
            goal_service.clone(),
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
                goal_service,
                compaction: CompactionSettings {
                    max_chars: 100_000,
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
        let goal_service =
            GoalService::new(GoalStore::open_in_memory_blocking().expect("in-memory goal store"));
        let agent = Arc::new(Agent::new(
            Arc::new(ToolThenRetryProvider(AtomicUsize::new(0))),
            vec![Arc::new(RetryReadTool)],
            AgentMode::Build,
            goal_service.clone(),
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
                goal_service,
                compaction: CompactionSettings {
                    max_chars: 100_000,
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
