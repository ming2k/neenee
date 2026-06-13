//! Rendering engine: draws the chat UI using ratatui while recording
//! semantic-to-screen layout information.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as RtBlock, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use std::collections::HashMap;

use crate::document::{Block, ChatMessage};
use crate::layout::{BlockRegion, LayoutMap};
use crate::selection::{floor_char_boundary, inclusive_end, SelectionState};
use neenee_core::{AgentMode, Goal, GoalStatus, PermissionRequest};

/// The byte range of a block covered by the selection.
/// `(start, None)` means "from start to the end of the block".
fn block_selection_range(
    selection: &SelectionState,
    message_idx: usize,
    block_idx: usize,
) -> Option<(usize, Option<usize>)> {
    match selection {
        SelectionState::None => None,
        SelectionState::Block {
            message_idx: mi,
            block_idx: bi,
        } => (*mi == message_idx && *bi == block_idx).then_some((0, None)),
        SelectionState::Range { .. } => {
            let (start, end) = selection.normalized_range()?;
            let here = (message_idx, block_idx);
            if here < (start.message_idx, start.block_idx)
                || here > (end.message_idx, end.block_idx)
            {
                return None;
            }
            let s = if here == (start.message_idx, start.block_idx) {
                start.byte_offset
            } else {
                0
            };
            let e = if here == (end.message_idx, end.block_idx) {
                Some(end.byte_offset)
            } else {
                None
            };
            Some((s, e))
        }
    }
}

/// Intersect a block selection range with one wrapped line, producing the
/// selected byte range *relative to the line text*. The selection head
/// character is included.
fn line_selection(
    range: Option<(usize, Option<usize>)>,
    wl: &WrappedLine,
) -> Option<(usize, usize)> {
    let (s, e) = range?;
    if let Some(e) = e {
        if e < wl.start_byte {
            return None;
        }
    }
    if s >= wl.end_byte && !(s == wl.start_byte && wl.text.is_empty()) {
        return None;
    }
    let lo = floor_char_boundary(&wl.text, s.saturating_sub(wl.start_byte));
    let hi = match e {
        Some(e) if e < wl.end_byte => inclusive_end(&wl.text, e - wl.start_byte),
        _ => wl.text.len(),
    };
    (lo < hi).then_some((lo, hi))
}

/// Build a rendered line: decoration prefix plus the text split into
/// unselected / selected / unselected spans.
fn line_spans(
    prefix: &str,
    prefix_style: Style,
    text: &str,
    selected: Option<(usize, usize)>,
    base: Style,
    selected_bg: Color,
) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix.to_string(), prefix_style)];
    match selected {
        None => spans.push(Span::styled(text.to_string(), base)),
        Some((lo, hi)) => {
            if lo > 0 {
                spans.push(Span::styled(text[..lo].to_string(), base));
            }
            spans.push(Span::styled(text[lo..hi].to_string(), base.bg(selected_bg)));
            if hi < text.len() {
                spans.push(Span::styled(text[hi..].to_string(), base));
            }
        }
    }
    Line::from(spans)
}

/// A wrapped line with byte-offset bookkeeping.
struct WrappedLine {
    text: String,
    start_byte: usize,
    end_byte: usize,
}

