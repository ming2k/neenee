//! Live MCP runtime state — the single source of truth for which configured
//! servers are connected, their per-server tools, and their connection status.
//!
//! At startup [`McpRuntime::connect_all`] connects every enabled `[mcp.<name>]`
//! server and seeds the agent's shared tool holder. Thereafter three async
//! mutators keep it live:
//!
//! - [`McpRuntime::set_enabled`] — the `/mcp` modal's `Space` toggle: connect or
//!   disconnect one server for the session (config.toml is not rewritten).
//! - [`McpRuntime::reconnect`] — the modal's `r` action: re-establish one
//!   server's connection on demand.
//! - [`McpRuntime::refresh_all`] — the periodic [`crate::mcp_catalog::McpCatalog`]
//!   loop: reconnect every server.
//!
//! Every mutation rebuilds the agent's tool holder (the union of all live
//! servers' tools) and updates a synchronously-readable status table, so the
//! session-context snapshot ([`crate::session_view::build_session_context`])
//! always reflects the current state.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use neenee_core::Tool;
use neenee_core::mcp::{McpConnectionStatus, McpServerConfig};
use neenee_tools::mcp::{McpServer, connect_server, reconnect_server};
use tokio::sync::Mutex;

/// One configured server's live state. `server` is `None` while disabled or
/// when the last connect failed; `tools` is the server's current adapters
/// (empty unless connected).
struct McpEntry {
    name: String,
    server: Option<Arc<McpServer>>,
    tools: Vec<Arc<dyn Tool>>,
    status: McpConnectionStatus,
}

pub struct McpRuntime {
    /// All configured servers (`[mcp.*]`), by name — the source of truth for
    /// re-enabling a disabled server, which has no live handle to clone from.
    configs: HashMap<String, McpServerConfig>,
    /// Per-server live state, name-sorted. Behind an async mutex because every
    /// mutator performs network I/O while holding it, which serializes a user
    /// toggle against the background refresh loop.
    entries: Mutex<Vec<McpEntry>>,
    /// Synchronously-readable status table (name → status, name-sorted), kept in
    /// step with `entries`. The session-context snapshot is built from a sync
    /// context, so it reads this rather than the async `entries` mutex.
    statuses: RwLock<Vec<(String, McpConnectionStatus)>>,
    /// The agent's shared MCP-tools holder. Rebuilt (union of every live entry's
    /// tools) on any change so the model sees exactly the connected servers.
    holder: Arc<RwLock<Vec<Arc<dyn Tool>>>>,
}

impl McpRuntime {
    /// Connect every enabled configured server and seed `holder` with their
    /// tools. Disabled servers are recorded as such without a connection.
    pub async fn connect_all(
        configs: HashMap<String, McpServerConfig>,
        holder: Arc<RwLock<Vec<Arc<dyn Tool>>>>,
    ) -> Self {
        let mut names: Vec<String> = configs.keys().cloned().collect();
        names.sort();

        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            let config = &configs[&name];
            if !config.enabled {
                entries.push(McpEntry {
                    name,
                    server: None,
                    tools: Vec::new(),
                    status: McpConnectionStatus::Disabled,
                });
                continue;
            }
            entries.push(connect_entry(name, config).await);
        }

