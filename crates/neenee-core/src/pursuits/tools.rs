use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use crate::{Tool, ToolAccess};

use super::service::PursuitService;

/// Shared context injected into pursuit-aware tools so they know the current
/// thread/session id and can reach the pursuit service.
#[derive(Clone)]
pub struct PursuitToolContext {
    pub thread_id: Arc<Mutex<Option<String>>>,
    pub pursuit_service: PursuitService,
}

impl PursuitToolContext {
    fn thread_id(&self) -> Result<String, String> {
        self.thread_id
            .lock()
            .map_err(|err| err.to_string())?
            .clone()
            .ok_or_else(|| "no active session id".to_string())
    }
}

pub struct GetPursuitTool {
    context: PursuitToolContext,
}

impl GetPursuitTool {
    pub fn new(context: PursuitToolContext) -> Self {
        Self { context }
    }
}

#[async_trait]
impl Tool for GetPursuitTool {
    fn name(&self) -> &str {
        "get_pursuit"
    }

    fn description(&self) -> &str {
        "Get the current pursuit for this thread, including objective and completion state."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }

    async fn call(&self, _arguments: &str) -> Result<String, String> {
        let thread_id = self.context.thread_id()?;
        let pursuit = self.context.pursuit_service.get_pursuit(&thread_id).await?;
        match pursuit {
            Some(pursuit) => Ok(serde_json::to_string(&pursuit).unwrap_or_default()),
            None => Ok("{\"pursuit\": null}".to_string()),
        }
    }
}

pub struct StartPursuitTool {
    context: PursuitToolContext,
}

impl StartPursuitTool {
    pub fn new(context: PursuitToolContext) -> Self {
        Self { context }
    }
}

#[async_trait]
impl Tool for StartPursuitTool {
    fn name(&self) -> &str {
        "start_pursuit"
    }

    fn description(&self) -> &str {
        "Create a pursuit only when explicitly requested by the user or system/developer instructions; do not infer pursuits from ordinary tasks. Replaces any existing pursuit on this thread."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "Required. The concrete objective to start pursuing. Replaces the current pursuit if one exists."
                }
            },
            "required": ["objective"],
            "additionalProperties": false
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Write
    }

    fn permission_label(&self) -> String {
        "Create pursuit".to_string()
    }

    fn permission_description(&self) -> String {
        "Start a new active pursuit for this thread, replacing any existing pursuit.".to_string()
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        struct Args {
            objective: String,
        }

        let args: Args =
            serde_json::from_str(arguments).map_err(|err| format!("Invalid JSON: {err}"))?;
        let thread_id = self.context.thread_id()?;

        let pursuit = self
            .context
            .pursuit_service
            .set_pursuit(&thread_id, &args.objective)
            .await?;
        Ok(serde_json::to_string(&json!({ "pursuit": pursuit })).unwrap_or_default())
    }
}

pub struct CompletePursuitTool {
    context: PursuitToolContext,
}

impl CompletePursuitTool {
    pub fn new(context: PursuitToolContext) -> Self {
        Self { context }
    }
}

#[async_trait]
impl Tool for CompletePursuitTool {
    fn name(&self) -> &str {
        "complete_pursuit"
    }

    fn description(&self) -> &str {
        "Mark the existing pursuit as complete. Use this tool only when the objective has actually been achieved and no required work remains. Do not mark a pursuit complete merely because you are stopping work or because progress is slow."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["complete"],
                    "description": "Set to `complete` only when the objective is achieved."
                }
            },
            "required": ["status"],
            "additionalProperties": false
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Write
    }

    fn permission_label(&self) -> String {
        "Mark pursuit complete".to_string()
    }

    fn permission_description(&self) -> String {
        "Mark the active pursuit as complete.".to_string()
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        struct Args {
            status: String,
        }

        let args: Args =
            serde_json::from_str(arguments).map_err(|err| format!("Invalid JSON: {err}"))?;
        let thread_id = self.context.thread_id()?;

        match args.status.as_str() {
            "complete" => {
                let pursuit = self
                    .context
                    .pursuit_service
                    .mark_complete(&thread_id)
                    .await?;
                Ok(serde_json::to_string(&json!({ "pursuit": pursuit })).unwrap_or_default())
            }
            other => Err(format!("invalid complete_pursuit status: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::PursuitStore;
    use super::*;

    fn make_context() -> PursuitToolContext {
        PursuitToolContext {
            thread_id: Arc::new(Mutex::new(Some("test-thread".to_string()))),
            pursuit_service: PursuitService::new(PursuitStore::open_in_memory_blocking().unwrap()),
        }
    }

    #[test]
    fn create_pursuit_exposes_user_friendly_permission_text() {
        let tool = StartPursuitTool::new(make_context());
        assert_eq!(tool.permission_label(), "Create pursuit");
        // The model-facing description is full of model instructions; the
        // user-facing override must stay short, non-prescriptive, and free
        // of "do not infer..." style guidance aimed at the model.
        let desc = tool.permission_description();
        assert_ne!(desc, tool.description());
        assert!(!desc.contains("do not infer"));
        assert!(desc.split('.').count() <= 2);
    }

    #[test]
    fn update_pursuit_exposes_user_friendly_permission_text() {
        let tool = CompletePursuitTool::new(make_context());
        assert_eq!(tool.permission_label(), "Mark pursuit complete");
        let desc = tool.permission_description();
        assert_ne!(desc, tool.description());
        assert!(!desc.contains("do not"));
        assert!(!desc.contains("must"));
        assert!(desc.split('.').count() <= 2);
    }

    #[test]
    fn read_only_pursuit_tools_keep_trait_default_label() {
        // `get_pursuit` is Read and does not override permission_label, so the
        // default (the raw tool name) is used. This guards against the
        // override accidentally leaking onto tools that never prompt.
        let tool = GetPursuitTool::new(make_context());
        assert_eq!(tool.permission_label(), tool.name());
        assert_eq!(tool.permission_description(), tool.description());
    }
}
