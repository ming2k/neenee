//! MCP tool catalog — a [`DynamicCatalog`] that periodically reconnects MCP
//! servers and re-discovers their tools.
//!
//! On each refresh (every 10 minutes), every server's connection is reset and
//! re-established, and `tools/list` is re-run. The refreshed tool list replaces
//! the agent's live MCP tools via the shared holder — so new tools a server
//! exposes appear without a restart, and a recovered server is transparently
//! reconnected. Individual tool calls also auto-reconnect on failure (see
//! `McpTool::call`), so this periodic refresh is a belt-and-suspenders recovery
//! path.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use neenee_core::{DynamicCatalog, Tool};

/// A [`DynamicCatalog`] for MCP server tools. Holds the reconnect-capable
/// server handles from the initial `load_mcp_tools` call and a shared holder
/// into the agent's live tool list.
pub struct McpCatalog {
    servers: Vec<Arc<neenee_tools::mcp::McpServer>>,
    holder: Arc<RwLock<Vec<Arc<dyn Tool>>>>,
}

impl McpCatalog {
    pub fn new(
        servers: Vec<Arc<neenee_tools::mcp::McpServer>>,
        holder: Arc<RwLock<Vec<Arc<dyn Tool>>>>,
    ) -> Self {
        Self { servers, holder }
    }
}

impl DynamicCatalog for McpCatalog {
    fn id(&self) -> &'static str {
        "mcp"
    }

    async fn refresh(&self) -> Result<(), String> {
        if self.servers.is_empty() {
            return Ok(());
        }
        let (tools, statuses) = neenee_tools::mcp::refresh_mcp_tools(&self.servers).await;
        let connected = statuses
            .iter()
            .filter(|(_, s)| matches!(s, neenee_core::mcp::McpConnectionStatus::Connected { .. }))
            .count();
        tracing::info!(
            servers = self.servers.len(),
            connected,
            tools = tools.len(),
            "MCP refresh complete"
        );
        if let Ok(mut guard) = self.holder.write() {
            *guard = tools;
        }
        Ok(())
    }

    fn refresh_period(&self) -> Duration {
        Duration::from_secs(10 * 60)
    }
}
