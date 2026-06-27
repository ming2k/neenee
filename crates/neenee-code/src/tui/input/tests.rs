//! Tests for input handling — extracted from `mod.rs` to keep the
//! production input code focused. Resolves via `mod tests;` and reaches
//! the production items through `super::*` exactly as before.

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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
    // than falling through to subagent exit / interrupt / no-op.
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
            in_subagent_view: false,
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_subagent_view: false,
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_subagent_view: false,
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_subagent_view: false,
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_subagent_view: false,
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
        in_side_view: false,
        has_focused_target: false,
        has_queued: false,
        history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: true,
            has_queued: false,
            history_searching: false,
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
fn escape_exits_subagent_view() {
    let mut input = String::new();
    assert_eq!(
        key_in_view(KeyCode::Esc, true, &mut input),
        InputAction::ExitSubAgent
    );
    // Outside a subagent view, Esc does nothing when idle (no modal).
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
    // not navigation, even inside a subagent view.
    let mut typing = "x".to_string();
    key_in_view(KeyCode::Char('['), true, &mut typing);
    assert_eq!(typing, "x[");

    // Outside a subagent view, brackets always insert.
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
            in_subagent_view: false,
            in_side_view: false,
            has_focused_target: has_focus,
            has_queued: false,
            history_searching: false,
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

fn mouse_ctx_for(modal: crate::tui::Modal) -> InputContext {
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
        in_side_view: false,
        has_focused_target: false,
        has_queued: false,
        history_searching: false,
    }
}

#[test]
fn mouse_wheel_moves_selection_in_question_modal() {
    // The question modal's option selection is driven by ↑/↓. The mouse wheel
    // must route there too (QuestionUp/QuestionDown) instead of leaking through
    // to a transcript ScrollUp/ScrollDown behind the modal — the bug this guards.
    use crossterm::event::{MouseEvent, MouseEventKind};

    let mk = |kind| {
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        process_event(
            Event::Mouse(MouseEvent {
                kind,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }),
            &mut input,
            &mut cursor,
            mouse_ctx_for(crate::tui::Modal::Question),
            &mut drag,
        )
    };

    assert_eq!(mk(MouseEventKind::ScrollUp), InputAction::QuestionUp);
    assert_eq!(mk(MouseEventKind::ScrollDown), InputAction::QuestionDown);
}

#[test]
fn mouse_wheel_still_scrolls_when_no_modal_open() {
    // Regression guard: outside the question modal the wheel keeps its original
    // transcript-scroll behavior.
    use crossterm::event::{MouseEvent, MouseEventKind};

    let mk = |kind| {
        let mut input = String::new();
        let mut cursor = 0;
        let mut drag = SelectionDrag::default();
        process_event(
            Event::Mouse(MouseEvent {
                kind,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }),
            &mut input,
            &mut cursor,
            mouse_ctx_for(crate::tui::Modal::None),
            &mut drag,
        )
    };

    assert_eq!(mk(MouseEventKind::ScrollUp), InputAction::ScrollUp);
    assert_eq!(mk(MouseEventKind::ScrollDown), InputAction::ScrollDown);
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
        false,
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
        false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: false,
            has_queued,
            history_searching: false,
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
            in_side_view: false,
            has_focused_target: true,
            has_queued: true,
            history_searching: false,
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
            in_subagent_view: false,
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
            in_subagent_view: false,
            in_side_view: false,
            has_focused_target: false,
            has_queued: false,
            history_searching: false,
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