/// Wrap text into lines that fit within `max_width` display columns.
/// Returns each line along with the byte range it covers in the original text.
fn wrap_text(text: &str, max_width: usize) -> Vec<WrappedLine> {
    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;
    let mut line_start_byte = 0;

    for (byte_idx, ch) in text.char_indices() {
        let ch_width = ch.width().unwrap_or(0);

        if ch == '\n' {
            lines.push(WrappedLine {
                text: std::mem::take(&mut current_line),
                start_byte: line_start_byte,
                end_byte: byte_idx,
            });
            line_start_byte = byte_idx + 1;
            current_width = 0;
            continue;
        }

        // Keep closing CJK punctuation with the preceding character. If it
        // would start the next line, move the preceding character with it.
        if current_width + ch_width > max_width && !current_line.is_empty() {
            let move_previous = prohibited_line_start(ch)
                || current_line.chars().last().is_some_and(prohibited_line_end);
            if move_previous && current_line.chars().count() > 1 {
                let moved = current_line.pop().unwrap();
                let moved_start = byte_idx - moved.len_utf8();
                lines.push(WrappedLine {
                    text: std::mem::take(&mut current_line),
                    start_byte: line_start_byte,
                    end_byte: moved_start,
                });
                current_line.push(moved);
                current_width = moved.width().unwrap_or(0);
                line_start_byte = moved_start;
            } else {
                lines.push(WrappedLine {
                    text: std::mem::take(&mut current_line),
                    start_byte: line_start_byte,
                    end_byte: byte_idx,
                });
                line_start_byte = byte_idx;
                current_width = 0;
            }
        }

        current_line.push(ch);
        current_width += ch_width;
    }

    if !current_line.is_empty() || line_start_byte < text.len() {
        lines.push(WrappedLine {
            text: current_line,
            start_byte: line_start_byte,
            end_byte: text.len(),
        });
    }

    lines
}

fn prohibited_line_start(ch: char) -> bool {
    matches!(
        ch,
        '，' | '。'
            | '、'
            | '！'
            | '？'
            | '：'
            | '；'
            | '）'
            | '】'
            | '》'
            | '〉'
            | '」'
            | '』'
            | '〕'
            | '”'
            | '’'
            | ','
            | '.'
            | '!'
            | '?'
            | ':'
            | ';'
            | ')'
            | ']'
            | '}'
    )
}

fn prohibited_line_end(ch: char) -> bool {
    matches!(
        ch,
        '（' | '【' | '《' | '〈' | '「' | '『' | '〔' | '“' | '‘' | '(' | '[' | '{'
    )
}

/// Styles used during rendering.
pub struct Theme {
    pub user_fg: Color,
    pub assistant_fg: Color,
    pub error_fg: Color,
    pub system_fg: Color,
    pub code_fg: Color,
    pub code_bg: Color,
    pub heading_fg: Color,
    pub quote_fg: Color,
    pub dim_fg: Color,
    pub selected_bg: Color,
    pub header_bg: Color,
    pub accent: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user_fg: Color::Rgb(137, 180, 250),
            assistant_fg: Color::Rgb(205, 214, 244),
            error_fg: Color::Red,
            system_fg: Color::DarkGray,
            code_fg: Color::Rgb(148, 226, 213),
            code_bg: Color::Rgb(24, 24, 37),
            heading_fg: Color::Rgb(203, 166, 247),
            quote_fg: Color::Rgb(249, 226, 175),
            dim_fg: Color::DarkGray,
            selected_bg: Color::Rgb(69, 71, 112),
            header_bg: Color::Rgb(24, 24, 37),
            accent: Color::Rgb(203, 166, 247),
        }
    }
}

pub struct ChatView<'a> {
    pub messages: &'a [ChatMessage],
    pub scroll: u16,
    pub selection: &'a SelectionState,
    pub current_provider: &'a str,
    pub current_model: &'a str,
    pub current_mode: AgentMode,
    pub current_goal: Option<&'a Goal>,
    pub loop_status: &'a str,
    pub theme: &'a Theme,
}

