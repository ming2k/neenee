//! Rendering engine: draws the transcript (and footer chrome) using ratatui
//! while recording semantic-to-screen layout information.

mod chrome;
mod composer;
mod design;
mod markdown_table;
mod message_body;
mod overlays;
mod primitives;
mod text_layout;
mod theme;
/// Per-tool presentation registry: each tool's icon, collapsed summary,
/// optional preview, and expanded-body classification. `document.rs` and
/// `turn_artifacts.rs` dispatch through its `*_for` entry points instead of
/// matching on tool names (see tools/mod.rs).
pub(crate) mod tools;
mod turn_artifacts;

#[cfg(test)]
mod snapshot_tests;

pub use chrome::{draw_hint_bar, HintBarLayout, HintBarView};
pub use chrome::{draw_completion_menu, draw_status_bar};
pub use composer::{draw_composer, INPUT_MSG_IDX};
use design::{
    CARD_MIN_WIDTH, COMPOSER_MAX_HEIGHT_DIVISOR, COMPOSER_MIN_HEIGHT, COMPOSER_PROMPT_PREFIX_COLS,
    COMPOSER_RIGHT_PAD_COLS, COMPOSER_VERTICAL_CHROME_ROWS, FOOTER_H_INSET, HINT_BAR_ROWS,
    MESSAGE_GAP_ROWS, REASONING_TRACE_BLOCK_GAP_ROWS, REASONING_TRACE_BODY_BOTTOM_GAP_ROWS,
    REASONING_TRACE_BODY_TOP_GAP_ROWS, STATUS_BAR_ROWS, SUBAGENT_BAR_ROWS,
    TOOL_CARD_BODY_BOTTOM_GAP_ROWS, TOOL_CARD_BODY_TOP_GAP_ROWS, TOOL_CARD_CHILDREN_GAP_ROWS,
    TOOL_CARD_SECTION_GAP_ROWS, TRANSCRIPT_BODY_PREFIX_COLS, TRANSCRIPT_BODY_RIGHT_INSET,
    TRANSCRIPT_H_INSET,
};
#[cfg(test)]
use markdown_table::{build_table_render, shrink_column_widths};
use message_body::draw_message_body;
pub(crate) use overlays::draw_models_modal;
pub use overlays::{
    draw_api_key_modal, draw_armed_toast, draw_copy_toast, draw_help_modal, draw_history_modal,
    draw_permission_sheet, draw_sessions_modal, draw_solution_input_modal,
    draw_tool_step_detail_overlay, relative_time,
};
use primitives::viewport_rect;
#[cfg(test)]
use text_layout::WrappedLine;
#[cfg(test)]
use text_layout::{
    block_selection_range, line_selection, prohibited_line_end, prohibited_line_start,
};
pub use theme::Theme;
use turn_artifacts::{
    draw_reasoning_trace, draw_sticky_header_if_needed, draw_subagent_bar,
    draw_subagent_inline_card, draw_tool_step_card, StickyCard,
};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block as RtBlock, Paragraph},
    Frame,
};

use crate::document::TranscriptMessage;
use crate::layout::{InteractiveTarget, LayoutMap, THINKING_BLOCK_IDX, TOOL_STEP_BLOCK_IDX};
use crate::selection::SelectionState;
#[cfg(test)]
use neenee_core::PermissionRequest;
#[cfg(test)]
use std::collections::HashMap;

/// Inner rect of a transcript-area region after reserving the uniform
/// [`TRANSCRIPT_H_INSET`] left+right `app_bg` gutters. Use this as the render target
/// for any solid-background band (card headers/bodies, child tool steps) so
/// the band sits inside the gutters rather than spanning edge to edge. The
/// surrounding cells keep `app_bg` from the global frame fill.
pub(super) fn transcript_band_rect(area: Rect) -> Rect {
    Rect::new(
        area.x + TRANSCRIPT_H_INSET,
        area.y,
        area.width.saturating_sub(2 * TRANSCRIPT_H_INSET).max(1),
        area.height,
    )
}

