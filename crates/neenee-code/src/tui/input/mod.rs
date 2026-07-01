//! Input handling: keyboard and mouse events mapped to semantic actions.

use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};

use crate::tui::layout::{LayoutMap, SemanticCursor};
use crate::tui::selection::SelectionDrag;

#[derive(Default)]
pub struct InputContext {
    pub active_modal: super::Modal,
    pub is_responding: bool,
    /// Which completion menu (slash command vs `@path` mention) is active, or
    /// `None` when no menu is shown. Drives Tab/↑/↓ cycling and the
    /// slash-specific Enter auto-accept. Mirrors [`super::CompletionKind`].
    pub completion_kind: super::CompletionKind,
    pub suggestion_count: usize,
    pub has_exact_suggestion: bool,
    pub suggestion_index: Option<usize>,
    pub permission_confirm_always: bool,
    /// Whether the inline permission sheet is expanded to "Details". Drives
    /// whether ↑/↓ in the compose zone scroll the details body or the
    /// transcript behind it.
    pub permission_show_details: bool,
    /// Whether the view is zoomed into an envoy task (focus stack non-empty).
    pub in_envoy_view: bool,
    /// Whether the view is inside a `/btw` side conversation (ADR-0017). Esc
    /// and Ctrl+C return to the primary transcript instead of interrupting.
    pub in_side_view: bool,
    /// Whether a transcript step/action target currently holds keyboard focus.
    ///
    /// This is the TUI's only navigation state: there is no separate "browse
    /// mode". When `true`, a step is highlighted in the transcript and the
    /// keys that would otherwise edit/scroll instead act on that step — `↑`/`↓`
    /// (and `Ctrl+↑`/`Ctrl+↓`) cycle the focused step, `Enter` activates it,
    /// and `Esc` clears the focus. When `false` every key has its ordinary
    /// input-box meaning. Mirrors `App::focused_target.is_some()`.
    pub has_focused_target: bool,
    /// Whether the send queue holds at least one staged user message. While
    /// true, `↑` recalls the most-recently-queued message instead of walking
    /// input history.
    pub has_queued: bool,
    /// Whether the history modal's search sub-layer is active. Only meaningful
    /// while [`Self::active_modal`] is [`super::Modal::HistorySearch`]: `false`
    /// is browse mode (typing is inert, `/` enters search), `true` borrows the
    /// composer line as the live fuzzy query. Mirrors `App::history_search`.
    pub history_searching: bool,
    /// Whether the model picker's search sub-layer is active. Only meaningful
    /// while [`Self::active_modal`] is [`super::Modal::Provider`]: `false` is
    /// browse mode (typing is inert, `/` enters search, `*`/`e` act on the row),
    /// `true` borrows the composer line as the live fuzzy query. Mirrors
    /// `App::model_search`.
    pub model_searching: bool,
    /// Whether the provider picker is in its **stage-2** model sub-list (drilled
    /// into a multi-model provider). Only meaningful while [`Self::active_modal`]
    /// is [`super::Modal::Provider`]: it routes Esc to "back to the provider
    /// list" instead of "close the modal", and gates the stage-1-only `*`/`e`
    /// shortcuts. Mirrors `App::picker_provider.is_some()`.
    pub picker_in_models_stage: bool,
    /// Focused field index of the provider editor, or `None` when that modal is
    /// not open. Every visible field borrows the composer line (Name / Base URL /
    /// Token as plain text, Model as a live filter), so printable keys always edit
    /// it. Mirrors `App::custom_field` while [`Self::active_modal`] is
    /// [`super::Modal::CustomProvider`].
    pub custom_provider_field: Option<u8>,
    /// Focused field of the key editor (`Modal::ModelEditor`): `0` = API key,
    /// `1` = effort selector, `2` = thinking toggle. `None` when that modal is
    /// not open. Drives ←/→ effort cycling (field 1) and Space thinking toggle
    /// (field 2). Mirrors `App::editor_field` while the key editor is open.
    pub editor_field: Option<u8>,
    /// Whether the Question modal's synthetic "Other" free-text row is the
    /// highlighted row. Only meaningful while [`Self::active_modal`] is
    /// [`super::Modal::Question`]: when `true` the modal owns a text-input
    /// surface, so printable keys (including Space) insert into the "Other"
    /// field instead of toggling an option. Mirrors
    /// `App::question.is_some_and(|q| q.is_other_highlighted())`.
    pub question_other_highlighted: bool,
}

impl InputContext {
    /// Whether any provider-editor field is focused. Every visible field borrows
    /// the composer line (Name / Base URL / Token as plain text, Model as a live
    /// filter), so all are text fields now (ADR-0046 removed the Thinking toggle).
    fn custom_text_field_focused(&self) -> bool {
        self.custom_provider_field.is_some()
    }
}

/// Whether `modal` currently treats the composer line as an editable free-text
/// field — the surfaces where printable keys, Backspace, and the readline
/// editing family (Ctrl+A/E/W/U/K, Alt+B/F/D, …) act on the input buffer. The
/// history and model-picker modals only qualify while their search sub-layer is
/// active (`history_searching` / `model_searching`); in browse mode those keys
/// are inert so `/` can open search and stray letters never mutate a buffer the
/// user isn't editing.
fn edits_input_field(
    modal: super::Modal,
    history_searching: bool,
    model_searching: bool,
    custom_text_field: bool,
) -> bool {
    match modal {
        super::Modal::None
        | super::Modal::ModelEditor
        | super::Modal::AddModel
        | super::Modal::InputInjection => true,
        super::Modal::Provider => model_searching,
        super::Modal::HistorySearch => history_searching,
        // The provider editor edits the composer line on every visible field
        // (Name / Base URL / Token / Model all borrow it).
        super::Modal::CustomProvider => custom_text_field,
        _ => false,
    }
}

/// Result of processing an input event.
#[derive(Debug, PartialEq)]
pub enum InputAction {
    /// Nothing to do.
    None,
    /// Quit the application.
    Quit,
    /// Send a chat message.
    SendChat(String),
    /// Send a slash command.
    SendSlash(String),
    /// Run a shell command directly (the `!` prefix path). The `!` is
    /// stripped and the remaining text is executed through the `bash` tool
    /// without an LLM roundtrip.
    SendShell(String),
    /// Activate a model from the `/provider` picker: the default model when the
    /// filter is empty (fast path), otherwise the highlighted filtered row.
    /// Falls through to the API-key setup modal when the target has no key.
    ProviderPickerActivate,
    /// Step back from the picker's stage-2 model sub-list to the stage-1 provider
    /// list (Esc while drilled into a multi-model provider).
    ProviderPickerBack,
    /// Toggle the favorite flag on the highlighted picker row.
    ProviderPickerToggleFavorite,
    /// Open the unified provider editor (`e`) for the highlighted picker row.
    OpenModelEditor,
    /// Submit the unified provider editor: persist the entered key / model-id and
    /// activate the target model.
    SubmitModelEditor,
    /// Cycle focus between the editor's fields (API key ↔ effort).
    ModelEditorNextField,
    /// Cycle the effort selector (←/→) on the Anthropic key editor's effort
    /// field. Carries a delta of ±1; wraps around the five effort levels.
    ModelEditorEffortCycle {
        delta: i8,
    },
    /// Toggle extended thinking on/off (Space) on the Anthropic key editor's
    /// thinking field. Orthogonal to effort.
    ModelEditorThinkingToggle,
    /// Submit the custom-provider editor → `AgentRequest::AddProvider`.
    SubmitCustomProvider,
    /// Cancel the custom-provider editor and return to the provider picker.
    CancelCustomProvider,
    /// Move focus to the next / previous field of the custom-provider editor
    /// (`Tab` / `BackTab`), wrapping at the ends.
    CustomProviderNextField,
    CustomProviderPrevField,
    /// Move the suggestion highlight in the provider editor's Model filter field
    /// with `↑` / `↓`. `forward` = down.
    MoveCustomSuggestion {
        forward: bool,
    },
    /// Move the provider-template chooser selection with `↑` / `↓`. `forward` = down.
    MoveProviderTemplate {
        forward: bool,
    },
    /// Open the provider editor seeded from the highlighted template (`Enter`).
    SelectProviderTemplate,
    /// Cancel the provider-template chooser and return to the provider picker.
    CancelProviderTemplate,
    /// Remove the highlighted model from a custom provider's stage-2 list (`d`).
    ProviderPickerRemoveModel,
    /// Delete the entire highlighted custom provider from the stage-1 list
    /// (`Shift+D`). Built-in providers are ignored by the handler.
    DeleteProvider,
    /// Move the add-model overlay's suggestion highlight with `↑` / `↓`.
    MoveAddModel {
        forward: bool,
    },
    /// Submit the add-model overlay → `AgentRequest::AddProviderModel`.
    SubmitAddModel,
    /// Cancel the add-model overlay and return to the stage-2 model list.
    CancelAddModel,
    /// Interrupt current operation.
    Interrupt,
    /// Open models modal.
    OpenProvider,
    /// Open the input-history modal (Ctrl+R). Opens in browse mode — a plain
    /// newest-first list; `/` then enters the search sub-layer.
    OpenHistory,
    /// Open the command palette (slash commands).
    OpenCommands,
    /// Open the help / keybindings modal.
    OpenHelp,
    /// Open the permissions manager modal: a centered list of cached "always
    /// allow" rules with per-row revoke and clear-all. Reached via the
    /// `/permissions` slash command (intercepted locally, never sent to the
    /// backend). `/permissions clear` still goes to the backend.
    OpenPermissions,
    /// Open the tools manager modal: a centered, selectable list of every
    /// session tool with a `Space` toggle. Reached via the `/tools` slash
    /// command (intercepted locally, never sent to the backend). The request is
    /// never forwarded — it only opens the overlay.
    OpenTools,
    /// Open the MCP manager modal: a centered, selectable list of every
    /// configured MCP server with `Space` toggle and `r` reconnect. Reached via
    /// the `/mcp` slash command (intercepted locally, never sent to the
    /// backend). The request is never forwarded — it only opens the overlay.
    OpenMcp,
    /// Open the skills modal: a centered, selectable list of every loaded
    /// skill with a per-row detail expansion and an `r` reload. Reached via
    /// the `/skills` slash command (intercepted locally, never sent to the
    /// backend; `/skills list` / `/skills reload` with args still forward).
    /// The request is never forwarded — it only opens the overlay.
    OpenSkills,
    /// Toggle the detail expansion of the selected skill row in the skills
    /// modal. Bound to `Enter`.
    SkillsToggleDetail,
    /// Reload the skill registry from the skills modal by forwarding
    /// `/skills reload` to the backend. Bound to `r`.
    SkillsReload,
    /// Open the config manager modal: a centered list of configurable
    /// categories (Nudge, …). Reached via the `/config` slash command
    /// (intercepted locally, never sent to the backend). `Enter` / `Space`
    /// on a category drills into its sub-page.
    OpenConfig,
    /// Connect/disconnect the selected MCP server in the MCP manager modal.
    /// Bound to `Space`.
    McpToggle,
    /// Reconnect the selected MCP server in the MCP manager modal. Bound to `r`.
    McpReconnect,
    /// Revoke the selected "always allow" rule in the permissions manager
    /// modal. Bound to `Space`.
    PermissionsActivate,
    /// Clear every cached "always allow" rule. Bound to `c` in the
    /// permissions manager modal.
    PermissionsClearAll,
    /// Drill into the selected config category's sub-page (from
    /// [`Modal::Config`](super::Modal::Config)). Bound to `Enter` / `Space`.
    ConfigActivate,
    /// Return from a config sub-page to the config root. Bound to `Esc`
    /// inside a sub-page (a second `Esc` closes the modal).
    ConfigBack,
    /// Toggle the nudge master switch (`enabled`) in the nudge sub-page.
    /// Bound to `Space` when the enabled row is selected.
    ConfigNudgeToggle,
    /// Adjust the selected nudge threshold by `delta` (±1). Bound to `←`
    /// (delta = -1) and `→` (delta = +1) in the nudge sub-page. The harness
    /// persists the new config and replies with
    /// `AgentResponse::NudgeConfigUpdated`.
    ConfigNudgeAdjust {
        delta: i32,
    },
    /// Apply the selected transcript layout strategy in the layout sub-page.
    /// Bound to `Enter` / `Space`. The harness persists the choice to
    /// `config.toml` and replies with `AgentResponse::TuiLayoutUpdated`.
    ConfigLayoutApply,
    /// Move the tool-selection cursor in the session-context dashboard when it
    /// still hosts the tools list, and in the tools manager modal otherwise.
    /// `forward` = down, else up.
    SessionSelect {
        forward: bool,
    },
    /// Toggle the selected tool's enabled flag in the tools manager modal.
    /// Bound to `Space`.
    SessionActivate,
    /// Open the currently-selected session in the sessions picker.
    OpenSelectedSession,
    /// Drill into the selected provider/model line in the TokenReport bill
    /// (the per-model detail). Bound to `Enter`.
    TokenReportActivate,
    /// Drill into / back out of a section in the Debug inspector modal.
    /// Bound to `Enter`.
    DebugActivate,
    /// Delete the currently-selected session in the sessions picker.
    DeleteSelectedSession,
    /// Close any modal.
    CloseModal,
    /// Scroll up.
    ScrollUp,
    /// Scroll down.
    ScrollDown,
    /// Scroll up by one viewport page.
    ScrollPageUp,
    /// Scroll down by one viewport page.
    ScrollPageDown,
    /// Scroll to the very top.
    ScrollTop,
    /// Scroll to the very bottom and re-engage auto-follow.
    ScrollBottom,
    /// Copy current selection.
    CopySelection,
    /// Plain Ctrl+C: copy selection, clear input, or arm quit. It never
    /// interrupts a running turn — only double-Esc does.
    CtrlC,
    /// Toggle expanded details for semantic tool steps.
    ToggleToolSteps,
    /// Move keyboard focus to the next activatable target. When no target is
    /// focused yet, focuses the first (oldest) step. Driven by `Ctrl+↓` and by
    /// `↓` while a step is already focused.
    FocusNextTarget,
    /// Move keyboard focus to the previous activatable target. When no target
    /// is focused yet, focuses the last (nearest-to-prompt) step. Driven by
    /// `Ctrl+↑` and by `↑` while a step is already focused.
    FocusPrevTarget,
    /// Activate the current keyboard-focused target (`Enter`).
    ActivateFocusedTarget,
    /// Clear the keyboard-focused target, returning every key to its ordinary
    /// input-box meaning. Triggered by `Esc` while a step is focused.
    ClearFocusedTarget,
    /// Paste from the system clipboard (image or text). Resolved by the app
    /// loop, which reads the clipboard asynchronously.
    Paste,
    /// Terminal-level bracketed paste. The text payload is already available;
    /// the app loop routes it through the same chip-or-inline logic as
    /// [`InputAction::Paste`].
    BracketedPaste(String),
    /// Input character.
    InsertChar(char),
    /// Delete character before cursor.
    Backspace,
    /// Cycle suggestion forward.
    SuggestNext,
    /// Cycle suggestion backward.
    SuggestPrev,
    /// Accept the next/previous completion item by index without closing the
    /// popup. Used by `Tab`, which cycles through candidates one splice at a
    /// time. The popup re-renders against the spliced input so the user can
    /// keep cycling.
    AcceptSuggestion(String),
    /// Like [`InputAction::AcceptSuggestion`] but the popup is closed
    /// afterwards. Used by `Enter` (both the slash-prefix auto-accept and the
    /// highlighted-item path). The harness latches a `completion_dismissed`
    /// flag so the popup stays hidden until the next `InsertChar` /
    /// `Backspace`, matching the expectation that pressing Enter "finishes"
    /// the current completion.
    CommitSuggestion(String),
    /// Dismiss the completion popup without accepting anything. Used by `Esc`
    /// when a slash/path completion menu is open. Latches the same
    /// `completion_dismissed` flag as [`InputAction::CommitSuggestion`] so the
    /// popup stays hidden until the next edit clears the latch.
    CloseCompletion,
    /// Navigate history up.
    HistoryPrev,
    /// Navigate history down.
    HistoryNext,
    /// Recall the most-recently-queued message: pop it off the send queue,
    /// remove its transcript marker, and load its text (and any pasted
    /// images) back into the input box for editing. Only dispatched while
    /// the queue is non-empty; otherwise `HistoryPrev` is used.
    RecallQueued,
    /// Accept the focused entry in the Ctrl+R history modal (Enter, in either
    /// browse or search mode): insert it into the input box and close the modal.
    /// The message is not sent — the user can edit and press Enter again to ship
    /// it.
    HistoryInsert,
    /// Toggle the "full prompt" preview of the selected history entry inside
    /// the Ctrl+R modal. In preview mode the body shows the entry's complete
    /// (possibly multi-line) text; ↑/↓ re-renders the newly focused entry.
    HistoryTogglePreview,
    /// Enter the history modal's search sub-layer (`/` in browse mode): start
    /// borrowing the composer line as a live fuzzy query and re-rank the list.
    HistoryEnterSearch,
    /// Leave the history modal's search sub-layer (first Esc while searching):
    /// clear the query and return to the full reverse-chronological browse
    /// list. A second Esc then closes the modal.
    HistoryExitSearch,
    /// Enter the model picker's search sub-layer (`/` in browse mode): start
    /// borrowing the composer line as a live fuzzy query and re-rank the list.
    ModelEnterSearch,
    /// Leave the model picker's search sub-layer (first Esc while searching):
    /// clear the query and return to the full browse list. A second Esc then
    /// closes the modal.
    ModelExitSearch,
    /// Select modal item up.
    ModalUp,
    /// Select modal item down.
    ModalDown,
    /// Submit the selected permission decision.
    PermissionSubmit,
    /// Reject the active permission request.
    PermissionReject,
    /// Return from the always-allow confirmation step.
    PermissionBack,
    /// Scroll the expanded "Details" body of the permission sheet up a row.
    PermissionDetailsUp,
    /// Scroll the expanded "Details" body of the permission sheet down a row.
    PermissionDetailsDown,
    /// Move the selection up inside the question modal.
    QuestionUp,
    /// Move the selection down inside the question modal.
    QuestionDown,
    /// Toggle/select the currently highlighted question option. For
    /// multi-select this flips the highlighted row on/off (Space); for
    /// single-select it is a harmless no-op because the highlight already
    /// *is* the live selection.
    QuestionToggle,
    /// Submit the question modal answers (Enter). For single-select this
    /// submits the highlighted option; for multi-select it submits the
    /// whole toggle set.
    QuestionSubmit,
    /// Cancel the question modal.
    QuestionCancel,
    /// Submit the input-injection panel's typed text (L3.5 β).
    InputSubmit,
    /// Cancel the input-injection panel (run the command non-interactively).
    InputCancel,
    /// Select a question option by its 1-based index.
    QuestionSelect(usize),
    /// Insert a character into the question modal's "Other" free-text field.
    QuestionInsertChar(char),
    /// Delete a character from the question modal's "Other" free-text field.
    QuestionBackspace,
    /// Start selection at screen coordinates.
    SelectionStart {
        x: u16,
        y: u16,
    },
    /// Update selection to screen coordinates.
    SelectionUpdate {
        x: u16,
        y: u16,
    },
    /// End selection.
    SelectionEnd,
    /// Select entire block at coordinates (e.g. triple-click).
    SelectBlock {
        x: u16,
        y: u16,
    },
    /// Right-click at screen coordinates. Opens a context/detail view for the
    /// interactive element under the cursor (e.g. a tool step's full output).
    RightClick {
        x: u16,
        y: u16,
    },
    /// Mouse pointer moved to screen coordinates (hover tracking). Used to
    /// drive hover affordances on clickable elements like reasoning-trace
    /// headers. Suppressed while an overlay modal is open.
    Hover {
        x: u16,
        y: u16,
    },
    /// Leave the current envoy view and return to the parent.
    ExitEnvoy,
    /// Leave the `/btw` side conversation and return to the primary transcript
    /// (ADR-0017). Mapped from Esc / Ctrl+C while the side view is focused.
    ExitSideView,
    /// Move to the previous sibling envoy task.
    PrevSibling,
    /// Move to the next sibling envoy task.
    NextSibling,
    /// Terminal was resized (SIGWINCH). The event loop forces a redraw and
    /// re-emits `EnableMouseCapture` so the crossterm parser's internal state
    /// machine is resynced: a resize frequently splits an in-flight SGR mouse
    /// sequence across `event::read()` boundaries, and crossterm then hands the
    /// leftover bytes back as spurious `KeyCode::Char` events (issue #854/#668).
    /// Re-arming capture is the cleanest way to get both sides back in step.
    TerminalResized,
}