/// Draw the main chat area, recording layout info.
pub fn draw_chat(frame: &mut Frame, layout_map: &mut LayoutMap, view: ChatView<'_>) -> Rect {
    let ChatView {
        messages,
        scroll,
        selection,
        current_provider,
        current_model,
        current_mode,
        current_goal,
        loop_status,
        theme,
    } = view;
    let size = frame.size();
    let checklist = current_goal.and_then(goal_checklist_summary);
    let header_height = if checklist.is_some() { 2 } else { 1 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height), // Header and optional checklist dock
            Constraint::Min(0),                // Chat
            Constraint::Length(3),             // Bordered input box (rendered separately)
        ])
        .split(size);

    // 1. Header
    let mode = match current_mode {
        AgentMode::Build => "BUILD",
        AgentMode::Plan => "PLAN",
    };
    let goal = current_goal.map(|goal| {
        let objective = goal.objective.chars().take(32).collect::<String>();
        let suffix = if goal.objective.chars().count() > 32 {
            "..."
        } else {
            ""
        };
        let mark = if goal.status == GoalStatus::Completed {
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
        Span::styled(" ● ", Style::default().fg(Color::Green)),
        Span::styled("neenee", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(
            format!("{} ({})", current_provider, current_model),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("  "),
        Span::styled(
            mode,
            Style::default()
                .fg(if current_mode == AgentMode::Build {
                    Color::Yellow
                } else {
                    Color::Blue
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(loop_status, Style::default().fg(Color::Magenta)),
    ];
    if let Some(goal) = goal {
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled(goal, Style::default().fg(Color::DarkGray)));
    }
    let mut header_lines = vec![Line::from(header_spans)];
    if let Some((done, total, current)) = checklist {
        header_lines.push(Line::from(vec![
            Span::styled(
                format!("   Tasks {}/{}  ", done, total),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(current, Style::default().fg(Color::DarkGray)),
        ]));
    }
    frame.render_widget(
        Paragraph::new(header_lines).style(Style::default().bg(theme.header_bg)),
        chunks[0],
    );

    // 2. Chat History
    let chat_area = chunks[1];
    let mut current_y = chat_area.y;
    // Account for scroll
    let mut skip_rows = scroll as usize;

    for (mi, msg) in messages.iter().enumerate() {
        // Render message header (role label, opencode-style)
        let (glyph, label, role_color) = match msg.role {
            neenee_core::Role::User => ("┃", "You", theme.user_fg),
            neenee_core::Role::Assistant => ("┃", "neenee", theme.accent),
            neenee_core::Role::System => ("┃", "system", theme.system_fg),
            neenee_core::Role::Tool => ("┃", "tool", theme.quote_fg),
        };

        if !msg.is_tool_step() {
            let header_line = Line::from(vec![
                Span::styled(
                    format!(" {} ", glyph),
                    Style::default().fg(role_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label,
                    Style::default().fg(role_color).add_modifier(Modifier::BOLD),
                ),
            ]);

            if skip_rows > 0 {
                skip_rows = skip_rows.saturating_sub(1);
            } else if current_y < chat_area.y + chat_area.height {
                let line_rect = Rect::new(chat_area.x, current_y, chat_area.width, 1);
                frame.render_widget(Paragraph::new(header_line), line_rect);
                current_y += 1;
            }
        }

        // Render blocks
        for (bi, block) in msg.blocks.iter().enumerate() {
            let sel_range = block_selection_range(selection, mi, bi);

            match block {
                Block::Text { content } => {
                    let base = match msg.role {
                        neenee_core::Role::User => Style::default().fg(theme.user_fg),
                        neenee_core::Role::System => Style::default().fg(theme.system_fg),
                        _ => Style::default().fg(theme.assistant_fg),
                    };
                    let lines = wrap_text(content, chat_area.width.saturating_sub(3) as usize);
                    for wl in &lines {
                        if skip_rows > 0 {
                            skip_rows = skip_rows.saturating_sub(1);
                            continue;
                        }
                        if current_y >= chat_area.y + chat_area.height {
                            break;
                        }

                        let line = line_spans(
                            "   ",
                            Style::default(),
                            &wl.text,
                            line_selection(sel_range, wl),
                            base,
                            theme.selected_bg,
                        );
                        let line_rect = Rect::new(chat_area.x, current_y, chat_area.width, 1);
                        frame.render_widget(Paragraph::new(line), line_rect);

                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols: 3,
                            rect: line_rect,
                        });

                        current_y += 1;
                    }
                }
                Block::Code { language, content } => {
                    // Code block header
                    let lang_label = language.as_deref().unwrap_or("code");
                    let header_spans = vec![
                        Span::raw("   "),
                        Span::styled(
                            format!("╭─ {} ", lang_label),
                            Style::default().fg(theme.dim_fg),
                        ),
                    ];
                    if skip_rows > 0 {
                        skip_rows = skip_rows.saturating_sub(1);
                    } else if current_y < chat_area.y + chat_area.height {
                        let line_rect = Rect::new(chat_area.x, current_y, chat_area.width, 1);
                        frame.render_widget(Paragraph::new(Line::from(header_spans)), line_rect);
                        current_y += 1;
                    }

                    let lines = wrap_text(content, chat_area.width.saturating_sub(6) as usize);
                    for wl in &lines {
                        if skip_rows > 0 {
                            skip_rows = skip_rows.saturating_sub(1);
                            continue;
                        }
                        if current_y >= chat_area.y + chat_area.height {
                            break;
                        }

                        let base = Style::default().fg(theme.code_fg).bg(theme.code_bg);
                        let line = line_spans(
                            "   │ ",
                            Style::default().fg(theme.dim_fg),
                            &wl.text,
                            line_selection(sel_range, wl),
                            base,
                            theme.selected_bg,
                        );
                        let line_rect = Rect::new(chat_area.x, current_y, chat_area.width, 1);
                        frame.render_widget(Paragraph::new(line), line_rect);

                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols: 5,
                            rect: line_rect,
                        });

                        current_y += 1;
                    }

                    // Code block footer
                    if skip_rows > 0 {
                        skip_rows = skip_rows.saturating_sub(1);
                    } else if current_y < chat_area.y + chat_area.height {
                        let footer = Line::from(vec![Span::styled(
                            "   ╰─",
                            Style::default().fg(theme.dim_fg),
                        )]);
                        let line_rect = Rect::new(chat_area.x, current_y, chat_area.width, 1);
                        frame.render_widget(Paragraph::new(footer), line_rect);
                        current_y += 1;
                    }
                }
                Block::Heading { level, content } => {
                    let prefix = "   ".to_string();
                    let prefix_cols = display_width_u16(&prefix);
                    let modifier = if *level == 1 {
                        Modifier::BOLD | Modifier::UNDERLINED
                    } else {
                        Modifier::BOLD
                    };
                    let style = Style::default().fg(theme.heading_fg).add_modifier(modifier);
                    let continuation = " ".repeat(prefix_cols as usize);
                    let lines = wrap_text(
                        content,
                        chat_area.width.saturating_sub(prefix_cols + 1) as usize,
                    );
                    for (line_index, wl) in lines.iter().enumerate() {
                        if skip_rows > 0 {
                            skip_rows = skip_rows.saturating_sub(1);
                            continue;
                        }
                        if current_y >= chat_area.y + chat_area.height {
                            break;
                        }
                        let line = line_spans(
                            if line_index == 0 {
                                &prefix
                            } else {
                                &continuation
                            },
                            style,
                            &wl.text,
                            line_selection(sel_range, wl),
                            style,
                            theme.selected_bg,
                        );
                        let line_rect = Rect::new(chat_area.x, current_y, chat_area.width, 1);
                        frame.render_widget(Paragraph::new(line), line_rect);

                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols,
                            rect: line_rect,
                        });

                        current_y += 1;
                    }
                }
                Block::Quote { content } => {
                    let lines = wrap_text(content, chat_area.width.saturating_sub(5) as usize);
                    for wl in &lines {
                        if skip_rows > 0 {
                            skip_rows = skip_rows.saturating_sub(1);
                            continue;
                        }
                        if current_y >= chat_area.y + chat_area.height {
                            break;
                        }

                        let base = Style::default().fg(theme.quote_fg);
                        let line = line_spans(
                            "   ▎ ",
                            Style::default().fg(theme.quote_fg),
                            &wl.text,
                            line_selection(sel_range, wl),
                            base,
                            theme.selected_bg,
                        );
                        let line_rect = Rect::new(chat_area.x, current_y, chat_area.width, 1);
                        frame.render_widget(Paragraph::new(line), line_rect);

                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols: 5,
                            rect: line_rect,
                        });

                        current_y += 1;
                    }
                }
                Block::Rule => {
                    if skip_rows > 0 {
                        skip_rows = skip_rows.saturating_sub(1);
                    } else if current_y < chat_area.y + chat_area.height {
                        let width = chat_area.width.saturating_sub(6) as usize;
                        let text = format!("   {}", "─".repeat(width));
                        let line =
                            Line::from(vec![Span::styled(text, Style::default().fg(theme.dim_fg))]);
                        let line_rect = Rect::new(chat_area.x, current_y, chat_area.width, 1);
                        frame.render_widget(Paragraph::new(line), line_rect);
                        current_y += 1;
                    }
                }
                Block::Break => {
                    // Visual break, just skip a line
                    if skip_rows > 0 {
                        skip_rows = skip_rows.saturating_sub(1);
                    } else if current_y < chat_area.y + chat_area.height {
                        current_y += 1;
                    }
                }
                Block::ListItem {
                    content,
                    ordered,
                    depth,
                    checked,
                } => {
                    let marker = match (checked, ordered) {
                        (Some(true), _) => "[x]".to_string(),
                        (Some(false), _) => "[ ]".to_string(),
                        (None, Some(index)) => format!("{}.", index),
                        (None, None) => "•".to_string(),
                    };
                    let indent = "  ".repeat(*depth);
                    let prefix = format!("   {}{} ", indent, marker);
                    let prefix_cols = display_width_u16(&prefix);
                    let lines = wrap_text(
                        content,
                        chat_area.width.saturating_sub(prefix_cols + 1) as usize,
                    );
                    for wl in &lines {
                        if skip_rows > 0 {
                            skip_rows = skip_rows.saturating_sub(1);
                            continue;
                        }
                        if current_y >= chat_area.y + chat_area.height {
                            break;
                        }

                        let base = Style::default().fg(theme.assistant_fg);
                        let line = line_spans(
                            &prefix,
                            Style::default().fg(theme.accent),
                            &wl.text,
                            line_selection(sel_range, wl),
                            base,
                            theme.selected_bg,
                        );
                        let line_rect = Rect::new(chat_area.x, current_y, chat_area.width, 1);
                        frame.render_widget(Paragraph::new(line), line_rect);

                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols,
                            rect: line_rect,
                        });

                        current_y += 1;
                    }
                }
            }
        }

        // Spacing between messages
        if skip_rows > 0 {
            skip_rows = skip_rows.saturating_sub(1);
        } else if current_y < chat_area.y + chat_area.height {
            current_y += 1;
        }
    }

    chunks[2] // Return the input box area
}

