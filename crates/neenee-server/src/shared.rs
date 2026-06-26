//! Process-level shared state: singletons constructed once at bootstrap and
//! borrowed by every session. This is the `neenee-code` `main.rs:96-278`
//! bootstrap block (config + provider holder + skills registry + MCP + embedding
//! + repeat store) lifted into a reusable type, so a server process and a TUI
//! process build the same foundation.

use std::sync::{Arc, RwLock};

use neenee_agent::orchestration::ProxyProvider;
use neenee_agent::skills::SkillRegistry;
use neenee_core::Provider;
use neenee_store::{
    RepeatStore,
    config::Config,
    embedding,
    provider_usage::ProviderUsage,
};
use tokio::sync::RwLock as AsyncRwLock;

/// Process-level singletons shared across every session in a server process.
///
/// Construct once at startup; hand clones of the `Arc`s to each
/// [`crate::SessionHandle`]. Nothing here is session-scoped — the provider
/// holder, skills registry, MCP catalog, and embedding index are global; the
/// per-session `Agent` is assembled from them.
///
/// This is the home for the bootstrap half of `neenee-code`'s `main.rs`. The
/// other half (session loading + agent construction + thread binding) lives in
/// [`crate::SessionRegistry::create_session`].
pub struct SharedState {
    /// The live config. Mutated by provider/favorite/default switches and saved.
    pub config: AsyncRwLock<Config>,
    /// Provider switched at runtime through `RwLock` so `/provider` can hot-swap
    /// without rebuilding the agent. Wrapped in `ProxyProvider` inside the
    /// agent; sessions hold the raw holder so they can rebind.
    pub provider_holder: Arc<RwLock<Arc<dyn Provider>>>,
    /// Shared skills registry — hourly background refresh re-scans sources.
    pub skills_registry: Arc<SkillRegistry>,
    /// Per-model usage telemetry (recency signal for the picker).
    pub provider_usage: AsyncRwLock<ProviderUsage>,
    /// Project embedding index for `/search`.
    pub embedding_store: Arc<AsyncRwLock<embedding::EmbeddingStore>>,
    /// Durable store for `/repeat` cron jobs.
    pub repeat_store: RepeatStore,
    /// MCP server name → connection status snapshot (read-only mirror; the
    /// live tool holder lives inside each session's `Agent`).
    pub mcp_statuses: Arc<RwLock<Vec<(String, neenee_core::McpConnectionStatus)>>>,
    /// The proxy provider assembled from `provider_holder`. Each session's
    /// `Agent` shares this so a `/provider` switch propagates instantly.
    pub agent_provider: Arc<ProxyProvider>,
    /// Project root sessions are pinned to. Side sessions fork under it.
    pub project_root: std::path::PathBuf,
}

impl SharedState {
    /// Construct the shared state from a loaded config + project root.
    ///
    /// This is the `main.rs:96-278` bootstrap sequence (minus session-specific
    /// agent construction): resolve the initial provider, wrap it in the proxy,
    /// load the skills registry, open the embedding index + repeat store. MCP
    /// tools are loaded by the caller and threaded in via
    /// [`Self::set_mcp_statuses`] because their load is async and the caller
    /// usually wants to spawn the background refresh loop alongside.
    pub async fn new(config: Config, project_root: std::path::PathBuf) -> Result<Self, String> {
        use neenee_store::paths;

        let initial_provider: Arc<dyn Provider> =
            neenee_agent::catalog::build_provider_for(&config, neenee_agent::catalog::default_provider_id(&config));
        let provider_holder = Arc::new(RwLock::new(initial_provider));
        let agent_provider = Arc::new(ProxyProvider::new(provider_holder.clone()));

        let skills_registry = Arc::new(SkillRegistry::load(&config.skills).await);

        let repeat_store = RepeatStore::open(paths::get().repeat_db())
            .await
            .map_err(|e| e.to_string())?;

        let embedding_store = Arc::new(AsyncRwLock::new(
            embedding::EmbeddingStore::open(
                paths::get().project_embeddings(&project_root),
                Arc::new(embedding::MockEmbeddingProvider::new(384)),
            )
            .await
            .map_err(|e| e.to_string())?,
        ));

        let provider_usage = ProviderUsage::load();

        Ok(Self {
            config: AsyncRwLock::new(config),
            provider_holder,
            skills_registry,
            provider_usage: AsyncRwLock::new(provider_usage),
            embedding_store,
            repeat_store,
            mcp_statuses: Arc::new(RwLock::new(Vec::new())),
            agent_provider,
            project_root,
        })
    }

    /// Record the MCP connection statuses after the caller has loaded the MCP
    /// tools and spawned the background reconnect loop.
    pub fn set_mcp_statuses(&self, statuses: Vec<(String, neenee_core::McpConnectionStatus)>) {
        *self
            .mcp_statuses
            .write()
            .unwrap_or_else(|e| e.into_inner()) = statuses;
    }
}

// The models.dev `DynamicCatalog` refresh path (`main.rs:118-122`) is not yet
// moved here — it stays in `neenee-code`'s `main` until the bootstrap sequence
// is fully lifted. It will be invoked from `SharedState::new` in a follow-up.
