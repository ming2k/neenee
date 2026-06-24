//! Plan-mode workflow tools.
//!
//! The build agent can autonomously switch into Plan mode via `plan_enter`
//! when a request would benefit from research and design before any edits.
//! While in Plan mode only read-only tools run, with one exception: files
//! under the project's `.neenee/plans/` directory may be written so the agent
//! can persist its plan. When the plan is ready the agent calls `plan_exit`
//! to switch back to Build mode and start implementing.
//!
//! Mode switches are performed through a shared `Arc<Mutex<AgentMode>>` (see
//! [`PlanToolContext`]) that is also owned by the host `Agent` type (in
//! `neenee-agent`), so a tool call takes effect immediately and is reflected in
//! the next system prompt.
//!
//! When `plan_exit` is approved the path to the written plan is recorded in
//! `active_plan_path` and surfaced in the system prompt of subsequent Build
//! rounds ("You are implementing the plan at `<path>`.") so the model keeps the
//! plan in context without re-reading the file each turn. The plan markdown is
//! also parsed into a [`TodoList`] (via [`TodoList::from_plan_markdown`]) which
//! the model then tracks with the `todo` / `todo_update` tools — there is no
//! separate plan-progress type. Entering Plan mode clears both the path and
//! the list.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use crate::{todos::TodoToolContext, AgentMode, TodoList, Tool, ToolAccess};

/// Directory (relative to the project root / current working directory) where
/// plan documents live. Mirrors opencode's `.opencode/plans/` convention.
pub const PLANS_DIR: &str = ".neenee/plans";

/// Shared handle injected into the plan tools so they can flip the agent's
/// mode, record the active plan path, and reseed/clear the unified [`TodoList`]
/// on plan transitions. The same `Arc`s are held by the host `Agent` type (in
/// `neenee-agent`), so mutations are visible to the agent, the harness, and
/// the TUI immediately.
#[derive(Clone)]
pub struct PlanToolContext {
    mode: Arc<Mutex<AgentMode>>,
    /// Path to the plan file most recently approved via `plan_exit`. Cleared
    /// by `plan_enter` and `/mode plan` since re-entering Plan mode
    /// invalidates the previously approved plan.
    active_plan_path: Arc<Mutex<Option<PathBuf>>>,
    /// Shared task-list handle. `plan_exit` seeds this from the approved plan
    /// markdown; `plan_enter` clears it. The same handle backs the
    /// `todo` / `todo_update` tools, so plan transitions and ad-hoc task
    /// edits move one shared list.
    todos: TodoToolContext,
}

impl PlanToolContext {
    /// Build a self-contained context with its own fresh cells. Convenient for
    /// tests that exercise the plan tools in isolation (no host `Agent`).
    pub fn new(mode: Arc<Mutex<AgentMode>>) -> Self {
        let todos = Arc::new(Mutex::new(TodoList::default()));
        let turn_counter = Arc::new(Mutex::new(0u64));
        Self {
            mode,
            active_plan_path: Arc::new(Mutex::new(None)),
            todos: TodoToolContext::new(todos, turn_counter),
        }
    }

    /// Build a context that shares state with cells owned by the `Agent`. The
    /// `todos` handle must wrap the same `Arc<Mutex<TodoList>>` (and turn
    /// counter) the agent owns and that the `todo` / `todo_update` tools
    /// mutate, so a plan transition and a task edit move one list.
    pub fn shared(
        mode: Arc<Mutex<AgentMode>>,
        active_plan_path: Arc<Mutex<Option<PathBuf>>>,
        todos: TodoToolContext,
    ) -> Self {
        Self {
            mode,
            active_plan_path,
            todos,
        }
    }

    fn set_mode(&self, mode: AgentMode) {
        if let Ok(mut guard) = self.mode.lock() {
            *guard = mode;
        }
    }

    /// Current active plan path (set by `plan_exit`, cleared by `plan_enter`).
    pub fn active_plan_path(&self) -> Option<PathBuf> {
        self.active_plan_path
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn set_active_plan_path(&self, path: Option<PathBuf>) {
        if let Ok(mut guard) = self.active_plan_path.lock() {
            *guard = path;
        }
    }

    /// Shared task-list handle. `plan_exit` seeds it; `plan_enter` clears it.
    pub fn todo_context(&self) -> TodoToolContext {
        self.todos.clone()
    }
}

/// Absolute form of [`PLANS_DIR`] resolved against the current working
/// directory. Canonicalization is best-effort: if the directory does not
/// exist yet we fall back to the joined (un-canonicalized) path so that a
/// not-yet-created plan file still resolves correctly.
fn plans_dir_abs() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let abs = cwd.join(PLANS_DIR);
    Some(abs.canonicalize().unwrap_or(abs))
}

/// Resolve an arbitrary (relative or absolute) path against the current
/// working directory. The parent directory is canonicalized and the file name
/// re-appended, so paths to files that do not exist yet still resolve.
fn resolve_path(path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    let cwd = std::env::current_dir().ok()?;
    let parent = p.parent();
    let file_name = p.file_name();
    let resolved = match (parent, file_name) {
        (Some(parent), Some(file_name)) if !parent.as_os_str().is_empty() => {
            let abs_parent = cwd.join(parent);
            let canon_parent = abs_parent.canonicalize().unwrap_or(abs_parent);
            canon_parent.join(file_name)
        }
        _ => {
            let abs = cwd.join(p);
            abs.canonicalize().unwrap_or(abs)
        }
    };
    Some(resolved)
}