fn display_width_u16(s: &str) -> u16 {
    s.width() as u16
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

/// Draw the bordered input box at the bottom of the screen (opencode-style).
pub fn draw_input(
    frame: &mut Frame,
    input: &str,
    cursor_display_x: u16,
    accent: Color,
    hint: &str,
) {
    let size = frame.size();
    let input_rect = Rect::new(0, size.height.saturating_sub(3), size.width, 3);

    let block = RtBlock::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .title_bottom(Span::styled(
            hint.to_string(),
            Style::default().fg(Color::DarkGray),
        ));
    let inner = block.inner(input_rect);
    frame.render_widget(block, input_rect);

    let line = Line::from(vec![
        Span::styled(
            "› ",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw(input),
    ]);
    frame.render_widget(Paragraph::new(line), inner);

    // Place cursor after the "› " prompt.
    frame.set_cursor(inner.x + cursor_display_x + 2, inner.y);
}

/// Draw a slash-command suggestion popup anchored above the input box.
pub fn draw_suggestions(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    suggestions: &[(&str, &str)],
    selected_idx: Option<usize>,
    anchor: Rect,
) {
    if suggestions.is_empty() {
        return;
    }

    const MAX_VISIBLE: usize = 6;
    const FOOTER_HEIGHT: u16 = 1;
    const BORDER_PAD: u16 = 2; // top + bottom borders

    let visible_count = suggestions.len().min(MAX_VISIBLE);
    let popup_height = visible_count as u16 + BORDER_PAD + FOOTER_HEIGHT;

    // Compute width from content.
    let max_cmd = suggestions
        .iter()
        .map(|(c, _)| c.width())
        .max()
        .unwrap_or(0);
    let max_desc = suggestions
        .iter()
        .map(|(_, d)| d.width())
        .max()
        .unwrap_or(0);
    let inner_width = (max_cmd + 3 + max_desc).max(30) as u16;
    let popup_width = inner_width + 4; // left + right borders + padding

    // Position: try above the input box; if not enough room, clip to top of screen.
    let mut y = anchor.y.saturating_sub(popup_height);
    if y == 0 && anchor.y < popup_height {
        // Not enough room above — clamp, let it overlap input slightly.
        y = 0;
    }
    let x = anchor
        .x
        .saturating_add(2)
        .min(frame.size().width.saturating_sub(popup_width));

    let area = Rect::new(x, y, popup_width.min(frame.size().width - x), popup_height);
    frame.render_widget(Clear, area);

    let items: Vec<ListItem> = suggestions
        .iter()
        .take(MAX_VISIBLE)
        .enumerate()
        .map(|(i, (cmd, desc))| {
            let is_selected = Some(i) == selected_idx;
            let arrow = if is_selected { "▸ " } else { "  " };
            let style = if is_selected {
                Style::default()
                    .bg(Color::Rgb(50, 50, 50))
                    .fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Gray)
            };
            ListItem::new(Line::from(vec![
                Span::styled(arrow, style),
                Span::styled(
                    format!("{:<width$} ", cmd, width = max_cmd),
                    style.add_modifier(Modifier::BOLD),
                ),
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(*desc, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let more_hint = if suggestions.len() > MAX_VISIBLE {
        format!(" … and {} more ", suggestions.len() - MAX_VISIBLE)
    } else {
        " ".to_string()
    };

    let block = RtBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title(Span::styled(
            " Commands ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            format!("{}↑↓ navigate · Enter select · Esc close", more_hint),
            Style::default().fg(Color::DarkGray),
        ));

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

/// Draw the models modal. `key_status` maps lowercase provider names to
/// whether a usable API key is available (env or config).
pub(crate) fn draw_models_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    solutions: &[crate::ModelSolution],
    current_provider: &str,
    modal_index: usize,
    key_status: &HashMap<String, bool>,
) {
    let area = centered_rect(88, 62, frame.size());
    frame.render_widget(Clear, area);

    let items: Vec<ListItem> = solutions
        .iter()
        .enumerate()
        .map(|(i, solution)| {
            let is_current = solution.id == current_provider;
            let style = if i == modal_index {
                Style::default()
                    .bg(Color::Rgb(50, 50, 50))
                    .fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Gray)
            };
            let prefix = if is_current { " ● " } else { "   " };
            let (key_label, key_style) = match key_status.get(solution.id) {
                Some(true) => ("✓ ready ", Style::default().fg(Color::Green)),
                Some(false) => ("✗ no key", Style::default().fg(Color::Red)),
                None => ("        ", Style::default()),
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    prefix,
                    if is_current {
                        Style::default().fg(Color::Green)
                    } else {
                        style
                    },
                ),
                Span::styled(
                    format!("{:<14} ", solution.name),
                    style.add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{} ", key_label), key_style),
                Span::styled(
                    format!("│ {} ", solution.model),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!("│ {}", solution.description),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let list = List::new(items).block(
        RtBlock::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Magenta))
            .title(Span::styled(
                " Select Model Solution ",
                Style::default().add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Span::styled(
                " ↑↓ navigate · Enter select/setup · k configure · Esc ",
                Style::default().fg(Color::DarkGray),
            )),
    );
    frame.render_widget(list, area);
}

pub fn draw_solution_input_modal(
    frame: &mut Frame,
    title: &str,
    help: &str,
    value: &str,
    masked: bool,
) {
    let area = centered_rect(72, 30, frame.size());
    frame.render_widget(Clear, area);
    let display = if masked {
        "•".repeat(value.chars().count())
    } else {
        value.to_string()
    };
    let block = RtBlock::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(Color::Magenta))
        .title(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            " Enter continue · Esc cancel ",
            Style::default().fg(Color::DarkGray),
        ));
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  {}", help),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  > ", Style::default().fg(Color::Gray)),
                Span::styled(display, Style::default().fg(Color::Yellow)),
                Span::styled("▏", Style::default().fg(Color::Yellow)),
            ]),
        ])
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: false }),
        area,
    );
}

