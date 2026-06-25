//! The `todo` and `todo_update` tools.
//!
//! These are orchestration-layer tools: they implement the core [`Tool`]
//! trait but mutate shared agent-owned state (`TodoToolContext`) rather than
//! touching the filesystem. They live here — not in `neenee-tools` alongside
//! the stateless file tools — because they need the same `Arc<Mutex<TodoList>>`
//! cell the `Agent` owns. Domain types (`TodoList`, `TodoStatus`, etc.) stay
//! in `neenee-core`; this module only wires them into the tool interface.

use std::collections::HashSet;

use async_trait::async_trait;
use serde_json::json;

use neenee_core::{TodoStatus, TodoToolContext, Tool, ToolAccess, MAX_TODOS};

const TODO_DESCRIPTION: &str =
    "Maintain the task list for the current work. Replace the whole list each call with the current \
     set of concrete steps, in the order you intend to tackle them. At most one item may be \
     in_progress. This list is the single source of truth shown in the activity bar and task \
     panel and persisted across restarts, so keep it honest: add an item when you commit to a step, \
     move it to in_progress when you start, and to completed the moment it is done. The returned \
     list reflects the reconciled state (items keep their identity when you resend the same content).";

/// Full-replace todo tool. The model sends the desired list each call; the
/// tool reconciles it against the current list preserving identity (see
/// [`TodoList::reconcile`]). This is the robust interface: the model never
/// has to track ids.
pub struct TodoWriteTool {
    context: TodoToolContext,
}

impl TodoWriteTool {
    pub fn new(context: TodoToolContext) -> Self {
        Self { context }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        TODO_DESCRIPTION
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "maxItems": MAX_TODOS,
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
        struct Arguments {
            items: Vec<TodoArgs>,
        }
        #[derive(serde::Deserialize)]
        struct TodoArgs {
            content: String,
            status: String,
        }

        let parsed: Arguments =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON: {e}"))?;
        if parsed.items.len() > MAX_TODOS {
            return Err(format!("Todo list is limited to {MAX_TODOS} items."));
        }

        let mut desired: Vec<(String, TodoStatus)> = Vec::with_capacity(parsed.items.len());
        let mut in_progress = 0;
        for entry in parsed.items {
            if entry.content.trim().is_empty() {
                return Err("Todo item content cannot be empty.".to_string());
            }
            let status = TodoStatus::parse(&entry.status).ok_or_else(|| {
                format!(
                    "Unknown todo status '{}'. Use pending / in_progress / completed / cancelled.",
                    entry.status
                )
            })?;
            if status == TodoStatus::InProgress {
                in_progress += 1;
            }
            desired.push((entry.content, status));
        }
        if in_progress > 1 {
            return Err("At most one todo item may be in_progress.".to_string());
        }

        let now = neenee_core::todos::unix_now();
        let turn = self.context.current_turn();
        let mut list = self.context.todos();
        let prev_ids: HashSet<u64> = list.items.iter().map(|i| i.id.0).collect();
        let prev_contents: HashSet<String> = list.items.iter().map(|i| i.content.clone()).collect();
        let changed = list.reconcile(&desired, now, turn);
        if changed {
            self.context.set_todos(list);
        }
        let current = self.context.todos();
        let rendered = current.render();

        let new_contents: Vec<&str> = current
            .items
            .iter()
            .filter(|i| !prev_ids.contains(&i.id.0))
            .map(|i| i.content.as_str())
            .filter(|c| !prev_contents.contains(*c))
            .collect();
        let note = if new_contents.is_empty() {
            String::new()
        } else if new_contents.len() == prev_contents.len()
            && !prev_contents.is_empty()
            && current.items.len() == prev_contents.len()
        {
            format!(
                "\nNote: {} item(s) were rewritten with new identity (content changed). \
                 Their created_at reset.",
                new_contents.len()
            )
        } else {
            format!("\nNote: {} new item(s) created.", new_contents.len())
        };
        Ok(format!("Todo list updated:\n{rendered}{note}"))
    }
}

const TODO_UPDATE_DESCRIPTION: &str =
    "Surgically update the status of one or more existing todo items without re-sending the whole \
     list. `key` is either a 1-based position as shown by the `todo` tool (\"1\", \"3\") or, when \
     not a valid position, a case-insensitive substring of the item content (all matches update). \
     Prefer this over `todo` when you only want to mark progress on a single step.";

/// Surgical update tool: change the status of items matched by position or
/// content substring, leaving everything else untouched. Complements
/// [`TodoWriteTool`] so the model can mark a step done without re-emitting the
/// entire list.
pub struct TodoUpdateTool {
    context: TodoToolContext,
}

impl TodoUpdateTool {
    pub fn new(context: TodoToolContext) -> Self {
        Self { context }
    }
}

#[async_trait]
impl Tool for TodoUpdateTool {
    fn name(&self) -> &str {
        "todo_update"
    }

