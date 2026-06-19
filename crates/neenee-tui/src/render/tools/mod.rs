//! Per-tool presentation registry.
//!
//! Each tool maps to a [`ToolPresenter`] that owns how that tool looks in the
//! transcript: the one-line collapsed summary, an optional collapsed preview,
//! and (later) its expanded body renderer. This collapses the per-tool `match
//! name { … }` branches that were previously scattered across `document.rs`
//! (`argument_summary`) and `turn_artifacts.rs` (preview and result rendering)
//! into one place — adding a tool means adding a file and one registry arm.
//!
//! Each presenter owns a collapsed [`summary`](ToolPresenter::summary), an
//! optional collapsed [`collapsed_preview`](ToolPresenter::collapsed_preview),
//! and declarative [`result_kind`](ToolPresenter::result_kind) /
//! [`arg_layout`](ToolPresenter::arg_layout) classifications that drive the
//! expanded body (`turn_artifacts` owns the drawing primitives; this module
//! owns the per-tool decisions). `document.rs` and `turn_artifacts.rs` call the
//! `*_for` entry points below instead of matching on tool names.

mod bash;
mod diff;
mod edit;
mod fallback;
mod grep;
mod meta;
mod read;
mod web;

pub use diff::{DiffLine, DiffOp};
pub(crate) use diff::line_diff;

use ratatui::style::Color;
use serde_json::Value;

use crate::document::ToolStepStatus;
use super::Theme;

/// Resolved run state of a tool step. The model-side source of truth is
/// [`ToolStepStatus`]; this is its presentation classification. Kept separate
/// so the model does not depend on the render layer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolStatus {
    /// No output yet — the call is still in flight.
    Running,
    /// Output present and not an error.
    Ok,
    /// Output present and starting with `Error` (matches the convention used
    /// by `core` tools, which return `Err(String)` rendered as `Error: …`).
    Failed,
    /// The call was aborted before producing a result (e.g. user interrupt).
    Cancelled,
}

impl ToolStatus {
    /// Classify a tool step from its stored lifecycle. This is the primary
    /// constructor now that the model carries an explicit status.
    pub fn from_status(status: ToolStepStatus) -> Self {
        match status {
            ToolStepStatus::Running => ToolStatus::Running,
            ToolStepStatus::Ok => ToolStatus::Ok,
            ToolStepStatus::Failed => ToolStatus::Failed,
            ToolStepStatus::Cancelled => ToolStatus::Cancelled,
        }
    }

    /// Theme color used for the status rail / card accent. Centralizes the
    /// status→color mapping that card headers, sticky pins, and sub-agent cards
    /// previously each duplicated.
    pub fn color(self, theme: &Theme) -> Color {
        match self {
            ToolStatus::Running => theme.info(),
            ToolStatus::Ok => theme.ok(),
            ToolStatus::Failed => theme.err(),
            // No dedicated cancelled accent: reuse the muted tone so a
            // cancelled card reads as inert rather than as a fresh failure.
            ToolStatus::Cancelled => theme.muted(),
        }
    }
}

/// How a tool's result output is rendered in the expanded card body. The
/// drawing primitives live in `turn_artifacts`; presenters only declare which
/// one applies, so the per-tool dispatch lives in one place (the registry).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResultKind {
    /// Line-numbered code block (default / unknown tools, `read_file`).
    Code,
    /// Directory / glob listing.
    Listing,
    /// Ripgrep-style `path:line:match` rendering.
    Grep,
    /// Shell output with `$ command` framing and exit/section markers.
    Bash,
    /// A red/green line diff derived from the tool's arguments (edit/write)
    /// rather than its output.
    Diff,
}

/// How a tool's arguments are rendered in the expanded card body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArgLayout {
    /// No arguments section — the header summary already captures the inputs
    /// (the default for tools whose summary names their key argument, e.g.
    /// `Read path`, `Grep "pat" in path`). Edit/write also use this: the path
    /// is in the header and the content is in the diff.
    None,
    /// A single wrapped command string (bash), shown under an `Arguments`
    /// label without the `key:` prefix.
    Command,
    /// Flat `key: value` lines. Used for unknown / MCP tools whose generic
    /// header doesn't spell out the arguments.
    KeyValue,
}

