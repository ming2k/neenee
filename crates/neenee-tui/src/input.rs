//! Input handling: keyboard and mouse events mapped to semantic actions.

use crossterm::event::{Event, KeyCode, KeyModifiers, MouseButton, MouseEventKind};

use crate::layout::{LayoutMap, SemanticCursor};
use crate::selection::SelectionDrag;

pub struct InputContext {
    pub active_modal: super::Modal,
    pub is_responding: bool,
    pub input_starts_with_slash: bool,
    pub suggestion_count: usize,
    pub has_exact_suggestion: bool,
    pub suggestion_index: Option<usize>,
    pub permission_confirm_always: bool,
    /// Whether the view is zoomed into a sub-agent task (focus stack non-empty).
    pub in_subagent_view: bool,
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
    /// Switch provider.
    SwitchProvider {
        provider_type: String,
        model: String,
    },
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
    /// Start selection at screen coordinates.
    SelectionStart { x: u16, y: u16 },
    /// Update selection to screen coordinates.
    SelectionUpdate { x: u16, y: u16 },
    /// End selection.
    SelectionEnd,
    /// Select entire block at coordinates (e.g. triple-click).
    SelectBlock { x: u16, y: u16 },
    /// Submit the API key typed in the key-input modal.
    SubmitApiKey,
    /// Submit a custom OpenAI-compatible endpoint.
    SubmitEndpoint,
    /// Submit the model ID for a custom endpoint.
    SubmitModelName,
    /// Configure the API key for the selected provider in the models modal.
    ConfigureKey,
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
fn insert_newline(
    input: &mut String,
    cursor_position: &mut usize,
    active_modal: super::Modal,
) {
    if matches!(
        active_modal,
        super::Modal::None | super::Modal::ApiKey | super::Modal::Endpoint | super::Modal::ModelName
    ) {
        let byte_pos = input
            .char_indices()
            .map(|(i, _)| i)
            .nth(*cursor_position)
            .unwrap_or(input.len());
        input.insert(byte_pos, '\n');
        *cursor_position += 1;
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
                        } else {
                            InputAction::PermissionReject
                        }
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
                KeyCode::Char('q')
                    if input.is_empty() && context.active_modal == super::Modal::None =>
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
                    super::Modal::Models => InputAction::SwitchProvider {
                        provider_type: String::new(),
                        model: String::new(),
                    },
                    super::Modal::HistorySearch => InputAction::SendChat(String::new()),
                    super::Modal::Sessions => InputAction::OpenSelectedSession,
                    super::Modal::Permission => InputAction::PermissionSubmit,
                    super::Modal::ApiKey => InputAction::SubmitApiKey,
                    super::Modal::Endpoint => InputAction::SubmitEndpoint,
                    super::Modal::ModelName => InputAction::SubmitModelName,
                    super::Modal::Help => InputAction::CloseModal,
                    super::Modal::None => {
                        // If slash suggestions are visible and none selected, auto-pick first.
                        if context.input_starts_with_slash
                            && context.suggestion_count > 0
                            && context.suggestion_index.is_none()
                            && !context.has_exact_suggestion
                        {
                            return InputAction::AcceptSuggestion("0".to_string());
                        }
                        let text = std::mem::take(input);
                        *cursor_position = 0;
                        if text.starts_with('/') {
                            match text.as_str() {
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
                        && context.input_starts_with_slash
                        && context.suggestion_count > 0
                    {
                        let next = match context.suggestion_index {
                            Some(i) => (i + 1) % context.suggestion_count,
                            None => 0,
                        };
                        InputAction::AcceptSuggestion(next.to_string())
                    } else {
                        InputAction::None
                    }
                }
                // Ctrl+J: alias for Alt+Enter — insert a literal newline.
                KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    insert_newline(input, cursor_position, context.active_modal);
                    InputAction::None
                }
                KeyCode::Char(c) => {
                    // Sibling sub-agent navigation: only when not typing (empty
                    // input) and zoomed into a sub-agent view.
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
                    if context.active_modal == super::Modal::Models && c == 'k' {
                        InputAction::ConfigureKey
                    } else if context.active_modal == super::Modal::Sessions && c == 'd' {
                        InputAction::DeleteSelectedSession
                    } else if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::ApiKey
                            | super::Modal::Endpoint
                            | super::Modal::ModelName
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
                    if matches!(
                        context.active_modal,
                        super::Modal::None
                            | super::Modal::ApiKey
                            | super::Modal::Endpoint
                            | super::Modal::ModelName
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
                            | super::Modal::ApiKey
                            | super::Modal::Endpoint
                            | super::Modal::ModelName
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
                            | super::Modal::ApiKey
                            | super::Modal::Endpoint
                            | super::Modal::ModelName
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
                    super::Modal::Permission
                    | super::Modal::ApiKey
                    | super::Modal::Endpoint
                    | super::Modal::ModelName
                    | super::Modal::Help => InputAction::None,
                    super::Modal::None => {
                        if context.input_starts_with_slash && context.suggestion_count > 0 {
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
                    super::Modal::Permission
                    | super::Modal::ApiKey
                    | super::Modal::Endpoint
                    | super::Modal::ModelName
                    | super::Modal::Help => InputAction::None,
                    super::Modal::None => {
                        if context.input_starts_with_slash && context.suggestion_count > 0 {
                            InputAction::SuggestNext
                        } else {
                            InputAction::HistoryNext
                        }
                    }
                },
                KeyCode::PageUp if context.active_modal == super::Modal::None => {
                    InputAction::ScrollPageUp
                }
                KeyCode::PageDown if context.active_modal == super::Modal::None => {
                    InputAction::ScrollPageDown
                }
                KeyCode::Home if context.active_modal == super::Modal::None => {
                    InputAction::ScrollTop
                }
                KeyCode::End if context.active_modal == super::Modal::None => {
                    InputAction::ScrollBottom
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
                active_modal: crate::Modal::None,
                is_responding: false,
                input_starts_with_slash: true,
                suggestion_count: 1,
                has_exact_suggestion: exact,
                suggestion_index: None,
                permission_confirm_always: false,
                in_subagent_view: false,
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
                active_modal: crate::Modal::Permission,
                is_responding: true,
                input_starts_with_slash: false,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: true,
                in_subagent_view: false,
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
                active_modal: crate::Modal::None,
                is_responding: false,
                input_starts_with_slash: false,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                in_subagent_view: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::CtrlC);
    }

    #[test]
    fn k_in_models_modal_configures_api_key() {
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        let action = process_event(
            Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)),
            &mut input,
            &mut cursor,
            InputContext {
                active_modal: crate::Modal::Models,
                is_responding: false,
                input_starts_with_slash: false,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                in_subagent_view: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::ConfigureKey);
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
                active_modal: crate::Modal::None,
                is_responding: false,
                input_starts_with_slash: false,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                in_subagent_view: false,
            },
            &mut drag,
        );
        assert_eq!(action, InputAction::ToggleToolSteps);
    }

    fn key_in_view(code: KeyCode, in_subagent_view: bool, input: &mut String) -> InputAction {
        let mut cursor = input.chars().count();
        let mut drag = SelectionDrag::default();
        process_event(
            Event::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)),
            input,
            &mut cursor,
            InputContext {
                active_modal: crate::Modal::None,
                is_responding: false,
                input_starts_with_slash: false,
                suggestion_count: 0,
                has_exact_suggestion: false,
                suggestion_index: None,
                permission_confirm_always: false,
                in_subagent_view,
            },
            &mut drag,
        )
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
}
