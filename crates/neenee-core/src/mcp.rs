//! Shared configuration schema for MCP servers.
//!
//! Lives in `neenee-core` for the same reason `WebSearchConfig` does: both the
//! app-layer `Config` (which owns the `[mcp]` table) and the MCP loader (in
//! `neenee-tools`) need the type, and `neenee-store` does not depend on
//! `neenee-tools`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One MCP server entry from the `[mcp.<name>]` table of `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub command: Vec<String>,
    pub environment: HashMap<String, String>,
    pub enabled: bool,
    pub read_only: bool,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            command: Vec::new(),
            environment: HashMap::new(),
            enabled: true,
            read_only: false,
        }
    }
}

/// Runtime status reported by the MCP loader for each configured server.
///
/// Lives in `neenee-core` (alongside [`McpServerConfig`]) so the TUI can
/// consume it without depending on `neenee-tools`: the loader in
/// `neenee-tools::mcp` produces these, and frontends render them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpConnectionStatus {
    /// A connection attempt is in flight (background connect at startup, or a
    /// reconnect). The server is not usable yet; the model sees none of its
    /// tools until it transitions to `Connected`.
    Connecting,
    Connected { tools: usize },
    Disabled,
    Failed(String),
}

impl std::fmt::Display for McpConnectionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connecting => write!(f, "connecting…"),
            Self::Connected { tools } => write!(f, "connected ({} tools)", tools),
            Self::Disabled => write!(f, "disabled"),
            Self::Failed(error) => write!(f, "failed: {}", error),
        }
    }
}
