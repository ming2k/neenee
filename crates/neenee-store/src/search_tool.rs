use neenee_core::{Tool, ToolOutput, async_trait};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::embedding::EmbeddingStore;
use crate::session::SessionStore;

/// Read-only semantic search over the current project's session history.
pub struct SearchHistoryTool {
    store: Arc<RwLock<EmbeddingStore>>,
    session: Arc<SessionStore>,
}

impl SearchHistoryTool {
    pub fn new(store: Arc<RwLock<EmbeddingStore>>, session: Arc<SessionStore>) -> Self {
        Self { store, session }
    }
}

#[async_trait]
impl Tool for SearchHistoryTool {
    fn name(&self) -> &str {
        "search_history"
    }

    fn description(&self) -> &str {
        "Semantic search over the current project's session history. Returns \
         the most relevant past messages for a natural-language query."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural-language query describing the information you want to find."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return.",
                    "default": 5
                }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("invalid arguments: {e}"))?;
        let query = args["query"]
            .as_str()
            .ok_or_else(|| "missing 'query' argument".to_string())?;
        let limit = args["limit"].as_u64().unwrap_or(5) as usize;

        let messages = self.session.transcript().await;
        {
            let mut store = self.store.write().await;
            let session_id = self.session.id().await;
            store.index(&messages, &session_id).await?;
        }

        let store = self.store.read().await;
        let results = store.search(query, limit).await?;
        if results.is_empty() {
            return Ok("No relevant history found.".to_string());
        }
        let mut lines = vec!["Relevant history (most similar first):".to_string()];
        for (i, (text, score)) in results.iter().enumerate() {
            lines.push(format!("{}. [score={:.3}]\n{}", i + 1, score, text));
        }
        Ok(lines.join("\n\n"))
    }

    async fn call_structured(&self, arguments: &str) -> Result<ToolOutput, String> {
        self.call(arguments).await.map(ToolOutput::text)
    }
}

// --- Self-registration -----------------------------------------------------
//
// Pulls the per-project embedding index and session store out of the build
// context (provided as `Arc<RwLock<EmbeddingStore>>` and `Arc<SessionStore>`).
// Declines when either is absent, so a context without history search simply
// omits the tool.

neenee_core::register_tool!(SearchHistoryFactory => |ctx| {
    let store = ctx.shared::<RwLock<EmbeddingStore>>()?;
    let session = ctx.shared::<SessionStore>()?;
    SearchHistoryTool::new(store, session)
});