/// Emphasis level for a collapsed-preview line. The renderer maps these to
/// theme colors so presenters stay decoupled from the palette.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PreviewTone {
    /// Primary text (e.g. the `$ command` line).
    Primary,
    /// Secondary text (output / match excerpts).
    Muted,
    /// Faint text (truncation markers like `…`).
    Faint,
}

/// One line of a collapsed-state preview rendered under the header band. The
/// `text` is the intended display content (already ANSI-stripped and
/// line-limited by the presenter); the renderer handles width truncation,
/// padding, and color.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewLine {
    pub text: String,
    pub tone: PreviewTone,
}

/// A read-only view of a tool step, handed to a [`ToolPresenter`]. Arguments
/// are pre-parsed into a JSON object by the registry entry points so each
/// presenter can pull typed fields without re-parsing.
pub struct ToolView<'a> {
    pub name: &'a str,
    pub args: &'a serde_json::Map<String, Value>,
    pub output: Option<&'a str>,
}

impl ToolView<'_> {
    /// Fetch a string-valued argument, or `None` when absent / non-string.
    pub fn str(&self, key: &str) -> Option<&str> {
        self.args.get(key).and_then(Value::as_str)
    }
}

/// How a single tool renders in the transcript. Stateless: implementors are
/// zero-sized unit structs resolved via [`presenter_for`].
pub trait ToolPresenter {
    /// One-line, human-readable summary for the collapsed header. The registry
    /// truncates the result to the header budget, so implementors only need to
    /// truncate individual interpolated fields where it improves readability.
    fn summary(&self, view: &ToolView) -> String;

    /// Lines shown under the header while collapsed. Defaults to none; tools
    /// like bash / grep / read override this in step 5 to lift key output into
    /// the collapsed view.
    fn collapsed_preview(&self, _view: &ToolView) -> Vec<PreviewLine> {
        Vec::new()
    }

    /// Which result renderer the expanded body uses for this tool's output.
    fn result_kind(&self) -> ResultKind {
        ResultKind::Code
    }

    /// How the expanded body renders this tool's arguments.
    fn arg_layout(&self) -> ArgLayout {
        ArgLayout::None
    }
}

/// Resolve the presenter for a tool name, falling back to a generic presenter
/// for unknown / MCP tools.
pub fn presenter_for(name: &str) -> &'static dyn ToolPresenter {
    match name {
        "read_file" => &read::ReadPresenter,
        "edit_file" => &edit::EditPresenter,
        "write_file" => &edit::WritePresenter,
        "bash" => &bash::BashPresenter,
        "grep" => &grep::GrepPresenter,
        "glob" => &grep::GlobPresenter,
        "list_dir" => &grep::ListDirPresenter,
        "webfetch" => &web::WebFetchPresenter,
        "websearch" => &web::WebSearchPresenter,
        "todo" => &meta::TodoPresenter,
        "task" => &meta::TaskPresenter,
        "use_skill" => &meta::UseSkillPresenter,
        "create_project" => &meta::CreateProjectPresenter,
        "goal_checklist" => &meta::GoalChecklistPresenter,
        _ => &fallback::FallbackPresenter,
    }
}

/// Header budget for collapsed summaries (chars). Matches the previous
/// `argument_summary` cap so the migration is visually identical.
const SUMMARY_BUDGET: usize = 72;

/// Build the collapsed summary for a tool step from its raw JSON arguments.
///
/// Parses the arguments once: non-object / invalid JSON falls back to a
/// truncated raw string (preserving the pre-refactor behavior for malformed
/// or scalar argument payloads). This is the entry point step 2 will call from
/// `document.rs` in place of `argument_summary`.
pub fn summary_for(name: &str, arguments: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(arguments).ok();
    let Some(obj) = parsed.as_ref().and_then(Value::as_object) else {
        return truncate(arguments, SUMMARY_BUDGET);
    };
    let view = ToolView {
        name,
        args: obj,
        output: None,
    };
    truncate(&presenter_for(name).summary(&view), SUMMARY_BUDGET)
}

