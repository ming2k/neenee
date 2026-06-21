//! Input handling: keyboard and mouse events mapped to semantic actions.

use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};

use crate::tui::layout::{LayoutMap, SemanticCursor};
use crate::tui::selection::SelectionDrag;

/// Which surface currently owns keyboard focus.
///
/// The TUI splits keyboard input into two zones so the same key (arrows,
/// Enter) has a single, unambiguous meaning per zone:
///
/// - [`FocusZone::Compose`] — the input box owns the keys. Typing inserts into
///   the prompt, `↑`/`↓` walk input history or slash suggestions, `Tab`
///   accepts a slash suggestion (when one is open). `Ctrl+B` switches to
///   [`FocusZone::Browse`]. This is the default.
/// - [`FocusZone::Browse`] — the conversation stream owns the keys. `↑` / `↓`
///   cycle the keyboard-focused step, `Enter` / `Space` activate it, and any
///   printable character (typically `p` for "prompt") drops back into
///   [`FocusZone::Compose`] and inserts itself.
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
    /// Whether the send queue holds at least one staged user message. While
    /// true, `↑` in the compose zone recalls the most-recently-queued
    /// message instead of walking input history.
    pub has_queued: bool,
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
    /// Toggle the favorite flag on the highlighted picker row.
    ProviderPickerToggleFavorite,
    /// Open the unified provider editor (`e`) for the highlighted picker row.
    OpenModelEditor,
    /// Submit the unified provider editor: persist the entered key / model-id and
    /// activate the target model.
    SubmitModelEditor,
    /// Cycle focus between the editor's fields (API key ↔ model id).
    ModelEditorNextField,
    /// Interrupt current operation.
    Interrupt,
    /// Open models modal.
    OpenProvider,
    /// Open history search modal.
    OpenHistory,
    /// Open the command palette (slash commands).
    OpenCommands,
    /// Open the help / keybindings modal.
    OpenHelp,
    /// Open the session-context modal (Ctrl+I): tabbed overview of the live
    /// session's model, MCP servers, and (later) permissions / tools / skills.
    OpenSession,
    /// Cycle the active pane inside the session-context modal. `forward` picks
    /// the direction so Left/Right (and later Tab/Shift+Tab) share one action.
    SessionTabCycle { forward: bool },
    /// Move the row cursor inside the session-context modal's list panes
    /// (Skills / Permissions / Tools). `forward` = down, else up.
    SessionSelect { forward: bool },
    /// Tab-aware primary action on the selected row: revoke the selected
    /// permission rule, or toggle the selected tool's enabled flag. No-op on
    /// read-only panes (Model / MCP / Skills). Bound to `Space`.
    SessionActivate,
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
    /// Triggered by `Ctrl+B` in Compose. `backward` is reserved for future
    /// use; currently always `false`.
    EnterBrowseZone { backward: bool },
    /// Switch the keyboard focus zone back to the input box (Compose).
    ReturnToComposeZone,
    /// Paste from the system clipboard (image or text). Resolved by the app
    /// loop, which reads the clipboard asynchronously.
    Paste,
    /// Terminal-level bracketed paste. The text payload is already available;
    /// the app loop routes it through the same chip-or-inline logic as
    /// [`Paste`].
    BracketedPaste(String),
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
    /// Accept the next/previous completion item by index without closing the
    /// popup. Used by `Tab`, which cycles through candidates one splice at a
    /// time. The popup re-renders against the spliced input so the user can
    /// keep cycling.
    AcceptSuggestion(String),
    /// Like [`AcceptSuggestion`] but the popup is closed afterwards. Used by
    /// `Enter` (both the slash-prefix auto-accept and the highlighted-item
    /// path). The harness latches a `completion_dismissed` flag so the popup
    /// stays hidden until the next `InsertChar` / `Backspace`, matching the
    /// expectation that pressing Enter "finishes" the current completion.
    CommitSuggestion(String),
    /// Dismiss the completion popup without accepting anything. Used by `Esc`
    /// when a slash/path completion menu is open. Latches the same
    /// `completion_dismissed` flag as [`CommitSuggestion`] so the popup stays
    /// hidden until the next edit clears the latch.
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

