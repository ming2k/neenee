use crate::tui::start_tui;
use neenee_agent::catalog;
use neenee_agent::orchestration::{
    MidTurnPruneProjectionGate, ProxyProvider, refresh_agent_pursuit, start_repeat_scheduler, turn,
};
use neenee_agent::skills::SkillRegistry;
use neenee_agent::{Agent, EnvoyTool};
use neenee_core::{
    AgentRequest, AgentResponse, CHARS_PER_TOKEN, DynamicCatalog, EXPLORE, Provider, TurnEvent,
};
use neenee_store::{
    RepeatStore,
    config::Config,
    embedding, lock, paths, provider_usage,
    session::{self, SessionStore},
};
use neenee_tools::commands::{CustomCommand, discover_commands};
#[cfg(debug_assertions)]
mod showcase;
mod tui;

use mcp_runtime::McpRuntime;
pub(crate) use neenee_server::{
    agent_loop, agent_setup, hooks, mcp_catalog, mcp_runtime, pursuits, side, startup,
};

/// This CLI's identity, handed to the engine as its opening system prompt.
/// Lives here (not in `neenee-agent`) so the engine stays identity-agnostic
/// and a different frontend could reuse it as another agent.
///
/// The identity constants + [`neenee_identity`] now live in `neenee-server`
/// (the layer that constructs agents); this binary re-exports them.
use neenee_server::{
    neenee_identity,
    startup::{BuiltinCmd, StartupMode, init_tracing, parse_args},
};

use pursuits::load_legacy_pursuit_from_config;

