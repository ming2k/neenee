//! Rendering engine: draws the chat UI using ratatui while recording
//! semantic-to-screen layout information.

mod blocks;
mod cards;
mod input_box;
mod modals;
mod status;
mod table;
mod text;
mod theme;
mod util;

use blocks::render_message_blocks;
use cards::{
    draw_sticky_header_if_needed, draw_subagent_bar, render_subagent_inline_card,
    render_thinking_card, render_tool_step_card, StickyCard,
};
pub use input_box::{draw_input, INPUT_MSG_IDX};
pub(crate) use modals::draw_models_modal;
pub use modals::{
    draw_api_key_modal, draw_armed_toast, draw_copy_toast, draw_help_modal, draw_history_modal,
    draw_permission_sheet, draw_sessions_modal, draw_solution_input_modal, relative_time,
};
pub use status::{draw_hint, draw_status_bar, draw_suggestions};
#[cfg(test)]
use table::{build_table_render, shrink_column_widths};
use text::wrap_text;
#[cfg(test)]
use text::WrappedLine;
#[cfg(test)]
use text::{block_selection_range, line_selection, prohibited_line_end, prohibited_line_start};
pub use theme::Theme;
use util::viewport_rect;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as RtBlock, Borders, Paragraph},
    Frame,
};

use crate::document::ChatMessage;
use crate::layout::LayoutMap;
use crate::selection::SelectionState;
#[cfg(test)]
use neenee_core::PermissionRequest;
use neenee_core::{AgentMode, Goal, GoalStatus};
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use unicode_width::UnicodeWidthStr;

/// Uniform horizontal inset applied to every chat-area component so no band,
/// bar, or text touches the terminal frame. Both gutters show `app_bg` via the
/// global frame fill. Cards consume this via [`chat_band_rect`]; user panels
/// and code blocks render their own equivalent gutters; markdown text wraps
/// with `CHAT_H_INSET` cells of slack on the right.
pub(super) const CHAT_H_INSET: u16 = 2;

/// Inner rect of a chat-area region after reserving the uniform
/// [`CHAT_H_INSET`] left+right `app_bg` gutters. Use this as the render target
/// for any solid-background band (card headers/bodies, child tool steps) so
/// the band sits inside the gutters rather than spanning edge to edge. The
/// surrounding cells keep `app_bg` from the global frame fill.
pub(super) fn chat_band_rect(area: Rect) -> Rect {
    Rect::new(
        area.x + CHAT_H_INSET,
        area.y,
        area.width.saturating_sub(2 * CHAT_H_INSET).max(1),
        area.height,
    )
}

pub struct ChatView<'a> {
    pub messages: &'a [ChatMessage],
    pub scroll: u16,
    pub selection: &'a SelectionState,
    pub current_provider: &'a str,
    pub current_model: &'a str,
    pub current_mode: AgentMode,
    pub current_goal: Option<&'a Goal>,
    /// Transient running status shown in a thin bar above the input box.
    /// Empty / "idle" / "responding" means the status bar is hidden.
    pub activity: &'a str,
    /// Spinner animation phase (cycles through braille frames while active).
    pub spinner_phase: usize,
    /// The current input-box text (masked while the API-key modal is open). The
    /// chat layout reads this so the input box can grow to fit its wrapped text.
    pub input: &'a str,
    /// When true, the header and input box are hidden (overlay modal open).
    pub chrome_hidden: bool,
    /// When set, the view is zoomed into a sub-agent task: a navigation bar is
    /// rendered and `messages` is the focused task's child stream.
    pub subagent_bar: Option<SubagentBarInfo>,
    pub theme: &'a Theme,
}

/// Info for the sub-agent navigation bar (shown when zoomed into a task).
pub struct SubagentBarInfo {
    /// Label for the focused sub-agent (its task description).
    pub label: String,
    /// 1-based index of the focused sub-agent among its siblings.
    pub index: usize,
    /// Total number of sibling sub-agent tasks.
    pub total: usize,
}

