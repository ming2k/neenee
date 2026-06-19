//! Input handling: keyboard and mouse events mapped to semantic actions.

use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};

use crate::tui::layout::{LayoutMap, SemanticCursor};
use crate::tui::selection::SelectionDrag;

/// Which surface currently owns keyboard focus.
///
/// The TUI splits keyboard input into two zones so the same key (arrows,
/// Enter) has a single, unambiguous meaning per zone. `Tab` is reserved as the
/// symmetric toggle between the two zones:
///
/// - [`FocusZone::Compose`] — the input box owns the keys. Typing inserts into
///   the prompt, `↑`/`↓` walk input history or slash suggestions, `Tab`
///   accepts a slash suggestion (when one is open) or toggles into Browse.
///   This is the default.
/// - [`FocusZone::Browse`] — the conversation stream owns the keys. `↑` / `↓`
///   cycle the keyboard-focused step, `Enter` / `Space` activate it, `Tab`
///   toggles back to Compose, and any other printable character drops back
///   into [`FocusZone::Compose`] and inserts itself.
///
/// Transitions are explicit so the meaning of every key is derivable from the
/// current zone, and a visible indicator (input border + hint-bar label) tells
/// the user which zone they are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FocusZone {
    #[default]
    Compose,
    Browse,
}

impl FocusZone {
    pub fn is_compose(self) -> bool {
        matches!(self, FocusZone::Compose)
    }

