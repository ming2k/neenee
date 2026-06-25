//! `VerifyPlanExecutionTool` — spawns an independent verifier subagent
//! with a fixed prompt so the model has a single-call way to audit its
//! own implementation against the approved plan. The verifier reports
//! PASS / PARTIAL / FAIL per section with concrete evidence.
//!
//! Lives in `neenee-agent` (next to `SubagentTool`) because it constructs a
//! subagent via `SubagentTool`, which is an orchestration concern rather
//! than a domain-tool concern.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use neenee_core::{Message, Provider, Role, Tool, ToolAccess};

/// A lightweight pipeline verifier that replaces the old heavy subagent.
///
/// Phase 1: Deterministic Checks — extracts commands from the plan's `Test Plan`
/// section and runs them directly via bash.
/// Phase 2: Lightweight LLM Review — feeds the plan and test outputs to a single
/// `provider.chat()` call for a fast, token-efficient verdict.
pub struct VerifyPlanExecutionTool {
    provider: Arc<dyn Provider>,
    active_plan_path: Arc<Mutex<Option<PathBuf>>>,
}

impl VerifyPlanExecutionTool {
    pub fn new(
        provider: Arc<dyn Provider>,
        _tools: Vec<Arc<dyn Tool>>,
        active_plan_path: Arc<Mutex<Option<PathBuf>>>,
    ) -> Self {
        Self {
            provider,
            active_plan_path,
        }
    }
}

const VERIFY_DESCRIPTION: &str =
    "Run the plan verification pipeline. This automatically extracts and executes \
     commands from the plan's `Test Plan` section, then performs a lightweight LLM \
     review to produce a PASS / PARTIAL / FAIL verdict per section. Call this before \
     declaring the plan complete. Address every PARTIAL and FAIL before reporting \
     completion to the user.";

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
                    "description": "Optional section name or concern to focus the verifier on."
                }
            },
            "additionalProperties": false
        })
    }

    fn access(&self) -> ToolAccess {
        // Technically executes bash commands internally, but it does so via its own
        // deterministic extraction from the approved plan, not from model arguments.
        ToolAccess::Execute
    }

    fn spawns_subagent(&self) -> bool {
        // No longer spawns a heavy SubagentTool subagent
        false
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let focus = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| v.get("focus")?.as_str().map(str::to_string))
            .filter(|s| !s.trim().is_empty());

        let plan_path = self
            .active_plan_path
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .ok_or_else(|| "No active plan to verify. Call plan first.".to_string())?;

        let plan_display = plan_path.display().to_string();
        let plan_content = std::fs::read_to_string(&plan_path)
            .map_err(|e| format!("Could not read plan {}: {}", plan_display, e))?;

        // Phase 1: Deterministic Checks
        // Simple extraction: look for code blocks or list items in the Test Plan section
        let mut test_commands = Vec::new();
        let mut in_test_plan = false;
        let mut in_code_block = false;

        for line in plan_content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("## ") {
                in_test_plan = trimmed.to_lowercase().contains("test plan");
                continue;
            }
            if !in_test_plan {
                continue;
            }

            if trimmed.starts_with("```") {
                in_code_block = !in_code_block;
                continue;
            }

            if in_code_block && !trimmed.is_empty() {
                test_commands.push(trimmed.to_string());
            } else if trimmed.starts_with("- `") && trimmed.ends_with("`") {
                test_commands.push(trimmed[3..trimmed.len() - 1].to_string());
            }
        }

        // Phase 1.5 removed: user-configured verification commands now live on
        // the lifecycle hook bus (ADR-0025) rather than a bespoke
        // `[hooks] pre_complete` table.

        let mut check_results = String::new();
        if test_commands.is_empty() {
            check_results
                .push_str("No deterministic test commands found in the Test Plan section.\n");
        } else {
            for cmd in test_commands {
                check_results.push_str(&format!("> {}\n", cmd));
                match tokio::process::Command::new("bash")
                    .arg("-c")
                    .arg(&cmd)
                    .output()
                    .await
                {
                    Ok(output) => {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        if output.status.success() {
                            check_results.push_str("[PASS]\n");
                            if !stdout.trim().is_empty() {
                                check_results.push_str(&format!("{}\n", stdout.trim()));
                            }
                        } else {
                            check_results
                                .push_str(&format!("[FAIL] Exit code: {}\n", output.status));
                            if !stdout.trim().is_empty() {
                                check_results.push_str(&format!("STDOUT:\n{}\n", stdout.trim()));
                            }
                            if !stderr.trim().is_empty() {
                                check_results.push_str(&format!("STDERR:\n{}\n", stderr.trim()));
                            }
                        }
                    }
                    Err(e) => {
                        check_results.push_str(&format!("[ERROR] Failed to execute: {}\n", e));
                    }
                }
                check_results.push('\n');
            }
        }

        // Phase 2: Lightweight LLM Review
        let focus_clause = match &focus {
            Some(f) => format!("Focus especially on: {f}.\n\n", f = f),
            None => String::new(),
        };

        let prompt = format!(
            "Verify the implementation against the approved plan.\n\n\
             Plan at {path}:\n\
             {plan_content}\n\n\
             Phase 1 Deterministic Check Results:\n\
             {check_results}\n\n\
             Phase 2 Review Task: \n\
             Based on the plan requirements and the deterministic check results above, \
             report the status of each `##` section as PASS, PARTIAL, or FAIL. \n\
             - PASS: The check results prove it is done, or it requires no code changes.\n\
             - PARTIAL: Some parts are done, but evidence is missing for others.\n\
             - FAIL: The check results show failure, or crucial steps are clearly missing.\n\n\
             {focus}End with a one-line VERDICT: PASS / PARTIAL / FAIL summarizing the whole plan.",
            path = plan_display,
            plan_content = plan_content,
            check_results = check_results,
            focus = focus_clause,
        );

        let message = Message::new(Role::User, prompt);
        let result = self
            .provider
            .chat(vec![message])
            .await
            .map_err(|e| format!("LLM review failed: {}", e))?;

        let mut final_output = format!(
            "## Deterministic Checks\n\n{}\n\n## LLM Review\n\n{}",
            check_results, result.content
        );

        // Ensure the output is not overwhelmingly large if tests spam output
        if final_output.len() > 8000 {
            final_output.truncate(8000);
            final_output.push_str("\n...[output truncated due to length]");
        }

        Ok(final_output)
    }
}