        let runtime = Self {
            configs,
            entries: Mutex::new(entries),
            statuses: RwLock::new(Vec::new()),
            holder,
        };
        {
            let entries = runtime.entries.lock().await;
            runtime.publish(&entries);
        }
        runtime
    }

    /// A name-sorted snapshot of every configured server's connection status,
    /// readable synchronously (for the session-context snapshot).
    pub fn statuses_snapshot(&self) -> Vec<(String, McpConnectionStatus)> {
        self.statuses
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Enable or disable one server for the live session. Enabling connects it
    /// (or is a no-op when already connected); disabling drops its tools and
    /// closes the connection. Returns `Ok(())` once applied, or `Err` when the
    /// name is not configured.
    pub async fn set_enabled(&self, name: &str, enabled: bool) -> Result<(), String> {
        let Some(config) = self.configs.get(name).cloned() else {
            return Err(format!("MCP server '{name}' is not configured."));
        };
        let mut entries = self.entries.lock().await;
        let Some(entry) = entries.iter_mut().find(|e| e.name == name) else {
            return Err(format!("MCP server '{name}' is not configured."));
        };

        if enabled {
            if entry.server.is_some() {
                return Ok(()); // already connected
            }
            *entry = connect_entry(name.to_string(), &config).await;
        } else {
            // Dropping the handle kills the child process (kill_on_drop).
            entry.server = None;
            entry.tools.clear();
            entry.status = McpConnectionStatus::Disabled;
        }
        self.publish(&entries);
        Ok(())
    }

    /// Re-establish one enabled server's connection from config, re-discovering
    /// its tools. A no-op for a disabled server. Returns `Err` when the name is
    /// not configured.
    pub async fn reconnect(&self, name: &str) -> Result<(), String> {
        let Some(config) = self.configs.get(name).cloned() else {
            return Err(format!("MCP server '{name}' is not configured."));
        };
        let mut entries = self.entries.lock().await;
        let Some(entry) = entries.iter_mut().find(|e| e.name == name) else {
            return Err(format!("MCP server '{name}' is not configured."));
        };
        if matches!(entry.status, McpConnectionStatus::Disabled) {
            return Ok(());
        }
        match &entry.server {
            // Connected: reset + re-discover through the existing handle.
            Some(server) => {
                let (tools, status) = reconnect_server(server).await;
                entry.tools = tools;
                entry.status = status;
            }
            // Failed earlier (no live handle): try a fresh connect.
            None => *entry = connect_entry(name.to_string(), &config).await,
        }
        self.publish(&entries);
        Ok(())
    }

    /// Reconnect every enabled server (the periodic catalog refresh). Disabled
    /// servers stay disabled.
    pub async fn refresh_all(&self) {
        let mut entries = self.entries.lock().await;
        for entry in entries.iter_mut() {
            if matches!(entry.status, McpConnectionStatus::Disabled) {
                continue;
            }
            match &entry.server {
                Some(server) => {
                    let (tools, status) = reconnect_server(server).await;
                    entry.tools = tools;
                    entry.status = status;
                }
                None => {
                    if let Some(config) = self.configs.get(&entry.name).cloned() {
                        let name = entry.name.clone();
                        *entry = connect_entry(name, &config).await;
                    }
                }
            }
        }
        self.publish(&entries);
    }

    /// Whether any server is configured at all (the catalog skips its loop when
    /// none are).
    pub fn is_empty(&self) -> bool {
        self.configs.is_empty()
    }

    /// Rebuild the shared tool holder (union of live tools) and the sync status
    /// table from the current entries. Called after every mutation.
    fn publish(&self, entries: &[McpEntry]) {
        let tools: Vec<Arc<dyn Tool>> = entries
            .iter()
            .flat_map(|e| e.tools.iter().cloned())
            .collect();
        if let Ok(mut guard) = self.holder.write() {
            *guard = tools;
        }
        let statuses = entries
            .iter()
            .map(|e| (e.name.clone(), e.status.clone()))
            .collect();
        if let Ok(mut guard) = self.statuses.write() {
            *guard = statuses;
        }
    }
}

/// Connect one server from config, returning a fully-populated entry whether it
/// succeeds (`Connected`) or fails (`Failed`, no handle).
async fn connect_entry(name: String, config: &McpServerConfig) -> McpEntry {
    match connect_server(&name, config).await {
        Ok((server, tools)) => {
            let status = McpConnectionStatus::Connected { tools: tools.len() };
            McpEntry {
                name,
                server: Some(server),
                tools,
                status,
            }
        }
        Err(error) => McpEntry {
            name,
            server: None,
            tools: Vec::new(),
            status: McpConnectionStatus::Failed(error),
        },
    }
}
