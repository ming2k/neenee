//! Parallel hosted search via its MCP endpoint
//! (`https://search.parallel.ai/mcp`). Anonymous use works; an optional
//! `Authorization: Bearer <key>` header routes through the caller's own quota.
//! Like Exa it returns a pre-rendered text blob, passed through verbatim.

use super::{SearchProvider, cap_output, mcp_tools_call};
use async_trait::async_trait;

const PARALLEL_URL: &str = "https://search.parallel.ai/mcp";
const PARALLEL_TOOL: &str = "web_search";

pub(crate) struct ParallelProvider {
    pub api_key: Option<String>,
}

#[async_trait]
impl SearchProvider for ParallelProvider {
    fn name(&self) -> &'static str {
        "Parallel"
    }

    async fn search(&self, client: &reqwest::Client, query: &str) -> Result<String, String> {
        let mut headers: Vec<(String, String)> =
            vec![("User-Agent".to_string(), "neenee/0.1".to_string())];
        if let Some(key) = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            headers.push(("Authorization".to_string(), format!("Bearer {key}")));
        }
        let text = mcp_tools_call(
            client,
            PARALLEL_URL,
            PARALLEL_TOOL,
            serde_json::json!({
                "objective": query,
                "search_queries": [query],
            }),
            &headers,
        )
        .await?;
        Ok(cap_output(&format!(
            "Search results for '{}' (via Parallel):\n\n{}",
            query, text
        )))
    }
}
