//! Rendering engine: draws the chat UI using ratatui while recording
//! semantic-to-screen layout information.

mod chrome;
mod composer;
mod design;
mod markdown_table;
mod message_body;
mod overlays;
mod primitives;
mod sidebar;
mod text_layout;
mod theme;
mod turn_artifacts;

pub use chrome::{draw_hint, draw_status_bar, draw_suggestions};
pub use composer::{draw_composer, INPUT_MSG_IDX};
use design::{
    CARD_MIN_WIDTH, CHAT_BODY_PREFIX_COLS, CHAT_BODY_RIGHT_INSET, CHAT_H_INSET,
    COMPOSER_MAX_HEIGHT_DIVISOR, COMPOSER_MIN_HEIGHT, COMPOSER_PROMPT_PREFIX_COLS,
    COMPOSER_VERTICAL_CHROME_ROWS, CONTEXT_USAGE_BAR_CELLS, FOOTER_H_INSET,
    HEADER_CONTEXT_MIN_WIDTH, HEADER_GOAL_GAP, HEADER_GOAL_MAX_CHARS, HEADER_PANEL_INNER_PADDING,
    HEADER_PATH_MAX_CHARS, HEADER_RIGHT_GAP_MIN, HEADER_ROWS, HEADER_WITH_CHECKLIST_ROWS,
    HINT_LINE_ROWS,
    MESSAGE_GAP_ROWS, REASONING_TRACE_BLOCK_GAP_ROWS, REASONING_TRACE_BODY_BOTTOM_GAP_ROWS,
    REASONING_TRACE_BODY_TOP_GAP_ROWS, STATUS_BAR_ROWS, SUBAGENT_BAR_ROWS,
    TOOL_CARD_BODY_BOTTOM_GAP_ROWS, TOOL_CARD_BODY_TOP_GAP_ROWS, TOOL_CARD_CHILDREN_GAP_ROWS,
    TOOL_CARD_SECTION_GAP_ROWS,
};
#[cfg(test)]
use markdown_table::{build_table_render, shrink_column_widths};
use message_body::draw_message_body;
pub(crate) use overlays::draw_models_modal;
pub use overlays::{
    draw_api_key_modal, draw_armed_toast, draw_copy_toast, draw_help_modal, draw_history_modal,
    draw_permission_sheet, draw_sessions_modal, draw_solution_input_modal, relative_time,
};
use primitives::viewport_rect;
pub use sidebar::{draw_sidebar, SidebarRender, SidebarView, SIDEBAR_AUTO_WIDTH, SIDEBAR_WIDTH};
use text_layout::wrap_text;
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
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as RtBlock, Paragraph},
    Frame,
};

