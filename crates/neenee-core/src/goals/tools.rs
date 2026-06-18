use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use crate::{Tool, ToolAccess};

use super::service::GoalService;
use super::{Goal, GoalChecklistItem, GoalChecklistStatus, GoalStatus};

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
        "Get the current goal for this thread, including status, budgets, token and elapsed-time usage, and remaining token budget."
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
        "Create a goal only when explicitly requested by the user or system/developer instructions; do not infer goals from ordinary tasks. Set token_budget only when an explicit token budget is requested. Fails if an unfinished goal exists; use update_goal only for status."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "Required. The concrete objective to start pursuing. This starts a new active goal when no goal exists or replaces the current goal when it is complete."
                },
                "token_budget": {
                    "type": "integer",
                    "description": "Positive token budget for the new goal. Omit unless explicitly requested."
                }
            },
            "required": ["objective"],
            "additionalProperties": false
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Write
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        struct Args {
            objective: String,
            token_budget: Option<i64>,
        }

        let args: Args =
            serde_json::from_str(arguments).map_err(|err| format!("Invalid JSON: {err}"))?;
        let thread_id = self.context.thread_id()?;

        let goal = self
            .context
            .goal_service
            .set_goal(
                &thread_id,
                &args.objective,
                GoalStatus::Active,
                args.token_budget,
            )
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
        "Update the existing goal. Use this tool only to mark the goal achieved or genuinely blocked. Set status to `complete` only when the objective has actually been achieved and no required work remains. Set status to `blocked` only when the same blocking condition has repeated for at least three consecutive goal turns, counting the original/user-triggered turn and any automatic continuations, and the agent cannot make meaningful progress without user input or an external-state change. If the user resumes a goal that was previously marked `blocked`, treat the resumed run as a fresh blocked audit. If the same blocking condition then repeats for at least three consecutive resumed goal turns, set status to `blocked` again. Once the blocked threshold is satisfied, do not keep reporting that you are still blocked while leaving the goal active; set status to `blocked`. Do not use `blocked` merely because the work is hard, slow, uncertain, incomplete, or would benefit from clarification. Do not mark a goal complete merely because its budget is nearly exhausted or because you are stopping work. You cannot use this tool to pause, resume, budget-limit, or usage-limit a goal; those status changes are controlled by the user or system."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["complete", "blocked"],
                    "description": "Required. Set to `complete` only when the objective is achieved and no required work remains. Set to `blocked` only after the same blocking condition has recurred for at least three consecutive goal turns and the agent is at an impasse."
                }
            },
            "required": ["status"],
            "additionalProperties": false
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Write
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        struct Args {
            status: String,
        }

        let args: Args =
            serde_json::from_str(arguments).map_err(|err| format!("Invalid JSON: {err}"))?;
        let thread_id = self.context.thread_id()?;

        let goal = match args.status.as_str() {
            "complete" => self.context.goal_service.mark_complete(&thread_id).await?,
            "blocked" => self.context.goal_service.mark_blocked(&thread_id).await?,
            other => return Err(format!("invalid update_goal status: {other}")),
        };

        Ok(serde_json::to_string(&json!({ "goal": goal })).unwrap_or_default())
    }
}

pub struct GoalChecklistTool {
    goal: Arc<Mutex<Option<Goal>>>,
}

impl GoalChecklistTool {
    pub fn new(_context: GoalToolContext, goal: Arc<Mutex<Option<Goal>>>) -> Self {
        Self { goal }
    }
}

#[async_trait]
impl Tool for GoalChecklistTool {
    fn name(&self) -> &str {
        "goal_checklist"
    }

    fn description(&self) -> &str {
        "Replace the active goal's structured checklist. Use this to expose concrete progress. Keep exactly one item in_progress while working; mark verified work completed."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "maxItems": 50,
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"]
                            }
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["items"],
            "additionalProperties": false
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        struct Args {
            items: Vec<GoalChecklistItem>,
        }

        let args: Args =
            serde_json::from_str(arguments).map_err(|err| format!("Invalid JSON: {err}"))?;
        if args.items.len() > 50 {
            return Err("Goal checklist is limited to 50 items.".to_string());
        }
        if args.items.iter().any(|item| item.content.trim().is_empty()) {
            return Err("Goal checklist item content cannot be empty.".to_string());
        }
        let in_progress = args
            .items
            .iter()
            .filter(|item| item.status == GoalChecklistStatus::InProgress)
            .count();
        if in_progress > 1 {
            return Err("At most one goal checklist item may be in_progress.".to_string());
        }

        let mut guard = self.goal.lock().map_err(|err| err.to_string())?;
        let goal = guard
            .as_mut()
            .ok_or_else(|| "No active goal. Set one with /goal <objective>.".to_string())?;
        if goal.status != GoalStatus::Active {
            return Err("The goal is not active.".to_string());
        }
        if args.items.is_empty() && !goal.checklist.is_empty() {
            return Err(
                "An active checklist cannot be cleared. Mark each item completed or cancelled."
                    .to_string(),
            );
        }
        goal.checklist = args.items;
        let resolved = goal
            .checklist
            .iter()
            .filter(|item| {
                matches!(
                    item.status,
                    GoalChecklistStatus::Completed | GoalChecklistStatus::Cancelled
                )
            })
            .count();
        Ok(format!(
            "Goal checklist updated: {}/{} resolved.",
            resolved,
            goal.checklist.len()
        ))
    }
}
