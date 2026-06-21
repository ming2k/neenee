use crate::tui::start_tui;
use neenee_agent::catalog;
use neenee_agent::orchestration::{
    compact_turn_history, emit_goal_updated, refresh_agent_goal, send_compaction,
    send_harness_state, start_goal_loop, start_interactive_turn, CompactionSettings,
    InteractiveTurnContext, LoopRunContext, MidTurnCompactionGate, ProxyProvider,
    RelayCompactionHooks, TurnInput,
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
use neenee_core::{async_trait, ProviderStreamEvent};
use neenee_core::{
    AgentMode, AgentRequest, AgentResponse, Goal, GoalService, GoalStore, McpConnectionStatus,
    McpServerInfo, Message, ModelInfo, Provider, SessionContextSnapshot, SessionOverview, Tool,
    EXPLORE,
};
use neenee_providers::MockProvider;
use neenee_store::{
    config::Config,
    embedding, lock, paths, provider_usage,
    search_tool::SearchHistoryTool,
    session::{self, discard_trailing_loop_prompts, SessionStore, UNCAPPED_ITERATIONS},
};
use neenee_tools::commands::{discover_commands, expand_command, CustomCommand};
use neenee_tools::{
    mcp::load_mcp_tools,
    project::{init_neenee_config, CreateProjectTool, InitConfigTool},
    AskUserTool, BashTool, EditFileTool, GlobTool, GrepTool, ListDirTool, ReadFileTool,
    TodoWriteTool, WebFetchTool, WebSearchTool, WriteFileTool,
};
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
        is_complete: legacy.harness_goal_completed,
        checklist: legacy.harness_goal_checklist,
    })
}