pub struct TranscriptView<'a> {
    pub messages: &'a [TranscriptMessage],
    pub scroll: u16,
    pub selection: &'a SelectionState,
    /// Transient running status shown in a thin bar above the input box.
    /// Empty / "idle" / "responding" means the status bar is hidden.
    pub activity: &'a str,
    /// Spinner animation phase (cycles through braille frames while active).
    pub spinner_phase: usize,
    /// The current input-box text (masked while the API-key modal is open). The
    /// transcript layout reads this so the input box can grow to fit its wrapped text.
    pub input: &'a str,
    /// Byte offset of the caret inside `input` (see [`App::byte_cursor`]). The
    /// box grows one extra row when the caret rests past the last wrapped line
    /// (e.g. just after an inserted newline), so its height matches what
    /// [`composer::draw_composer`] actually renders.
    pub byte_cursor: usize,
    /// When true, the hint bar and input box are hidden (overlay modal open).
    pub chrome_hidden: bool,
    /// When set, the view is zoomed into a sub-agent task: a navigation bar is
    /// rendered and `messages` is the focused task's child stream.
    pub subagent_bar: Option<SubagentBarInfo>,
    /// Message index of the reasoning trace whose header is currently hovered,
    /// so its header renders brightened (dark→bright) as a click affordance.
    /// `None` when nothing is hovered.
    pub hovered_reasoning: Option<usize>,
    /// Keyboard-focused activatable target.
    pub focused_target: Option<InteractiveTarget>,
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