/// True when `path` points inside the project's plan directory. Used by the
/// write/edit tools to opt back in to Plan mode, and by the Plan-mode guard
/// (in `neenee-agent`) to decide whether a write is permitted while planning.
pub fn is_plan_path(path: &str) -> bool {
    matches!((resolve_path(path), plans_dir_abs()), (Some(p), Some(base)) if p.starts_with(&base))
}

/// Tool invoked by the build agent to enter Plan mode. After it returns, the
/// agent runs read-only (plus plan-file writes) until `plan_exit` is called.
pub struct PlanEnterTool {
    context: PlanToolContext,
}

impl PlanEnterTool {
    pub fn new(context: PlanToolContext) -> Self {
        Self { context }
    }
}

const PLAN_ENTER_DESCRIPTION: &str =
    "Enter Plan mode to research and design a solution before making any edits. In Plan mode \
     write/edit tools are blocked except for files under .neenee/plans/, where you should write \
     the plan document. Call this tool when the user's request is complex, spans multiple files, \
     or would benefit from designing before implementing. Do NOT call it for simple, \
     straightforward tasks or when the user wants immediate implementation. After researching, \
     write the plan to .neenee/plans/<name>.md, then call plan_exit to switch back to Build mode \
     and implement it. Plan mode follows a three-phase workflow: (1) ground yourself in the \
     environment by reading code, configs, and types before asking anything; (2) clarify intent \
     and tradeoffs that cannot be derived from the repo; (3) write a decision-complete plan to \
     .neenee/plans/<name>.md and call plan_exit. The user must approve plan_exit before the mode \
     flips, so do not call it until the plan is genuinely ready.";

#[async_trait]
impl Tool for PlanEnterTool {
    fn name(&self) -> &str {
        "plan_enter"
    }

    fn description(&self) -> &str {
        PLAN_ENTER_DESCRIPTION
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
        self.context.set_mode(AgentMode::Plan);
        // Re-entering Plan mode invalidates any previously approved plan:
        // the agent is about to draft a new one, so the Build-mode hint that
        // points at the old file would be misleading, and the task list's
        // items belong to the old plan.
        self.context.set_active_plan_path(None);
        self.context.todo_context().set_todos(TodoList::default());
        Ok(
            "Switched to Plan mode. Write tools are now blocked except under .neenee/plans/. \
             Research the request using read-only tools, then write the plan to \
             .neenee/plans/<name>.md and call plan_exit to implement it."
                .to_string(),
        )
    }
}

/// Tool invoked by the plan agent once the plan is written and ready for
/// implementation. Switches back to Build mode (full tool access).
pub struct PlanExitTool {
    context: PlanToolContext,
}

impl PlanExitTool {
    pub fn new(context: PlanToolContext) -> Self {
        Self { context }
    }
}

const PLAN_EXIT_DESCRIPTION: &str =
    "Exit Plan mode and switch back to Build mode to start implementing the plan. Call this only \
     after you have written a complete, decision-complete plan to .neenee/plans/. 'Decision \
     complete' means the implementer does not need to make any new design choices — only \
     mechanical execution. The optional `plan_path` should reference the plan file you just \
     wrote; its contents are echoed back to you on approval so you start implementation with \
     full context. The user must approve this transition; if they reject, stay in Plan mode \
     and refine the plan based on their feedback. Do NOT call plan_exit as a way to ask 'is \
     this plan okay?' — that is exactly what the approval step does.";

#[async_trait]
impl Tool for PlanExitTool {
    fn name(&self) -> &str {
        "plan_exit"
    }