    pub fn is_browse(self) -> bool {
        matches!(self, FocusZone::Browse)
    }
}

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
    /// Whether the view is zoomed into a sub-agent task (focus stack non-empty).
    pub in_subagent_view: bool,
    /// Whether a keyboard-focusable step or action target is active.
    pub has_focused_target: bool,
    /// Which surface (input box vs conversation stream) owns keyboard focus.
    pub focus_zone: FocusZone,
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
    /// Activate a model from the `/models` picker: the default model when the
    /// filter is empty (fast path), otherwise the highlighted filtered row.
    /// Falls through to the API-key setup modal when the target has no key.
    ModelPickerActivate,
    /// Toggle the favorite flag on the highlighted picker row.
    ModelPickerToggleFavorite,
    /// Open the unified model editor (`e`) for the highlighted picker row.
    OpenModelEditor,
    /// Submit the unified model editor: persist the entered key / model-id and
    /// activate the target model.
    SubmitModelEditor,
    /// Cycle focus between the editor's fields (API key ↔ model id).
    ModelEditorNextField,
    /// Interrupt current operation.
    Interrupt,
    /// Open models modal.
    OpenModels,
    /// Open history search modal.
    OpenHistory,
    /// Open the command palette (slash commands).
    OpenCommands,
    /// Open the help / keybindings modal.
    OpenHelp,
    /// Open the currently-selected session in the sessions picker.
    OpenSelectedSession,
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
    /// Plain Ctrl+C: copy selection, interrupt, clear input, or arm quit.
    CtrlC,
    /// Toggle expanded details for semantic tool steps.
    ToggleToolSteps,
    /// Move keyboard focus to the next activatable target.
    FocusNextTarget,
    /// Move keyboard focus to the previous activatable target.
    FocusPrevTarget,
    /// Activate the current keyboard-focused target.
    ActivateFocusedTarget,
    /// Switch the keyboard focus zone to the conversation stream (Browse).
    /// One half of the Tab toggle: `backward` is `true` for Shift+Tab (lands
    /// on the last step) and `false` for Tab (lands on the first step).
    EnterBrowseZone { backward: bool },
    /// Switch the keyboard focus zone back to the input box (Compose).
    ReturnToComposeZone,
    /// Paste from the system clipboard (image or text). Resolved by the app
    /// loop, which reads the clipboard asynchronously.
    Paste,
    /// Input character.
    InsertChar(char),
    /// Delete character before cursor.
    Backspace,
    /// Move cursor left.
    CursorLeft,
    /// Move cursor right.
    CursorRight,
    /// Cycle suggestion forward.
    SuggestNext,
    /// Cycle suggestion backward.
    SuggestPrev,
    /// Accept suggestion.
    AcceptSuggestion(String),
    /// Navigate history up.
    HistoryPrev,
    /// Navigate history down.
    HistoryNext,
    /// Accept the highlighted fuzzy match from the Ctrl+R history-search modal:
    /// insert the selected entry into the input box (replacing the query) and
    /// close the modal. The message is not sent — the user can edit and press
    /// Enter again to ship it.
    HistoryInsert,
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
    /// Toggle/select the currently highlighted question option.
    QuestionToggle,
    /// Submit the question modal answers.
    QuestionSubmit,
    /// Cancel the question modal.
    QuestionCancel,
    /// Select a question option by its 1-based index.
    QuestionSelect(usize),
    /// Insert a character into the question modal's "Other" free-text field.
    QuestionInsertChar(char),
    /// Delete a character from the question modal's "Other" free-text field.
    QuestionBackspace,
    /// Start selection at screen coordinates.
    SelectionStart { x: u16, y: u16 },
    /// Update selection to screen coordinates.
    SelectionUpdate { x: u16, y: u16 },
    /// End selection.
    SelectionEnd,
    /// Select entire block at coordinates (e.g. triple-click).
    SelectBlock { x: u16, y: u16 },
    /// Right-click at screen coordinates. Opens a context/detail view for the
    /// interactive element under the cursor (e.g. a tool step's full output).
    RightClick { x: u16, y: u16 },
    /// Mouse pointer moved to screen coordinates (hover tracking). Used to
    /// drive hover affordances on clickable elements like reasoning-trace
    /// headers. Suppressed while an overlay modal is open.
    Hover { x: u16, y: u16 },
    /// Leave the current sub-agent view and return to the parent.
    ExitSubAgent,
    /// Move to the previous sibling sub-agent task.
    PrevSibling,
    /// Move to the next sibling sub-agent task.
    NextSibling,
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
                MouseEventKind::ScrollUp => InputAction::ScrollUp,
                MouseEventKind::ScrollDown => InputAction::ScrollDown,
                MouseEventKind::Down(MouseButton::Left) => {
                    if context.active_modal == super::Modal::None {
                        drag.start(SemanticCursor::new(0, 0, 0));
                        InputAction::SelectionStart { x, y }
                    } else {
                        InputAction::None
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if drag.active && context.active_modal == super::Modal::None {
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
                    if context.active_modal == super::Modal::None {
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
                // mode 1003 "all motion"). Only forwarded on the main view so
                // hover affordances don't fire behind an overlay modal.
                MouseEventKind::Moved => {
                    if context.active_modal == super::Modal::None {
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
            // Plain Ctrl+C: semantic copy/interrupt/clear/quit, resolved by the app.
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
                        } else if context.focus_zone.is_browse() {
                            // While browsing the transcript behind a permission
                            // sheet, Esc returns focus to the sheet rather than
                            // rejecting outright — a second Esc decides it.
                            InputAction::ReturnToComposeZone
                        } else {
                            InputAction::PermissionReject
                        }
                    } else if context.active_modal == super::Modal::Question {
                        InputAction::QuestionCancel
                    } else if context.active_modal != super::Modal::None {
                        InputAction::CloseModal
                    } else if context.in_subagent_view {
                        InputAction::ExitSubAgent
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
                        InputAction::OpenModels
                    } else {
                        InputAction::None
                    }
                }
                KeyCode::Char('q')
                    if input.is_empty()
                        && context.active_modal == super::Modal::None
                        && context.focus_zone.is_compose() =>
                {
                    InputAction::Quit
                }
                // Alt+Enter / Ctrl+J: insert a literal newline so the input
                // box supports multi-line drafting. Plain Enter sends the
                // message, so these are the only multi-line entry paths.
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                    insert_newline(input, cursor_position, context.active_modal);
                    InputAction::None
                }
                KeyCode::Enter => match context.active_modal {
                    super::Modal::Models => InputAction::ModelPickerActivate,
                    super::Modal::ModelEditor => InputAction::SubmitModelEditor,
                    super::Modal::HistorySearch => InputAction::HistoryInsert,
                    super::Modal::Sessions => InputAction::OpenSelectedSession,
                    super::Modal::Permission => InputAction::PermissionSubmit,
                    super::Modal::Question => InputAction::QuestionSubmit,
                    super::Modal::Help => InputAction::CloseModal,
                    super::Modal::ToolStepDetail => InputAction::CloseModal,
                    super::Modal::None => {
                        if context.focus_zone.is_browse() {
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
                            return InputAction::AcceptSuggestion("0".to_string());
                        }
                        let text = std::mem::take(input);
                        *cursor_position = 0;
                        if text.starts_with('/') {
                            // Match on the trimmed text: accepting a slash
                            // completion appends a trailing space, which would
                            // otherwise make "/models " miss the exact-match
                            // arm and silently no-op in the backend.
                            match text.trim() {
                                "/models" => InputAction::OpenModels,
                                "/exit" => InputAction::Quit,
                                _ => InputAction::SendSlash(text),
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
                        // next entry rather than toggling zones.
                        let next = match context.suggestion_index {
                            Some(i) => (i + 1) % context.suggestion_count,
                            None => 0,
                        };
                        InputAction::AcceptSuggestion(next.to_string())
                    } else if context.active_modal == super::Modal::ModelEditor {
                        // Tab cycles focus between the editor's API-key and
                        // model-id fields.
                        InputAction::ModelEditorNextField
                    } else if context.active_modal != super::Modal::None
                        && context.active_modal != super::Modal::Permission
                    {
                        InputAction::None
                    } else if context.focus_zone.is_browse() {
                        // Browse → Compose: Tab is a symmetric zone toggle.
                        InputAction::ReturnToComposeZone
                    } else {
                        // Compose → Browse: hand the keyboard over to the
                        // conversation stream and focus the first step.
                        // Works with or without text in the prompt (the draft
                        // is preserved in the buffer).
                        InputAction::EnterBrowseZone { backward: false }
                    }
                }
                KeyCode::BackTab => {
                    if context.active_modal != super::Modal::None
                        && context.active_modal != super::Modal::Permission
                    {
                        InputAction::None
                    } else if context.focus_zone.is_browse() {
                        // Browse → Compose: Shift+Tab mirrors Tab's toggle.
                        InputAction::ReturnToComposeZone
                    } else {
                        // Compose → Browse, focusing the last step.
                        InputAction::EnterBrowseZone { backward: true }
                    }
                }
                // Ctrl+J: alias for Alt+Enter — insert a literal newline.
                KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    insert_newline(input, cursor_position, context.active_modal);
                    InputAction::None
                }
                // Ctrl+V: paste from the system clipboard. Only active on the
                // main prompt (not inside modals); the app loop reads the
                // clipboard asynchronously and either attaches an image or
                // inserts the text at the cursor.
                KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if context.active_modal == super::Modal::None {
                        InputAction::Paste
                    } else {
                        InputAction::None
                    }
                }
                // Ctrl+A: move the caret to the start of the current line
                // (readline convention). Works wherever free text is being
                // edited — the main prompt in Compose zone and the free-text
                // modals. Outside those (Browse zone, read-only modals) it is
                // a no-op so it never inserts a literal 'a' or scrolls.
                KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Models | super::Modal::ModelEditor
                    ) {
                        cursor_line_start(input, cursor_position);
                    }
                    InputAction::None
                }
                // Ctrl+E: move the caret to the end of the current line.
                KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Models | super::Modal::ModelEditor
                    ) {
                        cursor_line_end(input, cursor_position);
                    }
                    InputAction::None
                }
                KeyCode::Char(c) => {
                    // Sibling sub-agent navigation works in both zones (it is a
                    // sub-agent view feature, not a typing-navigation thing)
                    // but only when no text is being composed.
                    if context.active_modal == super::Modal::None
                        && context.in_subagent_view
                        && input.is_empty()
                    {
                        match c {
                            '[' => return InputAction::PrevSibling,
                            ']' => return InputAction::NextSibling,
                            _ => {}
                        }
                    }
                    if context.active_modal == super::Modal::Question && c == ' ' {
                        return InputAction::QuestionToggle;
                    }
                    if context.active_modal == super::Modal::Question {
                        if let Some(d) = c.to_digit(10) {
                            if d >= 1 && d <= 9 {
                                return InputAction::QuestionSelect(d as usize);
                            }
                        }
                    }
                    if context.active_modal == super::Modal::None
                        && context.focus_zone.is_browse()
                        && c == ' '
                    {
                        return InputAction::ActivateFocusedTarget;
                    }
                    // Any printable character in the Browse zone drops back to
                    // the Compose zone and inserts itself, mirroring the
                    // "type to edit" affordance users expect from modal editors.
                    // The insertion happens here (process_event owns `input`),
                    // and the zone switch is signalled back so the renderer
                    // can update on the next frame.
                    if context.active_modal == super::Modal::None && context.focus_zone.is_browse()
                    {
                        let byte_pos = input
                            .char_indices()
                            .map(|(i, _)| i)
                            .nth(*cursor_position)
                            .unwrap_or(input.len());
                        input.insert(byte_pos, c);
                        *cursor_position += 1;
                        return InputAction::ReturnToComposeZone;
                    }
                    if context.active_modal == super::Modal::Models && c == '*' {
                        // Star a model as a favorite. `*` is chosen over `f`
                        // because every letter collides with the fuzzy filter
                        // (you could never start a query for "flash" or
                        // "deepseek"). `*` evokes the favorite star and never
                        // begins a model-name query.
                        InputAction::ModelPickerToggleFavorite
                    } else if context.active_modal == super::Modal::Models
                        && c == 'e'
                        && input.is_empty()
                    {
                        // `e` opens the editor for the highlighted row. Gated
                        // on an empty filter so it never fights typing: no
                        // built-in model name starts with `e`, so clearing the
                        // filter then pressing `e` always reaches the editor.
                        InputAction::OpenModelEditor
                    } else if context.active_modal == super::Modal::Sessions && c == 'd' {
                        InputAction::DeleteSelectedSession
                    } else if context.active_modal == super::Modal::Question {
                        InputAction::QuestionInsertChar(c)
                    } else if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Models | super::Modal::ModelEditor
                    ) {
                        let byte_pos = input
                            .char_indices()
                            .map(|(i, _)| i)
                            .nth(*cursor_position)
                            .unwrap_or(input.len());
                        input.insert(byte_pos, c);
                        *cursor_position += 1;
                        InputAction::None
                    } else {
                        InputAction::None
                    }
                }
                KeyCode::Backspace => {
                    if context.active_modal == super::Modal::Question {
                        InputAction::QuestionBackspace
                    } else if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Models | super::Modal::ModelEditor
                    ) && *cursor_position > 0
                    {
                        *cursor_position -= 1;
                        let byte_pos = input
                            .char_indices()
                            .map(|(i, _)| i)
                            .nth(*cursor_position)
                            .unwrap_or(input.len());
                        input.remove(byte_pos);
                        InputAction::None
                    } else {
                        InputAction::None
                    }
                }
                KeyCode::Left => {
                    if context.active_modal == super::Modal::Permission {
                        return InputAction::ModalUp;
                    }
                    if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Models | super::Modal::ModelEditor
                    ) && *cursor_position > 0
                    {
                        *cursor_position -= 1;
                    }
                    InputAction::None
                }
                KeyCode::Right => {
                    if context.active_modal == super::Modal::Permission {
                        return InputAction::ModalDown;
                    }
                    if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Models | super::Modal::ModelEditor
                    ) && *cursor_position < input.chars().count()
                    {
                        *cursor_position += 1;
                    }
                    InputAction::None
                }
                KeyCode::Up => match context.active_modal {
                    super::Modal::Models => InputAction::ModalUp,
                    super::Modal::HistorySearch => InputAction::ModalUp,
                    super::Modal::Sessions => InputAction::ModalUp,
                    super::Modal::Question => InputAction::QuestionUp,
                    super::Modal::Permission => {
                        // Browse zone: walk transcript targets. Compose zone:
                        // scroll the expanded details, otherwise fall through
                        // to a transcript scroll so the history stays readable
                        // even while a prompt is pending.
                        if context.focus_zone.is_browse() {
                            InputAction::FocusPrevTarget
                        } else if context.permission_show_details {
                            InputAction::PermissionDetailsUp
                        } else {
                            InputAction::ScrollUp
                        }
                    }
                    super::Modal::ToolStepDetail => InputAction::ScrollUp,
                    super::Modal::ModelEditor
                    | super::Modal::Help => InputAction::None,
                    super::Modal::None => {
                        if context.focus_zone.is_browse() {
                            InputAction::FocusPrevTarget
                        } else if context.completion_kind != super::CompletionKind::None
                            && context.suggestion_count > 0
                        {
                            InputAction::SuggestPrev
                        } else {
                            InputAction::HistoryPrev
                        }
                    }
                },
                KeyCode::Down => match context.active_modal {
                    super::Modal::Models => InputAction::ModalDown,
                    super::Modal::HistorySearch => InputAction::ModalDown,
                    super::Modal::Sessions => InputAction::ModalDown,
                    super::Modal::Question => InputAction::QuestionDown,
                    super::Modal::Permission => {
                        if context.focus_zone.is_browse() {
                            InputAction::FocusNextTarget
                        } else if context.permission_show_details {
                            InputAction::PermissionDetailsDown
                        } else {
                            InputAction::ScrollDown
                        }
                    }
                    super::Modal::ToolStepDetail => InputAction::ScrollDown,
                    super::Modal::ModelEditor
                    | super::Modal::Help => InputAction::None,
                    super::Modal::None => {
                        if context.focus_zone.is_browse() {
                            InputAction::FocusNextTarget
                        } else if context.completion_kind != super::CompletionKind::None
                            && context.suggestion_count > 0
                        {
                            InputAction::SuggestNext
                        } else {
                            InputAction::HistoryNext
                        }
                    }
                },
                KeyCode::PageUp
                    if matches!(
                        context.active_modal,
                        super::Modal::None | super::Modal::Permission
                    ) =>
                {
                    InputAction::ScrollPageUp
                }
                KeyCode::PageDown
                    if matches!(
                        context.active_modal,
                        super::Modal::None | super::Modal::Permission
                    ) =>
                {
                    InputAction::ScrollPageDown
                }
                KeyCode::Home => {
                    // Now that focus zones disambiguate editing (Compose)
                    // from navigating (Browse), Home no longer clashes with
                    // conversation scrolling:
                    //   - Permission modal / Browse zone: scroll to the top.
                    //   - Compose zone / free-text modals: move the input
                    //     caret to the start of the current line.
                    if context.active_modal == super::Modal::Permission
                        || (context.active_modal == super::Modal::None
                            && context.focus_zone.is_browse())
                    {
                        InputAction::ScrollTop
                    } else if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Models | super::Modal::ModelEditor
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
                            && context.focus_zone.is_browse())
                    {
                        InputAction::ScrollBottom
                    } else if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Models | super::Modal::ModelEditor
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
        _ => InputAction::None,
    }
}

