use async_trait::async_trait;
use neenee_core::{Tool, ToolOutput};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Model-facing tool for concise, glanceable work-status updates.
pub struct ProgressUpdateTool {
    max_chars: Arc<AtomicUsize>,
}

impl ProgressUpdateTool {
    pub fn new(max_chars: Arc<AtomicUsize>) -> Self {
        Self { max_chars }
    }

    pub fn clean_summary(summary: &str, max_chars: usize) -> String {
        let collapsed = summary
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        truncate_chars(&collapsed, max_chars.max(1))
    }
}

#[async_trait]
impl Tool for ProgressUpdateTool {
    fn name(&self) -> &str {
        "progress_update"
    }

    fn description(&self) -> &str {
        "Report a very short current work status for the user's activity bar. Use at key phase changes only; keep it concrete, terse, and under the configured character limit."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Very short current work status, e.g. 'Refactor config modal'. No markdown or explanation."
                }
            },
            "required": ["summary"]
        })
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        Ok(self.call_structured(arguments).await?.to_text())
    }

    async fn call_structured(&self, arguments: &str) -> Result<ToolOutput, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {e}"))?;
        let summary = args
            .get("summary")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "progress_update requires a string `summary`.".to_string())?;
        let clean = Self::clean_summary(summary, self.max_chars.load(Ordering::Relaxed));
        if clean.is_empty() {
            return Err("progress_update summary cannot be empty.".to_string());
        }
        Ok(ToolOutput::Text(clean))
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        head
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::ProgressUpdateTool;

    #[test]
    fn clean_summary_collapses_whitespace_and_truncates() {
        assert_eq!(
            ProgressUpdateTool::clean_summary("  Refactor\n\nconfig   modal  ", 15),
            "Refactor config"
        );
    }
}