    fn description(&self) -> &str {
        PLAN_EXIT_DESCRIPTION
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "plan_path": {
                    "type": "string",
                    "description": "Path to the plan file under .neenee/plans/ that was written"
                }
            },
            "additionalProperties": false
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let plan_path = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|value| value.get("plan_path")?.as_str().map(str::to_string))
            .filter(|value| !value.is_empty());

        // Flip the mode first so subsequent tool calls in the same turn see
        // Build. The harness's ask_user approval gate (in `Agent::execute_tool`)
        // runs *before* this method: if the user rejects, `call` never runs.
        self.context.set_mode(AgentMode::Build);

        // Record the plan path so the next system prompt can tell the model
        // which file to follow. Missing path is allowed (the model may omit
        // it), in which case the Build-mode hint is skipped.
        let recorded = plan_path.as_deref().map(PathBuf::from);
        self.context.set_active_plan_path(recorded);

        // Read the plan content so the model has it in the tool result and
        // so we can seed a `TodoList` from its `##` headings for the sticky
        // panel (the model then tracks progress with the `todo` / `todo_update`
        // tools — there is no separate plan-progress type).
        let content = match &plan_path {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(text) if !text.trim().is_empty() => Some(text),
                Ok(_) => None,
                Err(err) => {
                    // The plan path was provided but the file cannot be read.
                    // Switching to Build mode still succeeded; surface the
                    // error so the model knows the plan body is unavailable
                    // and can choose to re-read or proceed without it.
                    self.context.todo_context().set_todos(TodoList::default());
                    return Ok(format!(
                        "Plan approved. Switched to Build mode with full tool access. \
                         (Could not read plan at {}: {}.) Ask the user or read the file \
                         directly to recover the plan.",
                        path, err
                    ));
                }
            },
            None => None,
        };

        // Seed the unified task list from the plan's `##` headings so the
        // sticky panel reflects the plan shape from the start. Each heading
        // becomes a `Pending` todo item. When there is no readable plan body
        // there is nothing to seed, so the list is cleared.
        let turn = self.context.todo_context().current_turn();
        if let Some(text) = &content {
            let list = TodoList::from_plan_markdown(text, crate::todos::unix_now(), turn);
            self.context.todo_context().set_todos(list);
        } else {
            self.context.todo_context().set_todos(TodoList::default());
        }

        let reference = match &plan_path {
            Some(path) => format!(" Follow the plan at {}.", path),
            None => String::new(),
        };

        let body = match content {
            Some(text) => format!(
                "Plan approved. Switched to Build mode with full tool access. Implement the \
                 plan now.{reference}\n\n## Approved Plan:\n{text}",
                reference = reference,
                text = text
            ),
            None => format!(
                "Plan approved. Switched to Build mode with full tool access. Implement the \
                 plan now.{reference}",
                reference = reference
            ),
        };
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_plan_path_matches_relative_and_absolute() {
        let cwd = std::env::current_dir().unwrap();
        let dir = cwd.join(PLANS_DIR);
        std::fs::create_dir_all(&dir).unwrap();

        assert!(is_plan_path(".neenee/plans/feature.md"));
        assert!(is_plan_path(&dir.join("feature.md").to_string_lossy()));
        assert!(!is_plan_path("src/main.rs"));
        assert!(!is_plan_path(".neenee/commands/foo.md"));
    }

    #[tokio::test]
    async fn plan_enter_switches_to_plan_and_exit_back_to_build() {
        let mode = Arc::new(Mutex::new(AgentMode::Build));
        let context = PlanToolContext::new(Arc::clone(&mode));

        PlanEnterTool::new(context.clone())
            .call("{}")
            .await
            .unwrap();
        assert_eq!(*mode.lock().unwrap(), AgentMode::Plan);

        PlanExitTool::new(context)
            .call(r#"{"plan_path":".neenee/plans/x.md"}"#)
            .await
            .unwrap();
        assert_eq!(*mode.lock().unwrap(), AgentMode::Build);
    }

    #[tokio::test]
    async fn plan_enter_clears_active_plan_path_and_todos() {
        let mode = Arc::new(Mutex::new(AgentMode::Build));
        let context = PlanToolContext::new(mode);
        // Seed both the path and the task list (as plan_exit would).
        context.set_active_plan_path(Some(PathBuf::from(".neenee/plans/x.md")));
        context.todo_context().set_todos(TodoList::from_plan_markdown(
            "## X\n",
            100,
            1,
        ));
        assert!(context.active_plan_path().is_some());
        assert_eq!(context.todo_context().todos().len(), 1);

        PlanEnterTool::new(context.clone())
            .call("{}")
            .await
            .unwrap();
        assert_eq!(context.active_plan_path(), None);
        assert!(context.todo_context().todos().is_empty());
    }

    #[tokio::test]
    async fn plan_exit_echoes_plan_content_and_seeds_todos() {
        let mode = Arc::new(Mutex::new(AgentMode::Plan));
        let context = PlanToolContext::new(Arc::clone(&mode));

        let cwd = std::env::current_dir().unwrap();
        let plans_dir = cwd.join(PLANS_DIR);
        std::fs::create_dir_all(&plans_dir).unwrap();
        let plan_path = plans_dir.join("echo-content.md");
        std::fs::write(
            &plan_path,
            "# Echo Plan\n\n## Summary\nbody\n\n## Key Changes\n- step A\n",
        )
        .unwrap();

        let relative = ".neenee/plans/echo-content.md";
        let args = format!("{{\"plan_path\":\"{}\"}}", relative.replace('\\', "\\\\"));
        let body = PlanExitTool::new(context.clone())
            .call(&args)
            .await
            .unwrap();
        assert!(body.contains("## Approved Plan:"));
        assert!(body.contains("step A"));
        assert_eq!(*mode.lock().unwrap(), AgentMode::Build);
        assert_eq!(context.active_plan_path(), Some(PathBuf::from(relative)));

        // The plan's `##` headings seed the unified task list (one Pending
        // item each), not a separate plan-progress type.
        let list = context.todo_context().todos();
        let names: Vec<_> = list.items.iter().map(|i| i.content.as_str()).collect();
        assert_eq!(names, vec!["Summary", "Key Changes"]);
        assert!(list
            .items
            .iter()
            .all(|i| i.status == crate::TodoStatus::Pending));
    }
}