/// Layout information returned by [`draw_chat`].
pub struct ChatRender {
    /// The input box area (unchanged from before).
    pub input_rect: Rect,
    /// Total height (in lines) of the rendered message stream, ignoring the
    /// viewport clip. Used by the app loop to pin the view to the bottom.
    pub content_lines: usize,
    /// Height of the chat viewport.
    pub view_height: u16,
    /// The expanded card whose body is currently scrolled into view, so the app
    /// can render/click a sticky header pinned under the HUD bar. `None` when no
    /// expanded card body covers the top of the viewport.
    pub sticky: Option<StickyInfo>,
}

/// A sticky pinned card header (returned to the app for click handling).
pub struct StickyInfo {
    pub message_idx: usize,
    pub header: String,
    pub color: Color,
    pub block_idx: usize,
    pub rect: Rect,
    /// The content-line index of the real header inside the stream. The app
    /// uses this to re-anchor the scroll offset when the user collapses the
    /// pinned card, so the real header takes the sticky's place at the top of
    /// the viewport instead of jumping to unrelated content.
    pub header_line: usize,
}

/// Draw the main chat area, recording layout info.
pub fn draw_chat(frame: &mut Frame, layout_map: &mut LayoutMap, view: ChatView<'_>) -> ChatRender {
    let ChatView {
        messages,
        scroll,
        selection,
        current_provider: _,
        current_model,
        current_mode: _,
        current_goal,
        activity,
        spinner_phase,
        input,
        chrome_hidden,
        subagent_bar,
        theme,
    } = view;
    let full = frame.size();
    // Components render inside the vertical viewport margins (1 cell top and
    // bottom); only the background fill uses the full terminal rect.
    let size = viewport_rect(frame);

    // Paint the entire frame with the app background so the TUI owns every
    // pixel rather than leaving gaps at the terminal emulator's default color.
    frame.render_widget(
        RtBlock::default().style(Style::default().bg(theme.app_bg)),
        full,
    );

    let checklist = current_goal.and_then(goal_checklist_summary);
    // +1 for the thin separator rule drawn beneath the header content.
    // Hidden entirely when an overlay modal is open (chrome_hidden).
    let header_height: u16 = if chrome_hidden {
        0
    } else if checklist.is_some() {
        3
    } else {
        2
    };
    // The status bar (animated spinner + activity text) sits on its own line
    // directly above the input box. It is shown only for non-streaming,
    // non-idle activity so the chat reclaims that row when nothing is running.
    let status_active =
        !chrome_hidden && !activity.is_empty() && activity != "idle" && activity != "responding";
    let status_height: u16 = if status_active { 1 } else { 0 };

    // The input box grows with its content: the typed text wraps onto new
    // lines and the box expands to fit, up to roughly half the terminal so the
    // chat history always stays visible. The inner text width reserves the
    // thick left bar and a leading padding space.
    let input_text_width = (size.width as usize).saturating_sub(6).max(1);
    let input_wrapped_lines = wrap_text(input, input_text_width).len().max(1);
    let desired_input_height = input_wrapped_lines as u16 + 2; // top/bottom padding rows
    let max_input_height = (size.height / 2).max(3);
    let input_box_height = desired_input_height.min(max_input_height);
    let footer_height: u16 = if chrome_hidden {
        0
    } else {
        status_height + input_box_height + 1 // + hint line (bottom spacing comes from the viewport margin)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height), // Header and optional checklist dock
            Constraint::Min(0),                // Chat
            Constraint::Length(footer_height), // Status? + input box + hint line + bottom gap
        ])
        .split(size);

    // 1. Header — just the model name, plus optional goal.
    //    Skipped entirely when an overlay modal is open.
    if !chrome_hidden {
        let goal = current_goal.map(|goal| {
            let objective = goal.objective.chars().take(32).collect::<String>();
            let suffix = if goal.objective.chars().count() > 32 {
                "..."
            } else {
                ""
            };
            let mark = if goal.status == GoalStatus::Complete {
                "✓"
            } else {
                "◎"
            };
            let progress = checklist
                .as_ref()
                .map(|(done, total, _)| format!(" [{}/{}]", done, total))
                .unwrap_or_default();
            format!("{} {}{}{}", mark, objective, suffix, progress)
        });
        let mut header_spans = vec![
            Span::raw(" "),
            Span::styled(
                current_model.to_string(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(goal) = goal {
            header_spans.push(Span::raw("   "));
            header_spans.push(Span::styled(goal, Style::default().fg(theme.text_muted)));
        }
        let mut header_lines = vec![Line::from(header_spans)];
        if let Some((done, total, current)) = checklist {
            header_lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    format!("Tasks {}/{}  ", done, total),
                    Style::default()
                        .fg(theme.primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(current, Style::default().fg(theme.text_muted)),
            ]));
        }
        // Header content with a thin separator rule along the bottom edge.
        let header_block = RtBlock::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(theme.border_subtle));
        frame.render_widget(Paragraph::new(header_lines).block(header_block), chunks[0]);
    } // end !chrome_hidden

    // 2. Chat History
    // When zoomed into a sub-agent, reserve a 1-line navigation band at the
    // bottom of the chat viewport for the sub-agent bar.
    let (chat_area, subagent_bar_rect) = if subagent_bar.is_some() {
        let sub = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(chunks[1]);
        (sub[0], Some(sub[1]))
    } else {
        (chunks[1], None)
    };
    let mut current_y = chat_area.y;
    // Account for scroll
    let mut skip_rows = scroll as usize;
    // Total stream height, counted independently of the viewport clip so the
    // app loop can follow the bottom.
    let mut content_lines: usize = 0;
    // Expanded cards collected during the pass, for the sticky pinned header.
    let mut sticky_cards: Vec<StickyCard> = Vec::new();

    for (mi, msg) in messages.iter().enumerate() {
        // Render blocks
        if msg.is_subagent_task() {
            render_subagent_inline_card(
                frame,
                chat_area,
                msg,
                mi,
                theme,
                layout_map,
                &mut skip_rows,
                &mut current_y,
                &mut content_lines,
            );
        } else if msg.is_tool_step() {
            render_tool_step_card(
                frame,
                chat_area,
                msg,
                mi,
                selection,
                theme,
                layout_map,
                &mut skip_rows,
                &mut current_y,
                &mut content_lines,
                &mut sticky_cards,
                spinner_phase,
            );
        } else if msg.is_thinking() {
            render_thinking_card(
                frame,
                chat_area,
                msg,
                mi,
                selection,
                theme,
                layout_map,
                &mut skip_rows,
                &mut current_y,
                &mut content_lines,
                &mut sticky_cards,
                spinner_phase,
            );
        } else {
            render_message_blocks(
                frame,
                chat_area,
                msg,
                mi,
                selection,
                theme,
                layout_map,
                &mut skip_rows,
                &mut current_y,
                &mut content_lines,
                true,
            );
        }

        // Spacing between messages. A user message's panel already ends with a
        // bottom transition row (▀) that separates it from the next message, so
        // the extra blank line is omitted there to keep the gap to a single row
        // (otherwise the sent message sits two rows above the following body).
        // The exception is when the next message is a card (thinking or tool
        // step): cards have their own solid background band, and a blank row
        // between the user panel's transition and the card header keeps the two
        // visually distinct. This matches the spacing produced by live reasoning
        // streams and restored history.
        let next_is_card = messages.get(mi + 1).is_some_and(|next| {
            next.is_thinking() || next.is_tool_step() || next.is_subagent_task()
        });
        if msg.role != neenee_core::Role::User || next_is_card {
            content_lines += 1;
            if skip_rows > 0 {
                skip_rows = skip_rows.saturating_sub(1);
            } else if current_y < chat_area.y + chat_area.height {
                current_y += 1;
            }
        }
    }

    // Sub-agent navigation band, drawn across the full chat width (inside the
    // app_bg gutters) so it reads as a continuous bar pinned above the input.
    if let (Some(bar), Some(rect)) = (subagent_bar.as_ref(), subagent_bar_rect) {
        draw_subagent_bar(frame, rect, bar, theme);
    }

    // The transient running status lives directly above the input box (a thin
    // animated bar). Hidden while text is actively streaming ("responding"),
    // since the streamed response is itself the feedback in that phase, and
    // hidden when idle.
    let input_rect = if status_active {
        let status_rect = Rect::new(
            chunks[2].x + 2,
            chunks[2].y,
            chunks[2].width.saturating_sub(4),
            1,
        );
        draw_status_bar(frame, status_rect, activity, spinner_phase, theme);
        Rect::new(
            chunks[2].x + 2,
            chunks[2].y + 1,
            chunks[2].width.saturating_sub(4),
            input_box_height,
        )
    } else {
        Rect::new(
            chunks[2].x + 2,
            chunks[2].y,
            chunks[2].width.saturating_sub(4),
            input_box_height,
        )
    };

    // Sticky pinned header: if an expanded card's body covers the top of the
    // viewport (its header is scrolled out of view), pin its header to the line
    // directly under the HUD bar so the user can always collapse it.
    let sticky_info = draw_sticky_header_if_needed(frame, chat_area, &sticky_cards, scroll, theme);

    ChatRender {
        input_rect,
        content_lines,
        view_height: chat_area.height,
        sticky: sticky_info,
    }
}

