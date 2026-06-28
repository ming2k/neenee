//! Pluggable web-search backends.
//!
//! Each backend implements the `SearchProvider` trait and lives in its own
//! module (`exa`, `parallel`, `duckduckgo`, `searxng`, `tavily`). The tool layer
//! ([`crate::WebSearchTool`]) is a thin shell that delegates to a primary
//! provider and an optional fallback, both built from `[websearch]` config via
//! the `build_provider` factory. Adding a new backend is one new module + one
//! match arm in `build_provider`; the tool and the other backends never
//! change.
//!
//! Default backend is the hosted Exa MCP endpoint (`mcp.exa.ai`), used
//! keylessly and anonymously — mirroring the approach taken by other coding
//! agents. Be aware that with the default, search queries are sent to a
//! third-party service; set a different `provider` (e.g. self-hosted
//! `searxng`) in `config.toml` if that matters.

use async_trait::async_trait;
use neenee_core::WebSearchConfig;

pub mod duckduckgo;
pub mod exa;
pub mod parallel;
pub mod searxng;
pub mod tavily;

/// A single search hit. Backends that return structured results (DDG, SearXNG,
/// Tavily) parse their responses into this; backends that return a pre-rendered
/// text blob (Exa, Parallel) ignore it and return the blob directly.
#[derive(Debug, Clone)]
pub(super) struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// The plugin contract. A backend turns a query into ready-to-show text for the
/// model. Implementations own their HTTP shape, parsing, and formatting; the
/// tool layer only handles argument parsing, client/proxy setup, and fallback.
#[async_trait]
pub(crate) trait SearchProvider: Send + Sync {
    /// Human-readable label included in the result header, e.g. `"Exa"`.
    fn name(&self) -> &'static str;
    /// Run the search and return formatted text, or an error describing what
    /// went wrong (surfaced verbatim to the model/user).
    async fn search(&self, client: &reqwest::Client, query: &str) -> Result<String, String>;
}

/// Build a provider by its config name. Unknown names fall back to Exa (the
/// default) rather than erroring at construction time, so a typo never leaves
/// the tool without a working backend; misconfiguration surfaces at call time
/// from the provider that needs the missing field (e.g. SearXNG/Tavily keys).
pub(crate) fn build_provider(cfg: &WebSearchConfig, name: &str) -> Box<dyn SearchProvider> {
    match name {
        "parallel" => Box::new(parallel::ParallelProvider {
            api_key: cfg.parallel_api_key.clone(),
        }),
        "duckduckgo" | "ddg" => Box::new(duckduckgo::DdgProvider),
        "searxng" => Box::new(searxng::SearxngProvider {
            url: cfg.searxng_url.clone(),
        }),
        "tavily" => Box::new(tavily::TavilyProvider {
            api_key: cfg.tavily_api_key.clone(),
        }),
        _ => Box::new(exa::ExaProvider {
            api_key: cfg.exa_api_key.clone(),
        }),
    }
}

/// A realistic browser User-Agent, shared by the scraping-style backends.
pub(super) const MOZILLA_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Render structured results as the standard numbered list. Shared by the
/// DDG/SearXNG/Tavily backends.
pub(super) fn format_results(query: &str, source: &str, results: Vec<SearchResult>) -> String {
    if results.is_empty() {
        return format!("No results found for '{}' (via {}).", query, source);
    }
    let formatted = results
        .iter()
        .enumerate()
        .map(|(idx, result)| {
            format!(
                "{}. {}\n   {}\n   {}",
                idx + 1,
                result.title,
                result.url,
                result.snippet
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "Search results for '{}' (via {}):\n\n{}",
        query, source, formatted
    )
}

/// Guard the model's context window against huge provider payloads.
pub(super) fn cap_output(text: &str) -> String {
    let max_chars = 16_000;
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars).collect();
    format!("{truncated}\n\n[... output truncated at {max_chars} characters ...]")
}

/// Invoke a hosted MCP-style search endpoint via JSON-RPC `tools/call` and
/// extract the first `text` content block from the response. Handles both the
/// single-JSON and Server-Sent-Events (`data: {...}`) response shapes used by
/// the Exa and Parallel endpoints.
pub(super) async fn mcp_tools_call(
    client: &reqwest::Client,
    url: &str,
    tool: &str,
    arguments: serde_json::Value,
    extra_headers: &[(String, String)],
) -> Result<String, String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    });
    let mut request = client
        .post(url)
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .json(&body);
    for (name, value) in extra_headers {
        if let Ok(v) = reqwest::header::HeaderValue::from_str(value)
            && let Ok(n) = reqwest::header::HeaderName::from_bytes(name.as_bytes())
        {
            request = request.header(n, v);
        }
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("{tool} request failed: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(format!(
            "{tool} returned HTTP {status}: {}",
            text.chars().take(300).collect::<String>()
        ));
    }
    let text = response
        .text()
        .await
        .map_err(|e| format!("{tool} response read failed: {e}"))?;
    extract_mcp_text(&text).ok_or_else(|| format!("{tool} returned no content (HTTP {status})"))
}

/// Pull the first `text` block out of an MCP response, trying the whole body as
/// JSON first, then each SSE `data:` line.
fn extract_mcp_text(body: &str) -> Option<String> {
    if let Some(text) = parse_mcp_payload(body.trim()) {
        return Some(text);
    }
    for line in body.lines() {
        if let Some(rest) = line.trim().strip_prefix("data:")
            && let Some(text) = parse_mcp_payload(rest.trim())
        {
            return Some(text);
        }
    }
    None
}

fn parse_mcp_payload(payload: &str) -> Option<String> {
    if payload.is_empty() || !payload.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let content = value.get("result")?.get("content")?.as_array()?;
    for item in content {
        let is_text = item
            .get("type")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t == "text");
        if is_text && let Some(text) = item.get("text").and_then(|t| t.as_str()) {
            return Some(text.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_results_handles_empty_and_nonempty() {
        assert!(format_results("q", "SearXNG", Vec::new()).contains("No results found"));
        let r = vec![SearchResult {
            title: "T".to_string(),
            url: "https://e.com".to_string(),
            snippet: "S".to_string(),
        }];
        assert!(format_results("q", "Tavily", r).contains("1. T\n   https://e.com"));
    }

    #[test]
    fn extract_mcp_text_parses_single_json_payload() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"hello world"}]}}"#;
        assert_eq!(extract_mcp_text(body), Some("hello world".to_string()));
    }

    #[test]
    fn extract_mcp_text_parses_sse_stream() {
        let body = "event: message\ndata: {\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n\n";
        assert_eq!(extract_mcp_text(body), Some("hi".to_string()));
    }

    #[test]
    fn extract_mcp_text_ignores_non_text_blocks() {
        let body = r#"{"result":{"content":[{"type":"image","data":"x"}]}}"#;
        assert_eq!(extract_mcp_text(body), None);
    }

    #[test]
    fn cap_output_truncates_long_text() {
        let long = "a".repeat(20_000);
        let out = cap_output(&long);
        assert!(out.contains("truncated"));
        assert!(out.chars().count() < 20_000);
    }

    #[test]
    fn build_provider_defaults_to_exa_for_unknown_name() {
        let cfg = WebSearchConfig::default();
        let p = build_provider(&cfg, "totally-bogus");
        assert_eq!(p.name(), "Exa");
    }
}