/// Draw the API-key entry modal. The key itself is already masked by the caller.
pub fn draw_api_key_modal(frame: &mut Frame, provider: &str, masked_key: &str) {
    let area = centered_rect(60, 30, frame.size());
    frame.render_widget(Clear, area);

    let body = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Key: ", Style::default().fg(Color::Gray)),
            Span::styled(
                masked_key.to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("▏", Style::default().fg(Color::Yellow)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Saved to the local config file; environment variables",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "  of the same provider still take precedence.",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = RtBlock::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(Color::Magenta))
        .title(Span::styled(
            format!(" API key · {} ", provider),
            Style::default().add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            " Enter save & switch · Esc cancel ",
            Style::default().fg(Color::DarkGray),
        ));
    frame.render_widget(
        Paragraph::new(body)
            .block(block)
            .wrap(ratatui::widgets::Wrap { trim: false }),
        area,
    );
}

/// Draw the history search modal.
pub fn draw_history_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    history: &[String],
    modal_index: usize,
) {
    let area = centered_rect(80, 60, frame.size());
    frame.render_widget(Clear, area);

    let items: Vec<ListItem> = history
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let style = if i == modal_index {
                Style::default()
                    .bg(Color::Rgb(50, 50, 50))
                    .fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Gray)
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {:>3} ", i + 1),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(h, style),
            ]))
        })
        .collect();

    let list = List::new(items).block(
        RtBlock::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(Span::styled(
                " Chat History ",
                Style::default().add_modifier(Modifier::BOLD),
            )),
    );
    frame.render_widget(list, area);
}

