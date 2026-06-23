use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use crate::{Tool, ToolAccess};

use super::service::GoalService;

/// Shared context injected into goal-aware tools so they know the current
/// thread/session id and can reach the goal service.
#[derive(Clone)]
pub struct GoalToolContext {
    pub thread_id: Arc<Mutex<Option<String>>>,
    pub goal_service: GoalService,
}

impl GoalToolContext {
    fn thread_id(&self) -> Result<String, String> {
        self.thread_id
            .lock()
            .map_err(|err| err.to_string())?
            .clone()
            .ok_or_else(|| "no active session id".to_string())
    }
}

pub struct GetGoalTool {
    context: GoalToolContext,
}

impl GetGoalTool {
    pub fn new(context: GoalToolContext) -> Self {
        Self { context }
    }
}

#[async_trait]
impl Tool for GetGoalTool {
    fn name(&self) -> &str {
        "get_goal"
    }

    fn description(&self) -> &str {
        "Get the current goal for this thread, including objective and completion state."
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
        let goal = self.context.goal_service.get_goal(&thread_id).await?;
        match goal {
            Some(goal) => Ok(serde_json::to_string(&goal).unwrap_or_default()),
            None => Ok("{\"goal\": null}".to_string()),
        }
    }
}

pub struct CreateGoalTool {
    context: GoalToolContext,
}

impl CreateGoalTool {
    pub fn new(context: GoalToolContext) -> Self {
        Self { context }
    }
}

#[async_trait]
impl Tool for CreateGoalTool {
    fn name(&self) -> &str {
        "create_goal"
    }

    fn description(&self) -> &str {
        "Create a goal only when explicitly requested by the user or system/developer instructions; do not infer goals from ordinary tasks. Replaces any existing goal on this thread."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "Required. The concrete objective to start pursuing. Replaces the current goal if one exists."
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
        "Create goal".to_string()
    }

    fn permission_description(&self) -> String {
        "Start a new active goal for this thread, replacing any existing goal.".to_string()
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        struct Args {
            objective: String,
        }

        let args: Args =
            serde_json::from_str(arguments).map_err(|err| format!("Invalid JSON: {err}"))?;
        let thread_id = self.context.thread_id()?;

        let goal = self
            .context
            .goal_service
            .set_goal(&thread_id, &args.objective)
            .await?;
        Ok(serde_json::to_string(&json!({ "goal": goal })).unwrap_or_default())
    }
}

pub struct UpdateGoalTool {
    context: GoalToolContext,
}

impl UpdateGoalTool {
    pub fn new(context: GoalToolContext) -> Self {
        Self { context }
    }
}

#[async_trait]
impl Tool for UpdateGoalTool {
    fn name(&self) -> &str {
        "update_goal"
    }

    fn description(&self) -> &str {
        "Mark the existing goal as complete. Use this tool only when the objective has actually been achieved and no required work remains. Do not mark a goal complete merely because you are stopping work or because progress is slow."
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
        "Mark goal complete".to_string()
    }

    fn permission_description(&self) -> String {
        "Mark the active goal as complete.".to_string()
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
                let goal = self.context.goal_service.mark_complete(&thread_id).await?;
                Ok(serde_json::to_string(&json!({ "goal": goal })).unwrap_or_default())
            }
            other => Err(format!("invalid update_goal status: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::GoalStore;
    use super::*;

    fn make_context() -> GoalToolContext {
        GoalToolContext {
            thread_id: Arc::new(Mutex::new(Some("test-thread".to_string()))),
            goal_service: GoalService::new(GoalStore::open_in_memory_blocking().unwrap()),
        }
    }

    #[test]
    fn create_goal_exposes_user_friendly_permission_text() {
        let tool = CreateGoalTool::new(make_context());
        assert_eq!(tool.permission_label(), "Create goal");
        // The model-facing description is full of model instructions; the
        // user-facing override must stay short, non-prescriptive, and free
        // of "do not infer..." style guidance aimed at the model.
        let desc = tool.permission_description();
        assert_ne!(desc, tool.description());
        assert!(!desc.contains("do not infer"));
        assert!(desc.split('.').count() <= 2);
    }

    #[test]
    fn update_goal_exposes_user_friendly_permission_text() {
        let tool = UpdateGoalTool::new(make_context());
        assert_eq!(tool.permission_label(), "Mark goal complete");
        let desc = tool.permission_description();
        assert_ne!(desc, tool.description());
        assert!(!desc.contains("do not"));
        assert!(!desc.contains("must"));
        assert!(desc.split('.').count() <= 2);
    }

    #[test]
    fn read_only_goal_tools_keep_trait_default_label() {
        // `get_goal` is Read and does not override permission_label, so the
        // default (the raw tool name) is used. This guards against the
        // override accidentally leaking onto tools that never prompt.
        let tool = GetGoalTool::new(make_context());
        assert_eq!(tool.permission_label(), tool.name());
        assert_eq!(tool.permission_description(), tool.description());
    }
}