/// Layout information returned by [`draw_transcript`].
pub struct TranscriptRender {
    /// The input box area.
    pub input_rect: Rect,
    /// The hint-bar area pinned below the input box (zero-sized when hidden).
    pub hint_rect: Rect,
    /// Total height (in lines) of the rendered message stream, ignoring the
    /// viewport clip. Used by the app loop to pin the view to the bottom.
    pub content_lines: usize,
    /// Height of the transcript viewport.
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

/// Draw the main transcript area, recording layout info.
pub fn draw_transcript(
    frame: &mut Frame,
    layout_map: &mut LayoutMap,
    view: TranscriptView<'_>,
) -> TranscriptRender {
    let TranscriptView {
        messages,
        scroll,
        selection,
        activity,
        spinner_phase,
        input,
        byte_cursor,
        chrome_hidden,
        subagent_bar,
        hovered_reasoning,
        focused_target,
        theme,
    } = view;
    let full = frame.size();
    // Components render inside the vertical viewport margins (1 cell top and
    // bottom); only the background fill uses the full terminal rect.
    let viewport = viewport_rect(frame);

    // Paint the entire frame with the app background so the TUI owns every
    // pixel rather than leaving gaps at the terminal emulator's default color.
    frame.render_widget(
        RtBlock::default().style(Style::default().bg(theme.app_bg)),
        full,
    );

    let size = viewport;

    // The status bar (animated spinner + activity text) sits on its own line
    // directly above the input box. It is shown only for non-streaming,
    // non-idle activity so the transcript reclaims that row when nothing is running.
    let status_active =
        !chrome_hidden && !activity.is_empty() && activity != "idle" && activity != "responding";
    let status_height: u16 = if status_active { STATUS_BAR_ROWS } else { 0 };

    // The input box grows with its content: the typed text wraps onto new
    // lines and the box expands to fit, up to roughly half the terminal so the
    // transcript history always stays visible. The inner text width reserves the
    // footer insets, the `> ` prompt prefix, and the matching right pad so the
    // height calculation wraps at the same width the composer renders.
    let input_text_width = (size.width as usize)
        .saturating_sub(
            (2 * FOOTER_H_INSET) as usize + COMPOSER_PROMPT_PREFIX_COLS + COMPOSER_RIGHT_PAD_COLS,
        )
        .max(1);
    let input_wrapped_lines = composer::input_row_count(input, input_text_width, byte_cursor);
    let desired_input_height = input_wrapped_lines as u16 + COMPOSER_VERTICAL_CHROME_ROWS;
    let max_input_height = (size.height / COMPOSER_MAX_HEIGHT_DIVISOR).max(COMPOSER_MIN_HEIGHT);
    let input_box_height = desired_input_height.min(max_input_height);
    // The hint bar is a single-line status strip pinned directly below the
    // input box. It carries the workspace / model / goal / MCP / context info
    // that the old top header showed. Hidden alongside the rest of the chrome
    // while an overlay modal is open.
    let hint_height: u16 = if chrome_hidden { 0 } else { HINT_BAR_ROWS };
    let footer_height: u16 = if chrome_hidden {
        0
    } else {
        status_height + input_box_height + hint_height
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),                // Transcript (header was here, now removed)
            Constraint::Length(footer_height), // Status? + input box + hint bar
        ])
        .split(size);

    // 1. Transcript History
    // When zoomed into a sub-agent, reserve a 1-line navigation band at the
    // bottom of the transcript viewport for the sub-agent bar.
    let (transcript_area, subagent_bar_rect) = if subagent_bar.is_some() {
        let sub = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(SUBAGENT_BAR_ROWS)])
            .split(chunks[0]);
        (sub[0], Some(sub[1]))
    } else {
        (chunks[0], None)
    };
    let mut current_y = transcript_area.y;
    // Account for scroll
    let mut skip_rows = scroll as usize;
    // Total stream height, counted independently of the viewport clip so the
    // app loop can follow the bottom.
    let mut content_lines: usize = 0;
    // Expanded cards collected during the pass, for the sticky pinned header.
    let mut sticky_cards: Vec<StickyCard> = Vec::new();
    // The last model attribution badge drawn into the stream. A badge is shown
    // once at the start of an assistant turn and again only when the producing
    // model changes, so a session that mixes providers stays traceable without
    // repeating the label on every message of a single-model run.
    let mut last_shown_attribution: Option<(String, String)> = None;

    for (mi, msg) in messages.iter().enumerate() {
        // Model attribution badge: shown above the first assistant-side
        // message of a turn (reasoning, text, or tool step) and whenever the
        // producing provider/model changes. Tool results and tool cards share
        // the turn's model, so a single badge per model-run keeps the
        // transcript clean while remaining fully traceable.
        let is_assistant_side =
            msg.role == neenee_core::Role::Assistant || msg.is_thinking() || msg.is_tool_step();
        if is_assistant_side {
            if let Some(attribution) = msg.attribution_label() {
                if last_shown_attribution.as_ref() != Some(&attribution) {
                    draw_attribution_badge(
                        frame,
                        transcript_area,
                        &attribution,
                        &mut skip_rows,
                        &mut current_y,
                        &mut content_lines,
                        theme,
                    );
                    last_shown_attribution = Some(attribution);
                }
            }
        }

        // Render blocks
        if msg.is_subagent_task() {
            draw_subagent_inline_card(
                frame,
                transcript_area,
                msg,
                mi,
                theme,
                layout_map,
                &mut skip_rows,
                &mut current_y,
                &mut content_lines,
                focused_target.is_some_and(|target| {
                    target.message_idx == mi && target.block_idx == TOOL_STEP_BLOCK_IDX
                }),
            );
        } else if msg.is_tool_step() {
            draw_tool_step_card(
                frame,
                transcript_area,
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
                focused_target.is_some_and(|target| {
                    target.message_idx == mi && target.block_idx == TOOL_STEP_BLOCK_IDX
                }),
            );
        } else if msg.is_thinking() {
            draw_reasoning_trace(
                frame,
                transcript_area,
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
                hovered_reasoning == Some(mi)
                    || focused_target.is_some_and(|target| {
                        target.message_idx == mi && target.block_idx == THINKING_BLOCK_IDX
                    }),
            );
        } else {
            draw_message_body(
                frame,
                transcript_area,
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
            content_lines += MESSAGE_GAP_ROWS;
            if skip_rows > 0 {
                skip_rows = skip_rows.saturating_sub(1);
            } else if current_y < transcript_area.y + transcript_area.height {
                current_y += MESSAGE_GAP_ROWS as u16;
            }
        }
    }

    // Sub-agent navigation band, drawn across the full transcript width (inside the
    // app_bg gutters) so it reads as a continuous bar pinned above the input.
    if let (Some(bar), Some(rect)) = (subagent_bar.as_ref(), subagent_bar_rect) {
        draw_subagent_bar(frame, rect, bar, theme);
    }

    // The footer stacks, from top to bottom: the transient status bar (when
    // active), the input box, and the persistent hint bar. The status bar
    // anchors the top of the footer when present; the input box always sits
    // directly beneath it; the hint bar pins the bottom of the footer.
    let footer_x = chunks[1].x + FOOTER_H_INSET;
    let footer_w = chunks[1].width.saturating_sub(2 * FOOTER_H_INSET);

    // The transient running status lives directly above the input box. Hidden
    // while text is actively streaming ("responding"), since the streamed
    // response is itself the feedback in that phase, and hidden when idle.
    let status_y = chunks[1].y;
    if status_active {
        draw_status_bar(
            frame,
            Rect::new(footer_x, status_y, footer_w, STATUS_BAR_ROWS),
            activity,
            spinner_phase,
            theme,
        );
    }

    // The input box sits directly below the status bar (when active), or at
    // the top of the footer otherwise.
    let input_rect = Rect::new(
        footer_x,
        status_y + status_height,
        footer_w,
        input_box_height,
    );

    // The hint bar sits directly below the input box and carries the workspace
    // / model / goal / MCP / context info that the old top header showed.
    // Rendered last so its click-target rect is computed even though its draw
    // call is delegated to the app loop (which owns the masked input state).
    let hint_rect = if hint_height > 0 {
        Rect::new(
            footer_x,
            status_y + status_height + input_box_height,
            footer_w,
            hint_height,
        )
    } else {
        Rect::new(0, 0, 0, 0)
    };

    // Sticky pinned header: if an expanded card's body covers the top of the
    // viewport (its header is scrolled out of view), pin its header to the line
    // directly under the HUD bar so the user can always collapse it.
    let sticky_info = draw_sticky_header_if_needed(
        frame,
        transcript_area,
        &sticky_cards,
        scroll,
        hovered_reasoning,
        focused_target,
        theme,
    );

    TranscriptRender {
        input_rect,
        hint_rect,
        content_lines,
        view_height: transcript_area.height,
        sticky: sticky_info,
    }
}

