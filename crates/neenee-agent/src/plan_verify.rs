//! `VerifyPlanExecutionTool` — spawns an independent verifier sub-agent
//! with a fixed prompt so the model has a single-call way to audit its
//! own implementation against the approved plan. The verifier reports
//! PASS / PARTIAL / FAIL per section with concrete evidence.
//!
//! Lives in `neenee-agent` (next to `TaskTool`) because it constructs a
//! sub-agent via `TaskTool`, which is an orchestration concern rather
//! than a domain-tool concern.

use std::sync::Arc;

use async_trait::async_trait;
use neenee_core::{Provider, Tool, ToolAccess, VERIFY};

use crate::plan::PlanToolContext;
use crate::task_tool::TaskTool;

/// Spawn an independent verifier sub-agent that re-reads the active plan
/// and the current state of the workspace, then reports PASS / PARTIAL /
/// FAIL for each section with concrete evidence.
///
/// Internally constructs a `TaskTool` with the same provider + read-only
/// toolset as the parent agent, so the verifier inherits the parent's
/// capabilities without write access. The call still streams as a nested
/// tool step in the TUI (since the underlying `TaskTool` emits SubTask
/// events through `call_structured_with_events`).
pub struct VerifyPlanExecutionTool {
    task: Arc<TaskTool>,
    context: PlanToolContext,
}

impl VerifyPlanExecutionTool {
    /// `provider` and `tools` should be the same values the parent
    /// `TaskTool` was constructed with, so the verifier inherits the same
    /// capabilities. `context` is the shared plan context so we can pull
    /// the active plan path. The internal `TaskTool` binds the `VERIFY`
    /// profile so the verifier gets read tools *plus* command execution
    /// (tests/builds/type-checks) but no file-write, no user interaction,
    /// and no recursion. See ADR-0012.
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        context: PlanToolContext,
    ) -> Self {
        Self {
            task: Arc::new(TaskTool::new(provider, tools, &VERIFY)),
            context,
        }
    }
}

const VERIFY_DESCRIPTION: &str =
    "Spawn an independent verifier sub-agent that re-reads the active plan and the current state \
     of the workspace, then reports PASS / PARTIAL / FAIL for each section with concrete evidence \
     (file paths, command output). Call this before declaring the plan complete. The verifier \
     runs with a clean context and read-only tools, so it is not biased by what you wrote during \
     implementation. Address every PARTIAL and FAIL before reporting completion to the user. \
     Optional `plan_path` overrides the active plan path; if omitted, the active plan is used.";

#[async_trait]
impl Tool for VerifyPlanExecutionTool {
    fn name(&self) -> &str {
        "verify_plan_execution"
    }

    fn description(&self) -> &str {
        VERIFY_DESCRIPTION
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "focus": {
                    "type": "string",
                    "description": "Optional section name or concern to focus the verifier on. \
                                    When omitted the verifier walks every section of the plan."
                }
            },
            "additionalProperties": false
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }

    /// `verify_plan_execution` delegates to an internal `TaskTool`, i.e. it
    /// spawns a sub-agent; profiles exclude it alongside `task`.
    fn spawns_subagent(&self) -> bool {
        true
    }

    fn allowed_in_plan_mode(&self, _arguments: &str) -> bool {
        // Verification is a Build-mode concern (it audits implementation
        // work). In Plan mode there is nothing to verify, so block it via
        // the default gate by returning false. The agent's gate check
        // uses `access() == Read` as the default, which would allow this
        // tool — so we explicitly opt back into the gate here.
        false
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        // Parse optional `focus`.
        let focus = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| v.get("focus")?.as_str().map(str::to_string))
            .filter(|s| !s.trim().is_empty());

        // Resolve plan path: explicit override → active plan → error.
        let plan_path = self
            .context
            .active_plan_path()
            .ok_or_else(|| "No active plan to verify. Call plan_exit first.".to_string())?;

        let plan_display = plan_path.display().to_string();

        // Build the verifier task prompt. The *role* framing (independent,
        // unbiased, may run commands, must not edit, non-interactive) lives
        // in the `VERIFY` profile's system prompt; this user prompt carries
        // only the task-specific contract: the plan path, the per-section
        // PASS/PARTIAL/FAIL procedure, and the final verdict line.
        let focus_clause = match &focus {
            Some(f) => format!("Focus especially on: {f}.\n\n", f = f),
            None => String::new(),
        };
        let prompt = format!(
            "Verify the implementation against the approved plan.\n\n\
             Step 1. Read the plan at {path}.\n\n\
             Step 2. For each `##` section in the plan, examine the current state of the \
             workspace (read_file, grep, glob, list_dir, bash for tests / builds / \
             type-checks). Look for concrete evidence that the section's work was done.\n\n\
             Step 3. Report each section as PASS, PARTIAL, or FAIL with the evidence:\n\
             - PASS: name a file or command output that confirms the section is done.\n\
             - PARTIAL: name what is done and what is missing.\n\
             - FAIL: name what is wrong or absent.\n\n\
             {focus}Step 4. End with a one-line VERDICT: PASS / PARTIAL / FAIL summarizing \
             the whole plan. Do not echo the implementer's claims — only what you \
             directly observed.",
            path = plan_display,
            focus = focus_clause,
        );

        let task_args = serde_json::json!({
            "description": format!("Verify plan at {}", plan_display),
            "prompt": prompt,
        });
        // Delegate to the underlying TaskTool. We use `call` (the simple
        // variant) so the result comes back as a string the parent agent
        // can read; the trade-off is that the nested tool step in the TUI
        // will not stream sub-agent tokens live. A future iteration can
        // implement call_structured_with_events here too.
        let arguments_string = serde_json::to_string(&task_args)
            .map_err(|e| format!("could not serialize verifier task: {e}"))?;
        self.task.call(&arguments_string).await
    }
}