/// Draw a blocking tool permission request.
pub fn draw_permission_modal(
    frame: &mut Frame,
    request: &PermissionRequest,
    selected: usize,
    confirm_always: bool,
) {
    let area = centered_rect(82, 62, frame.size());
    frame.render_widget(Clear, area);

    let arguments = serde_json::from_str::<serde_json::Value>(&request.arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| request.arguments.clone());
    let labels: &[&str] = if confirm_always {
        &["Confirm always", "Cancel"]
    } else {
        &["Allow once", "Always allow", "Reject"]
    };
    let options = labels
        .iter()
        .enumerate()
        .flat_map(|(index, label)| {
            let style = if index == selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(if !confirm_always && index == 2 {
                        Color::Red
                    } else {
                        Color::Yellow
                    })
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            [
                Span::raw(if index == 0 { "" } else { "  " }),
                Span::styled(format!(" {} ", label), style),
            ]
        })
        .collect::<Vec<_>>();

    let body = vec![
        Line::from(vec![
            Span::styled("Tool  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                request.tool.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Scope ", Style::default().fg(Color::DarkGray)),
            Span::styled(request.scope.clone(), Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            request.description.clone(),
            Style::default().fg(Color::Gray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Arguments",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(arguments),
        Line::from(""),
        if confirm_always {
            Line::from(Span::styled(
                "This permits the tool until neenee exits.",
                Style::default().fg(Color::Yellow),
            ))
        } else {
            Line::from("")
        },
        Line::from(options),
    ];

    let block = RtBlock::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " Permission Required ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            " ↑↓ select · Enter confirm · Esc reject ",
            Style::default().fg(Color::DarkGray),
        ));
    frame.render_widget(
        Paragraph::new(body)
            .block(block)
            .wrap(ratatui::widgets::Wrap { trim: false }),
        area,
    );
}

/// Draw a "press Ctrl+C again to exit" toast.
pub fn draw_exit_toast(frame: &mut Frame) {
    let size = frame.size();
    let text = " press Ctrl+C again to exit ";
    let width = text.len() as u16 + 4;
    let area = Rect::new(size.width.saturating_sub(width + 2), 1, width, 1);
    let line = Line::from(vec![Span::styled(
        text,
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Draw a "copied to clipboard" toast.
pub fn draw_copy_toast(frame: &mut Frame, message: &str, failed: bool) {
    let size = frame.size();
    let text = format!(" {} ", message);
    let width = text.width() as u16 + 4;
    let area = Rect::new(size.width.saturating_sub(width + 2), 1, width, 1);
    let line = Line::from(vec![Span::styled(
        text,
        Style::default()
            .fg(Color::Black)
            .bg(if failed { Color::Red } else { Color::Green })
            .add_modifier(Modifier::BOLD),
    )]);
    frame.render_widget(Paragraph::new(line), area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