/// Draw a single-line model attribution badge above an assistant turn.
///
/// The badge labels which provider/model produced the following response, so a
/// session that mixes models stays traceable. It occupies one content line
/// (scrollable like any other). It reads as a turn heading rather than body
/// content, so it sits at the transcript horizontal inset (flush with the gutter)
/// instead of the body's leading indent, and is followed by a blank row that
/// separates it from the turn's content. Rendered in muted text so it reads as
/// metadata. The provider half is dropped when empty (e.g. providers without an
/// id).
fn draw_attribution_badge(
    frame: &mut Frame,
    area: Rect,
    attribution: &(String, String),
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    theme: &Theme,
) {
    let (provider, model) = attribution;
    // `provider · model`, dropping the provider half (and separator) when the
    // provider id is empty so untagged/legacy providers show just the model.
    let label = if provider.is_empty() {
        model.clone()
    } else {
        format!("{} · {}", provider, model)
    };

    // Badge line. Counts toward scroll height even when scrolled/clipped out;
    // only drawn and advanced when the row falls inside the viewport.
    *content_lines += 1;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(1);
    } else if *current_y < area.y + area.height {
        let prefix = " ".repeat(TRANSCRIPT_H_INSET as usize);
        let line = Line::from(vec![
            Span::styled(prefix, Style::default()),
            Span::styled("◆ ", Style::default().fg(theme.dim_fg)),
            Span::styled(label, Style::default().fg(theme.text_muted)),
        ]);
        let rect = Rect::new(area.x, *current_y, area.width, 1);
        frame.render_widget(Paragraph::new(line), rect);
        *current_y += 1;
    }

    // Blank row separating the heading from the turn's content.
    *content_lines += MESSAGE_GAP_ROWS;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(MESSAGE_GAP_ROWS);
    } else if *current_y < area.y + area.height {
        *current_y += MESSAGE_GAP_ROWS as u16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::text_layout::wrap_text;
    use unicode_width::UnicodeWidthStr;

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
                let mut thinking = TranscriptMessage::thinking("Reasoning about the task step by step.");
                thinking.set_thinking_expanded(true);
                let mut tool = TranscriptMessage::tool_step("call_1", "list_dir", r#"{"path":"."}"#);
                tool.set_tool_step_expanded(true);
                tool.finish_tool_step("call_1", "file_a\nfile_b", neenee_core::ToolOutput::text("file_a\nfile_b"), 12);
                let messages = vec![
                    TranscriptMessage::new(neenee_core::Role::User, "hi"),
                    TranscriptMessage::new(
                        neenee_core::Role::Assistant,
                        "Here is a table:\n\n| Tool | Count |\n| --- | ---: |\n| read | 1 |\n| webfetch | 250 |",
                    ),
                    thinking,
                    tool,
                ];
                let _ = draw_transcript(
                    f,
                    &mut layout_map,
                    TranscriptView {
                        messages: &messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        activity: "waiting for model",
                        spinner_phase: 0,
                        input: "hello",
                        byte_cursor: 5,
                        chrome_hidden: false,
                        subagent_bar: None,
                        hovered_reasoning: None,
                        focused_target: None,
                        theme: &theme,
                    },
                );
                draw_composer(
                    f,
                    Rect::new(0, 21, 80, 3),
                    "hello",
                    5,
                    true,
                    &theme,
                    &mut LayoutMap::new(),
                    true,
                    &mut 0,
                );
                draw_completion_menu(
                    f,
                    &mut layout_map,
                    &[
                        crate::Completion {
                            label: "/goal".to_string(),
                            description: "Set goal".to_string(),
                            replace_start: 0,
                            replace_end: 0,
                        },
                        crate::Completion {
                            label: "/clear".to_string(),
                            description: "Clear".to_string(),
                            replace_start: 0,
                            replace_end: 0,
                        },
                    ],
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
                let history_roster: Vec<String> = vec!["a".to_string()];
                let ranked: Vec<(usize, crate::fuzzy::FuzzyMatch)> =
                    crate::fuzzy::rank(&history_roster, "");
                draw_history_modal(
                    f,
                    &mut LayoutMap::new(),
                    &history_roster,
                    "",
                    &ranked,
                    0,
                    &theme,
                );
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
        let mut task = TranscriptMessage::tool_step(
            "task_1",
            "task",
            r#"{"description":"explore the codebase","prompt":"..."}"#,
        );
        task.push_subtask_event(&neenee_core::SubTaskEvent::ToolCall {
            id: "inner".into(),
            name: "grep".into(),
            arguments: r#"{"pattern":"foo"}"#.into(),
        });
        task.finish_tool_step("task_1", "found 3 matches", neenee_core::ToolOutput::text("found 3 matches"), 1200);
        let root_messages = vec![
            TranscriptMessage::new(neenee_core::Role::User, "explore please"),
            task,
        ];

        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                let _ = draw_transcript(
                    f,
                    &mut layout_map,
                    TranscriptView {
                        messages: &root_messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        activity: "running subagent",
                        spinner_phase: 0,
                        input: "",
                        byte_cursor: 0,
                        chrome_hidden: false,
                        subagent_bar: None,
                        hovered_reasoning: None,
                        focused_target: None,
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
                let _ = draw_transcript(
                    f,
                    &mut layout_map,
                    TranscriptView {
                        messages: &children,
                        scroll: 0,
                        selection: &SelectionState::None,
                        activity: "",
                        spinner_phase: 0,
                        input: "",
                        byte_cursor: 0,
                        chrome_hidden: false,
                        subagent_bar: Some(SubagentBarInfo {
                            label: "explore the codebase".to_string(),
                            index: 1,
                            total: 1,
                        }),
                        hovered_reasoning: None,
                        focused_target: None,
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

    /// The input box must reserve only a single content row for a short input
    /// but grow to fit wrapped text when the input is long.
    #[test]
    fn input_box_grows_with_wrapped_content() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = Theme::default();
        let messages: Vec<TranscriptMessage> = Vec::new();

        fn render_with(theme: &Theme, messages: &[TranscriptMessage], input: &str) -> Rect {
            let backend = TestBackend::new(40, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut rect = Rect::default();
            terminal
                .draw(|f| {
                    let mut layout_map = LayoutMap::new();
                    let r = draw_transcript(
                        f,
                        &mut layout_map,
                        TranscriptView {
                            messages,
                            scroll: 0,
                            selection: &SelectionState::None,
                            activity: "",
                            spinner_phase: 0,
                            input,
                            byte_cursor: input.len(),
                            chrome_hidden: false,
                            subagent_bar: None,
                            hovered_reasoning: None,
                            focused_target: None,
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

    /// `draw_composer` must not panic for tricky inputs and should place the caret
    /// on the second wrapped line when the cursor sits past the first wrap.
    #[test]
    fn draw_composer_wraps_and_positions_caret() {
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
                draw_composer(
                    f,
                    Rect::new(0, 0, 20, 8),
                    input,
                    input.len(),
                    true,
                    &theme,
                    &mut LayoutMap::new(),
                    true,
                    &mut 0,
                );
            })
            .unwrap();
    }

    /// Sent user messages and the composer must render as solid panels whose
    /// text keeps a 2-column inner padding on both sides (in the panel bg),
    /// matching the header. This locks the geometry so a refactor can't quietly
    /// drop the right-side padding again.
    #[test]
    fn user_message_and_composer_keep_symmetric_panel_padding() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = Theme::default();
        let user_bg = theme.user_panel_bg;
        let input_bg = theme.input_bg;
        let app_bg = theme.app_bg;
        let width = 60u16;
        let backend = TestBackend::new(width, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        // A long user message fills the first wrapped line edge to edge, so the
        // right-side padding is only present if the wrap width reserves it.
        let messages = vec![TranscriptMessage::new(
            neenee_core::Role::User,
            "x".repeat(200),
        )];
        let long_input = "y".repeat(200);

        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                // draw_transcript only computes the input box geometry; the composer
                // itself is drawn separately (as the live app does), using the
                // returned input_rect.
                let render = draw_transcript(
                    f,
                    &mut layout_map,
                    TranscriptView {
                        messages: &messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        activity: "",
                        spinner_phase: 0,
                        input: &long_input,
                        byte_cursor: 0,
                        chrome_hidden: false,
                        subagent_bar: None,
                        hovered_reasoning: None,
                        focused_target: None,
                        theme: &theme,
                    },
                );
                let mut input_scroll = 0;
                draw_composer(
                    f,
                    render.input_rect,
                    &long_input,
                    0,
                    true,
                    &theme,
                    &mut layout_map,
                    false,
                    &mut input_scroll,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();

        // Find the first user-message text row: col 0,1 are the app_bg outer
        // gutter, col 2,3 are the left inner pad (user_panel_bg), col 4 starts
        // the text. Scan for the row whose col 4 is 'x' under user_panel_bg.
        let user_row = (0..buffer.area.height)
            .find(|&y| {
                let c4 = buffer.get(4, y);
                c4.symbol() == "x" && c4.bg == user_bg
            })
            .expect("a user message text row should be rendered");

        // Left: 2-col app_bg outer gutter, then 2-col user_panel_bg inner pad.
        assert_eq!(buffer.get(0, user_row).bg, app_bg, "left outer gutter");
        assert_eq!(buffer.get(1, user_row).bg, app_bg, "left outer gutter");
        assert_eq!(
            buffer.get(2, user_row).bg,
            user_bg,
            "left inner padding must be user_panel_bg"
        );
        assert_eq!(
            buffer.get(3, user_row).bg,
            user_bg,
            "left inner padding is 2 cols, not 1"
        );
        assert_eq!(
            buffer.get(4, user_row).symbol(),
            "x",
            "text starts at col 4"
        );

        // Right: 2-col user_panel_bg inner pad, then 2-col app_bg outer gutter.
        // user_text_width = (60 - 4) - 4 = 52 -> text fills cols 4..56.
        assert_eq!(
            buffer.get(56, user_row).symbol(),
            " ",
            "right inner padding must stay clear of wrapped text"
        );
        assert_eq!(buffer.get(56, user_row).bg, user_bg, "right inner padding");
        assert_eq!(buffer.get(57, user_row).bg, user_bg, "right inner padding");
        assert_eq!(buffer.get(58, user_row).bg, app_bg, "right outer gutter");
        assert_eq!(buffer.get(59, user_row).bg, app_bg, "right outer gutter");

        // Composer: the input panel starts at x = FOOTER_H_INSET (2). `›` at
        // x=2, text from x=4, and a 2-col right pad in input_bg before the
        // app_bg gutter at the far right.
        let composer_row = (0..buffer.area.height)
            .find(|&y| {
                let c4 = buffer.get(4, y);
                c4.symbol() == "y" && c4.bg == input_bg
            })
            .expect("a composer text row should be rendered");
        assert_eq!(buffer.get(2, composer_row).symbol(), "›", "composer prompt");
        assert_eq!(
            buffer.get(4, composer_row).symbol(),
            "y",
            "composer text starts at col 4"
        );
        // full_w (composer panel) = 60 - 2*FOOTER_H_INSET = 56, panel spans
        // x=2..58. Right pad at x=56,57 (input_bg), gutter x=58,59 (app_bg).
        assert_eq!(
            buffer.get(56, composer_row).bg,
            input_bg,
            "composer right inner padding"
        );
        assert_eq!(
            buffer.get(57, composer_row).bg,
            input_bg,
            "composer right inner padding"
        );
        assert_eq!(
            buffer.get(58, composer_row).bg,
            app_bg,
            "composer right outer gutter"
        );
        assert_eq!(
            buffer.get(59, composer_row).bg,
            app_bg,
            "composer right outer gutter"
        );
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
                "Read".to_string(),
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

    /// Drive `draw_history_modal` against a real buffer across every input
    /// state the Ctrl+R picker can land in. The assertions are deliberately
    /// structural ("does not panic, produces a non-empty frame") because the
    /// fuzzy highlight math is already covered by `fuzzy::tests`; here we
    /// only need to prove the renderer consumes each state without exploding.
    #[test]
    fn history_modal_renders_every_query_state() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = Theme::default();
        let history = vec![
            "git status".to_string(),
            "git commit -am 'ship it'".to_string(),
            "cargo test".to_string(),
            "review the diff before sending".to_string(),
        ];

        let cases: &[(&str, usize)] = &[
            ("", history.len()), // empty query → everything surfaces
            ("git", 2),          // partial match → subset with highlights
            ("zzz", 0),          // no subsequence → empty placeholder
        ];

        for (query, expected_matches) in cases {
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut ranked = crate::fuzzy::rank(&history, query);
            crate::fuzzy::sort_by_score(&mut ranked);
            assert_eq!(
                ranked.len(),
                *expected_matches,
                "query {:?} should surface {} entries",
                query,
                expected_matches
            );
            terminal
                .draw(|f| {
                    draw_history_modal(
                        f,
                        &mut LayoutMap::new(),
                        &history,
                        query,
                        &ranked,
                        0,
                        &theme,
                    );
                })
                .expect("draw must not panic");
        }

        // Empty history must render the "(no history yet)" placeholder rather
        // than indexing into an empty slice.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let empty: Vec<String> = Vec::new();
        let ranked: Vec<(usize, crate::fuzzy::FuzzyMatch)> = crate::fuzzy::rank(&empty, "");
        terminal
            .draw(|f| {
                draw_history_modal(f, &mut LayoutMap::new(), &empty, "", &ranked, 0, &theme);
            })
            .expect("empty-history draw must not panic");
    }
}