    fn description(&self) -> &str {
        TODO_UPDATE_DESCRIPTION
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "1-based position or case-insensitive content substring"
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed", "cancelled"],
                    "description": "New status for the matched item(s)"
                }
            },
            "required": ["key", "status"],
            "additionalProperties": false
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let value = serde_json::from_str::<serde_json::Value>(arguments)
            .map_err(|e| format!("Invalid JSON: {e}"))?;
        let key = value
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or("Missing 'key'")?;
        let status_str = value
            .get("status")
            .and_then(|v| v.as_str())
            .ok_or("Missing 'status'")?;
        let status = TodoStatus::parse(status_str).ok_or_else(|| {
            format!(
                "Unknown status '{status_str}'. Use pending / in_progress / completed / cancelled."
            )
        })?;

        let now = neenee_core::todos::unix_now();
        let turn = self.context.current_turn();
        let mut list = self.context.todos();
        if list.is_empty() {
            return Ok(
                "No todos to update. Use the `todo` tool to create the list first.".to_string(),
            );
        }
        let changed = list.update(key, status, now, turn);
        if changed == 0 {
            return Ok(format!(
                "No todo matched '{key}'. Current todos:\n{}",
                list.render()
            ));
        }
        if status == TodoStatus::InProgress {
            let in_progress: Vec<&str> = list
                .items
                .iter()
                .filter(|i| i.status == TodoStatus::InProgress)
                .map(|i| i.content.as_str())
                .collect();
            if in_progress.len() > 1 {
                return Ok(format!(
                    "Cannot set '{key}' to in_progress — multiple items are now in_progress \
                     ({}). At most one todo item may be in_progress at a time. \
                     Mark the others completed or cancelled first.\n\nCurrent todos:\n{}",
                    in_progress.join(", "),
                    list.render()
                ));
            }
        }
        self.context.set_todos(list);
        let rendered = self.context.todos().render();
        Ok(format!(
            "Updated {changed} item(s) to {status_str}.\n{rendered}",
            status_str = status_str
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use neenee_core::TodoList;

    fn ctx() -> (TodoToolContext, Arc<Mutex<TodoList>>) {
        let list = Arc::new(Mutex::new(TodoList::new()));
        let turn = Arc::new(Mutex::new(5u64));
        (TodoToolContext::shared(Arc::clone(&list), turn), list)
    }

    #[tokio::test]
    async fn todo_write_tool_reconciles_and_preserves_identity() {
        let (context, list) = ctx();
        let tool = TodoWriteTool::new(context.clone());
        tool.call(r#"{"items":[{"content":"design","status":"pending"}]}"#)
            .await
            .unwrap();
        let first_id = list.lock().unwrap().items[0].id;

        tool.call(
            r#"{"items":[{"content":"design","status":"completed"},
                         {"content":"implement","status":"in_progress"}]}"#,
        )
        .await
        .unwrap();
        let guard = list.lock().unwrap();
        assert_eq!(guard.items[0].id, first_id);
        assert_eq!(guard.items[0].status, TodoStatus::Completed);
        assert_eq!(guard.items[1].content, "implement");
        assert_eq!(guard.updated_at_turn, 5);
    }

    #[tokio::test]
    async fn todo_write_tool_rejects_two_in_progress() {
        let (context, _) = ctx();
        let tool = TodoWriteTool::new(context);
        let err = tool
            .call(
                r#"{"items":[
                    {"content":"a","status":"in_progress"},
                    {"content":"b","status":"in_progress"}
                ]}"#,
            )
            .await
            .unwrap_err();
        assert!(err.contains("in_progress"));
    }

    #[tokio::test]
    async fn todo_update_tool_matches_by_position() {
        let (context, list) = ctx();
        let write = TodoWriteTool::new(context.clone());
        write
            .call(
                r#"{"items":[
                {"content":"a","status":"pending"},
                {"content":"b","status":"pending"}
            ]}"#,
            )
            .await
            .unwrap();
        let update = TodoUpdateTool::new(context);
        update
            .call(r#"{"key":"2","status":"completed"}"#)
            .await
            .unwrap();
        let guard = list.lock().unwrap();
        assert_eq!(guard.items[1].status, TodoStatus::Completed);
        assert_eq!(guard.items[0].status, TodoStatus::Pending);
    }

    #[tokio::test]
    async fn todo_update_tool_matches_by_content() {
        let (context, list) = ctx();
        let write = TodoWriteTool::new(context.clone());
        write
            .call(r#"{"items":[{"content":"Write tests","status":"pending"}]}"#)
            .await
            .unwrap();
        let update = TodoUpdateTool::new(context);
        let body = update
            .call(r#"{"key":"tests","status":"completed"}"#)
            .await
            .unwrap();
        assert!(body.contains("Updated 1"));
        assert_eq!(list.lock().unwrap().items[0].status, TodoStatus::Completed);
    }

    #[tokio::test]
    async fn todo_update_tool_empty_list_returns_hint() {
        let (context, _) = ctx();
        let tool = TodoUpdateTool::new(context);
        let body = tool.call(r#"{"key":"1","status":"done"}"#).await.unwrap();
        assert!(body.contains("No todos"));
    }

    #[tokio::test]
    async fn todo_update_tool_rejects_second_in_progress() {
        let (context, list) = ctx();
        let write = TodoWriteTool::new(context.clone());
        write
            .call(
                r#"{"items":[
                {"content":"a","status":"in_progress"},
                {"content":"b","status":"pending"}
            ]}"#,
            )
            .await
            .unwrap();
        let update = TodoUpdateTool::new(context);
        let body = update
            .call(r#"{"key":"b","status":"in_progress"}"#)
            .await
            .unwrap();
        assert!(body.contains("in_progress"));
        // The second item must NOT have been committed.
        assert_eq!(list.lock().unwrap().items[1].status, TodoStatus::Pending);
    }
}