/// Insert a literal newline at the cursor position, but only in modals that
/// accept free-text input. Used by the Alt+Enter and Ctrl+J multi-line
/// entry bindings (plain Enter sends the message).
fn insert_newline(input: &mut String, cursor_position: &mut usize, active_modal: super::Modal) {
    if matches!(active_modal, super::Modal::None) {
        let byte_pos = input
            .char_indices()
            .map(|(i, _)| i)
            .nth(*cursor_position)
            .unwrap_or(input.len());
        input.insert(byte_pos, '\n');
        *cursor_position += 1;
    }
}

/// Move the caret to the start of the current logical line.
///
/// Used by the `Home` key and `Ctrl+A` (readline convention). For a
/// single-line buffer this is the very start; for a multi-line buffer it
/// stops just past the nearest preceding newline. `cursor_position` is a
/// char index, so the newline search is translated back to chars.
fn cursor_line_start(input: &str, cursor_position: &mut usize) {
    let char_count = input.chars().count();
    let char_pos = (*cursor_position).min(char_count);
    let byte_offset = input
        .char_indices()
        .nth(char_pos)
        .map(|(i, _)| i)
        .unwrap_or(input.len());
    let before = &input[..byte_offset];
    if let Some(rel) = before.rfind('\n') {
        let after_newline = rel + '\n'.len_utf8();
        *cursor_position = before[..after_newline].chars().count();
    } else {
        *cursor_position = 0;
    }
}

/// Move the caret to the end of the current logical line.
///
/// Used by the `End` key and `Ctrl+E`. For a multi-line buffer the caret
/// stops just before the next newline rather than at the end of the whole
/// buffer, matching the readline/standard-editor behaviour users expect.
fn cursor_line_end(input: &str, cursor_position: &mut usize) {
    let char_count = input.chars().count();
    let char_pos = (*cursor_position).min(char_count);
    let byte_offset = input
        .char_indices()
        .nth(char_pos)
        .map(|(i, _)| i)
        .unwrap_or(input.len());
    let after = &input[byte_offset..];
    if let Some(rel) = after.find('\n') {
        let end_byte = byte_offset + rel;
        *cursor_position = input[..end_byte].chars().count();
    } else {
        *cursor_position = char_count;
    }
}