use agent_setup::reseed_prune_threshold;
use side::SideSession;

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::{Arc, atomic::AtomicU64};
use tokio::sync::{RwLock as AsyncRwLock, mpsc};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _tracing_guard = init_tracing();
    let (req_tx, req_rx) = mpsc::unbounded_channel::<AgentRequest>();
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

    let config = Config::load();

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
    //
    // Refresh the models.dev catalog first so the catalog-driven providers
    // (opencode-go) read a fresh model list and wire-format mapping. Best
    // effort: a failed fetch is logged and the catalog falls back to the
    // compiled-in registry, so this never blocks startup.
    let models_dev_catalog = neenee_agent::modelsdev::ModelsDevCatalog;
    if let Err(error) = models_dev_catalog.refresh().await {
        tracing::warn!(%error, "could not refresh models.dev catalog; using fallback");
    }
    neenee_agent::dynamic::spawn_refresh(models_dev_catalog);

    let initial_provider: Arc<dyn Provider> =
        catalog::build_provider_for(&config, catalog::default_provider_id(&config));

    let provider_holder = Arc::new(RwLock::new(initial_provider));
    let provider_for_task = provider_holder.clone();

    let agent_provider = Arc::new(ProxyProvider::new(provider_holder));

    // Shared skills registry for the skill tools. The background refresh loop
    // re-scans all sources (local dirs, remote repos, bundled) every hour so
    // new/updated skills appear without a restart; failed remote fetches fall
    // back to the cached copy.
    let skills_registry = Arc::new(SkillRegistry::load(&config.skills).await);
    neenee_agent::dynamic::spawn_refresh(neenee_agent::dynamic::SkillCatalog::new(
        (*skills_registry).clone(),
    ));

    // CLI: `neenee` -> fresh session; `neenee resume [id]` -> resume a session;
    // `neenee doctor` -> verify stored session integrity.
    let (startup, project_override, unattended_at_start, single_instance) =
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

    // Showcase: render a single UI component standalone. No agent, session, or
    // network — just the component's model + renderer on a live terminal. Early
    // return so none of the agent/session plumbing below runs.
    #[cfg(debug_assertions)]
    if let StartupMode::Showcase(component) = &startup {
        return showcase::run(component);
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
        #[cfg(debug_assertions)]
        StartupMode::Showcase(_) => unreachable!("showcase returns before this match"),
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

    // Built-in tools self-register via `inventory` (each tool carries a
    // `register_tool!` submission at its definition site) and are collected
    // here from a single opaque context. Tools that need runtime state (the web
    // tools' search config, the shared skill registry, the embedding index +
    // session store) pull it out of the context by type — see
    // `neenee_core::tool_registry`. Meta-tools that genuinely depend on the
    // *rest* of the toolset (the envoy dispatch `task`) cannot
    // self-register and are assembled explicitly below. MCP tools are
    // discovered at runtime from configured servers and layered on last.
    let tool_ctx = {
        let mut builder = neenee_core::ToolContextBuilder::new();
        builder.provide(config.websearch.clone());
        builder.provide(skills_registry.clone());
        builder.provide(embedding_store.clone());
        builder.provide(session.clone());
        // The abort tool needs a handle to signal the harness (request
        // channel). Provided by type so `neenee-tools` stays free of a CLI
        // dependency. See `abort.rs`.
        builder.provide(req_tx.clone());
        builder.build()
    };
    let mut toolset: neenee_core::ToolSet = neenee_core::collect_toolset(&tool_ctx);
    // MCP tools are NOT layered into the toolset here; they go into the agent's
    // dynamically-refreshable `mcp_tools` holder after Agent::new, so the
    // background McpCatalog can reconnect/re-discover them at runtime.
    // Snapshot of the shared toolset (built-in default variants) before the
    // `EnvoyTool` is layered on. A `/btw` side session (ADR-0017) rebuilds
    // its `Agent` from this same snapshot — minus its own `EnvoyTool`,
    // mirroring the envoy profile filter so a side chat can recurse no
    // further than the primary.
    let base_tools: Arc<Vec<Arc<dyn neenee_core::Tool>>> = Arc::new(toolset.default_view());
    // EnvoyTool gets the full capability set (excluding itself) so spawned
    // envoys cannot recurse and inherit the live provider. It binds the
    // EXPLORE profile (read-only / non-interactive / non-recursive).
    let envoy_tool = Arc::new(EnvoyTool::new(
        agent_provider.clone(),
        toolset.clone(),
        &EXPLORE,
    ));
    // Full-duplex (ADR-0029): capture the envoy tool's envoy registry so the
    // request loop can route a user's permission / ask_user reply down into the
    // specific live child that surfaced the request (looked up by the parent
    // tool-call id the TUI tags onto the reply). Captured before `envoy_tool`
    // is layered into the capability set.
    let envoy_registry = envoy_tool.registry();
    // Keep a typed handle so we can bind the parent's variant selection into the
    // envoy tool once the agent (which owns that selection) exists. The same
    // underlying `Arc<EnvoyTool>` is what gets layered into the toolset.
    let envoy_tool_handle = envoy_tool.clone();
    toolset.insert(envoy_tool);
    let agent = Arc::new(Agent::from_toolset(
        agent_provider,
        toolset,
        (*skills_registry).clone(),
        neenee_identity(),
    ));
    // Override axis (model): envoys are agents on the same model, so they
    // inherit the parent's tool-variant selection. The profile still owns the
    // orthogonal scope axis.
    envoy_tool_handle.bind_variant_selection(agent.variant_selection_handle());
    // Wire the per-project "always allow" allowlist so prior `Always`
    // approvals survive across sessions in this project. Best-effort: a
    // missing or unreadable permissions.json just means we re-prompt.
    agent.set_project_root(Some(project_root.clone()));
    // Seed declarative permission rules from `[permissions]` config so default
    // policies are data-driven. Runtime "Always" decisions still write to
    // permissions.json; these config rules re-apply on every start.
    agent.seed_permissions_from_config(&config.permissions.allow);
    // Connect every configured MCP server, seeding the agent's shared tool
    // holder, and start the background reconnect/re-discover loop. The runtime
    // is the live source of truth the `/mcp` modal toggles/reconnects against.
    // Individual tool calls also auto-reconnect on failure (McpTool::call).
    let mcp_runtime =
        Arc::new(McpRuntime::connect_all(config.mcp.clone(), agent.mcp_tools_holder()).await);
    neenee_agent::dynamic::spawn_refresh(crate::mcp_catalog::McpCatalog::new(mcp_runtime.clone()));
    if unattended_at_start {
        agent.set_unattended(true);
        let _ = resp_tx.send(turn(
            &session.id().await,
            TurnEvent::Text(
                "Unattended ON: write tools will execute without permission prompts.".to_string(),
            ),
        ));
    }

    let active_messages = session.model_window().await;
    let restored_messages = session.full_transcript().await;
    let history = Arc::new(tokio::sync::Mutex::new(active_messages));

    // Mid-turn context projection: when pruning is enabled, install a gate that
    // clears old tool results between tool rounds once pressure crosses the
    // prune threshold. The threshold is derived from the active model's context
    // window and re-seeded whenever the provider switches (see
    // `reseed_prune_threshold`), so it tracks the live model rather than a
    // fixed character budget.
    if config.compaction_prune {
        agent.set_context_projection_gate(Some(Arc::new(MidTurnPruneProjectionGate {
            session: session.clone(),
            prune_protect_chars: config.compaction_prune_protect_tokens * CHARS_PER_TOKEN,
        })));
        reseed_prune_threshold(&agent, &config);
    }

    // Seed per-model tool-variant selection for the startup model. Each listed
    // capability is realized by its chosen variant in the schemas sent to the
    // provider; re-seeded on provider/model switch.
    agent_setup::reseed_tool_variants(&agent, &config);

    // Wire the `[agent]` config table: the opt-in hard-stop budget and the
    // verify hard-nudge toggle. (Session review is on-demand via `/review`,
    // so it has no config to seed.) All default to sensible values when the
    // table is absent, so this is a no-op for the common case.
    agent.set_hard_stop_rounds(config.principal.hard_stop_rounds);
    agent.set_loop_review_enabled(config.principal.loop_review_enabled);

    // Lifecycle event hooks (ADR-0025): each `[[hooks]]` entry runs a shell
    // command at one lifecycle point (PreToolUse / PostToolUse / Stop / …).
    agent.set_hooks(hooks::build_hook_registry(&config.hooks));

    // Tie the agent to this session/thread and restore the durable pursuit.
    // ADR-0032: pursuit now lives on `SessionData` (via `SessionStore`), not a
    // separate SQLite db. Two legacy sources are folded in once on startup:
    //   1. pre-ADR-0032 `pursuits.db` (keyed by session id) — best-effort.
    //   2. pre-ADR-0010 config-file `harness_goal*` keys — best-effort.
    // If the session already has a pursuit (e.g. resuming a post-0032 session),
    // both legacy sources are skipped.
    let thread_id = session.id().await;
    agent.set_thread_id(&thread_id);
    if session.pursuit().await.is_none() {
        let legacy_db = paths::get().pursuits_db();
        if let Some(pursuit) =
            neenee_store::legacy_pursuit::read_legacy_pursuit(&legacy_db, &thread_id)
        {
            let _ = session.set_pursuit(Some(pursuit)).await;
        } else if let Some(pursuit) = load_legacy_pursuit_from_config() {
            let _ = session.set_pursuit(Some(pursuit)).await;
        }
    }
    refresh_agent_pursuit(&agent, &session).await;

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
    // `/btw` side-conversation state (ADR-0017). The primary turn machinery is
    // left exactly as-is; this slot peers it with an optional live side
    // session + an "active view" flag that routes `Chat` to whichever session
    // the user is currently composing into.
    let side: Arc<AsyncRwLock<Option<SideSession>>> = Arc::new(AsyncRwLock::new(None));
    let active_view_side = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let base_tools_for_side = base_tools.clone();
    let project_root_for_side = project_root.clone();

    // Initial values for TUI
    let initial_p_name = catalog::default_provider_id(&config).to_string();
    let initial_m_name = catalog::resolved_model_name(&config, &initial_p_name);

    // Spawn Agent Background Task
    let mcp_statuses_for_tui = mcp_runtime.statuses_snapshot();
    // The agent background task takes ownership of `config`; pull the TUI
    // presentation config out first so it can be handed to the TUI later.
    let tui_config = config.tui.clone();
    // Keep an Arc handle on the main thread so SessionEnd hooks (ADR-0025) can
    // fire after the TUI returns — the background task below moves `agent`.
    let agent_for_session_end = Arc::clone(&agent);
    let harness = agent_loop::Harness {
        tx: resp_tx,
        req_tx: req_tx_for_commands,
        agent,
        session: session.clone(),
        history,
        config,
        provider_usage,
        provider_holder: provider_for_task,
        skills_registry,
        envoy_registry,
        mcp_runtime,
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
        ui: Arc::new(crate::tui::clipboard::TuiClipboard),
    };
    tokio::spawn(agent_loop::run(req_rx, harness));

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
        session.clone(),
    )
    .await
    {
        Ok(history) => {
            // SessionEnd hooks (ADR-0025): observers fire on clean exit.
            agent_for_session_end.fire_session_end().await;
            let _ = Config::save_history(&history);
        }
        Err(e) => return Err(e),
    }

    Ok(())
}

#[cfg(test)]
mod tests;
