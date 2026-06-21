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
//! plan in context without re-reading the file each turn. The plan markdown
//! is also parsed into [`PlanProgress`] sections so the model can report
//! per-section status through `update_plan_progress`, which the TUI mirrors
//! to a sticky panel above the input box.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{AgentMode, Tool, ToolAccess};

/// Directory (relative to the project root / current working directory) where
/// plan documents live. Mirrors opencode's `.opencode/plans/` convention.
pub const PLANS_DIR: &str = ".neenee/plans";

/// Number of harness turns the plan progress panel may go without an
/// `update_plan_progress` call before the TUI renders it dimmed with a
/// "not updated for N turns" hint. Calibrated so a normal "explore + edit +
/// edit + verify + update" cadence never trips it, but a model that abandons
/// the panel after the first section does. Tunable in future; this is the
/// initial conservative value.
pub const PLAN_STALE_TURN_THRESHOLD: u64 = 5;

/// Shared handle injected into the plan tools so they can flip the agent's
/// mode, record the active plan path, and track per-section progress. The
/// same `Arc`s are held by the host `Agent` type (in `neenee-agent`), so
/// mutations are visible to the agent, the harness, and the TUI immediately.
#[derive(Clone)]
pub struct PlanToolContext {
    mode: Arc<Mutex<AgentMode>>,
    /// Path to the plan file most recently approved via `plan_exit`. Cleared
    /// by `plan_enter` and `/mode plan` since re-entering Plan mode
    /// invalidates the previously approved plan.
    active_plan_path: Arc<Mutex<Option<PathBuf>>>,
    /// Live plan progress snapshot, parsed from the approved plan markdown
    /// and updated by the `update_plan_progress` tool. Drives the sticky
    /// TUI panel above the input box.
    plan_progress: Arc<Mutex<Option<PlanProgress>>>,
    /// Harness turn counter shared with the `Agent`. Used to stamp
    /// `PlanProgress::updated_at_turn` so the TUI stale detector works.
    turn_counter: Arc<Mutex<u64>>,
}

impl PlanToolContext {
    pub fn new(mode: Arc<Mutex<AgentMode>>) -> Self {
        Self {
            mode,
            active_plan_path: Arc::new(Mutex::new(None)),
            plan_progress: Arc::new(Mutex::new(None)),
            turn_counter: Arc::new(Mutex::new(0)),
        }
    }