/// Resolve a screen coordinate to a semantic cursor using the layout map.
pub fn resolve_cursor(layout_map: &LayoutMap, x: u16, y: u16) -> Option<SemanticCursor> {
    layout_map.hit_test(x, y)
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
            },
            &mut drag,
        )
    }

    #[test]
    fn enter_executes_an_exact_slash_command() {
        let mut input = "/goal".to_string();
        assert_eq!(
            enter(&mut input, true),
            InputAction::SendSlash("/goal".to_string())
        );
    }

    #[test]
    fn enter_completes_a_slash_prefix() {
        let mut input = "/go".to_string();
        assert_eq!(
            enter(&mut input, false),
            InputAction::AcceptSuggestion("0".to_string())
        );
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
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
                active_modal: crate::tui::Modal::Models,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::ModelPickerToggleFavorite);
    }

    #[test]
    fn letter_in_models_modal_feeds_the_fuzzy_filter() {
        // `k` used to open the key configurator; now every letter feeds the
        // fuzzy filter so users can search for "kimi" or "deepseek".
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::tui::Modal::Models,
                is_responding: false,
                completion_kind: crate::tui::CompletionKind::None,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::None);
        assert_eq!(input, "k");
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
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
            in_subagent_view: false,
            has_focused_target: false,
            focus_zone: FocusZone::Compose,
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
        assert_eq!(action, InputAction::OpenModels);

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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::None);
    }

    fn key_in_view(code: KeyCode, in_subagent_view: bool, input: &mut String) -> InputAction {
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
                in_subagent_view,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
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
                in_subagent_view: false,
                has_focused_target: true,
                focus_zone: FocusZone::Browse,
            },
            &mut drag,
        )
    }

    #[test]
    fn tab_toggles_between_compose_and_browse() {
        // Compose + Tab hands focus to the conversation stream (forward =
        // first step). Tab is a pure zone toggle, so it fires whether or not
        // the prompt has text — the draft stays in the buffer.
        let mut input = String::new();
        assert_eq!(
            key_in_view(KeyCode::Tab, false, &mut input),
            InputAction::EnterBrowseZone { backward: false }
        );
        let mut input = String::from("draft");
        assert_eq!(
            key_in_view(KeyCode::Tab, false, &mut input),
            InputAction::EnterBrowseZone { backward: false }
        );
        // Shift+Tab enters Browse as well, but lands on the last step.
        let mut input = String::new();
        assert_eq!(
            key_in_view(KeyCode::BackTab, false, &mut input),
            InputAction::EnterBrowseZone { backward: true }
        );
    }

    #[test]
    fn tab_toggles_back_to_compose_in_browse_zone() {
        // In Browse, Tab / Shift+Tab hand focus back to the prompt. Only the
        // arrow keys still walk across the interactive targets.
        assert_eq!(
            key_with_focus(KeyCode::Tab),
            InputAction::ReturnToComposeZone
        );
        assert_eq!(
            key_with_focus(KeyCode::BackTab),
            InputAction::ReturnToComposeZone
        );
        assert_eq!(key_with_focus(KeyCode::Up), InputAction::FocusPrevTarget);
        assert_eq!(key_with_focus(KeyCode::Down), InputAction::FocusNextTarget);
    }

    #[test]
    fn enter_and_space_activate_focused_target() {
        assert_eq!(
            key_with_focus(KeyCode::Enter),
            InputAction::ActivateFocusedTarget
        );
        assert_eq!(
            key_with_focus(KeyCode::Char(' ')),
            InputAction::ActivateFocusedTarget
        );
    }

    #[test]
    fn escape_in_browse_does_not_switch_zone() {
        // Zone switching (Browse ↔ Compose) is Tab-only. Esc in Browse no
        // longer returns to the input box — that overloaded Esc with too many
        // meanings. Idle Browse + Esc is a no-op; Esc still exits sub-agent
        // views, interrupts a running turn, and closes modals on its own.
        assert_eq!(key_with_focus(KeyCode::Esc), InputAction::None);
    }

    #[test]
    fn typing_in_browse_returns_to_compose_and_inserts() {
        let action = key_with_focus(KeyCode::Char('a'));
        assert_eq!(action, InputAction::ReturnToComposeZone);

        // Re-run with access to the input buffer so we can assert the char
        // was inserted alongside the zone switch.
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('a'),
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
                in_subagent_view: false,
                has_focused_target: true,
                focus_zone: FocusZone::Browse,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::ReturnToComposeZone);
        assert_eq!(input, "a");
        assert_eq!(cursor, 1);
    }

    #[test]
    fn q_in_browse_inserts_instead_of_quitting() {
        // 'q' is only a quit shortcut in Compose. In Browse it behaves like
        // any other printable character so the user does not accidentally
        // exit the program while navigating steps.
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('q'),
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
                in_subagent_view: false,
                has_focused_target: true,
                focus_zone: FocusZone::Browse,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::ReturnToComposeZone);
        assert_eq!(input, "q");
    }

    #[test]
    fn escape_exits_subagent_view() {
        let mut input = String::new();
        assert_eq!(
            key_in_view(KeyCode::Esc, true, &mut input),
            InputAction::ExitSubAgent
        );
        // Outside a sub-agent view, Esc does nothing when idle (no modal).
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
        // not navigation, even inside a sub-agent view.
        let mut typing = "x".to_string();
        key_in_view(KeyCode::Char('['), true, &mut typing);
        assert_eq!(typing, "x[");

        // Outside a sub-agent view, brackets always insert.
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
        zone: FocusZone,
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: zone,
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
            FocusZone::Compose,
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
            FocusZone::Compose,
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
                FocusZone::Browse
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
                FocusZone::Browse
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
                FocusZone::Compose
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
                FocusZone::Compose
            ),
            InputAction::ScrollBottom
        );
    }

    #[test]
    fn home_and_end_move_caret_in_free_text_modals() {
        // The unified model editor borrows the input line for one field at a
        // time; Home/End should edit there too, not be swallowed.
        for modal in [crate::tui::Modal::ModelEditor, crate::tui::Modal::HistorySearch] {
            let mut input = "abc".to_string();
            let mut cursor = 2;
            let action = run_key(
                &mut input,
                &mut cursor,
                KeyCode::Home,
                KeyModifiers::NONE,
                modal,
                FocusZone::Compose,
            );
            assert_eq!(action, InputAction::None);
            assert_eq!(cursor, 0, "Home should reach line start");

            let action = run_key(
                &mut input,
                &mut cursor,
                KeyCode::End,
                KeyModifiers::NONE,
                modal,
                FocusZone::Compose,
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
            FocusZone::Compose,
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
            FocusZone::Compose,
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
                FocusZone::Browse
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
                FocusZone::Browse
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
            FocusZone::Compose,
        );
        assert_eq!(cursor, 6, "Home should land at start of current line");

        // End -> end of "line2" (char index 11, just before the second '\n').
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::End,
            KeyModifiers::NONE,
            crate::tui::Modal::None,
            FocusZone::Compose,
        );
        assert_eq!(cursor, 11, "End should land at end of current line");

        // Ctrl+A from the end of line2 should also snap to line start.
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            FocusZone::Compose,
        );
        assert_eq!(cursor, 6);
        // Ctrl+E snaps back to the line end without running off the buffer.
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('e'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            FocusZone::Compose,
        );
        assert_eq!(cursor, 11);
    }

    /// Drive the history-search modal with `code` (+ `modifiers`) and return
    /// the resulting action plus the final cursor position. The modal is open
    /// and the focus zone is Compose, matching the live state while the user
    /// is editing the fuzzy query.
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::None);
    }
}
