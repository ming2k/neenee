use async_trait::async_trait;
use neenee_core::{truncate_utf8, Tool, ToolAccess, WebSearchConfig};
use serde_json::json;
use std::sync::Arc;

use crate::search::SearchProvider;

/// Fetch a URL and return its text content (HTML stripped to text).
pub struct WebFetchTool {
    config: Arc<WebSearchConfig>,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            config: Arc::new(WebSearchConfig::default()),
        }
    }
    pub fn with_config(config: WebSearchConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the shared HTTP client honoring the web tools' proxy and timeout.
fn http_client(config: &WebSearchConfig) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.timeout_secs.max(1)))
        .user_agent("neenee/0.1 (+ai-coding-agent)");
    if let Some(proxy_url) = config
        .proxy
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let proxy = reqwest::Proxy::all(proxy_url)
            .map_err(|e| format!("Invalid proxy '{}': {}", proxy_url, e))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))
}

/// Naive HTML → text conversion. Collapses whitespace and strips tags/scripts.
pub(crate) fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut skip = false;
    let lower = html.to_ascii_lowercase();
    let mut chars = html.char_indices().peekable();
    while let Some((byte_idx, c)) = chars.next() {
        if !in_tag && lower[byte_idx..].starts_with("<script") {
            skip = true;
        } else if skip && lower[byte_idx..].starts_with("</script") {
            skip = false;
            // jump to end of tag
            if let Some(idx) = lower[byte_idx..].find('>') {
                let next_byte = byte_idx + idx + 1;
                while chars
                    .peek()
                    .is_some_and(|(peek_byte, _)| *peek_byte < next_byte)
                {
                    chars.next();
                }
                continue;
            }
        } else if !in_tag && lower[byte_idx..].starts_with("<style") {
            skip = true;
        } else if skip && lower[byte_idx..].starts_with("</style") {
            skip = false;
            if let Some(idx) = lower[byte_idx..].find('>') {
                let next_byte = byte_idx + idx + 1;
                while chars
                    .peek()
                    .is_some_and(|(peek_byte, _)| *peek_byte < next_byte)
                {
                    chars.next();
                }
                continue;
            }
        }
        if skip {
            continue;
        }
        if c == '<' {
            in_tag = true;
        } else if c == '>' && in_tag {
            in_tag = false;
            out.push(' ');
        } else if !in_tag {
            out.push(c);
        }
    }
    // Decode a handful of common entities
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut collapsed = String::with_capacity(decoded.len());
    let mut prev_ws = false;
    for c in decoded.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                collapsed.push(' ');
                prev_ws = true;
            }
        } else {
            collapsed.push(c);
            prev_ws = false;
        }
    }
    collapsed.trim().to_string()
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "webfetch"
    }
    fn description(&self) -> &str {
        "Fetch the content of a web page or URL and return it as text. Use for reading \
         documentation, APIs, or any publicly accessible resource. HTML pages are converted to \
         plain text. Output is truncated for very large pages."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The fully-qualified URL to fetch (http/https)" },
                "raw": { "type": "boolean", "description": "If true, return raw content without HTML stripping (default false)" }
            },
            "required": ["url"]
        })
    }
    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let url = args["url"].as_str().ok_or("Missing 'url'")?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("URL must start with http:// or https://".to_string());
        }
        let raw = args["raw"].as_bool().unwrap_or(false);
        let client = http_client(&self.config)?;
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("HTTP {} for {}", status, url));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let text = response
            .text()
            .await
            .map_err(|e| format!("Failed to read body: {}", e))?;
        let body = if raw || !content_type.contains("html") {
            text
        } else {
            html_to_text(&text)
        };
        if body.len() > 16_000 {
            return Ok(format!(
                "[Fetched {} chars from {}, truncated]\n{}\n\n[Use raw=true or a more specific URL for full content]",
                body.len(),
                url,
                truncate_utf8(&body, 8_000)
            ));
        }
        Ok(body)
    }
}

/// Search the web via a pluggable backend. The provider (and an optional
/// fallback) are selected from `[websearch]` config; see the [`search`] module
/// for the available backends. Default backend is Exa (hosted, anonymous,
/// reliable) with Parallel as fallback — mirroring other coding agents.
///
/// This struct is a thin shell: it only parses arguments, builds the shared
/// HTTP client (proxy/timeout), and delegates to the provider chain. All
/// backend-specific logic lives behind the `SearchProvider` trait so new
/// backends can be added without touching this tool.
pub struct WebSearchTool {
    config: Arc<WebSearchConfig>,
    primary: Box<dyn SearchProvider>,
    fallback: Option<Box<dyn SearchProvider>>,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self::with_config(WebSearchConfig::default())
    }

    pub fn with_config(config: WebSearchConfig) -> Self {
        let primary = crate::search::build_provider(&config, &config.provider);
        let fallback_name = config.fallback.trim();
        let fallback = if fallback_name.is_empty() {
            None
        } else {
            Some(crate::search::build_provider(&config, fallback_name))
        };
        Self {
            config: Arc::new(config),
            primary,
            fallback,
        }
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "websearch"
    }
    fn description(&self) -> &str {
        "Search the web and return results as text. The backend is configurable via the \
         `[websearch]` table in config.toml: `exa` (default; hosted, anonymous, reliable), \
         `parallel` (hosted), `duckduckgo` (keyless scraping, frequently blocked), `searxng` \
         (self-hosted, keyless), or `tavily` (hosted, needs key). A `fallback` backend is \
         tried automatically if the primary fails. Best for current information, \
         documentation, or examples."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The search query" }
            },
            "required": ["query"]
        })
    }
    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }
    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {}", e))?;
        let query = args["query"].as_str().ok_or("Missing 'query'")?;
        let client = http_client(&self.config)?;

        match self.primary.search(&client, query).await {
            Ok(text) => Ok(text),
            Err(primary_err) => match &self.fallback {
                Some(fallback) => match fallback.search(&client, query).await {
                    Ok(text) => Ok(text),
                    Err(fallback_err) => Err(format!(
                        "Primary backend {} failed: {}\nFallback backend {} also failed: {}",
                        self.primary.name(),
                        primary_err,
                        fallback.name(),
                        fallback_err
                    )),
                },
                None => Err(primary_err),
            },
        }
    }
}

neenee_core::register_tool!(WebFetchFactory => |ctx| {
    let cfg = ctx
        .get::<neenee_core::WebSearchConfig>()
        .cloned()
        .unwrap_or_default();
    WebFetchTool::with_config(cfg)
});
neenee_core::register_tool!(WebSearchFactory => |ctx| {
    let cfg = ctx
        .get::<neenee_core::WebSearchConfig>()
        .cloned()
        .unwrap_or_default();
    WebSearchTool::with_config(cfg)
});