    /// Build a context that shares state with the provided cells. Used by
    /// the harness so the tools, the `Agent`, and the TUI all see the same
    /// picture.
    pub fn shared(
        mode: Arc<Mutex<AgentMode>>,
        active_plan_path: Arc<Mutex<Option<PathBuf>>>,
        plan_progress: Arc<Mutex<Option<PlanProgress>>>,
        turn_counter: Arc<Mutex<u64>>,
    ) -> Self {
        Self {
            mode,
            active_plan_path,
            plan_progress,
            turn_counter,
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

    /// Current plan progress snapshot, if any. Read by the TUI to render
    /// the sticky panel.
    pub fn plan_progress(&self) -> Option<PlanProgress> {
        self.plan_progress
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn set_plan_progress(&self, progress: Option<PlanProgress>) {
        if let Ok(mut guard) = self.plan_progress.lock() {
            *guard = progress;
        }
    }

    fn current_turn(&self) -> u64 {
        self.turn_counter.lock().map(|g| *g).unwrap_or(0)
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

// ─────────────────────────────────────────────────────────────────────
// Plan progress tracking
//
// When `plan_exit` is approved the agent parses the plan markdown into
// sections (one per `##` heading), all starting as `Pending`. The model
// marks them `InProgress` / `Done` / `Skipped` via the
// `update_plan_progress` tool as it works through the implementation. The
// TUI renders a sticky panel above the input box so the user can see at a
// glance which sections are still outstanding.
//
// The model is the source of truth for status — there is no automatic
// inference from edits. A section that the model forgets to mark stays
// `Pending`, which is honest (the work has not been verified) rather than
// a stale auto-progress that misleads.
// ─────────────────────────────────────────────────────────────────────

/// Lifecycle of one plan section, mirrors how the model reports progress.
///
/// `Pending` is the default; the model moves a section to `InProgress`
/// when it starts working on it and to `Done` when it believes the work
/// for that section is complete. `Skipped` is for sections that turned
/// out not to apply (e.g. a "Migrations" heading when no migration is
/// needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanSectionStatus {
    Pending,
    InProgress,
    Done,
    Skipped,
}

impl PlanSectionStatus {
    /// Single-character glyph used by the TUI panel and any text rendering.
    pub fn glyph(self) -> &'static str {
        match self {
            PlanSectionStatus::Done => "✓",
            PlanSectionStatus::InProgress => "●",
            PlanSectionStatus::Pending => "○",
            PlanSectionStatus::Skipped => "—",
        }
    }

    /// Wire string used by the `update_plan_progress` tool's `status` field.
    pub fn as_str(self) -> &'static str {
        match self {
            PlanSectionStatus::Pending => "pending",
            PlanSectionStatus::InProgress => "in_progress",
            PlanSectionStatus::Done => "done",
            PlanSectionStatus::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSection {
    pub name: String,
    pub status: PlanSectionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanProgress {
    /// Path to the plan file this progress applies to. Matches
    /// `Agent::active_plan_path`.
    pub path: PathBuf,
    pub sections: Vec<PlanSection>,
    /// Harness turn counter value the last time any section's status
    /// changed. The TUI compares this against the current turn counter to
    /// detect a stale panel ("not updated for N turns") — a signal that
    /// the model may have forgotten to call `update_plan_progress` and the
    /// user should not trust the green checks at face value. Bumped by
    /// `plan_exit` (initial seed) and `update_plan_progress` (every call).
    #[serde(default)]
    pub updated_at_turn: u64,
}

impl PlanProgress {
    /// Parse a plan markdown into sections, one per `##` heading. All
    /// sections start as `Pending`. If the plan has no `##` headings a
    /// single synthetic "Plan" section is used so the panel still has
    /// something to render.
    pub fn from_markdown(path: PathBuf, content: &str) -> Self {
        let headings = parse_plan_headings(content);
        let sections = if headings.is_empty() {
            vec![PlanSection {
                name: "Plan".to_string(),
                status: PlanSectionStatus::Pending,
            }]
        } else {
            headings
                .into_iter()
                .map(|name| PlanSection {
                    name,
                    status: PlanSectionStatus::Pending,
                })
                .collect()
        };
        Self {
            path,
            sections,
            updated_at_turn: 0,
        }
    }

    pub fn done_count(&self) -> usize {
        self.sections
            .iter()
            .filter(|s| matches!(s.status, PlanSectionStatus::Done))
            .count()
    }

    /// Update the first section whose name matches (case-insensitive
    /// substring). Returns `true` if a section was updated. Stamps
    /// `updated_at_turn` so the stale detector resets.
    pub fn update(&mut self, section: &str, status: PlanSectionStatus, current_turn: u64) -> bool {
        let needle = section.trim().to_lowercase();
        if needle.is_empty() {
            return false;
        }
        let mut updated = false;
        for s in &mut self.sections {
            if s.name.to_lowercase().contains(&needle) {
                s.status = status;
                updated = true;
            }
        }
        if updated {
            self.updated_at_turn = current_turn;
        }
        updated
    }
}

/// Extract `## ` (level-2) heading text from a markdown plan. Level-1 is
/// reserved for the plan title; levels 3+ are sub-bullets inside a section
/// and do not become progress entries.
fn parse_plan_headings(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("## ") else {
            continue;
        };
        // Skip deeper headings (###, ####, …). strip_prefix("## ") already
        // excludes them by requiring the space, but be defensive in case of
        // `##\t` or `##X` (no space, not a real ATX heading).
        if rest.starts_with('#') {
            continue;
        }
        let name = rest.trim().trim_end_matches('#').trim();
        if !name.is_empty() {
            out.push(name.to_string());
        }
    }
    out
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
        // points at the old file would be misleading, and the progress
        // panel's sections belong to the old plan.
        self.context.set_active_plan_path(None);
        self.context.set_plan_progress(None);
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
        self.context.set_active_plan_path(recorded.clone());

        // Read the plan content so the model has it in the tool result and
        // so we can parse it into a `PlanProgress` snapshot for the sticky
        // panel.
        let content = match &plan_path {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(text) if !text.trim().is_empty() => Some(text),
                Ok(_) => None,
                Err(err) => {
                    // The plan path was provided but the file cannot be read.
                    // Switching to Build mode still succeeded; surface the
                    // error so the model knows the plan body is unavailable
                    // and can choose to re-read or proceed without it.
                    self.context.set_plan_progress(None);
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

        // Parse the plan into sections and seed the progress panel.
        if let Some(text) = &content {
            if let Some(path) = recorded.clone() {
                let mut progress = PlanProgress::from_markdown(path, text);
                progress.updated_at_turn = self.context.current_turn();
                self.context.set_plan_progress(Some(progress));
            }
        } else {
            self.context.set_plan_progress(None);
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

/// Tool invoked by the build agent to mark the status of a plan section.
/// The argument is a free-form substring (case-insensitive) of the section
/// name; the first match is updated. Intentionally loose so the model does
/// not have to echo the exact heading.
pub struct UpdatePlanProgressTool {
    context: PlanToolContext,
    /// Shared harness turn counter, used to stamp `updated_at_turn` on
    /// every successful update so the TUI stale detector can tell whether
    /// the panel has been neglected. The same `Arc` is owned by the
    /// `Agent`, which bumps it at the start of each `execute_turn`.
    turn_counter: Arc<Mutex<u64>>,
}

impl UpdatePlanProgressTool {
    pub fn new(context: PlanToolContext, turn_counter: Arc<Mutex<u64>>) -> Self {
        Self {
            context,
            turn_counter,
        }
    }

    fn current_turn(&self) -> u64 {
        self.turn_counter.lock().map(|g| *g).unwrap_or(0)
    }
}

const UPDATE_PLAN_PROGRESS_DESCRIPTION: &str =
    "Mark a section of the active plan as pending / in_progress / done / skipped. Use this as \
     you work through the implementation so the plan panel reflects current state. The section \
     argument is matched case-insensitively as a substring of any `##` heading in the plan; \
     call this as soon as you start or finish a section rather than batching at the end. Has no \
     effect if there is no active plan (you have not called plan_exit yet).";

#[async_trait]
impl Tool for UpdatePlanProgressTool {
    fn name(&self) -> &str {
        "update_plan_progress"
    }

    fn description(&self) -> &str {
        UPDATE_PLAN_PROGRESS_DESCRIPTION
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "section": {
                    "type": "string",
                    "description": "Substring of the `##` heading to update (case-insensitive)"
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "done", "skipped"],
                    "description": "New status for the section"
                }
            },
            "required": ["section", "status"]
        })
    }

    fn access(&self) -> ToolAccess {
        ToolAccess::Read
    }

    fn allowed_in_plan_mode(&self, _arguments: &str) -> bool {
        // The build agent owns progress updates; in Plan mode there is no
        // active plan to update, so this tool is a no-op there. Allow it
        // through the gate so the model does not see a spurious error if
        // it tries (the call still returns a clear "no active plan"
        // message).
        true
    }

    async fn call(&self, arguments: &str) -> Result<String, String> {
        let value = serde_json::from_str::<serde_json::Value>(arguments)
            .map_err(|e| format!("Invalid JSON: {e}"))?;
        let section = value
            .get("section")
            .and_then(|v| v.as_str())
            .ok_or("Missing 'section'")?;
        let status_str = value
            .get("status")
            .and_then(|v| v.as_str())
            .ok_or("Missing 'status'")?;
        let status = match status_str {
            "pending" => PlanSectionStatus::Pending,
            "in_progress" => PlanSectionStatus::InProgress,
            "done" => PlanSectionStatus::Done,
            "skipped" => PlanSectionStatus::Skipped,
            other => {
                return Err(format!(
                    "Unknown status '{other}'. Use pending / in_progress / done / skipped."
                ));
            }
        };

        let mut guard = self.context.plan_progress.lock().ok();
        let Some(progress) = guard.as_mut().and_then(|g| g.as_mut()) else {
            return Ok(
                "No active plan to update. Call plan_exit to approve a plan first.".to_string(),
            );
        };
        let current_turn = self.current_turn();
        if progress.update(section, status, current_turn) {
            Ok(format!("Updated '{}' to {}.", section, status_str))
        } else {
            Ok(format!(
                "No plan section matched '{}'. Available sections: {}",
                section,
                progress
                    .sections
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
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
    async fn plan_enter_clears_active_plan_path_and_progress() {
        let mode = Arc::new(Mutex::new(AgentMode::Build));
        let context = PlanToolContext::new(mode);
        context.set_plan_progress(Some(PlanProgress::from_markdown(
            PathBuf::from(".neenee/plans/x.md"),
            "## X\n",
        )));
        context.set_active_plan_path(Some(PathBuf::from(".neenee/plans/x.md")));
        assert!(context.active_plan_path().is_some());
        assert!(context.plan_progress().is_some());

        PlanEnterTool::new(context.clone())
            .call("{}")
            .await
            .unwrap();
        assert_eq!(context.active_plan_path(), None);
        assert_eq!(context.plan_progress(), None);
    }

    #[tokio::test]
    async fn plan_exit_echoes_plan_content_and_seeds_progress() {
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

        let progress = context.plan_progress().expect("plan progress seeded");
        let names: Vec<_> = progress.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["Summary", "Key Changes"]);
    }

    #[test]
    fn plan_progress_parses_level_two_headings() {
        let progress = PlanProgress::from_markdown(
            PathBuf::from(".neenee/plans/x.md"),
            "# Title\n\nintro\n\n## Summary\nbody\n\n## Key Changes\n- a\n- b\n\n### Sub\n\n## Test Plan\n",
        );
        let names: Vec<_> = progress.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["Summary", "Key Changes", "Test Plan"]);
        assert_eq!(progress.done_count(), 0);
    }

    #[test]
    fn plan_progress_falls_back_to_synthetic_section_without_headings() {
        let progress = PlanProgress::from_markdown(
            PathBuf::from(".neenee/plans/x.md"),
            "just prose, no headings",
        );
        assert_eq!(progress.sections.len(), 1);
        assert_eq!(progress.sections[0].name, "Plan");
    }

    #[test]
    fn plan_progress_update_matches_case_insensitive_substring() {
        let mut progress = PlanProgress::from_markdown(
            PathBuf::from(".neenee/plans/x.md"),
            "## Key Changes\n## Test Plan\n",
        );
        assert!(progress.update("key changes", PlanSectionStatus::Done, 1));
        assert_eq!(progress.sections[0].status, PlanSectionStatus::Done);
        assert_eq!(progress.sections[1].status, PlanSectionStatus::Pending);
        assert_eq!(progress.done_count(), 1);
        // No match returns false and leaves state untouched.
        assert!(!progress.update("nonexistent", PlanSectionStatus::Done, 1));
    }

    #[tokio::test]
    async fn update_plan_progress_tool_updates_active_plan() {
        let mode = Arc::new(Mutex::new(AgentMode::Build));
        let turn_counter = Arc::new(Mutex::new(7u64));
        let context = PlanToolContext::shared(
            Arc::clone(&mode),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::clone(&turn_counter),
        );
        context.set_plan_progress(Some(PlanProgress::from_markdown(
            PathBuf::from(".neenee/plans/x.md"),
            "## Summary\n## Key Changes\n",
        )));

        let tool = UpdatePlanProgressTool::new(context.clone(), Arc::clone(&turn_counter));
        let body = tool
            .call(r#"{"section":"summary","status":"done"}"#)
            .await
            .unwrap();
        assert!(body.contains("Updated"));
        let progress = context.plan_progress().unwrap();
        assert_eq!(progress.sections[0].status, PlanSectionStatus::Done);
        assert_eq!(progress.sections[1].status, PlanSectionStatus::Pending);
        // The update stamps the current turn counter so the stale detector
        // resets on every successful call.
        assert_eq!(progress.updated_at_turn, 7);
    }

    #[tokio::test]
    async fn update_plan_progress_tool_no_active_plan_returns_hint() {
        let mode = Arc::new(Mutex::new(AgentMode::Build));
        let turn_counter = Arc::new(Mutex::new(0u64));
        let context = PlanToolContext::shared(
            mode,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::clone(&turn_counter),
        );
        let tool = UpdatePlanProgressTool::new(context, turn_counter);
        let body = tool
            .call(r#"{"section":"x","status":"done"}"#)
            .await
            .unwrap();
        assert!(body.contains("No active plan"));
    }
}