/// Find the start char index of the previous whitespace-delimited word.
/// Skips trailing whitespace (including newlines), then removes the
/// contiguous run of non-whitespace before the caret.  Returns 0 when
/// the caret is at the very start of the buffer; otherwise the returned
/// position can cross newline boundaries.
///
/// Matches readline's `unix-word-rubout` (Ctrl+W) and the
/// `backward-word` / `backward-kill-word` motions users expect from
/// shells and editors.
fn prev_word_start(input: &str, cursor_position: usize) -> usize {
    let chars: Vec<char> = input.chars().collect();
    let mut i = cursor_position.min(chars.len());
    // Skip whitespace between caret and the previous word (includes \n).
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    // Skip the contiguous run of non-whitespace that forms the word.
    while i > 0 && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

/// Find the end char index of the next whitespace-delimited word.
/// Skips leading whitespace (including newlines), then skips the
/// contiguous run of non-whitespace.  Returns `input.len()` when the
/// caret is at the very end; otherwise the returned position can cross
/// newline boundaries.
///
/// Matches readline's `kill-word` (Alt+D) and `forward-word` motions.
fn next_word_end(input: &str, cursor_position: usize) -> usize {
    let chars: Vec<char> = input.chars().collect();
    let mut i = cursor_position.min(chars.len());
    // Skip whitespace between caret and the next word (includes \n).
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    // Skip the contiguous run of non-whitespace that forms the word.
    while i < chars.len() && !chars[i].is_whitespace() {
        i += 1;
    }
    i
}

/// Char index of the start of the current logical line, mirroring
/// [`cursor_line_start`] but operating on a borrowed char slice so the
/// word-boundary helpers can call it without re-allocating.
fn cursor_line_start_char(chars: &[char], cursor_position: usize) -> usize {
    let char_pos = cursor_position.min(chars.len());
    if let Some(rel) = chars[..char_pos].iter().rposition(|&c| c == '\n') {
        rel + 1
    } else {
        0
    }
}

/// Char index of the end of the current logical line, mirroring
/// [`cursor_line_end`] on a borrowed char slice.
fn cursor_line_end_char(chars: &[char], cursor_position: usize) -> usize {
    let char_pos = cursor_position.min(chars.len());
    if let Some(rel) = chars[char_pos..].iter().position(|&c| c == '\n') {
        char_pos + rel
    } else {
        chars.len()
    }
}

/// Try to move the caret up one logical line in a multi-line buffer,
/// preserving the column (char offset within the line) clamped to the
/// previous line's length. Returns `true` and updates `cursor_position`
/// when there is a line above; returns `false` (without moving) when the
/// caret is already on the first line, so the caller can fall through to
/// history navigation.
///
/// This is what lets `↑` walk lines inside a multi-line draft instead of
/// always jumping to the previous history entry — only at the top line
/// does it hand off to input history.
fn cursor_line_up(input: &str, cursor_position: &mut usize) -> bool {
    let chars: Vec<char> = input.chars().collect();
    let pos = (*cursor_position).min(chars.len());
    let line_start = cursor_line_start_char(&chars, pos);
    if line_start == 0 {
        return false;
    }
    let col = pos - line_start;
    // The char just before `line_start` is the newline that ends the
    // previous line; the previous line's text lives in [prev_start, prev_end).
    let prev_end = line_start - 1;
    let prev_start = if let Some(rel) = chars[..prev_end].iter().rposition(|&c| c == '\n') {
        rel + 1
    } else {
        0
    };
    let target = prev_start + col.min(prev_end - prev_start);
    *cursor_position = target;
    true
}

/// Try to move the caret down one logical line, mirroring
/// [`cursor_line_up`]. Returns `false` (without moving) when the caret is
/// already on the last line, so `↓` hands off to history navigation there.
fn cursor_line_down(input: &str, cursor_position: &mut usize) -> bool {
    let chars: Vec<char> = input.chars().collect();
    let pos = (*cursor_position).min(chars.len());
    let line_end = cursor_line_end_char(&chars, pos);
    if line_end >= chars.len() {
        return false;
    }
    let line_start = cursor_line_start_char(&chars, pos);
    let col = pos - line_start;
    // `line_end` is the index of the newline; the next line starts after it.
    let next_start = line_end + 1;
    let next_end = if let Some(rel) = chars[next_start..].iter().position(|&c| c == '\n') {
        next_start + rel
    } else {
        chars.len()
    };
    let target = next_start + col.min(next_end - next_start);
    *cursor_position = target;
    true
}

/// SGR mouse-sequence leakage guard.
///
/// Background: crossterm sometimes fails to reassemble a mouse report that
/// arrives split across two `event::read()` calls (issue #854/#668). When that
/// happens the bytes of an SGR mouse sequence (`ESC [ < btn ; col ; row M/m`)
/// are handed back as a stream of ordinary `Event::Key` / `KeyCode::Char`
/// events: `Esc`, `[`, `<`, `6`, `5`, `;`, … `M`. Because the composer's
/// `KeyCode::Char` arm inserts every printable char into the input box, the
/// split sequence shows up as garbage text (e.g. `;25M[<35;56;25M…`). This is
/// observed across terminals on resize, fast trackpad scrolling, and inside
/// multiplexers (tmux/screen/xterm.js).
///
/// `SgrLeakGuard` is a tiny state machine fed one event at a time. While it is
/// tracking what looks like a leaked SGR sequence it reports [`Feed::Drop`],
/// swallowing the fragments *before* they reach `process_event` and mutate the
/// input line. The pattern is deliberately narrow so a genuine `Esc` keypress
/// still works: it only enters the suppression state on the `ESC [ <` prefix
/// (the mouse-sequence intro) — a bare `Esc` with nothing following stays a
/// real key.
///
/// The guard is best-effort at the symbol layer; the primary defense is the
/// reader-thread reassembler in `event_loop::InputReader`, which keeps whole
/// sequences intact in the common case so the guard rarely sees anything.
#[derive(Debug, Default, Clone)]
pub struct SgrLeakGuard {
    state: SgrState,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum SgrState {
    /// Idle: no suspicious prefix seen.
    #[default]
    Idle,
    /// Saw `ESC`; waiting to see if `[` follows (start of a CSI).
    SawEsc,
    /// Saw `ESC [`; waiting for `<` (SGR mouse) — anything else aborts.
    SawCsi,
    /// Inside an SGR mouse payload after `ESC [ <`. Swallow digits/`;` and the
    /// terminating `M`/`m`, then return to idle.
    InSgr,
}

/// Outcome of feeding one event to the guard.
pub enum Feed {
    /// The event is not part of a leaked sequence — handle it normally.
    Accept,
    /// The event looks like part of a leaked SGR sequence — drop it silently.
    Drop,
}

impl SgrLeakGuard {
    /// Feed one event. Returns whether the caller should still process it.
    /// Pure: performs no I/O and never mutates the input line.
    pub fn feed(&mut self, event: &Event) -> Feed {
        let Event::Key(key) = event else {
            // A non-key event (Mouse/Resize/Paste/Focus) always resets the
            // tracker: if crossterm *did* manage to parse a whole mouse event
            // we clearly are no longer mid-leak, and a resize is exactly the
            // disruption that starts one, so resync here.
            self.state = SgrState::Idle;
            return Feed::Accept;
        };
        let c = match key.code {
            KeyCode::Char(c) => c,
            // Esc as a control key (not a printable char) — a possible SGR
            // prefix start. Treat it as the intro byte.
            KeyCode::Esc => '\x1b',
            _ => {
                // Any other real key (Backspace, arrows, F-keys, Enter, …)
                // breaks a half-formed sequence.
                self.state = SgrState::Idle;
                return Feed::Accept;
            }
        };

        // The match returns (next_state, is_part_of_sequence). A character is
        // "part of a leaked sequence" — and therefore dropped — only when it is
        // a payload byte of an `ESC [ < …` mouse report (the `[`, `<`, digits,
        // `;`, and the `M`/`m` terminator). A bare `Esc` keypress is *never*
        // dropped: it is a real control key (never inserted as text), it is the
        // double-Esc interrupt path, and it clears focus / closes modals.
        // Dropping it silently — as the first version of this guard did — broke
        // double-Esc interrupt entirely. Instead we *deliver* the Esc (Accept)
        // and merely enter the tracking state, so the `[` that follows a
        // genuine leak still starts suppression without ever swallowing the Esc
        // itself.
        let (next, part) = match (self.state, c) {
            // A bare Esc from idle: deliver it, but arm the tracker so a
            // following `[` still opens a leak window.
            (SgrState::Idle, '\x1b') => (SgrState::SawEsc, false),
            // `ESC [`: the `[` is the first byte that can only be leak noise
            // (a real `[` key arrives as a printable char from idle), so start
            // suppressing here. The leading Esc was already delivered above.
            (SgrState::SawEsc, '[') => (SgrState::SawCsi, true),
            // The SGR mouse intro. Once we see this prefix the rest of the
            // payload is unambiguously a mouse report fragment.
            (SgrState::SawCsi, '<') => (SgrState::InSgr, true),
            // Terminators: the final byte of the report.
            (SgrState::InSgr, 'M') | (SgrState::InSgr, 'm') => (SgrState::Idle, true),
            // Continuation bytes of the payload.
            (SgrState::InSgr, '0'..='9' | ';' | '\u{1b}') => (SgrState::InSgr, true),
            // Aborted sequences: the bytes we tentatively buffered were not an
            // SGR mouse report after all. Hand the *current* char back for
            // normal processing (it is genuine input) and resync to idle.
            (SgrState::InSgr, _) => (SgrState::Idle, false),
            // A second Esc while one is already buffered: this is a genuine
            // double-Esc (the double-Esc interrupt pattern), not a leak — a
            // real SGR sequence has `[` next, never another Esc. Deliver it and
            // stay armed so the next non-`[` char cleanly aborts to idle.
            (SgrState::SawEsc, '\x1b') => (SgrState::SawEsc, false),
            (SgrState::SawEsc | SgrState::SawCsi, _) => (SgrState::Idle, false),
            (SgrState::Idle, _) => (SgrState::Idle, false),
        };
        self.state = next;
        if part { Feed::Drop } else { Feed::Accept }
    }

    /// Reset the tracker. Called after a resize so a fresh, fully-armed mouse
    /// session starts from a known state.
    pub fn reset(&mut self) {
        self.state = SgrState::Idle;
    }

    /// Whether the tracker is currently idle (not mid-sequence). Used by the
    /// reader-thread reassembler to know when a drain has completed.
    pub fn is_idle(&self) -> bool {
        self.state == SgrState::Idle
    }
}

/// Process a crossterm event into a high-level action.
///
/// `input` and `cursor_position` are mutable because some events modify them directly.
pub fn process_event(
    event: Event,
    input: &mut String,
    cursor_position: &mut usize,
    context: InputContext,
    drag: &mut SelectionDrag,
) -> InputAction {
    match event {
        Event::Mouse(mouse) => {
            let x = mouse.column;
            let y = mouse.row;
            match mouse.kind {
                // The wheel always scrolls the body of whatever modal owns the
                // surface (or the transcript when none does). The event loop's
                // ScrollUp/ScrollDown handlers translate it per-modal — including
                // the question modal, whose body scroll is decoupled from the ↑/↓
                // highlight so wheeling browses a long option list without moving
                // the selection cursor.
                MouseEventKind::ScrollUp => InputAction::ScrollUp,
                MouseEventKind::ScrollDown => InputAction::ScrollDown,
                MouseEventKind::Down(MouseButton::Left) => {
                    // The permission sheet replaces the composer but leaves the
                    // transcript above fully interactive, so a click there can
                    // still toggle steps, drag-select text, follow links, etc.
                    // The sheet itself has no click targets (its buttons are
                    // keyboard-driven) and covers only the composer/hint slot,
                    // which has no registered transcript region, so a press
                    // landing on it resolves to nothing and stays inert.
                    if matches!(
                        context.active_modal,
                        super::Modal::None | super::Modal::Permission
                    ) {
                        drag.start(SemanticCursor::new(0, 0, 0));
                        InputAction::SelectionStart { x, y }
                    } else if context.active_modal == super::Modal::Question {
                        InputAction::SelectionStart { x, y }
                    } else if context.active_modal.dismissable_by_outside_click() {
                        // A dismissable modal owns this click — forward it as
                        // a SelectionStart without arming a drag; the event
                        // loop's SelectionStart handler closes the modal when
                        // the press lands outside the panel (and consumes it
                        // either way so it never reaches the transcript
                        // behind the backdrop). Entry modals keep swallowing.
                        InputAction::SelectionStart { x, y }
                    } else {
                        InputAction::None
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if drag.active
                        && matches!(
                            context.active_modal,
                            super::Modal::None | super::Modal::Permission
                        )
                    {
                        InputAction::SelectionUpdate { x, y }
                    } else {
                        InputAction::None
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if drag.active {
                        drag.end();
                        InputAction::SelectionEnd
                    } else {
                        InputAction::None
                    }
                }
                // Triple-click detection would need a timer; for now we map
                // middle click to "select block" as a quick approximation.
                MouseEventKind::Down(MouseButton::Middle) => {
                    if matches!(
                        context.active_modal,
                        super::Modal::None | super::Modal::Permission
                    ) {
                        InputAction::SelectBlock { x, y }
                    } else {
                        InputAction::None
                    }
                }
                MouseEventKind::Down(MouseButton::Right) => {
                    // Right-click opens detail/feedback for interactive
                    // transcript elements. Allowed during a permission prompt
                    // because the transcript stays interactive.
                    if matches!(
                        context.active_modal,
                        super::Modal::None | super::Modal::Permission
                    ) {
                        InputAction::RightClick { x, y }
                    } else {
                        InputAction::None
                    }
                }
                // Mouse motion (reported because `EnableMouseCapture` requests
                // mode 1003 "all motion"). Forwarded on the main view and
                // during a permission prompt so hover affordances keep working
                // on the still-interactive transcript; blocked behind other
                // overlay modals.
                MouseEventKind::Moved => {
                    if matches!(
                        context.active_modal,
                        super::Modal::None | super::Modal::Permission
                    ) {
                        InputAction::Hover { x, y }
                    } else {
                        InputAction::None
                    }
                }
                _ => InputAction::None,
            }
        }
        Event::Key(key) => {
            // Copy selection with Ctrl+Shift+C or Cmd+C
            if key.code == KeyCode::Char('c')
                && (key.modifiers.contains(KeyModifiers::SHIFT)
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::SUPER))
            {
                return InputAction::CopySelection;
            }
            // Plain Ctrl+C: semantic copy/clear/quit, resolved by the app.
            // It does not interrupt a running task — only double-Esc does.
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return InputAction::CtrlC;
            }
            if key.code == KeyCode::Char('t')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && context.active_modal == super::Modal::None
            {
                return InputAction::ToggleToolSteps;
            }

            match key.code {
                KeyCode::Esc => {
                    if context.active_modal == super::Modal::Permission {
                        if context.permission_confirm_always {
                            InputAction::PermissionBack
                        } else if context.has_focused_target {
                            // A step is focused behind the permission sheet:
                            // Esc clears the focus and returns to the sheet
                            // rather than rejecting outright — a second Esc
                            // decides it.
                            InputAction::ClearFocusedTarget
                        } else {
                            InputAction::PermissionReject
                        }
                    } else if context.active_modal == super::Modal::Question {
                        InputAction::QuestionCancel
                    } else if context.active_modal == super::Modal::ProviderTemplate {
                        // Esc cancels the template chooser back to the provider
                        // picker it was opened from.
                        InputAction::CancelProviderTemplate
                    } else if context.active_modal == super::Modal::CustomProvider {
                        // Esc cancels the custom-provider editor and returns to the
                        // provider picker it was opened from.
                        InputAction::CancelCustomProvider
                    } else if context.active_modal == super::Modal::AddModel {
                        // Esc cancels the add-model overlay back to the stage-2
                        // model list it was opened from.
                        InputAction::CancelAddModel
                    } else if context.active_modal == super::Modal::InputInjection {
                        InputAction::InputCancel
                    } else if context.active_modal == super::Modal::HistorySearch
                        && context.history_searching
                    {
                        // Two-stage Esc: leave the search sub-layer back to the
                        // browse list first; the next Esc (browse mode) closes.
                        InputAction::HistoryExitSearch
                    } else if context.active_modal == super::Modal::Provider
                        && context.model_searching
                    {
                        // Same two-stage Esc as the history modal: the first Esc
                        // drops the model picker's search sub-layer back to the
                        // browse list; the next Esc (browse mode) closes.
                        InputAction::ModelExitSearch
                    } else if context.active_modal == super::Modal::Provider
                        && context.picker_in_models_stage
                    {
                        // In the stage-2 model sub-list (browse mode): Esc steps
                        // back to the stage-1 provider list rather than closing.
                        InputAction::ProviderPickerBack
                    } else if context.active_modal == super::Modal::ConfigNudge {
                        // Esc in the nudge sub-page returns to the config root
                        // rather than closing the whole modal.
                        InputAction::ConfigBack
                    } else if context.active_modal == super::Modal::ConfigLayout {
                        // Esc in the layout sub-page returns to the config root.
                        InputAction::ConfigBack
                    } else if context.active_modal != super::Modal::None {
                        InputAction::CloseModal
                    } else if context.in_side_view {
                        // `/btw` side view: Esc returns to the primary
                        // transcript (ADR-0017). Takes priority over focus
                        // clearing and completion so one Esc always exits.
                        InputAction::ExitSideView
                    } else if context.in_envoy_view {
                        // Envoy zoom: Esc returns to the parent view.
                        // Takes priority over focus clearing so one Esc
                        // always exits the zoom, even if a step inside the
                        // envoy is keyboard-focused.
                        InputAction::ExitEnvoy
                    } else if context.has_focused_target {
                        // A transcript step is focused: Esc clears the focus
                        // and hands every key back to the input box.
                        InputAction::ClearFocusedTarget
                    } else if context.completion_kind != super::CompletionKind::None
                        && context.suggestion_count > 0
                    {
                        // A completion popup (slash command or `@path`) is
                        // open: Esc dismisses it without touching the input
                        // text. The popup stays hidden until the next edit
                        // clears the dismissal latch, so Esc then ↑/↓ walks
                        // history instead of suggestions.
                        InputAction::CloseCompletion
                    } else if context.is_responding {
                        InputAction::Interrupt
                    } else {
                        InputAction::None
                    }
                }
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if context.active_modal == super::Modal::None {
                        InputAction::OpenHistory
                    } else {
                        InputAction::None
                    }
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if context.active_modal == super::Modal::None {
                        InputAction::OpenCommands
                    } else {
                        InputAction::None
                    }
                }
                KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if context.active_modal == super::Modal::None {
                        InputAction::OpenHelp
                    } else {
                        InputAction::None
                    }
                }
                // Ctrl+M: open the models modal. In a raw terminal Ctrl+M is
                // byte-identical to Enter, so this only fires when the Kitty
                // enhanced-keyboard protocol is active (enabled in `run_tui`).
                // On terminals without it, Ctrl+M arrives as Enter and leaves
                // input behavior untouched — no regression.
                KeyCode::Char('m') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if context.active_modal == super::Modal::None {
                        InputAction::OpenProvider
                    } else {
                        InputAction::None
                    }
                }
                // Alt+Enter / Ctrl+J: insert a literal newline so the input
                // box supports multi-line drafting. Plain Enter sends the
                // message, so these are the only multi-line entry paths.
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                    insert_newline(input, cursor_position, context.active_modal);
                    InputAction::None
                }
                KeyCode::Enter => match context.active_modal {
                    super::Modal::Provider => InputAction::ProviderPickerActivate,
                    super::Modal::ModelEditor => InputAction::SubmitModelEditor,
                    super::Modal::ProviderTemplate => InputAction::SelectProviderTemplate,
                    super::Modal::CustomProvider => InputAction::SubmitCustomProvider,
                    super::Modal::AddModel => InputAction::SubmitAddModel,
                    super::Modal::HistorySearch => InputAction::HistoryInsert,
                    super::Modal::Sessions => InputAction::OpenSelectedSession,
                    super::Modal::Permission => InputAction::PermissionSubmit,
                    super::Modal::Question => InputAction::QuestionSubmit,
                    super::Modal::InputInjection => InputAction::InputSubmit,
                    super::Modal::Help => InputAction::CloseModal,
                    super::Modal::Tools => InputAction::CloseModal,
                    super::Modal::Mcp => InputAction::CloseModal,
                    super::Modal::Skills => InputAction::SkillsToggleDetail,
                    super::Modal::Permissions => InputAction::CloseModal,
                    super::Modal::Config => InputAction::ConfigActivate,
                    super::Modal::ConfigNudge => InputAction::ConfigNudgeToggle,
                    super::Modal::ConfigLayout => InputAction::ConfigLayoutApply,
                    super::Modal::Activity => InputAction::CloseModal,
                    super::Modal::TokenReport => InputAction::TokenReportActivate,
                    super::Modal::Debug => InputAction::DebugActivate,
                    super::Modal::None => {
                        if context.has_focused_target {
                            return InputAction::ActivateFocusedTarget;
                        }
                        // Slash-only: pressing Enter on a unique prefix
                        // auto-accepts the first suggestion rather than
                        // sending `/go` as a (rejected) command. Path
                        // mentions skip this so Enter still sends the message.
                        if context.completion_kind == super::CompletionKind::Slash
                            && context.suggestion_count > 0
                            && context.suggestion_index.is_none()
                            && !context.has_exact_suggestion
                        {
                            return InputAction::CommitSuggestion("0".to_string());
                        }
                        // If a completion menu is open and the user has
                        // highlighted an item (via ↑/↓ or Tab cycling),
                        // Enter accepts that item rather than sending the
                        // partial input. Applies to both slash commands and
                        // `@path` mentions. An explicit highlight is a
                        // stronger signal than the raw text in the box, so
                        // this wins over the exact-match slash fast path
                        // below.
                        if let Some(i) = context.suggestion_index
                            && context.completion_kind != super::CompletionKind::None
                            && context.suggestion_count > 0
                        {
                            return InputAction::CommitSuggestion(i.to_string());
                        }
                        let text = std::mem::take(input);
                        *cursor_position = 0;
                        if text.starts_with('/') {
                            // Match on the trimmed text so a slash command
                            // typed with a trailing space (e.g. the user
                            // typed `/provider ` themselves) still hits the
                            // exact-match arm instead of silently no-op'ing.
                            match text.trim() {
                                "/provider" => InputAction::OpenProvider,
                                "/permissions" => InputAction::OpenPermissions,
                                "/tools" => InputAction::OpenTools,
                                "/mcp" => InputAction::OpenMcp,
                                "/skills" => InputAction::OpenSkills,
                                "/config" => InputAction::OpenConfig,
                                "/exit" => InputAction::Quit,
                                _ => InputAction::SendSlash(text),
                            }
                        } else if let Some(rest) = text.strip_prefix('!') {
                            // `!<command>` runs the rest directly through the
                            // bash tool, bypassing the LLM. Leading whitespace
                            // after the bang is tolerated so `! ls` matches
                            // the shell convention. A bare `!` is a no-op.
                            let command = rest.trim_start().to_string();
                            if command.is_empty() {
                                InputAction::None
                            } else {
                                InputAction::SendShell(command)
                            }
                        } else if !text.is_empty() {
                            InputAction::SendChat(text)
                        } else {
                            InputAction::None
                        }
                    }
                },
                KeyCode::Tab => {
                    if context.active_modal == super::Modal::None
                        && context.completion_kind != super::CompletionKind::None
                        && context.suggestion_count > 0
                    {
                        // A slash/path suggestion menu is open: accept the
                        // next entry.
                        let next = match context.suggestion_index {
                            Some(i) => (i + 1) % context.suggestion_count,
                            None => 0,
                        };
                        InputAction::AcceptSuggestion(next.to_string())
                    } else if context.active_modal == super::Modal::ModelEditor {
                        // Tab cycles focus between the editor's API-key and
                        // model-id fields.
                        InputAction::ModelEditorNextField
                    } else if context.active_modal == super::Modal::CustomProvider {
                        // Tab advances through the editor's visible fields.
                        InputAction::CustomProviderNextField
                    } else if context.active_modal == super::Modal::HistorySearch {
                        // Tab toggles the full-prompt preview of the selected
                        // entry. The fuzzy filter is a free-text field, so an
                        // alpha key would clash; Tab is the unambiguous gesture.
                        InputAction::HistoryTogglePreview
                    } else {
                        // No completion open and no modal field to cycle: Tab
                        // is a no-op. (There is no zone switching: focus is
                        // toggled with Ctrl+Up/Ctrl-Down, never Tab.)
                        InputAction::None
                    }
                }
                KeyCode::BackTab => {
                    // Shift+Tab steps backward through the custom-provider
                    // editor's fields; elsewhere it is a no-op (transcript focus
                    // uses Ctrl+Up/Ctrl-Down, not Tab).
                    if context.active_modal == super::Modal::CustomProvider {
                        InputAction::CustomProviderPrevField
                    } else {
                        InputAction::None
                    }
                }
                // Ctrl+J: alias for Alt+Enter — insert a literal newline.
                KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    insert_newline(input, cursor_position, context.active_modal);
                    InputAction::None
                }
                // Ctrl+V: paste from the system clipboard. Active on the
                // main prompt and in the free-text modals (provider editor,
                // provider picker filter, history search) which borrow the
                // input line as a single-line field. The app loop reads the
                // clipboard asynchronously and either attaches an image,
                // inserts the text at the cursor (main prompt), or splices it
                // inline into the modal field (modals).
                KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) {
                        InputAction::Paste
                    } else {
                        InputAction::None
                    }
                }
                // Ctrl+B: move the caret back one character (readline
                // `backward-char`). Mirrors Left and sits alongside the
                // Ctrl+A / Ctrl+E line-motion family. Active wherever free text
                // is edited; a no-op elsewhere so it never inserts a literal
                // 'b' or scrolls.
                KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) && *cursor_position > 0
                    {
                        *cursor_position -= 1;
                    }
                    InputAction::None
                }
                // Ctrl+A: move the caret to the start of the current line
                // (readline convention). Works wherever free text is being
                // edited — the main prompt in Compose zone and the free-text
                // modals. Outside those (Browse zone, read-only modals) it is
                // a no-op so it never inserts a literal 'a' or scrolls.
                KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) {
                        cursor_line_start(input, cursor_position);
                    }
                    InputAction::None
                }
                // Ctrl+E: move the caret to the end of the current line.
                KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) {
                        cursor_line_end(input, cursor_position);
                    }
                    InputAction::None
                }
                // Ctrl+W: delete the previous whitespace-delimited word
                // (readline `unix-word-rubout`). Skips trailing whitespace
                // then removes the contiguous run of non-whitespace before
                // the caret, crossing newline boundaries.
                // No-op outside free-text surfaces so it never closes a
                // modal or inserts a literal 'w'.
                KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) {
                        let start = prev_word_start(input, *cursor_position);
                        if start < *cursor_position {
                            let start_byte = input
                                .char_indices()
                                .nth(start)
                                .map(|(i, _)| i)
                                .unwrap_or(input.len());
                            let end_byte = input
                                .char_indices()
                                .nth(*cursor_position)
                                .map(|(i, _)| i)
                                .unwrap_or(input.len());
                            input.replace_range(start_byte..end_byte, "");
                            *cursor_position = start;
                            return InputAction::Backspace;
                        }
                    }
                    InputAction::None
                }
                // Ctrl+U: delete from the caret to the start of the current
                // logical line (readline `unix-line-discard`). Multi-line
                // drafts only lose the current line; Ctrl+C still clears the
                // whole buffer when the user wants a full wipe.
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) {
                        let mut start = *cursor_position;
                        cursor_line_start(input, &mut start);
                        if start < *cursor_position {
                            let start_byte = input
                                .char_indices()
                                .nth(start)
                                .map(|(i, _)| i)
                                .unwrap_or(input.len());
                            let end_byte = input
                                .char_indices()
                                .nth(*cursor_position)
                                .map(|(i, _)| i)
                                .unwrap_or(input.len());
                            input.replace_range(start_byte..end_byte, "");
                            *cursor_position = start;
                            return InputAction::Backspace;
                        }
                    }
                    InputAction::None
                }
                // Ctrl+K: delete from the caret to the end of the current
                // logical line (readline `kill-line`). Stops at the next
                // newline so multi-line drafts keep their other lines.
                KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) {
                        let mut end = *cursor_position;
                        cursor_line_end(input, &mut end);
                        if end > *cursor_position {
                            let start_byte = input
                                .char_indices()
                                .nth(*cursor_position)
                                .map(|(i, _)| i)
                                .unwrap_or(input.len());
                            let end_byte = input
                                .char_indices()
                                .nth(end)
                                .map(|(i, _)| i)
                                .unwrap_or(input.len());
                            input.replace_range(start_byte..end_byte, "");
                            return InputAction::Backspace;
                        }
                    }
                    InputAction::None
                }
                // Alt+B: jump back one word (readline `backward-word`).
                KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                    if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) {
                        *cursor_position = prev_word_start(input, *cursor_position);
                    }
                    InputAction::None
                }
                // Alt+F: jump forward one word (readline `forward-word`).
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                    if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) {
                        *cursor_position = next_word_end(input, *cursor_position);
                    }
                    InputAction::None
                }
                // Alt+D: delete the next whitespace-delimited word (readline
                // `kill-word`). Symmetric counterpart to Ctrl+W.
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::ALT) => {
                    if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) {
                        let end = next_word_end(input, *cursor_position);
                        if end > *cursor_position {
                            let start_byte = input
                                .char_indices()
                                .nth(*cursor_position)
                                .map(|(i, _)| i)
                                .unwrap_or(input.len());
                            let end_byte = input
                                .char_indices()
                                .nth(end)
                                .map(|(i, _)| i)
                                .unwrap_or(input.len());
                            input.replace_range(start_byte..end_byte, "");
                            return InputAction::Backspace;
                        }
                    }
                    InputAction::None
                }
                KeyCode::Char(c) => {
                    // Sibling envoy navigation works in both zones (it is a
                    // envoy view feature, not a typing-navigation thing)
                    // but only when no text is being composed.
                    if context.active_modal == super::Modal::None
                        && context.in_envoy_view
                        && input.is_empty()
                    {
                        match c {
                            '[' => return InputAction::PrevSibling,
                            ']' => return InputAction::NextSibling,
                            _ => {}
                        }
                    }
                    if context.active_modal == super::Modal::Question
                        && c == ' '
                        && !context.question_other_highlighted
                    {
                        return InputAction::QuestionToggle;
                    }
                    // Space inside the tools manager toggles the selected
                    // tool's enabled flag.
                    if context.active_modal == super::Modal::Tools && c == ' ' {
                        return InputAction::SessionActivate;
                    }
                    // Space toggles the selected server in the MCP manager;
                    // `r` reconnects it.
                    if context.active_modal == super::Modal::Mcp && c == ' ' {
                        return InputAction::McpToggle;
                    }
                    if context.active_modal == super::Modal::Mcp && c == 'r' {
                        return InputAction::McpReconnect;
                    }
                    // `r` in the skills modal reloads the skill registry.
                    if context.active_modal == super::Modal::Skills && c == 'r' {
                        return InputAction::SkillsReload;
                    }
                    // Space inside the permissions manager revokes the
                    // selected rule.
                    if context.active_modal == super::Modal::Permissions && c == ' ' {
                        return InputAction::PermissionsActivate;
                    }
                    // Space in the config root drills into the selected
                    // category; in the nudge sub-page it toggles the enabled
                    // flag (when the enabled row is selected) or drills into
                    // a threshold (no-op — thresholds are adjusted with ←/→).
                    if context.active_modal == super::Modal::Config && c == ' ' {
                        return InputAction::ConfigActivate;
                    }
                    if context.active_modal == super::Modal::ConfigNudge && c == ' ' {
                        return InputAction::ConfigNudgeToggle;
                    }
                    // Space in the layout sub-page applies the selected
                    // strategy (same as Enter).
                    if context.active_modal == super::Modal::ConfigLayout && c == ' ' {
                        return InputAction::ConfigLayoutApply;
                    }
                    if context.active_modal == super::Modal::Question
                        && let Some(d) = c.to_digit(10)
                        && (1..=9).contains(&d)
                    {
                        return InputAction::QuestionSelect(d as usize);
                    }
                    // A focused transcript step does not capture typing: with
                    // no separate browse mode, printable characters always fall
                    // through to the input box below (the focus highlight stays
                    // until Esc / Enter). `Enter` activates the focused step;
                    // `Space` just inserts a space.
                    if context.active_modal == super::Modal::Provider
                        && !context.model_searching
                        && c == '/'
                    {
                        // Browse mode: `/` opens the search sub-layer rather than
                        // inserting a literal slash — mirrors the history modal.
                        InputAction::ModelEnterSearch
                    } else if context.active_modal == super::Modal::Provider
                        && !context.model_searching
                        && !context.picker_in_models_stage
                        && c == '*'
                    {
                        // Stage-1 browse mode only: star the highlighted provider
                        // as a favorite. In the search sub-layer `*` is a query
                        // char; favoriting is a provider-level action so it is not
                        // offered in the stage-2 model sub-list.
                        InputAction::ProviderPickerToggleFavorite
                    } else if context.active_modal == super::Modal::Provider
                        && !context.model_searching
                        && c == 'e'
                    {
                        // Stage 1: edit the highlighted provider. Stage 2:
                        // edit the highlighted model/channel settings.
                        InputAction::OpenModelEditor
                    } else if context.active_modal == super::Modal::Provider
                        && !context.model_searching
                        && context.picker_in_models_stage
                        && c == 'd'
                    {
                        // Stage-2 browse mode: `d` removes the highlighted model
                        // from a custom provider (ignored for built-ins / the
                        // "＋ Add model" row by the handler).
                        InputAction::ProviderPickerRemoveModel
                    } else if context.active_modal == super::Modal::Provider
                        && !context.model_searching
                        && !context.picker_in_models_stage
                        && c == 'D'
                    {
                        // Stage-1 browse mode: `Shift+D` deletes the entire
                        // highlighted custom provider (ignored for built-ins and
                        // the "＋ Add provider" row by the handler).
                        InputAction::DeleteProvider
                    } else if context.active_modal == super::Modal::Sessions && c == 'd' {
                        InputAction::DeleteSelectedSession
                    } else if context.active_modal == super::Modal::Permissions && c == 'c' {
                        InputAction::PermissionsClearAll
                    } else if c == ' '
                        && context.active_modal == super::Modal::ModelEditor
                        && context.editor_field == Some(2)
                    {
                        // Space toggles the key editor's thinking field
                        // (Anthropic, field 2) instead of inserting a space.
                        InputAction::ModelEditorThinkingToggle
                    } else if context.active_modal == super::Modal::Question {
                        InputAction::QuestionInsertChar(c)
                    } else if context.active_modal == super::Modal::HistorySearch
                        && !context.history_searching
                        && c == '/'
                    {
                        // Browse mode: `/` opens the search sub-layer rather than
                        // inserting a literal slash. Other printable keys are
                        // inert here (`edits_input_field` is false), so the list
                        // stays a pure browse surface until search is entered.
                        InputAction::HistoryEnterSearch
                    } else if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) && !(context.active_modal == super::Modal::ModelEditor
                        && context.editor_field == Some(2))
                    {
                        // The key editor's thinking field (2) is a toggle, not
                        // a text field — don't let printable chars mutate the
                        // borrowed input line while it's focused.
                        let byte_pos = input
                            .char_indices()
                            .map(|(i, _)| i)
                            .nth(*cursor_position)
                            .unwrap_or(input.len());
                        input.insert(byte_pos, c);
                        *cursor_position += 1;
                        // Return InsertChar so the event loop can reset the
                        // completion-dismissal latch and suggestion highlight.
                        // The input mutation already happened above; the event
                        // loop's InsertChar handler treats the char as a signal
                        // only (it does not re-insert).
                        InputAction::InsertChar(c)
                    } else {
                        InputAction::None
                    }
                }
                KeyCode::Backspace => {
                    if context.active_modal == super::Modal::Question {
                        InputAction::QuestionBackspace
                    } else if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) && *cursor_position > 0
                    {
                        // Alt+Backspace / Ctrl+Backspace delete the previous
                        // whitespace-delimited word in one stroke, matching
                        // readline's `backward-kill-word`. Plain Backspace
                        // keeps the chip-aware atomic delete below so pasted
                        // attachment placeholders vanish in a single tap.
                        if key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        {
                            let start = prev_word_start(input, *cursor_position);
                            if start < *cursor_position {
                                let start_byte = input
                                    .char_indices()
                                    .nth(start)
                                    .map(|(i, _)| i)
                                    .unwrap_or(input.len());
                                let end_byte = input
                                    .char_indices()
                                    .nth(*cursor_position)
                                    .map(|(i, _)| i)
                                    .unwrap_or(input.len());
                                input.replace_range(start_byte..end_byte, "");
                                *cursor_position = start;
                                return InputAction::Backspace;
                            }
                        }
                        // Chip-aware atomic delete: when the cursor sits
                        // immediately after an attachment placeholder (and
                        // optionally one trailing space the paste path
                        // inserts), one Backspace removes the whole chip in
                        // a single keystroke — mirroring codex / claude-code
                        // / opencode. The event loop runs the reconcile pass
                        // on the returned `Backspace` action, which drops
                        // the orphaned entry from `pending_images` /
                        // `pending_text_pastes` and relabels survivors.
                        let byte_cursor = input
                            .char_indices()
                            .map(|(i, _)| i)
                            .nth(*cursor_position)
                            .unwrap_or(input.len());
                        if let Some((start, end)) =
                            crate::tui::composer_attachments::chip_range_for_backspace(
                                input,
                                byte_cursor,
                            )
                        {
                            let removed_chars = input[start..end].chars().count();
                            input.replace_range(start..end, "");
                            *cursor_position -= removed_chars;
                            return InputAction::Backspace;
                        }
                        *cursor_position -= 1;
                        let byte_pos = input
                            .char_indices()
                            .map(|(i, _)| i)
                            .nth(*cursor_position)
                            .unwrap_or(input.len());
                        input.remove(byte_pos);
                        // Return Backspace so the event loop resets the
                        // completion-dismissal latch and suggestion highlight,
                        // matching InsertChar above.
                        InputAction::Backspace
                    } else {
                        InputAction::None
                    }
                }
                KeyCode::Left => {
                    if context.active_modal == super::Modal::Permission {
                        return InputAction::ModalUp;
                    }
                    // In the model editor's effort field, ← cycles the effort
                    // level down (wrapping). Only when field 1 is focused.
                    if context.active_modal == super::Modal::ModelEditor
                        && context.editor_field == Some(1)
                    {
                        return InputAction::ModelEditorEffortCycle { delta: -1 };
                    }
                    // In the nudge sub-page, ← decreases the selected
                    // threshold by 1 (no-op on the enabled row, which is
                    // toggled with Space).
                    if context.active_modal == super::Modal::ConfigNudge {
                        return InputAction::ConfigNudgeAdjust { delta: -1 };
                    }
                    // In the provider editor every field borrows the composer
                    // line, so ←/→ move the caret within the focused field.
                    if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) && *cursor_position > 0
                    {
                        // Ctrl+Left (and Alt+Left on terminals that translate
                        // it) jumps back one whitespace-delimited word,
                        // matching readline's `backward-word`.
                        if key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        {
                            *cursor_position = prev_word_start(input, *cursor_position);
                        } else {
                            *cursor_position -= 1;
                        }
                    }
                    InputAction::None
                }
                KeyCode::Right => {
                    if context.active_modal == super::Modal::Permission {
                        return InputAction::ModalDown;
                    }
                    // Effort field: → cycles the level up (wrapping).
                    if context.active_modal == super::Modal::ModelEditor
                        && context.editor_field == Some(1)
                    {
                        return InputAction::ModelEditorEffortCycle { delta: 1 };
                    }
                    // In the nudge sub-page, → increases the selected
                    // threshold by 1.
                    if context.active_modal == super::Modal::ConfigNudge {
                        return InputAction::ConfigNudgeAdjust { delta: 1 };
                    }
                    if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) && *cursor_position < input.chars().count()
                    {
                        // Ctrl+Right (and Alt+Right) jump forward one word.
                        if key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        {
                            *cursor_position = next_word_end(input, *cursor_position);
                        } else {
                            *cursor_position += 1;
                        }
                    }
                    InputAction::None
                }
                // Ctrl+↑ / Ctrl+↓: the gesture that drives transcript item
                // focus. From the input box it focuses the step closest to the
                // prompt (the last interactive target → `FocusPrevTarget` lands
                // on the last entry when nothing is focused yet); once a step is
                // focused it cycles like the bare arrows. This keeps the bare
                // ↑/↓ free for history / caret motion until a step is focused.
                // No-op while a modal owns focus.
                KeyCode::Up
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && context.active_modal == super::Modal::None =>
                {
                    InputAction::FocusPrevTarget
                }
                KeyCode::Down
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && context.active_modal == super::Modal::None =>
                {
                    InputAction::FocusNextTarget
                }
                KeyCode::Up => match context.active_modal {
                    super::Modal::Provider => InputAction::ModalUp,
                    super::Modal::HistorySearch => InputAction::ModalUp,
                    super::Modal::Sessions => InputAction::ModalUp,
                    super::Modal::Question => InputAction::QuestionUp,
                    super::Modal::Permission => {
                        // Browse zone: walk transcript targets. Compose zone:
                        // scroll the expanded details, otherwise fall through
                        // to a transcript scroll so the history stays readable
                        // even while a prompt is pending.
                        if context.has_focused_target {
                            InputAction::FocusPrevTarget
                        } else if context.permission_show_details {
                            InputAction::PermissionDetailsUp
                        } else {
                            InputAction::ScrollUp
                        }
                    }
                    super::Modal::Activity => InputAction::ScrollUp,
                    super::Modal::Tools => InputAction::SessionSelect { forward: false },
                    super::Modal::Mcp => InputAction::SessionSelect { forward: false },
                    super::Modal::Skills => InputAction::SessionSelect { forward: false },
                    super::Modal::Permissions => InputAction::ModalUp,
                    super::Modal::Config => InputAction::ModalUp,
                    super::Modal::ConfigNudge => InputAction::ModalUp,
                    super::Modal::ConfigLayout => InputAction::ModalUp,
                    super::Modal::ProviderTemplate => {
                        InputAction::MoveProviderTemplate { forward: false }
                    }
                    super::Modal::CustomProvider => {
                        InputAction::MoveCustomSuggestion { forward: false }
                    }
                    super::Modal::AddModel => InputAction::MoveAddModel { forward: false },
                    super::Modal::ModelEditor | super::Modal::InputInjection => InputAction::None,
                    super::Modal::Help => InputAction::ScrollUp,
                    super::Modal::TokenReport => InputAction::ModalUp,
                    super::Modal::Debug => InputAction::ModalUp,
                    super::Modal::None => {
                        if context.has_focused_target {
                            InputAction::FocusPrevTarget
                        } else if context.completion_kind != super::CompletionKind::None
                            && context.suggestion_count > 0
                        {
                            InputAction::SuggestPrev
                        } else if context.has_queued {
                            // A queued message is waiting to ship; ↑ recalls
                            // the most-recently-queued one into the input for
                            // editing instead of walking input history. Once
                            // the queue drains, ↑ resumes its normal role.
                            InputAction::RecallQueued
                        } else if cursor_line_up(input, cursor_position) {
                            // Multi-line draft: ↑ first walks the caret to the
                            // previous line (preserving the column). Only when
                            // the caret is already on the first line does ↑
                            // hand off to input-history navigation below.
                            InputAction::None
                        } else {
                            InputAction::HistoryPrev
                        }
                    }
                },
                KeyCode::Down => match context.active_modal {
                    super::Modal::Provider => InputAction::ModalDown,
                    super::Modal::HistorySearch => InputAction::ModalDown,
                    super::Modal::Sessions => InputAction::ModalDown,
                    super::Modal::Question => InputAction::QuestionDown,
                    super::Modal::Permission => {
                        if context.has_focused_target {
                            InputAction::FocusNextTarget
                        } else if context.permission_show_details {
                            InputAction::PermissionDetailsDown
                        } else {
                            InputAction::ScrollDown
                        }
                    }
                    super::Modal::Activity => InputAction::ScrollDown,
                    super::Modal::Tools => InputAction::SessionSelect { forward: true },
                    super::Modal::Mcp => InputAction::SessionSelect { forward: true },
                    super::Modal::Skills => InputAction::SessionSelect { forward: true },
                    super::Modal::Permissions => InputAction::ModalDown,
                    super::Modal::Config => InputAction::ModalDown,
                    super::Modal::ConfigNudge => InputAction::ModalDown,
                    super::Modal::ConfigLayout => InputAction::ModalDown,
                    super::Modal::ProviderTemplate => {
                        InputAction::MoveProviderTemplate { forward: true }
                    }
                    super::Modal::CustomProvider => {
                        InputAction::MoveCustomSuggestion { forward: true }
                    }
                    super::Modal::AddModel => InputAction::MoveAddModel { forward: true },
                    super::Modal::ModelEditor | super::Modal::InputInjection => InputAction::None,
                    super::Modal::Help => InputAction::ScrollDown,
                    super::Modal::TokenReport => InputAction::ModalDown,
                    super::Modal::Debug => InputAction::ModalDown,
                    super::Modal::None => {
                        if context.has_focused_target {
                            InputAction::FocusNextTarget
                        } else if context.completion_kind != super::CompletionKind::None
                            && context.suggestion_count > 0
                        {
                            InputAction::SuggestNext
                        } else if cursor_line_down(input, cursor_position) {
                            // Multi-line draft: ↓ first walks the caret to the
                            // next line (preserving the column). Only when the
                            // caret is already on the last line does ↓ hand
                            // off to input-history navigation below.
                            InputAction::None
                        } else {
                            InputAction::HistoryNext
                        }
                    }
                },
                KeyCode::PageUp
                    if matches!(
                        context.active_modal,
                        super::Modal::None | super::Modal::Permission | super::Modal::Question
                    ) =>
                {
                    InputAction::ScrollPageUp
                }
                KeyCode::PageDown
                    if matches!(
                        context.active_modal,
                        super::Modal::None | super::Modal::Permission | super::Modal::Question
                    ) =>
                {
                    InputAction::ScrollPageDown
                }
                KeyCode::Home => {
                    // A focused step disambiguates Home from caret motion, so it
                    // no longer clashes with conversation scrolling:
                    //   - Permission modal / a step is focused: scroll to top.
                    //   - Otherwise (free text): move the input caret to the
                    //     start of the current line.
                    if context.active_modal == super::Modal::Permission
                        || (context.active_modal == super::Modal::None
                            && context.has_focused_target)
                    {
                        InputAction::ScrollTop
                    } else if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) {
                        cursor_line_start(input, cursor_position);
                        InputAction::None
                    } else {
                        InputAction::None
                    }
                }
                KeyCode::End => {
                    if context.active_modal == super::Modal::Permission
                        || (context.active_modal == super::Modal::None
                            && context.has_focused_target)
                    {
                        InputAction::ScrollBottom
                    } else if edits_input_field(
                        context.active_modal,
                        context.history_searching,
                        context.model_searching,
                        context.custom_text_field_focused(),
                    ) {
                        cursor_line_end(input, cursor_position);
                        InputAction::None
                    } else {
                        InputAction::None
                    }
                }
                _ => InputAction::None,
            }
        }
        Event::Paste(text) => {
            // Terminal-level bracketed paste. Route the payload through the
            // same chip-or-inline logic as Ctrl+V on the main prompt, and
            // splice it inline into the focused field in the free-text
            // modals (provider editor, provider picker filter, history
            // search).
            if edits_input_field(
                context.active_modal,
                context.history_searching,
                context.model_searching,
                context.custom_text_field_focused(),
            ) {
                InputAction::BracketedPaste(text)
            } else {
                InputAction::None
            }
        }
        Event::Resize(..) => {
            // The event loop does the real work (redraw + re-arm mouse capture)
            // off this signal; here we just surface that the terminal geometry
            // changed rather than leaving it to the catch-all `None`.
            InputAction::TerminalResized
        }
        _ => InputAction::None,
    }
}