fn goal_checklist_summary(goal: &Goal) -> Option<(usize, usize, String)> {
    if goal.checklist.is_empty() {
        return None;
    }
    let done = goal
        .checklist
        .iter()
        .filter(|item| {
            matches!(
                item.status,
                neenee_core::GoalChecklistStatus::Completed
                    | neenee_core::GoalChecklistStatus::Cancelled
            )
        })
        .count();
    let current = goal
        .checklist
        .iter()
        .find(|item| item.status == neenee_core::GoalChecklistStatus::InProgress)
        .or_else(|| {
            goal.checklist
                .iter()
                .find(|item| item.status == neenee_core::GoalChecklistStatus::Pending)
        })
        .or_else(|| goal.checklist.last())
        .map(|item| item.content.clone())
        .unwrap_or_default();
    Some((done, goal.checklist.len(), current))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke-render every redesigned component into a buffer to catch panics
    /// (border math, rect underflows, empty content) without a live terminal.
    #[test]
    fn redesigned_components_render_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = Theme::default();
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                let mut thinking = ChatMessage::thinking("Reasoning about the task step by step.");
                thinking.set_thinking_expanded(true);
                let mut tool = ChatMessage::tool_step("call_1", "list_dir", r#"{"path":"."}"#);
                tool.set_tool_step_expanded(true);
                tool.finish_tool_step("call_1", "file_a\nfile_b", 12);
                let messages = vec![
                    ChatMessage::new(neenee_core::Role::User, "hi"),
                    ChatMessage::new(
                        neenee_core::Role::Assistant,
                        "Here is a table:\n\n| Tool | Count |\n| --- | ---: |\n| read | 1 |\n| webfetch | 250 |",
                    ),
                    thinking,
                    tool,
                ];
                let _ = draw_chat(
                    f,
                    &mut layout_map,
                    ChatView {
                        messages: &messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        current_provider: "mock",
                        current_model: "mock-model",
                        current_mode: AgentMode::Build,
                        current_goal: None,
                        activity: "waiting for model",
                        spinner_phase: 0,
                        input: "hello",
                        chrome_hidden: false,
                        subagent_bar: None,
                        theme: &theme,
                    },
                );
                draw_input(
                    f,
                    Rect::new(0, 21, 80, 3),
                    "hello",
                    5,
                    theme.accent,
                    &theme,
                    &mut LayoutMap::new(),
                    true,
                    &mut 0,
                );
                draw_hint(
                    f,
                    Rect::new(0, 24, 80, 1),
                    &[("ctrl+p", "commands"), ("ctrl+h", "help"), ("enter", "send")],
                    &theme,
                );
                draw_suggestions(
                    f,
                    &mut layout_map,
                    &[("/goal", "Set goal"), ("/clear", "Clear")],
                    Some(0),
                    Rect::new(0, 20, 80, 3),
                    &theme,
                );
                draw_copy_toast(f, "copied to clipboard", false, &theme);
                draw_armed_toast(f, "press Ctrl+C again to exit", &theme);
            })
            .unwrap();

        // Modals + permission sheet on a fresh frame.
        terminal
            .draw(|f| {
                draw_models_modal(
                    f,
                    &mut LayoutMap::new(),
                    &[],
                    "mock",
                    0,
                    &HashMap::new(),
                    &theme,
                );
                draw_history_modal(f, &mut LayoutMap::new(), &["a".to_string()], 0, &theme);
                draw_api_key_modal(f, "openai", "sk-•••", &theme);
                draw_solution_input_modal(f, " Endpoint", "url", "https://x", false, &theme);
                draw_help_modal(f, &theme);
                draw_sessions_modal(
                    f,
                    &[
                        neenee_core::SessionOverview {
                            id: "abc123".to_string(),
                            overview: "Refactor the renderer".to_string(),
                            created_at: 0,
                            updated_at: 0,
                            message_count: 12,
                            active: true,
                        },
                        neenee_core::SessionOverview {
                            id: "def456".to_string(),
                            overview: "Fix the tool_call_id bug".to_string(),
                            created_at: 0,
                            updated_at: 0,
                            message_count: 4,
                            active: false,
                        },
                    ],
                    0,
                    &theme,
                );
            })
            .unwrap();

        terminal
            .draw(|f| {
                let request = PermissionRequest {
                    id: "p1".to_string(),
                    tool: "bash".to_string(),
                    description: "run a command".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                    scope: "*".to_string(),
                };
                let _ = draw_permission_sheet(f, &request, 0, false, 0, &theme);
            })
            .unwrap();
    }

    /// Render both the compact sub-agent card (root view) and the zoomed-in
    /// sub-agent view with its navigation bar, ensuring no layout panics.
    #[test]
    fn subagent_card_and_view_render_without_panicking() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = Theme::default();
        let backend = TestBackend::new(80, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        // Root view: a completed sub-agent task renders as a compact card.
        let mut task = ChatMessage::tool_step(
            "task_1",
            "task",
            r#"{"description":"explore the codebase","prompt":"..."}"#,
        );
        task.push_subtask_event(&neenee_core::SubTaskEvent::ToolCall {
            id: "inner".into(),
            name: "grep".into(),
            arguments: r#"{"pattern":"foo"}"#.into(),
        });
        task.finish_tool_step("task_1", "found 3 matches", 1200);
        let root_messages = vec![
            ChatMessage::new(neenee_core::Role::User, "explore please"),
            task,
        ];

        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                let _ = draw_chat(
                    f,
                    &mut layout_map,
                    ChatView {
                        messages: &root_messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        current_provider: "mock",
                        current_model: "mock-model",
                        current_mode: AgentMode::Build,
                        current_goal: None,
                        activity: "running subagent",
                        spinner_phase: 0,
                        input: "",
                        chrome_hidden: false,
                        subagent_bar: None,
                        theme: &theme,
                    },
                );
            })
            .unwrap();

        // Zoomed-in sub-agent view: the task's children are the message stream
        // and the navigation bar is shown.
        let children = root_messages[1].subagent_children().unwrap().to_vec();
        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                let _ = draw_chat(
                    f,
                    &mut layout_map,
                    ChatView {
                        messages: &children,
                        scroll: 0,
                        selection: &SelectionState::None,
                        current_provider: "mock",
                        current_model: "mock-model",
                        current_mode: AgentMode::Build,
                        current_goal: None,
                        activity: "",
                        spinner_phase: 0,
                        input: "",
                        chrome_hidden: false,
                        subagent_bar: Some(SubagentBarInfo {
                            label: "explore the codebase".to_string(),
                            index: 1,
                            total: 1,
                        }),
                        theme: &theme,
                    },
                );
            })
            .unwrap();
    }

    #[test]
    fn line_selection_intersects_wrapped_lines() {
        use crate::layout::SemanticCursor;
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 2),
            head: SemanticCursor::new(0, 0, 8),
        };
        let range = block_selection_range(&sel, 0, 0);

        // Line covering bytes 0..5 ("hello"): selected from 2 to end.
        let first = WrappedLine {
            text: "hello".to_string(),
            start_byte: 0,
            end_byte: 5,
        };
        assert_eq!(line_selection(range, &first), Some((2, 5)));

        // Line covering bytes 5..10 ("world"): selected up to head char (8 → rel 3, inclusive → 4).
        let second = WrappedLine {
            text: "world".to_string(),
            start_byte: 5,
            end_byte: 10,
        };
        assert_eq!(line_selection(range, &second), Some((0, 4)));

        // A line after the selection has no overlap.
        let third = WrappedLine {
            text: "after".to_string(),
            start_byte: 10,
            end_byte: 15,
        };
        assert_eq!(line_selection(range, &third), None);
    }

    #[test]
    fn block_selection_covers_middle_blocks_fully() {
        use crate::layout::SemanticCursor;
        let sel = SelectionState::Range {
            anchor: SemanticCursor::new(0, 0, 3),
            head: SemanticCursor::new(0, 2, 1),
        };
        assert_eq!(block_selection_range(&sel, 0, 0), Some((3, None)));
        assert_eq!(block_selection_range(&sel, 0, 1), Some((0, None)));
        assert_eq!(block_selection_range(&sel, 0, 2), Some((0, Some(1))));
        assert_eq!(block_selection_range(&sel, 0, 3), None);
        assert_eq!(block_selection_range(&sel, 1, 0), None);
    }

    #[test]
    fn test_wrap_text() {
        let lines = wrap_text("hello world", 5);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "hello");
        assert_eq!(lines[1].text, " worl");
        assert_eq!(lines[2].text, "d");
    }

    #[test]
    fn test_wrap_with_newlines() {
        let lines = wrap_text("hi\nthere", 10);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "hi");
        assert_eq!(lines[1].text, "there");
    }

    #[test]
    fn wrap_avoids_cjk_punctuation_at_line_start() {
        let lines = wrap_text("人生需要坚持，才能前进。", 12);
        assert!(lines.len() > 1);
        assert!(lines.iter().skip(1).all(|line| line
            .text
            .chars()
            .next()
            .is_none_or(|ch| !prohibited_line_start(ch))));
        assert!(lines.iter().all(|line| line
            .text
            .chars()
            .last()
            .is_none_or(|ch| !prohibited_line_end(ch))));
    }

    #[test]
    fn checklist_summary_prefers_current_work() {
        let goal = Goal {
            objective: "ship".to_string(),
            status: GoalStatus::Active,
            tokens_used: 0,
            token_budget: None,
            time_used_seconds: 0,
            checklist: vec![
                neenee_core::GoalChecklistItem {
                    content: "implemented".to_string(),
                    status: neenee_core::GoalChecklistStatus::Completed,
                },
                neenee_core::GoalChecklistItem {
                    content: "run tests".to_string(),
                    status: neenee_core::GoalChecklistStatus::InProgress,
                },
            ],
        };

        assert_eq!(
            goal_checklist_summary(&goal),
            Some((1, 2, "run tests".to_string()))
        );
    }

    /// The input box must reserve only a single content row for a short input
    /// but grow to fit wrapped text when the input is long.
    #[test]
    fn input_box_grows_with_wrapped_content() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = Theme::default();
        let messages: Vec<ChatMessage> = Vec::new();

        fn render_with(theme: &Theme, messages: &[ChatMessage], input: &str) -> Rect {
            let backend = TestBackend::new(40, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut rect = Rect::default();
            terminal
                .draw(|f| {
                    let mut layout_map = LayoutMap::new();
                    let r = draw_chat(
                        f,
                        &mut layout_map,
                        ChatView {
                            messages,
                            scroll: 0,
                            selection: &SelectionState::None,
                            current_provider: "mock",
                            current_model: "m",
                            current_mode: AgentMode::Build,
                            current_goal: None,
                            activity: "",
                            spinner_phase: 0,
                            input,
                            chrome_hidden: false,
                            subagent_bar: None,
                            theme,
                        },
                    );
                    rect = r.input_rect;
                })
                .unwrap();
            rect
        }

        // Short input: one content line + two padding rows = 3.
        let short = render_with(&theme, &messages, "hi");
        assert_eq!(short.height, 3);

        // Long input wraps across many lines on a 40-wide terminal; the box
        // must grow beyond the single-line baseline.
        let long_input = "word ".repeat(40);
        let tall = render_with(&theme, &messages, &long_input);
        assert!(
            tall.height > 3,
            "wrapped input should grow the box, got height {}",
            tall.height
        );
        // ...but never more than half the terminal.
        assert!(tall.height <= 12);
    }

    /// `draw_input` must not panic for tricky inputs and should place the caret
    /// on the second wrapped line when the cursor sits past the first wrap.
    #[test]
    fn draw_input_wraps_and_positions_caret() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = Theme::default();
        let backend = TestBackend::new(20, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        // "aaaa bbbb cccc" wraps within the ~17-wide inner area; cursor at the
        // very end should be on a later line, not off the box.
        let input = "aaaa bbbb cccc dddd eeee";
        terminal
            .draw(|f| {
                draw_input(
                    f,
                    Rect::new(0, 0, 20, 8),
                    input,
                    input.width() as u16,
                    theme.accent,
                    &theme,
                    &mut LayoutMap::new(),
                    true,
                    &mut 0,
                );
            })
            .unwrap();
    }

    /// Wide tables (including CJK content) must keep borders intact and never
    /// overflow the viewport: columns shrink to fit, cell text wraps, and
    /// every rendered line stays within the available width.
    #[test]
    fn wide_table_shrinks_columns_and_keeps_borders_intact() {
        use crate::document::TableAlignment;

        let headers = vec![
            "工具".to_string(),
            "类型".to_string(),
            "底层实现".to_string(),
            "关键特性".to_string(),
        ];
        let rows = vec![
            vec![
                "bash".to_string(),
                "Write".to_string(),
                "std::process::Command（sh -c / cmd /C）".to_string(),
                "执行 shell 命令，支持 timeout，输出截断".to_string(),
            ],
            vec![
                "read_file".to_string(),
                "ReadOnly".to_string(),
                "std::fs::read_to_string".to_string(),
                "支持 offset/limit".to_string(),
            ],
        ];
        let aligns = vec![
            TableAlignment::None,
            TableAlignment::None,
            TableAlignment::None,
            TableAlignment::None,
        ];

        // ── Narrow terminal (34 cols): table is far wider, must shrink ──
        let lines = build_table_render(&headers, &rows, &aligns, 34).lines;
        assert!(!lines.is_empty(), "table must produce output");

        for (i, line) in lines.iter().enumerate() {
            assert!(
                line.width() <= 34,
                "line {i} overflows: {} cols: {}",
                line.width(),
                line
            );
        }
        assert!(lines.first().unwrap().starts_with('┌'));
        assert!(lines.last().unwrap().starts_with('└'));
        assert!(
            lines.iter().any(|l| l.starts_with('├')),
            "missing header/body separator"
        );
        // Two body rows → one separator between them (plus one after header).
        let sep_count = lines.iter().filter(|l| l.starts_with('├')).count();
        assert_eq!(
            sep_count, 2,
            "expected 2 separators (header→body + row→row), got {sep_count}"
        );
        let pipe_counts: Vec<usize> = lines
            .iter()
            .filter(|l| l.starts_with('│'))
            .map(|l| l.matches('│').count())
            .collect();
        assert!(!pipe_counts.is_empty(), "must have data lines");
        assert!(
            pipe_counts.iter().all(|&c| c == pipe_counts[0]),
            "all data lines must have the same number of column separators"
        );

        // ── Wide terminal (80 cols): table fits without shrinking ──
        let wide_lines = build_table_render(&headers, &rows, &aligns, 76).lines;
        for (i, line) in wide_lines.iter().enumerate() {
            assert!(
                line.width() <= 76,
                "wide line {i} overflows: {} cols",
                line.width()
            );
        }
        // When it fits, the table should be shorter (no wrapping needed).
        assert!(
            wide_lines.len() <= lines.len(),
            "wide table should have fewer lines than shrunk table"
        );
    }

    #[test]
    fn shrink_columns_preserves_minimum_and_proportions() {
        // Intrinsic [10, 5, 20], target 24, min 3.
        // total_min = 9, shrinkable = 26, available = 15.
        // col0: 3 + 7*15/26 = 3 + 4 = 7
        // col1: 3 + 2*15/26 = 3 + 1 = 4
        // col2: 3 + 17*15/26 = 3 + 9 = 12
        let result = shrink_column_widths(&[10, 5, 20], 24, 3);
        assert_eq!(result.len(), 3);
        assert!(result.iter().all(|&w| w >= 3), "must respect minimum");
        assert!(
            result.iter().sum::<usize>() <= 24,
            "must fit within target, got {}",
            result.iter().sum::<usize>()
        );
        // Largest intrinsic column stays largest after shrinking.
        let max_val = *result.iter().max().unwrap();
        let max_idx = result.iter().position(|&v| v == max_val).unwrap();
        assert_eq!(max_idx, 2);
    }

    #[test]
    fn shrink_columns_with_tiny_target_returns_all_minimum() {
        let result = shrink_column_widths(&[10, 20, 30], 5, 3);
        assert_eq!(result, vec![3, 3, 3]);
    }
}