/// Build the collapsed-state preview lines for a finished tool step. Parses
/// the arguments once and hands the presenter a [`ToolView`] carrying the
/// output; presenters that don't override `collapsed_preview` return none.
/// This is the entry point `turn_artifacts` calls for the under-header preview.
pub fn collapsed_preview_for(name: &str, arguments: &str, output: &str) -> Vec<PreviewLine> {
    let parsed: Option<Value> = serde_json::from_str(arguments).ok();
    let empty = serde_json::Map::new();
    let obj = parsed.as_ref().and_then(Value::as_object).unwrap_or(&empty);
    let view = ToolView {
        name,
        args: obj,
        output: Some(output),
    };
    presenter_for(name).collapsed_preview(&view)
}

/// Build the renderable diff for a tool step whose [`ToolPresenter::result_kind`]
/// is [`ResultKind::Diff`]. Reads `old_string`/`new_string` for `edit_file` and
/// the full `content` for `write_file` (an all-added diff). Returns an empty
/// diff for any other tool or malformed arguments.
pub fn diff_lines_for(name: &str, arguments: &str) -> Vec<DiffLine> {
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return Vec::new();
    };
    let get = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or("");
    match name {
        "edit_file" => diff::line_diff(get("old_string"), get("new_string")),
        "write_file" => diff::line_diff("", get("content")),
        _ => Vec::new(),
    }
}

/// Truncate to `max_chars` characters, appending an ellipsis when clipped.
/// Local copy of `document::truncate`; the document-side copy is removed in
/// step 2 once `argument_summary` is gone.
pub(crate) fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{}...", prefix)
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(name: &str, args: serde_json::Value) -> String {
        summary_for(name, &args.to_string())
    }

    #[test]
    fn dispatches_known_tools_to_named_summaries() {
        assert_eq!(
            summary("read_file", serde_json::json!({"path": "src/main.rs"})),
            "Read src/main.rs"
        );
        assert_eq!(
            summary("edit_file", serde_json::json!({"path": "a.rs"})),
            "Edit a.rs"
        );
        assert_eq!(
            summary("write_file", serde_json::json!({"path": "a.rs"})),
            "Write a.rs"
        );
        assert_eq!(
            summary(
                "grep",
                serde_json::json!({"pattern": "ToolStep", "path": "src"})
            ),
            "Grep \"ToolStep\" in src"
        );
    }

    #[test]
    fn bash_summary_uses_first_command_line() {
        assert_eq!(
            summary("bash", serde_json::json!({"command": "cargo build\nmore"})),
            "Run cargo build"
        );
    }

    #[test]
    fn unknown_tool_leads_with_cleaned_name_then_key() {
        assert_eq!(
            summary("mcp__foo__bar", serde_json::json!({"query": "hello"})),
            "foo / bar hello"
        );
        // No recognizable argument: just the cleaned name.
        assert_eq!(
            summary("mcp__foo__bar", serde_json::json!({"unknown": 1})),
            "foo / bar"
        );
    }

    #[test]
    fn non_object_arguments_truncate_raw() {
        assert_eq!(summary_for("bash", "not json"), "not json");
    }

    #[test]
    fn from_status_classifies_every_lifecycle_including_cancelled() {
        use crate::document::ToolStepStatus;
        assert_eq!(
            ToolStatus::from_status(ToolStepStatus::Running),
            ToolStatus::Running
        );
        assert_eq!(ToolStatus::from_status(ToolStepStatus::Ok), ToolStatus::Ok);
        assert_eq!(
            ToolStatus::from_status(ToolStepStatus::Failed),
            ToolStatus::Failed
        );
        // The new terminal state must round-trip so an aborted card can never
        // be misclassified as still running.
        assert_eq!(
            ToolStatus::from_status(ToolStepStatus::Cancelled),
            ToolStatus::Cancelled
        );
    }
}
