//! Exa hosted search via its MCP endpoint (`https://mcp.exa.ai/mcp`).
//!
//! Works anonymously (no key) with generous rate limits; an optional key routes
//! through the caller's own quota. Returns a pre-rendered, model-optimized text
//! blob, which we pass through largely verbatim. This is the default backend.

use super::{cap_output, mcp_tools_call, SearchProvider};
use async_trait::async_trait;

const EXA_URL: &str = "https://mcp.exa.ai/mcp";
const EXA_TOOL: &str = "web_search_exa";

pub(crate) struct ExaProvider {
    pub api_key: Option<String>,
}

#[async_trait]
impl SearchProvider for ExaProvider {
    fn name(&self) -> &'static str {
        "Exa"
    }

    async fn search(
        &self,
        client: &reqwest::Client,
        query: &str,
    ) -> Result<String, String> {
        let url = endpoint_with_key(self.api_key.as_deref());
        let text = mcp_tools_call(
            client,
            &url,
            EXA_TOOL,
            serde_json::json!({
                "query": query,
                "type": "auto",
                "numResults": 10,
                "livecrawl": "fallback",
            }),
            &[],
        )
        .await?;
        Ok(cap_output(&format!(
            "Search results for '{}' (via Exa):\n\n{}",
            query, text
        )))
    }
}

/// Build the Exa MCP URL, appending `?exaApiKey=` when a key is configured.
/// Uses reqwest's URL encoder so keys with special characters stay intact.
fn endpoint_with_key(api_key: Option<&str>) -> String {
    let base = reqwest::Url::parse(EXA_URL).expect("hardcoded Exa URL is valid");
    let key = api_key.map(str::trim).filter(|s| !s.is_empty());
    match key {
        Some(k) => {
            let mut url = base;
            url.query_pairs_mut().append_pair("exaApiKey", k);
            url.into()
        }
        None => base.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_omits_key_when_absent() {
        assert_eq!(endpoint_with_key(None), EXA_URL);
    }

    #[test]
    fn endpoint_appends_encoded_key_when_present() {
        let url = endpoint_with_key(Some("secret key&more"));
        assert!(url.contains("exaApiKey=secret+key%26more"), "got: {url}");
    }
}
