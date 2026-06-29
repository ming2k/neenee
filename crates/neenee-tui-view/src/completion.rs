//! Completion-menu data types shared with the app shell.
//!
//! The matching *logic* (slash/path candidate derivation) lives in the shell
//! (`neenee_code::tui::completion`, an `impl App`); only these render-facing
//! data types live here so the completion menu renderer can draw them.

/// Kind of completion menu the input box is currently offering. Drives the
/// keyboard shortcuts that cycle / accept entries: Tab, ↑/↓, and (for slash
/// only) plain Enter on a unique prefix. Path mentions only complete via Tab
/// so a plain Enter still sends the message as typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompletionKind {
    /// No completion menu is active.
    #[default]
    None,
    /// `/command` and subcommand completion (replaces the whole input).
    Slash,
    /// `@path` file mention completion (splices into the input at the cursor).
    Path,
}

/// A single completion candidate rendered in the completion menu. The
/// `replace_start..replace_end` byte range is the slice of the current input
/// that gets overwritten by `label` when the candidate is accepted, so slash
/// commands (which replace the whole input) and inline `@path` mentions
/// (which replace only the `@prefix` token) share one accept path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Text to insert at the replace range.
    pub label: String,
    /// Hint shown to the right of the label (e.g. "Set pursuit", "dir", "1.2k").
    pub description: String,
    /// Byte offset in `App::input` where the replacement starts.
    pub replace_start: usize,
    /// Byte offset in `App::input` where the replacement ends.
    pub replace_end: usize,
}

impl Completion {
    /// Build a slash-command style completion that replaces the whole input
    /// (`replace_start = 0`, `replace_end = input_len`).
    pub fn whole_input(label: &str, description: &str, input_len: usize) -> Completion {
        Completion {
            label: label.to_string(),
            description: description.to_string(),
            replace_start: 0,
            replace_end: input_len,
        }
    }
}
