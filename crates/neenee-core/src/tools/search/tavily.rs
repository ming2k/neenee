//! Tavily backend — hosted search REST API (requires an API key). A reliable
//! drop-in for users who want a key-based hosted backend rather than the
//! anonymous Exa/Parallel MCP endpoints.

use super::{format_results, SearchProvider, SearchResult};
use async_trait::async_trait;

const TAVILY_URL: &str = "https://api.tavily.com/search";

pub(crate) struct TavilyProvider {
    pub api_key: Option<String>,
}

#[async_trait]
impl SearchProvider for TavilyProvider {
    fn name(&self) -> &'static str {
        "Tavily"
    }

    async fn search(&self, client: &reqwest::Client, query: &str) -> Result<String, String> {
        let key = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "Tavily backend selected but `[websearch].tavily_api_key` is not set.".to_string()
            })?;
        let response = client
            .post(TAVILY_URL)
            .json(&serde_json::json!({
                "api_key": key,
                "query": query,
                "search_depth": "basic",
                "include_answer": false,
                "max_results": 10
            }))
            .send()
            .await
            .map_err(|e| format!("Tavily request failed: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "Tavily returned HTTP {status} (check tavily_api_key): {}",
                body.chars().take(300).collect::<String>()
            ));
        }
        let body = response
            .text()
            .await
            .map_err(|e| format!("Failed to read Tavily response: {e}"))?;
        let json: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| format!("Tavily returned invalid JSON: {e}"))?;
        let results = json
            .get("results")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(parse_item)
            .take(10)
            .collect();
        Ok(format_results(query, "Tavily", results))
    }
}

fn parse_item(item: &serde_json::Value) -> Option<SearchResult> {
    let url = item.get("url")?.as_str()?.to_string();
    let title = item
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let snippet = item
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if url.is_empty() || title.trim().is_empty() {
        return None;
    }
    Some(SearchResult {
        title,
        url,
        snippet,
    })
}
