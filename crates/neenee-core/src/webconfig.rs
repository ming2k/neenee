//! Shared configuration schema for the web tools.
//!
//! Lives in `neenee-core` (not `neenee-tools`) because both the app-layer
//! `Config` (which owns the `[websearch]` table) and the tool implementations
//! need the type, and we do not want `neenee-store` to depend on
//! `neenee-tools`. It is plain serialisable data; the tool implementations
//! live in `neenee-tools::web` and read this struct as input.

use serde::{Deserialize, Serialize};

/// User-tunable web-tool configuration, deserialized from the `[websearch]`
/// table of `config.toml`. All fields default sensibly, so a `config.toml`
/// with no `[websearch]` table (or a partially specified one) is valid.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSearchConfig {
    /// Primary search backend. One of: `"exa"` (default; hosted MCP, anonymous
    /// or `exa_api_key`), `"parallel"` (hosted MCP), `"duckduckgo"` (best-effort
    /// scraping, frequently blocked), `"searxng"` (self-hosted, keyless), or
    /// `"tavily"` (hosted API, requires `tavily_api_key`).
    pub provider: String,
    /// Fallback backend tried when `provider` fails. Empty string disables it.
    /// Default `"parallel"`.
    pub fallback: String,
    /// Optional proxy URL applied to both `webfetch` and `websearch`.
    /// Supports `http://`, `https://`, `socks5://`, and `socks5h://`. Takes
    /// precedence over the `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` env vars.
    pub proxy: Option<String>,
    /// Per-request timeout in seconds (default 20).
    pub timeout_secs: u64,
    /// Exa API key (optional; anonymous use works without it).
    pub exa_api_key: Option<String>,
    /// Parallel Search API key (optional; anonymous use works without it).
    pub parallel_api_key: Option<String>,
    /// SearXNG JSON search endpoint, e.g. `http://localhost:8080/search`.
    /// Required when `provider = "searxng"`.
    pub searxng_url: Option<String>,
    /// Tavily API key. Required when `provider = "tavily"`.
    pub tavily_api_key: Option<String>,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: "exa".to_string(),
            fallback: "parallel".to_string(),
            proxy: None,
            timeout_secs: 20,
            exa_api_key: None,
            parallel_api_key: None,
            searxng_url: None,
            tavily_api_key: None,
        }
    }
}