const BUILTIN_COMMANDS: &[&str] = &[
    "models",
    "mode",
    "mcp",
    "permissions",
    "auto-approve",
    "stall-threshold",
    "verify-nudge",
    "session",
    "sessions",
    "resume",
    "compact",
    "goal",
    "loop",
    "init",
    "skills",
    "skill",
    "export",
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
/// Derived from the provider catalog so the readiness signal and the actual
/// provider construction share one resolution path.
fn provider_key_status(config: &Config) -> Vec<(String, bool)> {
    catalog::build_catalog(config)
        .iter()
        .map(|entry| (entry.id.clone(), entry.key_ready()))
        .collect()
}

/// Build a render-ready snapshot of the live session for the session-context
/// modal. Pulls model info from the catalog, tools/permissions/skills from the
/// agent, and MCP per-server tool names by matching the `mcp__<server>__*`
/// naming convention against the agent's installed tools.
///
/// Sent in reply to [`AgentRequest::QuerySessionContext`] and re-sent after any
/// mutation ([`AgentRequest::RevokePermission`] / [`AgentRequest::ToggleTool`])
/// so the modal always reflects the post-change state.
fn build_session_context(
    agent: &Agent,
    _skills_registry: &SkillRegistry,
    mcp_statuses: &[(String, McpConnectionStatus)],
    config: &Config,
) -> SessionContextSnapshot {
    let provider_id = catalog::default_provider_id(config).to_string();
    let model = catalog::resolved_model_name(config, &provider_id);

    // Catalog entry carries the authoritative display metadata; fall back to
    // the raw model id / empty when the provider isn't a known catalog entry.
    let entry = catalog::build_catalog(config)
        .into_iter()
        .find(|e| e.id == provider_id);
    let display_name = entry
        .as_ref()
        .map(|e| e.name.clone())
        .unwrap_or_else(|| model.clone());
    let description = entry
        .as_ref()
        .map(|e| e.description.clone())
        .unwrap_or_default();
    let context_window = entry.as_ref().map(|e| e.context_window()).unwrap_or(0);
    let api_key_ready = entry.as_ref().map(|e| e.key_ready()).unwrap_or(false);

    let model_info = ModelInfo {
        provider: provider_id,
        capabilities: derive_capabilities(&model),
        display_name,
        model,
        context_window,
        api_key_ready,
        description,
    };

    let tools = agent.snapshot_tools();
    let permissions = agent.allowed_tools_structured();
    let skills = agent.snapshot_skills();

    // Per-server tool names: match the agent's installed tools by their
    // `mcp__<server>__<tool>` naming convention. The status enum only carries a
    // count, so this is where the per-server list is reconstructed.
    let mcp = mcp_statuses
        .iter()
        .map(|(name, status)| {
            let prefix = format!("mcp:{}", name);
            let tool_names: Vec<String> = tools
                .iter()
                .filter(|t| t.source == prefix)
                .map(|t| t.name.clone())
                .collect();
            let (connected, disabled, failure) = match status {
                McpConnectionStatus::Connected { .. } => (true, false, None),
                McpConnectionStatus::Disabled => (false, true, None),
                McpConnectionStatus::Failed(reason) => (false, false, Some(reason.clone())),
            };
            McpServerInfo {
                name: name.clone(),
                connected,
                disabled,
                failure,
                tool_names,
            }
        })
        .collect();

    SessionContextSnapshot {
        model: model_info,
        tools,
        permissions,
        skills,
        mcp,
    }
}

/// Heuristic model-capability hints for the session modal. Per-model capability
/// data is resolved from the [`neenee_core::model`] registry; the harness
/// depends on tool calling for every provider, so it is always advertised.
fn derive_capabilities(model: &str) -> Vec<String> {
    let mut caps = vec!["tool calling".to_string()];
    if neenee_core::resolve_model(model).reasoning {
        caps.push("reasoning".to_string());
    }
    caps
}

#[derive(Debug)]
enum StartupMode {
    Fresh,
    Resume(Option<String>),
    Picker,
    Doctor,
}

fn parse_args(args: Vec<String>) -> (StartupMode, Option<std::path::PathBuf>, bool) {
    let mut iter = args.into_iter().peekable();
    let mut project: Option<std::path::PathBuf> = None;
    let mut auto_approve = false;
    let mut rest = Vec::new();
    while let Some(arg) = iter.next() {
        if arg == "--project" {
            project = iter.next().map(std::path::PathBuf::from);
        } else if let Some(value) = arg.strip_prefix("--project=") {
            project = Some(std::path::PathBuf::from(value));
        } else if arg == "--auto-approve" {
            auto_approve = true;
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
                "Unknown command '{}'. Usage:\n  neenee              start a fresh session\n  neenee resume [id]  resume a session (picker when no id)\n  neenee doctor       verify stored session integrity\n\nOptions:\n  --project <path>    operate on the project at <path>\n  --auto-approve      bypass write-tool permission prompts for this session",
                cmd
            );
            std::process::exit(2);
        }
    };
    (mode, project, auto_approve)
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
    let (startup, project_override, auto_approve_at_start) =
        parse_args(std::env::args().skip(1).collect());
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
    // sub-agents cannot recurse and inherit the live provider. It binds the
    // EXPLORE profile (read-only / non-interactive / non-recursive).
    let task_tool = Arc::new(TaskTool::new(agent_provider.clone(), tools.clone(), &EXPLORE));
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

    // Wire the `[agent]` config table: stall detector threshold (0
    // disables) and verify hard-nudge toggle. Both default to sensible
    // values when the table is absent, so this is a no-op for the common
    // case.
    agent.set_stall_threshold(config.agent.stall_threshold);
    agent.set_verify_nudge_enabled(config.agent.verify_nudge_enabled);

    // Tie the agent and its goal persistence to this session/thread.
    let thread_id = session.id().await;
    agent.set_thread_id(&thread_id);
    if goal_service.get_goal(&thread_id).await?.is_none() {
        if let Some(goal) = load_legacy_goal_from_config() {
            let _ = goal_service.set_goal(&thread_id, &goal.objective).await;
        }
    }
    refresh_agent_goal(&agent, &goal_service, &thread_id).await;

    // Restore the active plan path + plan progress from the persisted
    // session so resume re-enters Build mode with the "you are implementing
    // X" hint intact and the sticky panel showing the same sections. If the
    // session was in Plan mode when last saved both will be None (plan_enter
    // clears them), so there is nothing to restore.
    if let Some(plan_path) = session.active_plan_path().await {
        agent.set_active_plan_path(Some(plan_path));
    }
    if let Some(progress) = session.plan_progress().await {
        agent.set_plan_progress(Some(progress));
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
                    match parts[0] {
                        "/provider" => {
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
                                // `set_mode(Plan)` clears the active plan
                                // path and progress in-memory; mirror that
                                // to the session so a resume after a manual
                                // mode switch does not resurrect a stale
                                // plan hint or stale progress.
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
                                let agent_progress = agent.plan_progress();
                                let stored_progress = session.plan_progress().await;
                                if agent_progress != stored_progress {
                                    if let Err(err) =
                                        session.set_plan_progress(agent_progress).await
                                    {
                                        let _ = resp_tx.send(AgentResponse::Error(format!(
                                            "could not persist plan progress: {err}"
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
                        "/plan" => {
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
                        "/verify" => {
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
                        "/auto-approve" => {
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
                        "/stall-threshold" => {
                            // /stall-threshold        → show current value
                            // /stall-threshold N      → set to N (0 disables)
                            // /stall-threshold default → restore the config default
                            match parts.get(1).map(|s| s.trim()).filter(|s| !s.is_empty()) {
                                None => {
                                    let current = agent.get_stall_threshold();
                                    let label = if current == 0 {
                                        "0 (detection disabled)".to_string()
                                    } else {
                                        current.to_string()
                                    };
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "Stall threshold: {label}. Use `/stall-threshold N` \
                                         to set (0 disables), `/stall-threshold default` to \
                                         reset to the config value ({}).",
                                        config.agent.stall_threshold
                                    )));
                                }
                                Some("default") => {
                                    let value = config.agent.stall_threshold;
                                    agent.set_stall_threshold(value);
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "Stall threshold reset to config default ({value})."
                                    )));
                                }
                                Some(raw) => {
                                    let parsed = raw.parse::<usize>();
                                    let value = match parsed {
                                        Ok(v) => v,
                                        Err(_) => {
                                            let _ = resp_tx.send(AgentResponse::Error(format!(
                                                "Unknown value '{raw}'. Use \
                                                     `/stall-threshold N` (non-negative integer) \
                                                     or `/stall-threshold default`.",
                                            )));
                                            continue;
                                        }
                                    };
                                    agent.set_stall_threshold(value);
                                    let label = if value == 0 {
                                        "0 (detection disabled)".to_string()
                                    } else {
                                        value.to_string()
                                    };
                                    let _ = resp_tx.send(AgentResponse::Text(format!(
                                        "Stall threshold set to {label}."
                                    )));
                                }
                            }
                        }
                        "/verify-nudge" => {
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
                            } else if rest.starts_with("edit ") {
                                let new_objective = rest.strip_prefix("edit ").unwrap_or("").trim();
                                if new_objective.is_empty() {
                                    let _ = resp_tx.send(AgentResponse::Error(
                                        "Usage: /goal edit <new objective>".to_string(),
                                    ));
                                } else {
                                    match goal_service
                                        .update_objective(&thread_id, new_objective)
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
                            } else if rest == "pause"
                                || rest == "resume"
                                || rest.starts_with("budget ")
                            {
                                // Pre-ADR-0010 subcommands removed: the status
                                // machine and token budget no longer exist.
                                let _ = resp_tx.send(AgentResponse::Error(
                                    "The /goal pause, /goal resume, and /goal budget subcommands were removed: \
                                     the goal no longer carries a status machine or token budget. \
                                     Use /goal <objective>, /goal edit, /goal done, or /goal clear."
                                        .to_string(),
                                ));
                            } else {
                                // Set a new goal.
                                match goal_service.set_goal(&thread_id, rest).await {
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
                            // Everything after "/loop " — preserves spaces inside
                            // the objective text so `/loop fix the bug` carries the
                            // full sentence as the goal.
                            let raw_args = cmd.strip_prefix("/loop").unwrap_or("").trim();
                            if raw_args == "stop" {
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
                            if raw_args == "status" {
                                let running = ctt_clone.read().await.is_some();
                                let status = if running { "running" } else { "idle" };
                                let checkpoint = session.checkpoint().await;
                                let detail = checkpoint
                                    .map(|checkpoint| {
                                        let budget =
                                            if checkpoint.max_iterations == UNCAPPED_ITERATIONS {
                                                "uncapped".to_string()
                                            } else {
                                                // Legacy pre-ADR-0009 checkpoint: show the
                                                // original finite budget for traceability.
                                                format!("cap {}", checkpoint.max_iterations)
                                            };
                                        format!(
                                            "{} · iteration {} ({}) for {}",
                                            checkpoint.status,
                                            checkpoint.iteration,
                                            budget,
                                            checkpoint.goal,
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
                            if raw_args == "resume" {
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

                                match goal_service.set_goal(&thread_id, &checkpoint.goal).await {
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
                                    "Resuming goal loop at iteration {}{}.",
                                    start_iteration,
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
                                )
                                .await;
                                continue;
                            }

                            // Reject the legacy numeric form `/loop <N>`: the
                            // iteration cap was removed (ADR-0009). A pure-number
                            // argument is unambiguously the old syntax, so route
                            // the user to the new commands instead of silently
                            // treating the number as a goal.
                            if raw_args.parse::<usize>().is_ok() {
                                let _ = resp_tx.send(AgentResponse::Error(
                                    "The `/loop <N>` form was removed: the loop now runs unbounded. \
                                     Use `/loop` to start on the current goal, `/loop <objective>` \
                                     to set a fresh goal and start, or `/loop stop` to interrupt."
                                        .to_string(),
                                ));
                                continue;
                            }

                            // `/loop <content>` sets a fresh goal from the content
                            // and starts an uncapped loop on it; `/loop` (empty)
                            // starts an uncapped loop on the existing goal. Either
                            // way the loop runs until the model emits the completion
                            // marker, the user interrupts, or an error aborts.
                            let objective = if raw_args.is_empty() {
                                match goal_service.active_goal(&thread_id).await {
                                    Ok(Some(goal)) => {
                                        let _ = resp_tx.send(AgentResponse::Text(format!(
                                            "Starting autonomous loop on existing goal: {}",
                                            goal.objective
                                        )));
                                        goal.objective
                                    }
                                    Ok(None) => {
                                        let _ = resp_tx.send(AgentResponse::Error(
                                            "No active goal. Set one with /goal <objective>, \
                                             then /loop, or start a fresh loop with \
                                             /loop <objective>."
                                                .to_string(),
                                        ));
                                        continue;
                                    }
                                    Err(error) => {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                        continue;
                                    }
                                }
                            } else {
                                match goal_service.set_goal(&thread_id, raw_args).await {
                                    Ok(goal) => {
                                        agent.set_goal(goal.clone());
                                        emit_goal_updated(&resp_tx, &goal);
                                        goal.objective
                                    }
                                    Err(error) => {
                                        let _ = resp_tx.send(AgentResponse::Error(error));
                                        continue;
                                    }
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
                                objective,
                                1,
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
                        "/export" => {
                            let messages = history.lock().await.clone();
                            let session_id = session.id().await;
                            let provider_id = agent.provider.provider_id();
                            let model_name = agent.provider.model();
                            let mode = match agent.get_mode() {
                                AgentMode::Build => "build",
                                AgentMode::Plan => "plan",
                            };
                            let goal = agent.get_goal();
                            let plan_path = agent.active_plan_path();
                            let markdown = crate::tui::export::format_export_markdown(
                                crate::tui::export::ExportContext {
                                    session_id: &session_id,
                                    provider: &provider_id,
                                    model: &model_name,
                                    mode,
                                    goal: goal.as_ref(),
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
                                /provider — Select an LLM provider\n\
                                /mode     — Show or switch mode (build, plan)\n\
                                /mcp      — Show configured MCP server status\n\
                                /compact  — Compact older complete turns now\n\
                                /clear    — Clear the conversation history\n\
                                                                /permissions [clear] — Show or clear always-allowed tool rules
                                /auto-approve [on|off] — Toggle bypassing write-tool permission prompts
                                /stall-threshold [N] — Show or set the agent stall threshold (0 disables)
                                /verify-nudge [on|off] — Toggle the verify-plan hard nudge at turn end
                                /search <query> — Semantic search over the project's session history
\n\
                                /session [status|list|resume|fork|open|new] — Manage durable sessions\n\
                                /sessions — Browse past sessions\n\
                                /resume [id] — Resume the most recent or selected session\n\
                                /goal     — Set, inspect, complete, or clear the active goal\n\
                                /loop [objective|resume|status|stop] — Run an uncapped autonomous goal loop\n\
                                /init [path] — Initialize a .neenee/ config tree\n\
                                /skills [list|reload] — List or reload available skills\n\
                                /skill <name> — Load a skill by name\n\
                                /export   — Export this conversation to the clipboard as Markdown\n\
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

fn format_goal_status(goal: &Goal) -> String {
    use neenee_core::GoalChecklistStatus;

    let mut lines = Vec::new();
    lines.push(format!(
        "Goal [{}]: {}",
        if goal.is_complete {
            "complete"
        } else {
            "active"
        },
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

    lines.join("\n")
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
            is_complete: false,
            checklist: vec![neenee_core::GoalChecklistItem {
                content: "verify".to_string(),
                status: neenee_core::GoalChecklistStatus::InProgress,
            }],
        };

        let status = format_goal_status(&goal);
        assert!(status.contains("Goal [active]: ship"));
        assert!(status.contains("Plans (0/1):"));
        assert!(status.contains("◎ verify  (in progress)"));
    }

    #[test]
    fn goal_status_shows_complete_state() {
        // Post-ADR-0010: no budget bar, no time line. The state label is
        // the only thing beyond objective + checklist.
        let goal = Goal {
            objective: "ship".to_string(),
            is_complete: true,
            checklist: Vec::new(),
        };
        let status = format_goal_status(&goal);
        assert!(status.contains("Goal [complete]: ship"));
        assert!(!status.contains("Budget"));
        assert!(!status.contains("tokens"));
        assert!(!status.contains("time"));
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