/// Find the start char index of the previous whitespace-delimited word,
/// stopping at the beginning of the current logical line so multi-line
/// drafts never leak kills across newlines. Matches readline's
/// `unix-word-rubout` (Ctrl+W) and the `backward-word` /
/// `backward-kill-word` motions users expect from shells and editors.
fn prev_word_start(input: &str, cursor_position: usize) -> usize {
    let chars: Vec<char> = input.chars().collect();
    let line_start = cursor_line_start_char(&chars, cursor_position);
    let mut i = cursor_position.min(chars.len());
    // Skip whitespace between caret and the previous word.
    while i > line_start && chars[i - 1].is_whitespace() && chars[i - 1] != '\n' {
        i -= 1;
    }
    // Skip the contiguous run of non-whitespace that forms the word.
    while i > line_start && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

/// Find the end char index of the next whitespace-delimited word, stopping
/// at the end of the current logical line so the caret never crosses a
/// newline. Matches readline's `kill-word` (Alt+D) and `forward-word`
/// motions.
fn next_word_end(input: &str, cursor_position: usize) -> usize {
    let chars: Vec<char> = input.chars().collect();
    let line_end = cursor_line_end_char(&chars, cursor_position);
    let mut i = cursor_position.min(chars.len());
    // Skip whitespace between caret and the next word.
    while i < line_end && chars[i].is_whitespace() {
        i += 1;
    }
    // Skip the contiguous run of non-whitespace that forms the word.
    while i < line_end && !chars[i].is_whitespace() {
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
                    } else if context.completion_kind != super::CompletionKind::None
                        && context.suggestion_count > 0
                    {
                        // A completion popup (slash command or `@path`) is
                        // open: Esc dismisses it without touching the input
                        // text. The popup stays hidden until the next edit
                        // clears the dismissal latch, so Esc then ↑/↓ walks
                        // history instead of suggestions.
                        InputAction::CloseCompletion
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
                // Note: the session-context modal is opened via the
                // `/session` slash command (see the Enter submit path), not a
                // Ctrl+ combo — Ctrl+I collides byte-for-byte with Tab on most
                // terminals, so a Ctrl+I binding would fire as Tab (completion
                // accept or no-op) on terminals without Kitty protocol support.
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
                    super::Modal::Provider => InputAction::ProviderPickerActivate,
                    super::Modal::ModelEditor => InputAction::SubmitModelEditor,
                    super::Modal::HistorySearch => InputAction::HistoryInsert,
                    super::Modal::Sessions => InputAction::OpenSelectedSession,
                    super::Modal::Permission => InputAction::PermissionSubmit,
                    super::Modal::Question => InputAction::QuestionSubmit,
                    super::Modal::Help => InputAction::CloseModal,
                    super::Modal::ToolStepDetail => InputAction::CloseModal,
                    super::Modal::Session => InputAction::CloseModal,
                    super::Modal::PlanPreview => InputAction::CloseModal,
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
                        if let Some(i) = context.suggestion_index {
                            if context.completion_kind != super::CompletionKind::None
                                && context.suggestion_count > 0
                            {
                                return InputAction::CommitSuggestion(i.to_string());
                            }
                        }
                        let text = std::mem::take(input);
                        *cursor_position = 0;
                        if text.starts_with('/') {
                            // Match on the trimmed text: accepting a slash
                            // completion appends a trailing space, which would
                            // otherwise make "-----" miss the exact-match
                            // arm and silently no-op in the backend.
                            match text.trim() {
                                "/provider" => InputAction::OpenProvider,
                                "/session" => InputAction::OpenSession,
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
                    } else {
                        // No completion open and no modal field to cycle: Tab
                        // is a no-op. Zone switching is Ctrl+B / `p`, not Tab.
                        InputAction::None
                    }
                }
                KeyCode::BackTab => {
                    // Shift+Tab mirrors Tab in modals (no-op outside model
                    // editor). Zone switching uses Ctrl+B / `p`, not Tab.
                    InputAction::None
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
                // Ctrl+B: switch from Compose to Browse (B = Browse). Dedicated
                // zone-switch key so Tab is free for completion-only duty.
                // No-op outside the main prompt (modals, Browse zone).
                KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if context.active_modal == super::Modal::None && context.focus_zone.is_compose()
                    {
                        InputAction::EnterBrowseZone { backward: false }
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
                            | super::Modal::Provider
                            | super::Modal::ModelEditor
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
                            | super::Modal::Provider
                            | super::Modal::ModelEditor
                    ) {
                        cursor_line_end(input, cursor_position);
                    }
                    InputAction::None
                }
                // Ctrl+W: delete the previous whitespace-delimited word
                // (readline `unix-word-rubout`). Skips trailing whitespace
                // then removes the contiguous run of non-whitespace before
                // the caret, stopping at the start of the current logical
                // line so multi-line drafts never leak kills across
                // newlines. No-op outside free-text surfaces so it never
                // closes a modal or inserts a literal 'w'.
                KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Provider
                            | super::Modal::ModelEditor
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
                    if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Provider
                            | super::Modal::ModelEditor
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
                    if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Provider
                            | super::Modal::ModelEditor
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
                    if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Provider
                            | super::Modal::ModelEditor
                    ) {
                        *cursor_position = prev_word_start(input, *cursor_position);
                    }
                    InputAction::None
                }
                // Alt+F: jump forward one word (readline `forward-word`).
                KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                    if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Provider
                            | super::Modal::ModelEditor
                    ) {
                        *cursor_position = next_word_end(input, *cursor_position);
                    }
                    InputAction::None
                }
                // Alt+D: delete the next whitespace-delimited word (readline
                // `kill-word`). Symmetric counterpart to Ctrl+W.
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::ALT) => {
                    if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Provider
                            | super::Modal::ModelEditor
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
                    // Space inside the session modal is the tab-aware primary
                    // action: revoke the selected permission / toggle the
                    // selected tool. No-op on read-only panes.
                    if context.active_modal == super::Modal::Session && c == ' ' {
                        return InputAction::SessionActivate;
                    }
                    if context.active_modal == super::Modal::Question {
                        if let Some(d) = c.to_digit(10) {
                            if (1..=9).contains(&d) {
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
                    if context.active_modal == super::Modal::Provider && c == '*' {
                        // Star a model as a favorite. `*` is chosen over `f`
                        // because every letter collides with the fuzzy filter
                        // (you could never start a query for "flash" or
                        // "deepseek"). `*` evokes the favorite star and never
                        // begins a model-name query.
                        InputAction::ProviderPickerToggleFavorite
                    } else if context.active_modal == super::Modal::Provider
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
                            | super::Modal::Provider
                            | super::Modal::ModelEditor
                    ) {
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
                    } else if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Provider
                            | super::Modal::ModelEditor
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
                    if context.active_modal == super::Modal::Session {
                        return InputAction::SessionTabCycle { forward: false };
                    }
                    if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Provider
                            | super::Modal::ModelEditor
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
                    if context.active_modal == super::Modal::Session {
                        return InputAction::SessionTabCycle { forward: true };
                    }
                    if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::HistorySearch
                            | super::Modal::Provider
                            | super::Modal::ModelEditor
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
                        if context.focus_zone.is_browse() {
                            InputAction::FocusPrevTarget
                        } else if context.permission_show_details {
                            InputAction::PermissionDetailsUp
                        } else {
                            InputAction::ScrollUp
                        }
                    }
                    super::Modal::ToolStepDetail => InputAction::ScrollUp,
                    super::Modal::PlanPreview => InputAction::ScrollUp,
                    super::Modal::Session => InputAction::SessionSelect { forward: false },
                    super::Modal::ModelEditor | super::Modal::Help => InputAction::None,
                    super::Modal::None => {
                        if context.focus_zone.is_browse() {
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
                        if context.focus_zone.is_browse() {
                            InputAction::FocusNextTarget
                        } else if context.permission_show_details {
                            InputAction::PermissionDetailsDown
                        } else {
                            InputAction::ScrollDown
                        }
                    }
                    super::Modal::ToolStepDetail => InputAction::ScrollDown,
                    super::Modal::PlanPreview => InputAction::ScrollDown,
                    super::Modal::Session => InputAction::SessionSelect { forward: true },
                    super::Modal::ModelEditor | super::Modal::Help => InputAction::None,
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
                            | super::Modal::Provider
                            | super::Modal::ModelEditor
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
                            | super::Modal::Provider
                            | super::Modal::ModelEditor
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
            // same chip-or-inline logic as Ctrl+V; only on the main prompt.
            if context.active_modal == super::Modal::None {
                InputAction::BracketedPaste(text)
            } else {
                InputAction::None
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
                has_queued: false,
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
                has_queued: false,
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
            InputAction::CommitSuggestion("0".to_string())
        );
    }

    #[test]
    fn enter_accepts_a_highlighted_slash_suggestion() {
        // User typed `/m`, menu shows `/mode` / `/mcp` / `/provider`, user
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
        // User typed `/mode` (exact match) but then pressed ↓ to highlight
        // `/provider`. The explicit highlight is a stronger signal than the
        // exact-match fast path, so Enter accepts the highlight.
        let mut input = "/mode".to_string();
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
        // than falling through to sub-agent exit / interrupt / no-op.
        let mut input = "/mod".to_string();
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
                completion_kind: crate::tui::CompletionKind::Slash,
                suggestion_count: 2,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                permission_show_details: false,
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
                has_queued: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::CloseCompletion);
        // The input text is left untouched — Esc only closes the popup.
        assert_eq!(input, "/mod");
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
                has_queued: false,
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
                has_queued: false,
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
        let mut input = "/mod".to_string();
        let mut cursor = 4;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)),
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
                has_queued: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::InsertChar('e'));
        assert_eq!(input, "/mode");
        assert_eq!(cursor, 5);
    }

    #[test]
    fn backspace_in_compose_returns_backspace_action() {
        // Same signal contract as InsertChar: Backspace must be returned so
        // the event loop clears completion_dismissed + suggestion_index.
        let mut input = "/mode".to_string();
        let mut cursor = 5;
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
                has_queued: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::Backspace);
        assert_eq!(input, "/mod");
        assert_eq!(cursor, 4);
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
                has_queued: false,
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
                has_queued: false,
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
                has_queued: false,
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
                has_queued: false,
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
                has_queued: false,
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
                has_queued: false,
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
                has_queued: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::ProviderPickerToggleFavorite);
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
                active_modal: crate::tui::Modal::Provider,
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
                has_queued: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::InsertChar('k'));
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
                has_queued: false,
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
            has_queued: false,
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
                has_queued: false,
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
                has_queued: false,
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
                has_queued: false,
            },
            &mut drag,
        )
    }

    #[test]
    fn tab_in_compose_without_suggestions_is_noop() {
        // Tab is completion-only: with no suggestion menu open, it does
        // nothing. Zone switching is Ctrl+B, not Tab.
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
    fn ctrl_b_switches_compose_to_browse() {
        // Ctrl+B in Compose enters Browse, focusing the first step.
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char('b'),
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
                has_queued: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::EnterBrowseZone { backward: false });
    }

    #[test]
    fn tab_in_browse_is_noop() {
        // Tab / Shift+Tab in Browse are no-ops. Arrows still walk targets.
        // Return to Compose via any printable char (typically `p`).
        assert_eq!(key_with_focus(KeyCode::Tab), InputAction::None);
        assert_eq!(key_with_focus(KeyCode::BackTab), InputAction::None);
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
        // Zone switching uses Ctrl+B (compose → browse) and printable chars
        // (browse → compose). Esc in Browse is a no-op; Esc still exits
        // sub-agent views, interrupts a running turn, and closes modals.
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
                has_queued: false,
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
                has_queued: false,
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
                has_queued: false,
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
            FocusZone::Compose,
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
            FocusZone::Compose,
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
            FocusZone::Compose,
        );
        assert_eq!(action, InputAction::None);
        assert_eq!(input, "hello world");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn ctrl_w_does_not_cross_newline() {
        // Multi-line draft: Ctrl+W on the second line must not eat into the
        // first line. "line1\nworld" -> caret at end (11 chars).
        let mut input = "line1\nworld".to_string();
        let mut cursor = 11;
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('w'),
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            FocusZone::Compose,
        );
        assert_eq!(input, "line1\n");
        assert_eq!(cursor, 6);
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
            FocusZone::Compose,
        );
        assert_eq!(action, InputAction::None);
        assert_eq!(input, "abc");
        assert_eq!(cursor, 3);
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
            FocusZone::Compose,
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
            FocusZone::Compose,
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
            FocusZone::Compose,
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
            FocusZone::Compose,
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
            FocusZone::Compose,
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
            FocusZone::Compose,
        );
        assert_eq!(cursor, 10);
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('b'),
            KeyModifiers::ALT,
            crate::tui::Modal::None,
            FocusZone::Compose,
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
            FocusZone::Compose,
        );
        assert_eq!(cursor, 3);
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Char('f'),
            KeyModifiers::ALT,
            crate::tui::Modal::None,
            FocusZone::Compose,
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
            FocusZone::Compose,
        );
        assert_eq!(cursor, 12, "Ctrl+Left snaps to the start of 'charlie'");
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Left,
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            FocusZone::Compose,
        );
        assert_eq!(cursor, 6, "Ctrl+Left snaps to the start of 'bravo'");
        run_key(
            &mut input,
            &mut cursor,
            KeyCode::Right,
            KeyModifiers::CONTROL,
            crate::tui::Modal::None,
            FocusZone::Compose,
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
            FocusZone::Compose,
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
            FocusZone::Compose,
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
            FocusZone::Compose,
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
                FocusZone::Compose,
            );
            assert_eq!(action, InputAction::None);
            assert!(input.is_empty());
            assert_eq!(cursor, 0);
        }
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
                has_queued: false,
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
                has_queued: false,
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
                has_queued: false,
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
                in_subagent_view: false,
                has_focused_target: false,
                focus_zone: FocusZone::Compose,
                has_queued,
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
                in_subagent_view: false,
                has_focused_target: true,
                focus_zone: FocusZone::Browse,
                has_queued: true,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::FocusPrevTarget);
    }
}