/// Resolve a screen coordinate to the block it belongs to.
pub fn resolve_block(layout_map: &LayoutMap, x: u16, y: u16) -> Option<(usize, usize)> {
    layout_map
        .region_at(x, y)
        .map(|r| (r.message_idx, r.block_idx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};

    fn enter(input: &mut String, exact: bool) -> InputAction {
        let mut cursor = input.chars().count();
        let mut drag = SelectionDrag::default();
        process_event(
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::Slash,
                suggestion_count: 1,
                has_exact_suggestion: exact,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        )
    }

    // Like `enter`, but exposes the full completion state so we can reproduce
    // the "menu open + user highlighted an item" scenarios that decide
    // whether Enter accepts the highlighted completion or sends the partial
    // input as-is.
    #[allow(clippy::too_many_arguments)]
    fn enter_with_completion(
        input: &mut String,
        kind: crate::tui::CompletionKind,
        suggestion_count: usize,
        suggestion_index: Option<usize>,
        has_exact_suggestion: bool,
    ) -> InputAction {
        let mut cursor = input.chars().count();
        let mut drag = SelectionDrag::default();
        process_event(
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: kind,
                suggestion_count,
                has_exact_suggestion,
                suggestion_index,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        )
    }

    #[test]
    fn enter_executes_an_exact_slash_command() {
        let mut input = "/pursue".to_string();
        assert_eq!(
            enter(&mut input, true),
            InputAction::SendSlash("/pursue".to_string())
        );
    }

    #[test]
    fn enter_completes_a_slash_prefix() {
        let mut input = "/go".to_string();
        assert_eq!(
            enter(&mut input, false),
            InputAction::CommitSuggestion("0".to_string())
        );
    }

    #[test]
    fn enter_accepts_a_highlighted_slash_suggestion() {
        // User typed `/m`, menu shows `/mcp` / `/model` / `/provider`, user
        // pressed ↓ to highlight `/mcp` (index 1). Enter must accept the
        // highlighted item rather than sending `/m` as a (rejected) command.
        let mut input = "/m".to_string();
        assert_eq!(
            enter_with_completion(
                &mut input,
                crate::tui::CompletionKind::Slash,
                3,
                Some(1),
                false,
            ),
            InputAction::CommitSuggestion("1".to_string())
        );
    }

    #[test]
    fn enter_accepts_a_highlighted_path_suggestion() {
        // User typed `@src/foo`, path menu shows three candidates, user
        // highlighted the second. Enter must accept it rather than shipping
        // the partial `@src/foo` text in the chat message.
        let mut input = "@src/foo".to_string();
        assert_eq!(
            enter_with_completion(
                &mut input,
                crate::tui::CompletionKind::Path,
                3,
                Some(2),
                false,
            ),
            InputAction::CommitSuggestion("2".to_string())
        );
    }

    #[test]
    fn enter_highlight_wins_over_exact_slash_match() {
        // User typed `/mcp` (exact match) but then pressed ↓ to highlight
        // `/provider`. The explicit highlight is a stronger signal than the
        // exact-match fast path, so Enter accepts the highlight.
        let mut input = "/mcp".to_string();
        assert_eq!(
            enter_with_completion(
                &mut input,
                crate::tui::CompletionKind::Slash,
                2,
                Some(1),
                true,
            ),
            InputAction::CommitSuggestion("1".to_string())
        );
    }

    #[test]
    fn enter_without_highlight_still_sends_path_message() {
        // No explicit highlight on a path menu → Enter keeps sending the
        // message. Tab remains the way to accept the first path candidate
        // without first navigating with ↓.
        let mut input = "@src/foo".to_string();
        assert_eq!(
            enter_with_completion(&mut input, crate::tui::CompletionKind::Path, 3, None, false,),
            InputAction::SendChat("@src/foo".to_string())
        );
    }

    #[test]
    fn esc_closes_slash_completion_menu() {
        // When a slash completion popup is open, Esc dismisses it rather
        // than falling through to envoy exit / interrupt / no-op.
        let mut input = "/mc".to_string();
        let mut cursor = 3;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::Slash,
                suggestion_count: 2,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::CloseCompletion);
        // The input text is left untouched — Esc only closes the popup.
        assert_eq!(input, "/mc");
    }

    #[test]
    fn esc_closes_path_completion_menu() {
        // Same behaviour for `@path` mention completion.
        let mut input = "@src".to_string();
        let mut cursor = 4;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::Path,
                suggestion_count: 3,
                has_exact_suggestion: false,
                suggestion_index: Some(1),
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::CloseCompletion);
    }

    #[test]
    fn esc_falls_through_when_no_completion_is_open() {
        // With no popup, Esc in Compose with nothing going on is a no-op;
        // the completion-close branch only fires when a menu is visible.
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::None);
    }

    #[test]
    fn typing_in_compose_returns_insert_char() {
        // process_event must signal InsertChar (not None) so the event loop
        // can reset the completion-dismissal latch after an Enter commit or
        // Esc dismiss. The char is already spliced into `input` here; the
        // event loop treats the action as a signal only.
        let mut input = "/mc".to_string();
        let mut cursor = 3;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::Slash,
                suggestion_count: 2,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::InsertChar('p'));
        assert_eq!(input, "/mcp");
        assert_eq!(cursor, 4);
    }

    #[test]
    fn backspace_in_compose_returns_backspace_action() {
        // Same signal contract as InsertChar: Backspace must be returned so
        // the event loop clears completion_dismissed + suggestion_index.
        let mut input = "/mcp".to_string();
        let mut cursor = 4;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::Slash,
                suggestion_count: 1,
                has_exact_suggestion: true,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::Backspace);
        assert_eq!(input, "/mc");
        assert_eq!(cursor, 3);
    }

    #[test]
    fn backspace_atomically_deletes_an_image_chip() {
        // Pasting an image inserts `[Image #1] ` (chip + trailing space).
        // A single Backspace right after the space must erase both the
        // space and the chip — mirroring codex / claude-code / opencode's
        // atomic chip backspace. The reconcile pass in the event loop
        // drops the orphaned `pending_images` entry.
        let chip = crate::tui::composer_attachments::image_chip(1);
        let mut input = format!("look {chip} ");
        let mut cursor = input.chars().count();
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::Backspace);
        assert_eq!(input, "look ");
        assert_eq!(cursor, "look ".chars().count());
    }

    #[test]
    fn backspace_atomically_deletes_a_paste_chip_without_trailing_space() {
        // When the cursor lands right after `]` (no trailing space), a
        // single Backspace still removes the whole chip rather than
        // chipping away at the `]`.
        let chip = crate::tui::composer_attachments::paste_chip(1, 5);
        let mut input = format!("see {chip}!");
        // Cursor right after `]`, before `!`.
        let prefix_chars = "see ".chars().count() + chip.chars().count();
        let mut cursor = prefix_chars;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::Backspace);
        assert_eq!(input, "see !");
        assert_eq!(cursor, "see ".chars().count());
    }

    #[test]
    fn backspace_falls_through_to_single_char_outside_a_chip() {
        // Mid-word backspace must keep deleting one character at a time.
        let mut input = "hello".to_string();
        let mut cursor = 5;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::Backspace);
        assert_eq!(input, "hell");
        assert_eq!(cursor, 4);
    }

    #[test]
    fn bang_prefix_dispatches_a_shell_command() {
        let mut input = "!git status".to_string();
        assert_eq!(
            enter_shell(&mut input),
            InputAction::SendShell("git status".to_string())
        );
    }

    #[test]
    fn bang_prefix_tolerates_leading_whitespace() {
        // `! ls` matches the shell convention: the bang is a mode marker,
        // not part of the command.
        let mut input = "!   ls -la".to_string();
        assert_eq!(
            enter_shell(&mut input),
            InputAction::SendShell("ls -la".to_string())
        );
    }

    #[test]
    fn bare_bang_is_a_no_op() {
        // A bare `!` does not run an empty command.
        let mut input = "!".to_string();
        assert_eq!(enter_shell(&mut input), InputAction::None);
        // The input is still consumed (mirrors how `/` on its own is
        // swallowed), so the user does not get stuck with a stray `!`.
        assert_eq!(input, "");
    }

    #[test]
    fn bang_only_with_whitespace_is_a_no_op() {
        let mut input = "!   ".to_string();
        assert_eq!(enter_shell(&mut input), InputAction::None);
    }

    // Like `enter`, but with `completion_kind: None` and no suggestions, the
    // production state for `!`-prefixed input (slash completion only opens
    // when the input starts with `/`).
    fn enter_shell(input: &mut String) -> InputAction {
        let mut cursor = input.chars().count();
        let mut drag = SelectionDrag::default();
        process_event(
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        )
    }

    #[test]
    fn escape_returns_from_always_confirmation() {
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::Permission,
                is_responding: true,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: true,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::PermissionBack);
    }

    #[test]
    fn plain_ctrl_c_maps_to_semantic_ctrl_c() {
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::CtrlC);
    }

    #[test]
    fn star_in_models_modal_toggles_favorite() {
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent::new(KeyCode::Char('*'), KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::Provider,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::ProviderPickerToggleFavorite);
    }

    #[test]
    fn esc_in_stage2_steps_back_to_provider_list() {
        // In the stage-2 model sub-list (browse mode), Esc returns to the
        // provider list rather than closing the modal.
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::Provider,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: true,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::ProviderPickerBack);
    }

    #[test]
    fn esc_in_stage1_closes_the_modal() {
        // In the stage-1 provider list (browse mode), Esc closes the picker.
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::Provider,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::CloseModal);
    }

    #[test]
    fn star_in_stage2_is_inert_favorite_is_provider_level() {
        // `*` favorites a provider — a stage-1-only action. In the stage-2 model
        // sub-list it must not map to ToggleFavorite (it falls through to the
        // ordinary char path, which is inert in browse mode).
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent::new(KeyCode::Char('*'), KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::Provider,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: true,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_ne!(action, InputAction::ProviderPickerToggleFavorite);
    }

    #[test]
    fn letter_in_models_modal_feeds_the_fuzzy_filter() {
        // In the model picker's search sub-layer every letter feeds the fuzzy
        // filter so users can search for "kimi" or "deepseek". (In browse mode
        // the same letter is inert — see `letter_in_models_browse_mode_is_inert`.)
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::Provider,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: true,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::InsertChar('k'));
        assert_eq!(input, "k");
    }

    #[test]
    fn letter_in_models_browse_mode_is_inert_and_slash_enters_search() {
        // Browse mode (no `/` yet): printable letters do not mutate the borrowed
        // composer line; `/` is what enters the search sub-layer.
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let ctx = || InputContext {
            active_modal: crate::tui::Modal::Provider,
            is_responding: false,
            completion_kind: crate::tui::CompletionKind::None,
            suggestion_count: 0,
            has_exact_suggestion: false,
            suggestion_index: None,
            permission_confirm_always: false,
            permission_show_details: false,
            in_envoy_view: false,
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
            model_searching: false,
            picker_in_models_stage: false,
            editor_field: None,
            custom_provider_field: None,
            question_other_highlighted: false,
        };
        let letter = process_event(
            Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            ctx(),
            &mut drag,
        );
        assert_eq!(letter, InputAction::None);
        assert_eq!(input, "");
        let slash = process_event(
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            ctx(),
            &mut drag,
        );
        assert_eq!(slash, InputAction::ModelEnterSearch);
        assert_eq!(input, "");
    }

    #[test]
    fn ctrl_t_toggles_tool_steps() {
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('t'),
                KeyModifiers::CONTROL,
            )),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::ToggleToolSteps);
    }

    #[test]
    fn ctrl_m_opens_models_modal_when_no_modal_is_open() {
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let context = InputContext {
            active_modal: crate::tui::Modal::None,
            is_responding: false,
            completion_kind: crate::tui::CompletionKind::None,
            suggestion_count: 0,
            has_exact_suggestion: false,
            suggestion_index: None,
            permission_confirm_always: false,
            permission_show_details: false,
            in_envoy_view: false,
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
            model_searching: false,
            picker_in_models_stage: false,
            editor_field: None,
            custom_provider_field: None,
            question_other_highlighted: false,
        };
        let action = process_event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('m'),
                KeyModifiers::CONTROL,
            )),
            &mut input,
            &mut cursor,
            context,
            &mut drag,
        );
        assert_eq!(action, InputAction::OpenProvider);

        // While a modal is already open, Ctrl+M is ignored so it cannot yank
        // the user out of another modal mid-interaction.
        let action = process_event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('m'),
                KeyModifiers::CONTROL,
            )),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::Help,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::None);
    }

    fn key_in_view(code: KeyCode, in_envoy_view: bool, input: &mut String) -> InputAction {
        let mut cursor = input.chars().count();
        let mut drag = SelectionDrag::default();
        process_event(
            Event::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)),
            input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        )
    }

    fn key_with_focus(code: KeyCode) -> InputAction {
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        process_event(
            Event::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: true,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        )
    }

    #[test]
    fn tab_in_compose_without_suggestions_is_noop() {
        // Tab is completion-only: with no suggestion menu open, it does
        // nothing. (Transcript focus uses Ctrl+Up/Ctrl-Down, not Tab.)
        let mut input = String::new();
        assert_eq!(
            key_in_view(KeyCode::Tab, false, &mut input),
            InputAction::None
        );
        let mut input = String::from("draft");
        assert_eq!(
            key_in_view(KeyCode::Tab, false, &mut input),
            InputAction::None
        );
        // Shift+Tab is also a no-op (no zone switching).
        let mut input = String::new();
        assert_eq!(
            key_in_view(KeyCode::BackTab, false, &mut input),
            InputAction::None
        );
    }

    #[test]
    fn ctrl_b_moves_caret_back_one_char() {
        // Ctrl+B is readline backward-char: it moves the caret left and never
        // touches focus. (Focus navigation is Ctrl+↑/↓.)
        let mut input = String::from("abc");
        let mut cursor = 3;
        let action = run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(action, InputAction::None);
        assert_eq!(cursor, 2, "Ctrl+B moves the caret back one character");
    }

    #[test]
    fn ctrl_arrows_drive_focus() {
        // Ctrl+↑/↓ enter focus from the input box (no focus yet) and keep
        // cycling once a step is focused. Bare Tab stays a no-op.
        let mut input = String::new();
        let mut cursor = 0;
        assert_eq!(
            run_key(
                &mut input,
                &mut cursor,
                KeyCode::Up,
                KeyModifiers::CONTROL,
                crate::tui::Modal::None,
                false,
            ),
            InputAction::FocusPrevTarget
        );
        assert_eq!(
            run_key(
                &mut input,
                &mut cursor,
                KeyCode::Down,
                KeyModifiers::CONTROL,
                crate::tui::Modal::None,
                true,
            ),
            InputAction::FocusNextTarget
        );
        assert_eq!(key_with_focus(KeyCode::Tab), InputAction::None);
    }

    #[test]
    fn arrows_cycle_steps_while_focused() {
        // With a step focused, bare ↑/↓ cycle the focus instead of walking
        // history (history resumes once Esc clears the focus).
        assert_eq!(key_with_focus(KeyCode::Up), InputAction::FocusPrevTarget);
        assert_eq!(key_with_focus(KeyCode::Down), InputAction::FocusNextTarget);
    }

    #[test]
    fn enter_activates_focused_target_space_inserts() {
        // Enter activates the focused step; Space is an ordinary character (it
        // inserts a space — there is no "space activates" anymore).
        assert_eq!(
            key_with_focus(KeyCode::Enter),
            InputAction::ActivateFocusedTarget
        );
        assert_eq!(
            key_with_focus(KeyCode::Char(' ')),
            InputAction::InsertChar(' ')
        );
    }

    #[test]
    fn escape_clears_focus() {
        // Esc is the deliberate exit from a focused step, clearing the focus
        // so every key returns to its ordinary input-box meaning.
        assert_eq!(
            key_with_focus(KeyCode::Esc),
            InputAction::ClearFocusedTarget
        );
    }

    #[test]
    fn typing_while_focused_inserts_and_keeps_focus() {
        // A focused step does not capture typing: printable characters insert
        // into the prompt as usual and leave the focus highlight in place
        // (Esc / Enter, not typing, change the focus).
        let action = key_with_focus(KeyCode::Char('a'));
        assert_eq!(action, InputAction::InsertChar('a'));
    }

    #[test]
    fn q_while_focused_inserts_instead_of_quitting() {
        // 'q' only quits when nothing is focused. With a step focused it is an
        // ordinary character, so navigating never risks an accidental exit.
        let mut input = String::new();
        let mut cursor = 0;
        let action = run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            crate::tui::Modal::None,
            true,
        );
        assert_eq!(action, InputAction::InsertChar('q'));
        assert_eq!(input, "q");
    }

    #[test]
    fn escape_exits_envoy_view() {
        let mut input = String::new();
        assert_eq!(
            key_in_view(KeyCode::Esc, true, &mut input),
            InputAction::ExitEnvoy
        );
        // Outside an envoy view, Esc does nothing when idle (no modal).
        assert_eq!(
            key_in_view(KeyCode::Esc, false, &mut input),
            InputAction::None
        );
    }

    #[test]
    fn bracket_keys_cycle_siblings_only_when_typing_is_empty() {
        let mut input = String::new();
        assert_eq!(
            key_in_view(KeyCode::Char('['), true, &mut input),
            InputAction::PrevSibling
        );
        assert_eq!(
            key_in_view(KeyCode::Char(']'), true, &mut input),
            InputAction::NextSibling
        );

        // While typing (non-empty input), the brackets insert as characters,
        // not navigation, even inside an envoy view.
        let mut typing = "x".to_string();
        key_in_view(KeyCode::Char('['), true, &mut typing);
        assert_eq!(typing, "x[");

        // Outside an envoy view, brackets always insert.
        let mut other = String::new();
        key_in_view(KeyCode::Char(']'), false, &mut other);
        assert_eq!(other, "]");
    }

    /// Run `code` (+ `modifiers`) against a fully-specified context and return
    /// the resulting action plus the final cursor position. The input buffer is
    /// mutated in place so callers can assert on its contents too.
    fn run_key(
        input: &mut String,
        cursor: &mut usize,
        code: KeyCode,
        modifiers: KeyModifiers,
        modal: crate::tui::Modal,
        has_focus: bool,
    ) -> InputAction {
        let mut drag = SelectionDrag::default();
        process_event(
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            input,
            cursor,
            InputContext {
                active_modal: modal,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: has_focus,
                has_queued: false,
                // Editing text in the history and model-picker modals only
                // happens inside their search sub-layer, so treat those cases
                // here as search mode (browse mode never reaches editing keys).
                history_searching: modal == crate::tui::Modal::HistorySearch,
                model_searching: modal == crate::tui::Modal::Provider,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        )
    }

    #[test]
    fn home_and_end_move_caret_in_compose_zone() {
        // Caret starts mid-string; Home jumps to line start, End to line end.
        // The buffer contents are never modified by these keys.
        let mut input = "hello".to_string();
        let mut cursor = 3;

        let action = run_key(
            &mut input,
            &mut cursor,
            KeyCode::Home,
            KeyModifiers::NONE,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(action, InputAction::None);
        assert_eq!(input, "hello");
        assert_eq!(cursor, 0);

        let action = run_key(
            &mut input,
            &mut cursor,
            KeyCode::End,
            KeyModifiers::NONE,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(action, InputAction::None);
        assert_eq!(input, "hello");
        assert_eq!(cursor, 5);
    }

    #[test]
    fn home_and_end_scroll_in_browse_zone() {
        // In Browse the conversation owns focus, so Home/End drive scrolling
        // instead of moving the (unfocused) input caret.
        let mut input = "hello".to_string();
        let mut cursor = 3;
        assert_eq!(
            run_key(
                &mut input,
                &mut cursor,
                KeyCode::Home,
                KeyModifiers::NONE,
                crate::tui::Modal::None,
                true
            ),
            InputAction::ScrollTop
        );
        assert_eq!(cursor, 3, "Browse Home must not touch the caret");
        assert_eq!(
            run_key(
                &mut input,
                &mut cursor,
                KeyCode::End,
                KeyModifiers::NONE,
                crate::tui::Modal::None,
                true
            ),
            InputAction::ScrollBottom
        );
        assert_eq!(cursor, 3);
    }

    #[test]
    fn home_and_end_scroll_in_permission_modal() {
        let mut input = String::new();
        let mut cursor = 0;
        assert_eq!(
            run_key(
                &mut input,
                &mut cursor,
                KeyCode::Home,
                KeyModifiers::NONE,
                crate::tui::Modal::Permission,
                false
            ),
            InputAction::ScrollTop
        );
        assert_eq!(
            run_key(
                &mut input,
                &mut cursor,
                KeyCode::End,
                KeyModifiers::NONE,
                crate::tui::Modal::Permission,
                false
            ),
            InputAction::ScrollBottom
        );
    }

    #[test]
    fn page_keys_scroll_question_modal_body() {
        let mut input = String::new();
        let mut cursor = 0;
        assert_eq!(
            run_key(
                &mut input,
                &mut cursor,
                KeyCode::PageUp,
                KeyModifiers::NONE,
                crate::tui::Modal::Question,
                false
            ),
            InputAction::ScrollPageUp
        );
        assert_eq!(
            run_key(
                &mut input,
                &mut cursor,
                KeyCode::PageDown,
                KeyModifiers::NONE,
                crate::tui::Modal::Question,
                false
            ),
            InputAction::ScrollPageDown
        );
    }

    #[test]
    fn mouse_wheel_scrolls_question_modal_body() {
        let mk = |kind| {
            let mut input = String::new();
            let mut cursor = 0;
            let mut drag = SelectionDrag::default();
            process_event(
                Event::Mouse(crossterm::event::MouseEvent {
                    kind,
                    column: 5,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                }),
                &mut input,
                &mut cursor,
                InputContext {
                    active_modal: crate::tui::Modal::Question,
                    is_responding: false,
                    completion_kind: crate::tui::CompletionKind::None,
                    suggestion_count: 0,
                    has_exact_suggestion: false,
                    suggestion_index: None,
                    permission_confirm_always: false,
                    permission_show_details: false,
                    in_envoy_view: false,
                    in_side_view: false,
                    has_focused_target: false,
                    has_queued: false,
                    history_searching: false,
                    model_searching: false,
                    picker_in_models_stage: false,
                    editor_field: None,
                    custom_provider_field: None,
                    question_other_highlighted: false,
                },
                &mut drag,
            )
        };

        assert_eq!(
            mk(crossterm::event::MouseEventKind::ScrollUp),
            InputAction::ScrollUp
        );
        assert_eq!(
            mk(crossterm::event::MouseEventKind::ScrollDown),
            InputAction::ScrollDown
        );
    }

    #[test]
    fn home_and_end_move_caret_in_free_text_modals() {
        // The unified provider editor borrows the input line for one field at a
        // time; Home/End should edit there too, not be swallowed.
        for modal in [
            crate::tui::Modal::ModelEditor,
            crate::tui::Modal::HistorySearch,
        ] {
            let mut input = "abc".to_string();
            let mut cursor = 2;
            let action = run_key(
                &mut input,
                &mut cursor,
                KeyCode::Home,
                KeyModifiers::NONE,
                modal,
                false,
            );
            assert_eq!(action, InputAction::None);
            assert_eq!(cursor, 0, "Home should reach line start");

            let action = run_key(
                &mut input,
                &mut cursor,
                KeyCode::End,
                KeyModifiers::NONE,
                modal,
                false,
            );
            assert_eq!(action, InputAction::None);
            assert_eq!(cursor, 3, "End should reach line end");
        }
    }

    #[test]
    fn ctrl_a_and_ctrl_e_move_caret_in_compose_zone() {
        let mut input = "hello".to_string();
        let mut cursor = 2;

        let action = run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(action, InputAction::None);
        assert_eq!(cursor, 0);
        assert_eq!(input, "hello", "Ctrl+A must not insert a literal 'a'");

        let action = run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('e'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(action, InputAction::None);
        assert_eq!(cursor, 5);
        assert_eq!(input, "hello");
    }

    #[test]
    fn ctrl_a_and_ctrl_e_are_noop_in_browse_zone() {
        // Browse has no input editing; the keys fall through to no-ops rather
        // than scrolling or inserting characters.
        let mut input = String::new();
        let mut cursor = 0;
        assert_eq!(
            run_key(
                &mut input,
                &mut cursor,
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
                crate::tui::Modal::None,
                true
            ),
            InputAction::None
        );
        assert_eq!(
            run_key(
                &mut input,
                &mut cursor,
                KeyCode::Char('e'),
                KeyModifiers::CONTROL,
                crate::tui::Modal::None,
                true
            ),
            InputAction::None
        );
    }

    #[test]
    fn line_aware_movement_respects_newlines() {
        // Multi-line input: Home/End/Ctrl+A/Ctrl+E operate on the current
        // logical line, not the whole buffer.
        let mut input = "line1\nline2\nline3".to_string();
        // Place the caret in the middle of the second line ("line2").
        // "line1\n" = 6 chars, then 2 more into "line2" -> char index 8.
        let mut cursor = 8;

        // Home -> start of "line2" (char index 6, just past the first '\n').
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Home,
            KeyModifiers::NONE,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(cursor, 6, "Home should land at start of current line");

        // End -> end of "line2" (char index 11, just before the second '\n').
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::End,
            KeyModifiers::NONE,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(cursor, 11, "End should land at end of current line");

        // Ctrl+A from the end of line2 should also snap to line start.
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(cursor, 6);
        // Ctrl+E snaps back to the line end without running off the buffer.
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('e'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(cursor, 11);
    }

    #[test]
    fn ctrl_w_deletes_previous_word() {
        // "hello world" with the caret after "world" (char index 11).
        let mut input = "hello world".to_string();
        let mut cursor = 11;
        let action = run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('w'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(action, InputAction::Backspace);
        assert_eq!(input, "hello ");
        assert_eq!(cursor, 6);
    }

    #[test]
    fn ctrl_w_eats_trailing_whitespace_and_previous_word() {
        // Caret sits after the trailing spaces following "world"; Ctrl+W
        // (readline `unix-word-rubout`) eats both the trailing whitespace
        // AND the preceding word in one stroke, leaving "hello ".
        let mut input = "hello world   ".to_string();
        let mut cursor = 14;
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('w'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(input, "hello ");
        assert_eq!(cursor, 6);
    }

    #[test]
    fn ctrl_w_is_noop_at_line_start() {
        let mut input = "hello world".to_string();
        let mut cursor = 0;
        let action = run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('w'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(action, InputAction::None);
        assert_eq!(input, "hello world");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn ctrl_w_crosses_newline() {
        // Ctrl+W now crosses newline boundaries. "line1\nworld" with caret
        // at the end → first Ctrl+W deletes "world", second deletes "line1".
        let mut input = "line1\nworld".to_string();
        let mut cursor = 11;
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('w'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(input, "line1\n");
        assert_eq!(cursor, 6);

        // Second Ctrl+W eats the newline and "line1".
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('w'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(input, "");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn ctrl_w_is_noop_in_question_modal() {
        // Ctrl+W must never leak as a literal 'w' or close the modal in the
        // question modal; it should be a silent no-op there.
        let mut input = "abc".to_string();
        let mut cursor = 3;
        let action = run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('w'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::Question,
            false,
        );
        assert_eq!(action, InputAction::None);
        assert_eq!(input, "abc");
        assert_eq!(cursor, 3);
    }

    #[test]
    fn question_space_toggles_when_other_row_not_highlighted() {
        // On a normal option row, Space toggles the option — it must not be
        // swallowed as free text.
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent {
                code: KeyCode::Char(' '),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::Question,
                question_other_highlighted: false,
                ..Default::default()
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::QuestionToggle);
    }

    #[test]
    fn question_space_inserts_into_other_free_text_row() {
        // When the synthetic "Other" free-text row is highlighted, Space is an
        // ordinary character — it must insert into the field, not toggle.
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent {
                code: KeyCode::Char(' '),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::Question,
                question_other_highlighted: true,
                ..Default::default()
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::QuestionInsertChar(' '));
    }

    #[test]
    fn ctrl_u_deletes_to_line_start() {
        let mut input = "hello world".to_string();
        let mut cursor = 7;
        let action = run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(action, InputAction::Backspace);
        assert_eq!(input, "orld");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn ctrl_u_keeps_other_lines_in_multiline_draft() {
        // Multi-line draft: Ctrl+U on line 2 only wipes the part of line 2
        // before the caret, leaving line 1 untouched.
        let mut input = "keep me\nwipe me".to_string();
        let mut cursor = 15;
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(input, "keep me\n");
        assert_eq!(cursor, 8);
    }

    #[test]
    fn ctrl_k_deletes_to_line_end() {
        let mut input = "hello world".to_string();
        let mut cursor = 5;
        let action = run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(action, InputAction::Backspace);
        assert_eq!(input, "hello");
        assert_eq!(cursor, 5, "Ctrl+K keeps the caret put");
    }

    #[test]
    fn ctrl_k_does_not_eat_next_line() {
        let mut input = "first\nsecond".to_string();
        let mut cursor = 3;
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(input, "fir\nsecond");
        assert_eq!(cursor, 3);
    }

    #[test]
    fn alt_d_deletes_next_word() {
        // Caret at index 5 (the space); Alt+D should eat "world".
        let mut input = "hello world".to_string();
        let mut cursor = 5;
        let action = run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('d'),
            KeyModifiers::ALT,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(action, InputAction::Backspace);
        assert_eq!(input, "hello");
        assert_eq!(cursor, 5, "Alt+D keeps the caret put");
    }

    #[test]
    fn alt_b_jumps_back_one_word() {
        let mut input = "the quick fox".to_string();
        let mut cursor = 13;
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('b'),
            KeyModifiers::ALT,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(cursor, 10);
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('b'),
            KeyModifiers::ALT,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(cursor, 4);
    }

    #[test]
    fn alt_f_jumps_forward_one_word() {
        let mut input = "the quick fox".to_string();
        let mut cursor = 0;
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('f'),
            KeyModifiers::ALT,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(cursor, 3);
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('f'),
            KeyModifiers::ALT,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(cursor, 9);
    }

    #[test]
    fn ctrl_left_right_move_word_by_word() {
        // "alpha bravo charlie" — char indices:
        // alpha=0..4, ' '=5, bravo=6..10, ' '=11, charlie=12..18 (len 19).
        let mut input = "alpha bravo charlie".to_string();
        let mut cursor = 19;
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Left,
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(cursor, 12, "Ctrl+Left snaps to the start of 'charlie'");
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Left,
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(cursor, 6, "Ctrl+Left snaps to the start of 'bravo'");
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Right,
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(cursor, 11, "Ctrl+Right snaps to the end of 'bravo'");
    }

    #[test]
    fn alt_backspace_deletes_previous_word() {
        let mut input = "foo bar baz".to_string();
        let mut cursor = 11;
        let action = run_key(
            &mut input,
            &mut cursor,
            KeyCode::Backspace,
            KeyModifiers::ALT,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(action, InputAction::Backspace);
        assert_eq!(input, "foo bar ");
        assert_eq!(cursor, 8);
    }

    #[test]
    fn ctrl_backspace_deletes_previous_word() {
        // Ctrl+Backspace is the same word-rubout motion on terminals that
        // deliver it; mirror the Alt+Backspace behaviour.
        let mut input = "foo bar baz".to_string();
        let mut cursor = 11;
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Backspace,
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            false,
        );
        assert_eq!(input, "foo bar ");
        assert_eq!(cursor, 8);
    }

    #[test]
    fn ctrl_w_works_in_history_modal() {
        // Free-text modals (history search, models, provider editor) accept the
        // same line-editing vocabulary as the main prompt so the user is
        // never trapped mid-query.
        let mut input = "fuzzy query".to_string();
        let mut cursor = 11;
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('w'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::HistorySearch,
            false,
        );
        assert_eq!(input, "fuzzy ");
        assert_eq!(cursor, 6);
    }

    #[test]
    fn ctrl_keys_do_not_insert_literal_chars() {
        // Regression guard: none of the new Ctrl/Alt shortcuts may fall
        // through to the `Char(c)` insertion path. Each must leave the
        // buffer text untouched when there is nothing to delete.
        let mut input = String::new();
        let mut cursor = 0;
        for (code, mods) in [
            (KeyCode::Char('w'), KeyModifiers::CONTROL),
            (KeyCode::Char('u'), KeyModifiers::CONTROL),
            (KeyCode::Char('k'), KeyModifiers::CONTROL),
            (KeyCode::Char('b'), KeyModifiers::ALT),
            (KeyCode::Char('f'), KeyModifiers::ALT),
            (KeyCode::Char('d'), KeyModifiers::ALT),
        ] {
            let action = run_key(
                &mut input,
                &mut cursor,
                code,
                mods,
                crate::tui::Modal::None,
                false,
            );
            assert_eq!(action, InputAction::None);
            assert!(input.is_empty());
            assert_eq!(cursor, 0);
        }
    }

    /// Drive the history modal's **search sub-layer** with `code` (+
    /// `modifiers`) and return the resulting action. `history_searching` is set
    /// so the modal borrows the input line as the fuzzy query — matching the
    /// live state once the user has pressed `/` to enter search.
    fn run_history_key(
        input: &mut String,
        cursor: &mut usize,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> InputAction {
        let mut drag = SelectionDrag::default();
        process_event(
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            input,
            cursor,
            InputContext {
                active_modal: crate::tui::Modal::HistorySearch,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: true,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        )
    }

    #[test]
    fn typing_in_history_modal_appends_to_query() {
        // The history modal borrows the input line as the fuzzy query, so each
        // printable char must insert into `input` exactly like the ApiKey /
        // Endpoint / ModelName modals do.
        let mut input = String::new();
        let mut cursor = 0;
        run_history_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('g'),
            KeyModifiers::NONE,
        );
        run_history_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('i'),
            KeyModifiers::NONE,
        );
        run_history_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('t'),
            KeyModifiers::NONE,
        );
        assert_eq!(input, "git");
        assert_eq!(cursor, 3);
    }

    #[test]
    fn backspace_in_history_modal_trims_query() {
        let mut input = "rust".to_string();
        let mut cursor = 4;
        run_history_key(
            &mut input,
            &mut cursor,
            KeyCode::Backspace,
            KeyModifiers::NONE,
        );
        assert_eq!(input, "rus");
        assert_eq!(cursor, 3);
    }

    /// Drive a key against the history modal in **browse** mode
    /// (`history_searching: false`) and return the resulting action.
    fn run_history_browse_key(
        input: &mut String,
        cursor: &mut usize,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> InputAction {
        let mut drag = SelectionDrag::default();
        process_event(
            Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }),
            input,
            cursor,
            InputContext {
                active_modal: crate::tui::Modal::HistorySearch,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        )
    }

    #[test]
    fn slash_in_history_browse_enters_search() {
        // `/` is the gateway into the search sub-layer: it must emit
        // HistoryEnterSearch rather than inserting a literal slash.
        let mut input = String::new();
        let mut cursor = 0;
        let action = run_history_browse_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        );
        assert_eq!(action, InputAction::HistoryEnterSearch);
        assert!(input.is_empty(), "`/` must not land in the buffer");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn typing_in_history_browse_is_inert() {
        // Browse mode is a pure list: printable keys do nothing (only `/`
        // opens search), so a stray letter never mutates the borrowed buffer.
        let mut input = String::new();
        let mut cursor = 0;
        let action = run_history_browse_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('g'),
            KeyModifiers::NONE,
        );
        assert_eq!(action, InputAction::None);
        assert!(input.is_empty());
        assert_eq!(cursor, 0);
    }

    #[test]
    fn esc_in_history_browse_closes_modal() {
        // No search layer to peel back: Esc closes the modal outright.
        let mut input = String::new();
        let mut cursor = 0;
        let action =
            run_history_browse_key(&mut input, &mut cursor, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(action, InputAction::CloseModal);
    }

    #[test]
    fn esc_in_history_search_returns_to_browse() {
        // Two-stage Esc: while searching, the first Esc exits the sub-layer
        // back to the browse list (HistoryExitSearch) instead of closing.
        let mut input = "git".to_string();
        let mut cursor = 3;
        let action = run_history_key(&mut input, &mut cursor, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(action, InputAction::HistoryExitSearch);
    }

    #[test]
    fn slash_in_history_search_inserts_literal() {
        // Inside search, `/` is just another query character — the sub-layer
        // is already open, so it must splice into the buffer.
        let mut input = String::new();
        let mut cursor = 0;
        run_history_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        );
        assert_eq!(input, "/");
        assert_eq!(cursor, 1);
    }

    #[test]
    fn enter_in_history_modal_emits_history_insert() {
        // Enter must NOT send a chat — it inserts the highlighted match into
        // the input box for further editing. The dedicated HistoryInsert
        // action lets the app loop distinguish the two intents.
        let mut input = "go".to_string();
        let mut cursor = 2;
        let action = run_history_key(&mut input, &mut cursor, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(action, InputAction::HistoryInsert);
        assert_eq!(input, "go", "Enter must not consume the query");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn ctrl_r_opens_history_modal_when_no_modal_is_open() {
        // With no modal open, Ctrl+R routes through OpenHistory so the app
        // loop can stash the in-progress draft and show the fuzzy picker.
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL,
            )),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::OpenHistory);

        // Once any modal is open (including HistorySearch itself), Ctrl+R is
        // a no-op so it cannot yank the user out of the in-progress query.
        let action = process_event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL,
            )),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::HistorySearch,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::None);
    }

    /// Helper: send `code` in the compose zone with explicit `has_queued`.
    fn up_with_queued(has_queued: bool) -> InputAction {
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        process_event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::NONE,
            )),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        )
    }

    #[test]
    fn up_arrow_recalls_queued_when_queue_nonempty() {
        // While at least one message is staged in the send queue, ↑ recalls
        // the most-recently-queued one into the composer for editing instead
        // of walking input history. This is the user-facing undo for a
        // queued send: the user pressed Enter too eagerly while the AI was
        // still responding, and ↑ is the natural "go back" gesture.
        assert_eq!(up_with_queued(true), InputAction::RecallQueued);
    }

    #[test]
    fn up_arrow_walks_history_when_queue_empty() {
        // Once the queue drains (or was never populated), ↑ resumes its
        // normal role of walking the input history.
        assert_eq!(up_with_queued(false), InputAction::HistoryPrev);
    }

    #[test]
    fn up_arrow_in_browse_does_not_recall_queued() {
        // Browse zone owns ↑ for step navigation; the queued-message recall
        // only fires from Compose (where the user can actually edit the
        // recalled draft). In Browse, ↑ keeps walking activatable targets.
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Up,
                KeyModifiers::NONE,
            )),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: true,
                has_queued: true,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::FocusPrevTarget);
    }

    /// Drive `Event::Paste` (terminal bracketed paste) through `process_event`
    /// against the given modal and return the resulting action. The input
    /// buffer is mutated in place so callers can assert on its contents.
    fn run_paste(
        text: &str,
        input: &mut String,
        cursor: &mut usize,
        modal: crate::tui::Modal,
    ) -> InputAction {
        let mut drag = SelectionDrag::default();
        process_event(
            Event::Paste(text.to_string()),
            input,
            cursor,
            InputContext {
                active_modal: modal,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                // The history and model-picker modals only take text in their
                // search sub-layer; treat those cases as search mode here.
                history_searching: modal == crate::tui::Modal::HistorySearch,
                model_searching: modal == crate::tui::Modal::Provider,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        )
    }

    #[test]
    fn ctrl_v_returns_paste_in_free_text_modals() {
        // Ctrl+V routes to InputAction::Paste on the main prompt and in
        // every free-text modal (provider editor, provider picker filter,
        // history search). Other modals drop it so a paste never leaks into
        // a read-only overlay or the permission sheet.
        let free_text_modals = [
            crate::tui::Modal::None,
            crate::tui::Modal::ModelEditor,
            crate::tui::Modal::Provider,
            crate::tui::Modal::HistorySearch,
        ];
        for modal in free_text_modals {
            let mut input = String::new();
            let mut cursor = 0;
            let action = run_key(
                &mut input,
                &mut cursor,
                KeyCode::Char('v'),
                KeyModifiers::CONTROL,
                modal,
                false,
            );
            assert_eq!(
                action,
                InputAction::Paste,
                "Ctrl+V should paste in free-text modal"
            );
            assert!(input.is_empty(), "Ctrl+V must not mutate the buffer itself");
        }

        for modal in [
            crate::tui::Modal::Permission,
            crate::tui::Modal::Question,
            crate::tui::Modal::Help,
            crate::tui::Modal::Sessions,
        ] {
            let mut input = String::new();
            let mut cursor = 0;
            let action = run_key(
                &mut input,
                &mut cursor,
                KeyCode::Char('v'),
                KeyModifiers::CONTROL,
                modal,
                false,
            );
            assert_eq!(
                action,
                InputAction::None,
                "Ctrl+V should be a no-op in non-text modal"
            );
        }
    }

    #[test]
    fn bracketed_paste_routes_in_free_text_modals() {
        // Terminal-level bracketed paste mirrors Ctrl+V: it produces a
        // BracketedPaste action on the main prompt and in the free-text
        // modals, and is dropped elsewhere.
        let payload = "sk-test-1234";
        for modal in [
            crate::tui::Modal::None,
            crate::tui::Modal::ModelEditor,
            crate::tui::Modal::Provider,
            crate::tui::Modal::HistorySearch,
        ] {
            let mut input = String::new();
            let mut cursor = 0;
            let action = run_paste(payload, &mut input, &mut cursor, modal);
            match action {
                InputAction::BracketedPaste(text) => assert_eq!(
                    text, payload,
                    "bracketed paste payload should pass through in free-text modal"
                ),
                other => panic!("expected BracketedPaste in free-text modal, got {other:?}"),
            }
            assert!(
                input.is_empty(),
                "BracketedPaste must not mutate the buffer itself"
            );
        }

        let mut input = String::new();
        let mut cursor = 0;
        let action = run_paste(payload, &mut input, &mut cursor, crate::tui::Modal::Help);
        assert_eq!(
            action,
            InputAction::None,
            "bracketed paste should be dropped in Help"
        );
    }

    /// Helper: dispatch `code` in the compose zone against a pre-seeded
    /// multi-line buffer and return the resulting action. The cursor lands
    /// at `cursor` (in char units) before the keypress.
    fn multiline_arrow(seed: &str, cursor: usize, code: KeyCode) -> (InputAction, usize) {
        let mut input = seed.to_string();
        let mut cur = cursor;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)),
            &mut input,
            &mut cur,
            InputContext {
                active_modal: crate::tui::Modal::None,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_envoy_view: false,
                in_side_view: false,
                has_focused_target: false,
                has_queued: false,
                history_searching: false,
                model_searching: false,
                picker_in_models_stage: false,
                editor_field: None,
                custom_provider_field: None,
                question_other_highlighted: false,
            },
            &mut drag,
        );
        (action, cur)
    }

    #[test]
    fn up_arrow_walks_lines_in_multiline_before_history() {
        // In a multi-line draft, ↑ first moves the caret up a line instead
        // of jumping to input history — only at the top line does it fall
        // through to HistoryPrev.
        let seed = "hello\nworld";
        // Caret at end of second line: ↑ should move to the same column on
        // the first line ("hello", col 5) and return None, not HistoryPrev.
        let (action, cur) = multiline_arrow(seed, "hello\nworld".chars().count(), KeyCode::Up);
        assert_eq!(action, InputAction::None);
        assert_eq!(cur, 5, "up should land at col 5 on the first line");

        // Now sitting at the end of the first line: ↑ should hand off to
        // history navigation.
        let (action, _) = multiline_arrow(seed, 5, KeyCode::Up);
        assert_eq!(action, InputAction::HistoryPrev);
    }

    #[test]
    fn down_arrow_walks_lines_in_multiline_before_history() {
        let seed = "hello\nworld";
        // Caret at start of first line: ↓ moves to the same column on the
        // second line and returns None, not HistoryNext.
        let (action, cur) = multiline_arrow(seed, 0, KeyCode::Down);
        assert_eq!(action, InputAction::None);
        assert_eq!(cur, 6, "down should land at col 0 of the second line");

        // Caret at end of the second line: ↓ hands off to history.
        let (action, _) = multiline_arrow(seed, "hello\nworld".chars().count(), KeyCode::Down);
        assert_eq!(action, InputAction::HistoryNext);
    }

    #[test]
    fn up_arrow_clamps_column_to_shorter_line() {
        // Moving up to a shorter line clamps the column to that line's
        // length rather than overshooting into the newline.
        let seed = "hi\nlonger line";
        // Caret at col 7 of the second line ("longer line").
        let start = "hi\n".chars().count() + 7;
        let (action, cur) = multiline_arrow(seed, start, KeyCode::Up);
        assert_eq!(action, InputAction::None);
        assert_eq!(cur, 2, "column should clamp to the first line's length");
    }

    // --- SgrLeakGuard -------------------------------------------------------

    /// Build a crossterm `Event::Key` for a single character, the form crossterm
    /// returns when it fails to reassemble a split escape sequence.
    fn leaked_char(c: char) -> Event {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState};
        Event::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn leaked_esc() -> Event {
        use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};
        Event::Key(KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    /// Drive a sequence of events through a fresh guard and report how many it
    /// dropped vs accepted.
    fn drain_guard(events: &[Event]) -> (usize, usize) {
        let mut g = SgrLeakGuard::default();
        let mut dropped = 0;
        let mut accepted = 0;
        for ev in events {
            match g.feed(ev) {
                Feed::Drop => dropped += 1,
                Feed::Accept => accepted += 1,
            }
        }
        (accepted, dropped)
    }

    #[test]
    fn sgr_guard_drops_split_sgr_mouse_sequence() {
        // The exact symptom from the field: a mouse report crossterm split into
        // individual chars. `ESC [ < 0 ; 3 5 ; 4 6 M` — the `[ < … M` payload
        // is dropped, but the leading Esc is *delivered* (it is a real control
        // key, never inserted as text, and is the double-Esc interrupt path).
        let seq: Vec<Event> = [
            leaked_esc(),
            leaked_char('['),
            leaked_char('<'),
            leaked_char('0'),
            leaked_char(';'),
            leaked_char('3'),
            leaked_char('5'),
            leaked_char(';'),
            leaked_char('4'),
            leaked_char('6'),
            leaked_char('M'),
        ]
        .into_iter()
        .collect();
        let (accepted, dropped) = drain_guard(&seq);
        assert_eq!(
            accepted, 1,
            "the leading Esc is delivered, the rest dropped"
        );
        assert_eq!(dropped, seq.len() - 1);
    }

    #[test]
    fn sgr_guard_drops_release_variant_lowercase_m() {
        // SGR release uses lowercase `m`. Same coverage as the press variant:
        // only the leading Esc is delivered.
        let seq: Vec<Event> = [
            leaked_esc(),
            leaked_char('['),
            leaked_char('<'),
            leaked_char('3'),
            leaked_char('5'),
            leaked_char(';'),
            leaked_char('5'),
            leaked_char('6'),
            leaked_char('m'),
        ]
        .into_iter()
        .collect();
        let (accepted, _) = drain_guard(&seq);
        assert_eq!(accepted, 1);
    }

    #[test]
    fn sgr_guard_drops_run_of_split_sequences() {
        // The real complaint showed *many* sequences back to back (resize drag).
        // The guard must resync to idle after each terminating M/m and catch
        // the next one too. Each sequence's leading Esc is delivered; the
        // remaining payload bytes are swallowed.
        let one = |b: char| {
            vec![
                leaked_esc(),
                leaked_char('['),
                leaked_char('<'),
                leaked_char(b),
                leaked_char(';'),
                leaked_char('1'),
                leaked_char('M'),
            ]
        };
        let seq: Vec<Event> = [one('0'), one('3'), one('5')]
            .into_iter()
            .flatten()
            .collect();
        let (accepted, _) = drain_guard(&seq);
        assert_eq!(accepted, 3, "each sequence delivers its leading Esc");
    }

    #[test]
    fn sgr_guard_passes_through_normal_typing() {
        // Ordinary typing must be unaffected — the guard never enters a
        // tracking state and hands every char back as Accept.
        let seq: Vec<Event> = ['h', 'e', 'l', 'l', 'o']
            .into_iter()
            .map(leaked_char)
            .collect();
        let (accepted, dropped) = drain_guard(&seq);
        assert_eq!(accepted, seq.len());
        assert_eq!(dropped, 0);
    }

    #[test]
    fn sgr_guard_delivers_lone_esc() {
        // Regression: a standalone Esc (e.g. the first of a double-Esc
        // interrupt) must reach the app. The previous guard dropped it as a
        // suspected leak prefix, which broke double-Esc interrupt entirely.
        let mut g = SgrLeakGuard::default();
        assert!(matches!(g.feed(&leaked_esc()), Feed::Accept));
        // Not idle: it armed the tracker so a following `[` still opens a leak.
        assert!(!g.is_idle());
        // A subsequent normal char aborts the tracking and is delivered too.
        assert!(matches!(g.feed(&leaked_char('x')), Feed::Accept));
        assert!(g.is_idle());
    }

    #[test]
    fn sgr_guard_delivers_double_esc() {
        // The double-Esc interrupt path: two Escs with nothing between them.
        // Neither is part of an SGR sequence (a real leak has `[` next, never
        // another Esc), so both must be delivered.
        let mut g = SgrLeakGuard::default();
        assert!(matches!(g.feed(&leaked_esc()), Feed::Accept));
        assert!(matches!(g.feed(&leaked_esc()), Feed::Accept));
        assert!(!g.is_idle());
    }

    #[test]
    fn sgr_guard_recovers_from_aborted_prefix() {
        // `ESC [` followed by something other than `<` is a real CSI (e.g. an
        // arrow key's payload). The leading Esc is delivered; `[` is dropped
        // (it can only be leak noise from this state); the aborting char is
        // delivered and the guard returns to idle.
        let mut g = SgrLeakGuard::default();
        // ESC [ A = Up arrow, delivered as separate chars by a broken read.
        assert!(matches!(g.feed(&leaked_esc()), Feed::Accept));
        assert!(matches!(g.feed(&leaked_char('[')), Feed::Drop));
        // 'A' is not the SGR intro: the guard aborts and *this* event is
        // accepted (returned to the caller to deal with), then goes idle.
        assert!(matches!(g.feed(&leaked_char('A')), Feed::Accept));
        assert!(g.is_idle());
        // Subsequent normal typing is accepted.
        assert!(matches!(g.feed(&leaked_char('x')), Feed::Accept));
    }

    #[test]
    fn sgr_guard_resets_on_non_key_events() {
        // A genuine parsed mouse event or a resize resyncs the tracker, so a
        // half-buffered prefix can't poison the next real interaction.
        let mut g = SgrLeakGuard::default();
        assert!(matches!(g.feed(&leaked_esc()), Feed::Accept));
        assert!(matches!(g.feed(&leaked_char('[')), Feed::Drop));
        assert!(matches!(g.feed(&Event::Resize(80, 24)), Feed::Accept));
        assert!(g.is_idle());
        assert!(matches!(g.feed(&leaked_char('x')), Feed::Accept));
    }

    #[test]
    fn sgr_guard_survives_garbage_inside_payload() {
        // A malformed payload (non-digit, non-;) while inside an SGR sequence
        // resyncs to idle instead of swallowing arbitrary following text.
        let mut g = SgrLeakGuard::default();
        assert!(matches!(g.feed(&leaked_esc()), Feed::Accept));
        assert!(matches!(g.feed(&leaked_char('[')), Feed::Drop));
        assert!(matches!(g.feed(&leaked_char('<')), Feed::Drop));
        // A letter that is not the terminator: abort and resync.
        assert!(matches!(g.feed(&leaked_char('Z')), Feed::Accept));
        assert!(g.is_idle());
    }
}
