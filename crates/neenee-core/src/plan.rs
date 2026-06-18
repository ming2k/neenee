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
//! [`PlanToolContext`]) that is also owned by the [`crate::Agent`], so a tool
//! call takes effect immediately and is reflected in the next system prompt.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use crate::{AgentMode, Tool, ToolAccess};

/// Directory (relative to the project root / current working directory) where
/// plan documents live. Mirrors opencode's `.opencode/plans/` convention.
pub const PLANS_DIR: &str = ".neenee/plans";

/// Shared handle injected into the plan tools so they can flip the agent's
/// mode. The same `Arc` is held by the [`crate::Agent`], so mutations are
/// visible to the agent and the harness immediately.
#[derive(Clone)]
pub struct PlanToolContext {
    mode: Arc<Mutex<AgentMode>>,
}

impl PlanToolContext {
    pub fn new(mode: Arc<Mutex<AgentMode>>) -> Self {
        Self { mode }
    }

    fn set_mode(&self, mode: AgentMode) {
        if let Ok(mut guard) = self.mode.lock() {
            *guard = mode;
        }
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
/// write/edit tools to opt back in to Plan mode, and by the guard in
/// [`crate::Agent`] to decide whether a write is permitted while planning.
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
     and implement it.";

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
     after you have written a complete plan to .neenee/plans/. Do NOT call it before the plan is \
     finalized or while you still have open questions. The optional `plan_path` should reference \
     the plan file you just wrote so the build agent can follow it.";

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

        self.context.set_mode(AgentMode::Build);

        let reference = match &plan_path {
            Some(path) => format!(" Follow the plan at {}.", path),
            None => String::new(),
        };
        Ok(format!(
            "Plan approved. Switched to Build mode with full tool access. Implement the plan now.{}",
            reference
        ))
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
}