use crate::document::{estimate_context_tokens, ChatMessage};
use crate::layout::LayoutMap;
use crate::model_context_window;
use crate::selection::SelectionState;
#[cfg(test)]
use neenee_core::PermissionRequest;
use neenee_core::{AgentMode, Goal, GoalStatus};
#[cfg(test)]
use std::collections::HashMap;
use unicode_width::UnicodeWidthStr;

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
    /// Working-directory display string (home swapped for `~`), shown on the
    /// left side of the header.
    pub cwd: &'a str,
    pub current_mode: AgentMode,
    pub current_goal: Option<&'a Goal>,
    /// Transient running status shown in a thin bar below the input box.
    /// Empty / "idle" / "responding" means the status bar is hidden.
    pub activity: &'a str,
    /// Spinner animation phase (cycles through braille frames while active).
    pub spinner_phase: usize,
    /// The current input-box text (masked while the API-key modal is open). The
    /// chat layout reads this so the input box can grow to fit its wrapped text.
    pub input: &'a str,
    /// Byte offset of the caret inside `input` (see [`App::byte_cursor`]). The
    /// box grows one extra row when the caret rests past the last wrapped line
    /// (e.g. just after an inserted newline), so its height matches what
    /// [`composer::draw_composer`] actually renders.
    pub byte_cursor: usize,
    /// When true, the header and input box are hidden (overlay modal open).
    pub chrome_hidden: bool,
    /// When set, the view is zoomed into a sub-agent task: a navigation bar is
    /// rendered and `messages` is the focused task's child stream.
    pub subagent_bar: Option<SubagentBarInfo>,
    /// When `true`, the right-side persistent sidebar is rendered alongside
    /// the chat. The chat area shrinks by [`SIDEBAR_WIDTH`] columns.
    pub sidebar_visible: bool,
    /// Harness loop status string mirrored into the sidebar (e.g. `"idle"`,
    /// `"loop 3/8"`). Only shown when non-idle.
    pub loop_status: &'a str,
    /// Sidebar scroll offset (content lines) the caller is holding. Used only
    /// when `sidebar_visible` is `true`.
    pub sidebar_scroll: usize,
    pub theme: &'a Theme,
    /// MCP server statuses loaded at startup, shown as a compact count in the
    /// header right corner (e.g. "MCP 2/3 · 7 tools").
    pub mcp_statuses: &'a [(String, neenee_core::mcp::McpConnectionStatus)],
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
    /// The hint line area, stacked below the input box and the transient
    /// status bar (when visible).
    pub hint_rect: Rect,
    /// Total height (in lines) of the rendered message stream, ignoring the
    /// viewport clip. Used by the app loop to pin the view to the bottom.
    pub content_lines: usize,
    /// Height of the chat viewport.
    pub view_height: u16,
    /// The expanded card whose body is currently scrolled into view, so the app
    /// can render/click a sticky header pinned under the HUD bar. `None` when no
    /// expanded card body covers the top of the viewport.
    pub sticky: Option<StickyInfo>,
    /// Sidebar render result for this frame. `rect: None` means the sidebar
    /// was hidden (terminal too narrow and not forced on).
    pub sidebar: SidebarRender,
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
        current_provider,
        current_model,
        cwd,
        current_mode,
        current_goal,
        activity,
        spinner_phase,
        input,
        byte_cursor,
        chrome_hidden,
        subagent_bar,
        sidebar_visible,
        loop_status,
        sidebar_scroll,
        theme,
        mcp_statuses,
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

    // Reserve the right-side sidebar column before computing the chat layout
    // so every chat-area component (header, cards, input) shrinks to match.
    // The sidebar itself renders against the right edge of the viewport.
    let size = if sidebar_visible && viewport.width > SIDEBAR_WIDTH {
        Rect {
            x: viewport.x,
            y: viewport.y,
            width: viewport.width - SIDEBAR_WIDTH,
            height: viewport.height,
        }
    } else {
        viewport
    };
    let sidebar_visible_effective = sidebar_visible && viewport.width > SIDEBAR_WIDTH;

    let checklist = current_goal.and_then(goal_checklist_summary);
    // Header height is content rows only (no separator rule); the band is
    // separated from the chat by `header_bg`. Hidden entirely when an overlay
    // modal is open (chrome_hidden).
    let header_height: u16 = if chrome_hidden {
        0
    } else if checklist.is_some() {
        HEADER_WITH_CHECKLIST_ROWS
    } else {
        HEADER_ROWS
    };
    // The status bar (animated spinner + activity text) sits on its own line
    // directly below the input box. It is shown only for non-streaming,
    // non-idle activity so the chat reclaims that row when nothing is running.
    let status_active =
        !chrome_hidden && !activity.is_empty() && activity != "idle" && activity != "responding";
    let status_height: u16 = if status_active { STATUS_BAR_ROWS } else { 0 };

    // The input box grows with its content: the typed text wraps onto new
    // lines and the box expands to fit, up to roughly half the terminal so the
    // chat history always stays visible. The inner text width reserves the
    // footer insets and the `> ` prompt prefix.
    let input_text_width = (size.width as usize)
        .saturating_sub((2 * FOOTER_H_INSET) as usize + COMPOSER_PROMPT_PREFIX_COLS)
        .max(1);
    let input_wrapped_lines = composer::input_row_count(input, input_text_width, byte_cursor);
    let desired_input_height = input_wrapped_lines as u16 + COMPOSER_VERTICAL_CHROME_ROWS;
    let max_input_height = (size.height / COMPOSER_MAX_HEIGHT_DIVISOR).max(COMPOSER_MIN_HEIGHT);
    let input_box_height = desired_input_height.min(max_input_height);
    let footer_height: u16 = if chrome_hidden {
        0
    } else {
        status_height + input_box_height + HINT_LINE_ROWS
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height), // Header and optional checklist dock
            Constraint::Min(0),                // Chat
            Constraint::Length(footer_height), // Status? + input box + hint line + bottom gap
        ])
        .split(size);

    // 1. Header — a floating panel: 2-col `app_bg` side gutters, solid
    //    `panel_bg` top/bottom rows, content indented inside. The left side
    //    shows the working directory (home collapsed to `~`), the right side
    //    shows the model name plus the goal / MCP / context-usage cluster.
    //    Skipped entirely when an overlay modal is open.
    if !chrome_hidden {
        let goal = current_goal.map(|goal| {
            let objective = goal
                .objective
                .chars()
                .take(HEADER_GOAL_MAX_CHARS)
                .collect::<String>();
            let suffix = if goal.objective.chars().count() > HEADER_GOAL_MAX_CHARS {
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

        let panel_bg = theme.panel_bg;
        let app_bg = theme.app_bg;
        let inset = CHAT_H_INSET as usize;
        let full_w = chunks[0].width as usize;
        let panel_w = full_w.saturating_sub(2 * inset).max(1);
        let gutter = Span::styled(" ".repeat(inset), Style::default().bg(app_bg));
        let inner = HEADER_PANEL_INNER_PADDING;

        // --- Content row: cwd on the left, model name + goal + MCP / context
        //    bar on the right. The path is the workspace indicator; the model
        //    name + goal + context cluster moves to the right so the left side
        //    reads purely as "where am I".
        let path_display = truncate_path(cwd, HEADER_PATH_MAX_CHARS);
        let path_width = path_display.width();
        let mut content = vec![
            gutter.clone(),
            Span::styled(" ".repeat(inner), Style::default().bg(panel_bg)),
            Span::styled(
                path_display,
                Style::default().fg(theme.text_muted).bg(panel_bg),
            ),
        ];
        let mut panel_used = inner + path_width;

        // Right-aligned cluster: model name, then goal, then optional MCP
        // summary plus the context-usage bar. The bar fills with the *used*
        // fraction of the model's context window, so a nearly full bar means
        // the window is almost exhausted. Skipped for providers without a
        // known window (custom / local / mock) and on terminals too narrow
        // to fit it cleanly.
        let mut right_spans: Vec<Span> = Vec::new();
        let mut right_width = 0usize;
        right_spans.push(Span::styled(
            current_model.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
                .bg(panel_bg),
        ));
        right_width += current_model.width();
        if let Some(goal) = goal {
            let goal_width = goal.width();
            right_spans.push(Span::styled(
                " ".repeat(HEADER_GOAL_GAP),
                Style::default().bg(panel_bg),
            ));
            right_spans.push(Span::styled(
                goal,
                Style::default().fg(theme.text_muted).bg(panel_bg),
            ));
            right_width += HEADER_GOAL_GAP + goal_width;
        }
        let mcp_summary = format_mcp_summary(mcp_statuses);
        if !mcp_summary.is_empty() {
            let mcp_width = mcp_summary.width();
            right_spans.push(Span::styled(
                " ".repeat(HEADER_RIGHT_GAP_MIN),
                Style::default().bg(panel_bg),
            ));
            right_width += HEADER_RIGHT_GAP_MIN;
            right_spans.push(Span::styled(
                mcp_summary,
                Style::default().fg(theme.text_muted).bg(panel_bg),
            ));
            right_width += mcp_width;
        }
        let context_max = model_context_window(current_provider);
        if context_max > 0 && panel_w >= HEADER_CONTEXT_MIN_WIDTH {
            right_spans.push(Span::styled(
                " ".repeat(HEADER_RIGHT_GAP_MIN),
                Style::default().bg(panel_bg),
            ));
            right_width += HEADER_RIGHT_GAP_MIN;
            let used = estimate_context_tokens(messages);
            let usage_bar = context_usage_spans(used, context_max, theme, panel_bg);
            right_width += usage_bar.iter().map(|s| s.content.width()).sum::<usize>();
            right_spans.extend(usage_bar);
        }
        let gap = panel_w
            .saturating_sub(panel_used + right_width)
            .max(HEADER_RIGHT_GAP_MIN);
        content.push(Span::styled(
            " ".repeat(gap),
            Style::default().bg(panel_bg),
        ));
        content.extend(right_spans);
        panel_used += gap + right_width;
        // Trail the rest of the panel with `panel_bg` so the row reads as a
        // solid band up to the right gutter.
        content.push(Span::styled(
            " ".repeat(panel_w.saturating_sub(panel_used)),
            Style::default().bg(panel_bg),
        ));
        content.push(gutter.clone());

        // --- Optional checklist dock row, indented like the content row.
        let checklist_line = checklist.map(|(done, total, current)| {
            let tasks = format!("Tasks {}/{}  ", done, total);
            let tasks_w = tasks.width();
            let current_w = current.width();
            let fill = panel_w
                .saturating_sub(inner + tasks_w + current_w)
                .max(HEADER_RIGHT_GAP_MIN);
            Line::from(vec![
                gutter.clone(),
                Span::styled(" ".repeat(inner), Style::default().bg(panel_bg)),
                Span::styled(
                    tasks,
                    Style::default()
                        .fg(theme.primary)
                        .add_modifier(Modifier::BOLD)
                        .bg(panel_bg),
                ),
                Span::styled(current, Style::default().fg(theme.text_muted).bg(panel_bg)),
                Span::styled(" ".repeat(fill), Style::default().bg(panel_bg)),
                gutter.clone(),
            ])
        });

        // --- Top / bottom rows: solid `panel_bg` fills the full cell height
        // (no half-block fade) so the header reads as a solid block with the
        // content row, framed by the `app_bg` side gutters.
        let solid_line = Line::from(vec![
            gutter.clone(),
            Span::styled(
                " ".repeat(panel_w),
                Style::default().bg(panel_bg),
            ),
            gutter.clone(),
        ]);

        let mut lines = vec![solid_line.clone(), Line::from(content)];
        if let Some(line) = checklist_line {
            lines.push(line);
        }
        lines.push(solid_line);
        frame.render_widget(Paragraph::new(lines), chunks[0]);
    } // end !chrome_hidden

    // 2. Chat History
    // When zoomed into a sub-agent, reserve a 1-line navigation band at the
    // bottom of the chat viewport for the sub-agent bar.
    let (chat_area, subagent_bar_rect) = if subagent_bar.is_some() {
        let sub = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(SUBAGENT_BAR_ROWS)])
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
        let is_assistant_side = msg.role == neenee_core::Role::Assistant
            || msg.is_thinking()
            || msg.is_tool_step();
        if is_assistant_side {
            if let Some(attribution) = msg.attribution_label() {
                if last_shown_attribution.as_ref() != Some(&attribution) {
                    draw_attribution_badge(
                        frame,
                        chat_area,
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
            draw_tool_step_card(
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
            draw_reasoning_trace(
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
            draw_message_body(
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
            content_lines += MESSAGE_GAP_ROWS;
            if skip_rows > 0 {
                skip_rows = skip_rows.saturating_sub(1);
            } else if current_y < chat_area.y + chat_area.height {
                current_y += MESSAGE_GAP_ROWS as u16;
            }
        }
    }

    // Sub-agent navigation band, drawn across the full chat width (inside the
    // app_bg gutters) so it reads as a continuous bar pinned above the input.
    if let (Some(bar), Some(rect)) = (subagent_bar.as_ref(), subagent_bar_rect) {
        draw_subagent_bar(frame, rect, bar, theme);
    }

    // The footer stacks, from top to bottom: input box, transient status bar
    // (when active), and the hint line. The input box therefore always anchors
    // the top of the footer; the status bar and hint line follow it.
    let footer_x = chunks[2].x + FOOTER_H_INSET;
    let footer_w = chunks[2].width.saturating_sub(2 * FOOTER_H_INSET);
    let input_rect = Rect::new(footer_x, chunks[2].y, footer_w, input_box_height);

    // The transient running status lives directly below the input box. Hidden
    // while text is actively streaming ("responding"), since the streamed
    // response is itself the feedback in that phase, and hidden when idle.
    let status_y = input_rect.y + input_rect.height;
    if status_active {
        draw_status_bar(
            frame,
            Rect::new(footer_x, status_y, footer_w, STATUS_BAR_ROWS),
            activity,
            spinner_phase,
            theme,
        );
    }

    // The hint line sits below the status bar, or directly below the input
    // box when the status bar is hidden.
    let hint_rect = Rect::new(footer_x, status_y + status_height, footer_w, HINT_LINE_ROWS);

    // Sticky pinned header: if an expanded card's body covers the top of the
    // viewport (its header is scrolled out of view), pin its header to the line
    // directly under the HUD bar so the user can always collapse it.
    let sticky_info = draw_sticky_header_if_needed(frame, chat_area, &sticky_cards, scroll, theme);

    // Render the persistent right-side sidebar. It draws against the full
    // viewport (not the chat-narrowed `size`) so it spans top-to-bottom
    // independently of the chat's vertical layout. Hidden overlay modals skip
    // the sidebar too so the modal reads as a full-screen focus change.
    let sidebar = if sidebar_visible_effective && !chrome_hidden {
        draw_sidebar(
            frame,
            SidebarView {
                current_provider,
                current_model,
                current_mode,
                current_goal,
                loop_status,
                scroll: sidebar_scroll,
                theme,
            },
        )
    } else {
        SidebarRender::empty()
    };

    ChatRender {
        input_rect,
        hint_rect,
        content_lines,
        view_height: chat_area.height,
        sticky: sticky_info,
        sidebar,
    }
}

/// Draw a single-line model attribution badge above an assistant turn.
///
/// The badge labels which provider/model produced the following response, so a
/// session that mixes models stays traceable. It occupies one content line
/// (scrollable like any other), sits flush with the chat body prefix, and is
/// rendered in muted text so it reads as metadata rather than content. The
/// provider half is dropped when empty (e.g. providers without an id).
fn draw_attribution_badge(
    frame: &mut Frame,
    area: Rect,
    attribution: &(String, String),
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    theme: &Theme,
) {
    *content_lines += 1;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(1);
        return;
    }
    if *current_y >= area.y + area.height {
        return;
    }

    let (provider, model) = attribution;
    let prefix = " ".repeat(CHAT_BODY_PREFIX_COLS as usize);
    // `provider · model`, dropping the provider half (and separator) when the
    // provider id is empty so untagged/legacy providers show just the model.
    let label = if provider.is_empty() {
        model.clone()
    } else {
        format!("{} · {}", provider, model)
    };

    let line = Line::from(vec![
        Span::styled(prefix, Style::default()),
        Span::styled("◆ ", Style::default().fg(theme.dim_fg)),
        Span::styled(label, Style::default().fg(theme.text_muted)),
    ]);
    let rect = Rect::new(area.x, *current_y, area.width, 1);
    frame.render_widget(Paragraph::new(line), rect);
    *current_y += 1;
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

/// Shorten a working-directory display string to `max_chars` columns. Paths at
/// or below the budget come back untouched. Longer paths collapse from the
/// left: a leading `~/` home anchor is preserved when it fits (so the user's
/// request to show home as `~` survives on deep directories), followed by `…`
/// and the trailing path text. Anything else falls back to a plain `…` prefix.
fn truncate_path(path: &str, max_chars: usize) -> String {
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= max_chars {
        return path.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    // Preserve a leading "~/" anchor (home shortcut) when truncating from the
    // left so the home indicator survives even on deep directories.
    if let Some(rest) = path.strip_prefix("~/") {
        let anchor = "~/";
        let anchor_chars = 2;
        // Need room for anchor + `…` + at least one char of the rest.
        let budget = max_chars.saturating_sub(anchor_chars + 1);
        let rest_chars: Vec<char> = rest.chars().collect();
        if budget > 0 && rest_chars.len() > budget {
            let kept: String = rest_chars[rest_chars.len() - budget..].iter().collect();
            return format!("{}…{}", anchor, kept);
        } else if budget > 0 {
            return format!("{}{}", anchor, rest);
        }
    }
    let take = max_chars - 1;
    let truncated: String = chars[chars.len() - take..].iter().collect();
    format!("…{}", truncated)
}

fn format_mcp_summary(statuses: &[(String, neenee_core::mcp::McpConnectionStatus)]) -> String {
    if statuses.is_empty() {
        return String::new();
    }
    let total = statuses.len();
    let connected = statuses
        .iter()
        .filter(|(_, status)| {
            matches!(
                status,
                neenee_core::mcp::McpConnectionStatus::Connected { .. }
            )
        })
        .count();
    let tools: usize = statuses
        .iter()
        .filter_map(|(_, status)| match status {
            neenee_core::mcp::McpConnectionStatus::Connected { tools } => Some(*tools),
            _ => None,
        })
        .sum();
    format!("MCP {}/{} · {} tools", connected, total, tools)
}

/// Context-usage ratio at which the usage bar turns from green to yellow.
const CONTEXT_USAGE_WARN_THRESHOLD: f64 = 0.7;
/// Context-usage ratio at which the usage bar turns from yellow to red.
const CONTEXT_USAGE_CRIT_THRESHOLD: f64 = 0.9;

/// Format a token count with a single-letter SI suffix: `999`, `1.0k`, `20.2k`,
/// `1.5M`, `3.2B`.
fn format_token_count(n: usize) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}

/// Context-window usage indicator: `[████░░░░░░] 58% (20.2k/256k)`. The bar
/// fills with the used fraction of the model's context window, so a nearly full
/// bar means the window is almost exhausted. Color steps green → yellow → red
/// as usage climbs past the warn / critical thresholds. `bg` is applied to
/// every span so the indicator reads on a solid panel background.
fn context_usage_spans(used: usize, max: usize, theme: &Theme, bg: Color) -> Vec<Span<'static>> {
    let cells = CONTEXT_USAGE_BAR_CELLS;
    let ratio = if max == 0 {
        0.0
    } else {
        ((used as f64) / (max as f64)).clamp(0.0, 1.0)
    };
    let filled = (ratio * cells as f64).round() as usize;
    let color = if ratio < CONTEXT_USAGE_WARN_THRESHOLD {
        theme.success
    } else if ratio < CONTEXT_USAGE_CRIT_THRESHOLD {
        theme.warning
    } else {
        theme.error_fg
    };
    let pct = (ratio * 100.0).round() as u32;

    let mut spans = Vec::with_capacity(cells + 6);
    spans.push(Span::styled(
        " [",
        Style::default().fg(theme.text_muted).bg(bg),
    ));
    for i in 0..cells {
        if i < filled {
            spans.push(Span::styled("█", Style::default().fg(color).bg(bg)));
        } else {
            spans.push(Span::styled(
                "░",
                Style::default().fg(theme.text_muted).bg(bg),
            ));
        }
    }
    spans.push(Span::styled(
        "] ",
        Style::default().fg(theme.text_muted).bg(bg),
    ));
    spans.push(Span::styled(
        format!("{}%", pct),
        Style::default().fg(color).bg(bg),
    ));
    spans.push(Span::styled(
        format!(
            " ({}/{})",
            format_token_count(used),
            format_token_count(max)
        ),
        Style::default().fg(theme.text_muted).bg(bg),
    ));
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_summary_counts_connected_servers_and_tools() {
        use neenee_core::mcp::McpConnectionStatus;
        let statuses = vec![
            (
                "fs".to_string(),
                McpConnectionStatus::Connected { tools: 3 },
            ),
            (
                "git".to_string(),
                McpConnectionStatus::Failed("not found".to_string()),
            ),
        ];
        assert_eq!(
            format_mcp_summary(&statuses),
            "MCP 1/2 · 3 tools".to_string()
        );
    }

    #[test]
    fn mcp_summary_is_empty_when_no_servers() {
        assert!(format_mcp_summary(&[]).is_empty());
    }

    #[test]
    fn format_token_count_uses_si_suffixes() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1000), "1.0k");
        assert_eq!(format_token_count(20_200), "20.2k");
        assert_eq!(format_token_count(1_000_000), "1.0M");
        assert_eq!(format_token_count(3_200_000_000), "3.2B");
    }

    #[test]
    fn context_usage_spans_show_bar_percentage_and_counts() {
        let theme = Theme::default();
        let spans = context_usage_spans(20_200, 256_000, &theme, theme.panel_bg);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("[█░░░░░░░░░]"), "bar rendered: {text}");
        assert!(text.contains(" 8%"), "percentage rendered: {text}");
        assert!(text.contains("(20.2k/256.0k)"), "counts rendered: {text}");
        assert!(!text.contains('╸'), "nub character removed: {text}");
    }

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
                        cwd: "~/project",
                        current_mode: AgentMode::Build,
                        current_goal: None,
                        activity: "waiting for model",
                        spinner_phase: 0,
                        input: "hello",
                        byte_cursor: 5,
                        chrome_hidden: false,
                        subagent_bar: None,
                        sidebar_visible: false,
                        loop_status: "idle",
                        sidebar_scroll: 0,
                        theme: &theme,
                        mcp_statuses: &[],
                    },
                );
                draw_composer(
                    f,
                    Rect::new(0, 21, 80, 3),
                    "hello",
                    5,
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
                        cwd: "~/project",
                        current_mode: AgentMode::Build,
                        current_goal: None,
                        activity: "running subagent",
                        spinner_phase: 0,
                        input: "",
                        byte_cursor: 0,
                        chrome_hidden: false,
                        subagent_bar: None,
                        sidebar_visible: false,
                        loop_status: "idle",
                        sidebar_scroll: 0,
                        theme: &theme,
                        mcp_statuses: &[],
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
                        cwd: "~/project",
                        current_mode: AgentMode::Build,
                        current_goal: None,
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
                        sidebar_visible: false,
                        loop_status: "idle",
                        sidebar_scroll: 0,
                        theme: &theme,
                        mcp_statuses: &[],
                    },
                );
            })
            .unwrap();
    }

    /// End-to-end check that turning the sidebar on at a wide terminal width
    /// shrinks the chat column and renders the sidebar pane alongside it
    /// without any layout panic.
    #[test]
    fn chat_with_sidebar_shrinks_chat_and_renders_sidebar() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = Theme::default();
        let backend = TestBackend::new(160, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let goal = Goal {
            objective: "Render the sidebar end to end".to_string(),
            status: GoalStatus::Active,
            checklist: vec![
                neenee_core::GoalChecklistItem {
                    content: "Add module".to_string(),
                    status: neenee_core::GoalChecklistStatus::Completed,
                },
                neenee_core::GoalChecklistItem {
                    content: "Wire scroll".to_string(),
                    status: neenee_core::GoalChecklistStatus::InProgress,
                },
            ],
            tokens_used: 5_000,
            token_budget: Some(50_000),
            time_used_seconds: 120,
        };
        let messages = vec![ChatMessage::new(neenee_core::Role::User, "ping")];

        let mut captured_sidebar_rect: Option<Rect> = None;
        let mut captured_chat_width: u16 = 0;
        terminal
            .draw(|f| {
                let mut layout_map = LayoutMap::new();
                let r = draw_chat(
                    f,
                    &mut layout_map,
                    ChatView {
                        messages: &messages,
                        scroll: 0,
                        selection: &SelectionState::None,
                        current_provider: "mock",
                        current_model: "mock-model",
                        cwd: "~/project",
                        current_mode: AgentMode::Build,
                        current_goal: Some(&goal),
                        activity: "thinking",
                        spinner_phase: 0,
                        input: "hello",
                        byte_cursor: 5,
                        chrome_hidden: false,
                        subagent_bar: None,
                        sidebar_visible: true,
                        loop_status: "loop 1/4",
                        sidebar_scroll: 0,
                        theme: &theme,
                        mcp_statuses: &[],
                    },
                );
                captured_sidebar_rect = r.sidebar.rect;
                // The chat-area width is reported indirectly via `view_height`
                // and the input rect: the input box sits inside the chat
                // column, so its x+width bounds the chat column.
                captured_chat_width = r.input_rect.x + r.input_rect.width;
            })
            .unwrap();

        // Sidebar should be visible and sit at the right edge of the frame.
        let sidebar = captured_sidebar_rect.expect("sidebar should render when visible");
        assert_eq!(sidebar.width, SIDEBAR_WIDTH);
        assert_eq!(sidebar.x + sidebar.width, 160);
        // Chat column ends well before the sidebar begins.
        assert!(captured_chat_width <= sidebar.x);
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
    fn truncate_path_preserves_short_paths() {
        assert_eq!(truncate_path("~/projects/neenee", 40), "~/projects/neenee");
        assert_eq!(truncate_path("/", 40), "/");
        assert_eq!(truncate_path("~", 40), "~");
    }

    #[test]
    fn truncate_path_keeps_home_anchor_when_collapsing() {
        // A deep directory under home keeps `~/` and the leaf tail.
        let deep = "~/a/very/deeply/nested/project/directory/here";
        let out = truncate_path(deep, 20);
        assert!(
            out.starts_with("~/…"),
            "expected home anchor preserved, got: {}",
            out
        );
        assert!(out.ends_with("directory/here"));
        assert!(out.chars().count() <= 20);
    }

    #[test]
    fn truncate_path_falls_back_to_ellipsis_for_non_home() {
        let abs = "/var/log/some/very/deep/path/leaf";
        let out = truncate_path(abs, 12);
        assert!(out.starts_with('…'));
        assert!(out.chars().count() <= 12);
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
                            cwd: "~/project",
                            current_mode: AgentMode::Build,
                            current_goal: None,
                            activity: "",
                            spinner_phase: 0,
                            input,
                            byte_cursor: input.len(),
                            chrome_hidden: false,
                            subagent_bar: None,
                            sidebar_visible: false,
                            loop_status: "idle",
                            sidebar_scroll: 0,
                            theme,
                            mcp_statuses: &[],
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
