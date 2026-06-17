//! Rendering engine: draws the chat UI using ratatui while recording
//! semantic-to-screen layout information.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as RtBlock, Borders, Clear, Paragraph},
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
    // opencode-style semantic design tokens.
    /// Primary foreground text.
    pub text: Color,
    /// Muted/secondary text.
    pub text_muted: Color,
    /// Solid background for panels (modals, sheets, input).
    pub panel_bg: Color,
    /// Slightly raised background for footer/option bars.
    pub element_bg: Color,
    /// Background for menus / suggestion popups.
    pub menu_bg: Color,
    /// Dim overlay drawn behind modals to fake alpha.
    pub backdrop: Color,
    /// Brand / selection color.
    pub primary: Color,
    pub warning: Color,
    pub success: Color,
    pub info: Color,
    pub border_subtle: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user_fg: Color::Rgb(137, 180, 250),
            assistant_fg: Color::Rgb(205, 214, 244),
            error_fg: Color::Rgb(243, 139, 168),
            system_fg: Color::Rgb(127, 132, 156),
            code_fg: Color::Rgb(148, 226, 213),
            code_bg: Color::Rgb(22, 24, 35),
            heading_fg: Color::Rgb(94, 234, 212),
            quote_fg: Color::Rgb(249, 226, 175),
            dim_fg: Color::Rgb(127, 132, 156),
            selected_bg: Color::Rgb(30, 50, 70),
            header_bg: Color::Rgb(22, 24, 35),
            accent: Color::Rgb(94, 234, 212),
            // Cool palette: cyan / teal / sky — no purple-pink.
            text: Color::Rgb(205, 214, 244),
            text_muted: Color::Rgb(122, 132, 153),
            panel_bg: Color::Rgb(22, 24, 35),
            element_bg: Color::Rgb(33, 37, 54),
            menu_bg: Color::Rgb(27, 30, 44),
            backdrop: Color::Rgb(8, 9, 14),
            primary: Color::Rgb(34, 211, 238),
            warning: Color::Rgb(250, 204, 21),
            success: Color::Rgb(74, 222, 128),
            info: Color::Rgb(125, 211, 252),
            border_subtle: Color::Rgb(45, 50, 70),
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
    /// Transient running status shown inline at the end of the message stream
    /// (empty or "idle" when nothing is happening).
    pub activity: &'a str,
    pub theme: &'a Theme,
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
}

/// Render the blocks of a single message inside the given area.
///
/// This is extracted so that normal messages and tool-step cards can share
/// the same block-rendering logic while using different containing rects.
#[allow(clippy::too_many_arguments)]
fn render_message_blocks(
    frame: &mut Frame,
    area: Rect,
    msg: &ChatMessage,
    mi: usize,
    selection: &SelectionState,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
    record_layout: bool,
) {
    for (bi, block) in msg.blocks.iter().enumerate() {
        let sel_range = block_selection_range(selection, mi, bi);

        match block {
            Block::Text { content } => {
                let base = match msg.role {
                    neenee_core::Role::User => Style::default().fg(theme.user_fg),
                    neenee_core::Role::System => Style::default().fg(theme.system_fg),
                    _ => Style::default().fg(theme.assistant_fg),
                };
                let lines = wrap_text(content, area.width.saturating_sub(3) as usize);
                *content_lines += lines.len();
                for wl in &lines {
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                        continue;
                    }
                    if *current_y >= area.y + area.height {
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
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);

                    if record_layout {
                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols: 3,
                            rect: line_rect,
                        });
                    }

                    *current_y += 1;
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
                if *skip_rows > 0 {
                    *skip_rows = skip_rows.saturating_sub(1);
                } else if *current_y < area.y + area.height {
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(Line::from(header_spans)), line_rect);
                    *current_y += 1;
                }

                let lines = wrap_text(content, area.width.saturating_sub(6) as usize);
                *content_lines += 2 + lines.len();
                for wl in &lines {
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                        continue;
                    }
                    if *current_y >= area.y + area.height {
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
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);

                    if record_layout {
                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols: 5,
                            rect: line_rect,
                        });
                    }

                    *current_y += 1;
                }

                // Code block footer
                if *skip_rows > 0 {
                    *skip_rows = skip_rows.saturating_sub(1);
                } else if *current_y < area.y + area.height {
                    let footer = Line::from(vec![Span::styled(
                        "   ╰─",
                        Style::default().fg(theme.dim_fg),
                    )]);
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(footer), line_rect);
                    *current_y += 1;
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
                let lines = wrap_text(content, area.width.saturating_sub(prefix_cols + 1) as usize);
                *content_lines += lines.len();
                for (line_index, wl) in lines.iter().enumerate() {
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                        continue;
                    }
                    if *current_y >= area.y + area.height {
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
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);

                    if record_layout {
                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols,
                            rect: line_rect,
                        });
                    }

                    *current_y += 1;
                }
            }
            Block::Quote { content } => {
                let lines = wrap_text(content, area.width.saturating_sub(5) as usize);
                *content_lines += lines.len();
                for wl in &lines {
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                        continue;
                    }
                    if *current_y >= area.y + area.height {
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
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);

                    if record_layout {
                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols: 5,
                            rect: line_rect,
                        });
                    }

                    *current_y += 1;
                }
            }
            Block::Rule => {
                *content_lines += 1;
                if *skip_rows > 0 {
                    *skip_rows = skip_rows.saturating_sub(1);
                } else if *current_y < area.y + area.height {
                    let width = area.width.saturating_sub(6) as usize;
                    let text = format!("   {}", "─".repeat(width));
                    let line =
                        Line::from(vec![Span::styled(text, Style::default().fg(theme.dim_fg))]);
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);
                    *current_y += 1;
                }
            }
            Block::Break => {
                // Visual break, just skip a line
                *content_lines += 1;
                if *skip_rows > 0 {
                    *skip_rows = skip_rows.saturating_sub(1);
                } else if *current_y < area.y + area.height {
                    *current_y += 1;
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
                let lines = wrap_text(content, area.width.saturating_sub(prefix_cols + 1) as usize);
                *content_lines += lines.len();
                for wl in &lines {
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                        continue;
                    }
                    if *current_y >= area.y + area.height {
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
                    let line_rect = Rect::new(area.x, *current_y, area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);

                    if record_layout {
                        layout_map.push(BlockRegion {
                            message_idx: mi,
                            block_idx: bi,
                            start_byte: wl.start_byte,
                            end_byte: wl.end_byte,
                            text: wl.text.clone(),
                            prefix_cols,
                            rect: line_rect,
                        });
                    }

                    *current_y += 1;
                }
            }
        }
    }
}

/// Render a tool-step message as a bordered card with a summary header,
/// expandable body, and per-line scroll handling so tall cards scroll like
/// normal messages.
#[allow(clippy::too_many_arguments)]
fn render_tool_step_card(
    frame: &mut Frame,
    chat_area: Rect,
    msg: &ChatMessage,
    mi: usize,
    selection: &SelectionState,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) {
    let Some(header) = msg.tool_step_header() else {
        return;
    };

    let status_color = match &msg.kind {
        crate::document::MessageKind::ToolStep {
            output: Some(output),
            ..
        } if output.starts_with("Error") => theme.error_fg,
        crate::document::MessageKind::ToolStep {
            output: Some(_), ..
        } => theme.success,
        _ => theme.info,
    };

    let expanded = msg.tool_step_expanded() == Some(true);
    let card_width = chat_area.width.saturating_sub(6) as usize;
    if card_width < 8 {
        // Too narrow to draw a card; fall back to plain block rendering.
        render_message_blocks(
            frame,
            chat_area,
            msg,
            mi,
            selection,
            theme,
            layout_map,
            skip_rows,
            current_y,
            content_lines,
            true,
        );
        return;
    }

    // Top border with embedded header.
    let left = "   ╭─ ";
    let right = " ─";
    let corner = "╮";
    let header_width = header.width();
    let fill =
        card_width.saturating_sub(left.width() + header_width + right.width() + corner.width());
    let top = format!("{}{}{}{}{}", left, header, right, "─".repeat(fill), corner);

    let bottom = format!("   ╰{}╯", "─".repeat(card_width.saturating_sub(5)));

    // Body width is reduced by the card margins and left/right borders.
    let inner_width = chat_area.width.saturating_sub(10);

    // Top border.
    *content_lines += 1;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(1);
    } else if *current_y < chat_area.y + chat_area.height {
        let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                top.clone(),
                Style::default().fg(status_color),
            )])),
            line_rect,
        );
        // Record the header region so clicks on the card title can toggle it.
        layout_map.push(BlockRegion {
            message_idx: mi,
            block_idx: usize::MAX,
            start_byte: 0,
            end_byte: top.len(),
            text: top,
            prefix_cols: 0,
            rect: line_rect,
        });
        *current_y += 1;
    }

    // Body (only when expanded; collapsed cards show just the header bar).
    if expanded {
        for (bi, block) in msg.blocks.iter().enumerate() {
            let sel_range = block_selection_range(selection, mi, bi);
            match block {
                Block::Text { content } => {
                    let base = Style::default().fg(theme.assistant_fg);
                    let prefix = "   │ ";
                    let prefix_cols = display_width_u16(prefix);
                    let lines =
                        wrap_text(content, inner_width.saturating_sub(prefix_cols) as usize);
                    *content_lines += lines.len();
                    for wl in &lines {
                        if *skip_rows > 0 {
                            *skip_rows = skip_rows.saturating_sub(1);
                            continue;
                        }
                        if *current_y >= chat_area.y + chat_area.height {
                            break;
                        }
                        let line = line_spans(
                            prefix,
                            Style::default().fg(status_color),
                            &wl.text,
                            line_selection(sel_range, wl),
                            base,
                            theme.selected_bg,
                        );
                        let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
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
                        *current_y += 1;
                    }
                }
                Block::Code { language, content } => {
                    // Code header line.
                    let lang_label = language.as_deref().unwrap_or("code");
                    let code_header = format!("   │ {} ", lang_label);
                    *content_lines += 1;
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                    } else if *current_y < chat_area.y + chat_area.height {
                        let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
                        frame.render_widget(
                            Paragraph::new(Line::from(vec![Span::styled(
                                code_header,
                                Style::default().fg(theme.dim_fg),
                            )])),
                            line_rect,
                        );
                        *current_y += 1;
                    }

                    // Code content.
                    let prefix = "   │ ";
                    let lines = wrap_text(content, inner_width.saturating_sub(5) as usize);
                    *content_lines += lines.len();
                    for wl in &lines {
                        if *skip_rows > 0 {
                            *skip_rows = skip_rows.saturating_sub(1);
                            continue;
                        }
                        if *current_y >= chat_area.y + chat_area.height {
                            break;
                        }
                        let base = Style::default().fg(theme.code_fg).bg(theme.code_bg);
                        let line = line_spans(
                            prefix,
                            Style::default().fg(status_color),
                            &wl.text,
                            line_selection(sel_range, wl),
                            base,
                            theme.selected_bg,
                        );
                        let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
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
                        *current_y += 1;
                    }

                    // Code footer line.
                    *content_lines += 1;
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                    } else if *current_y < chat_area.y + chat_area.height {
                        let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
                        frame.render_widget(
                            Paragraph::new(Line::from(vec![Span::styled(
                                "   │",
                                Style::default().fg(status_color),
                            )])),
                            line_rect,
                        );
                        *current_y += 1;
                    }
                }
                // Tool-step bodies only contain text and code blocks.
                _ => {}
            }
        }

        // Render nested sub-agent children inside the expanded card.
        if let crate::document::MessageKind::ToolStep { children, .. } = &msg.kind {
            for child in children {
                if child.is_tool_step() {
                    render_child_tool_step(
                        frame,
                        chat_area,
                        child,
                        status_color,
                        theme,
                        skip_rows,
                        current_y,
                        content_lines,
                    );
                } else {
                    let remaining_height = chat_area
                        .y
                        .saturating_add(chat_area.height)
                        .saturating_sub(*current_y);
                    let child_area = Rect::new(
                        chat_area.x + 6,
                        *current_y,
                        chat_area.width.saturating_sub(12),
                        remaining_height,
                    );
                    render_message_blocks(
                        frame,
                        child_area,
                        child,
                        usize::MAX,
                        selection,
                        theme,
                        layout_map,
                        skip_rows,
                        current_y,
                        content_lines,
                        false,
                    );
                }
            }
        }
    }

    // Bottom border.
    *content_lines += 1;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(1);
    } else if *current_y < chat_area.y + chat_area.height {
        let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                bottom,
                Style::default().fg(status_color),
            )])),
            line_rect,
        );
        *current_y += 1;
    }
}

/// Render a nested child tool step as a compact header line plus its output.
#[allow(clippy::too_many_arguments)]
fn render_child_tool_step(
    frame: &mut Frame,
    chat_area: Rect,
    child: &ChatMessage,
    status_color: Color,
    theme: &Theme,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) {
    let Some(header) = child.tool_step_header() else {
        return;
    };

    let prefix = "   │   ⚒ ";
    let header_text = format!("{}{}", prefix, header);
    let header_lines = wrap_text(&header_text, chat_area.width.saturating_sub(3) as usize);
    *content_lines += header_lines.len();
    for wl in &header_lines {
        if *skip_rows > 0 {
            *skip_rows = skip_rows.saturating_sub(1);
            continue;
        }
        if *current_y >= chat_area.y + chat_area.height {
            break;
        }
        let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                wl.text.clone(),
                Style::default().fg(status_color),
            )])),
            line_rect,
        );
        *current_y += 1;
    }

    if let crate::document::MessageKind::ToolStep {
        output: Some(output),
        ..
    } = &child.kind
    {
        let output_prefix = "   │   ";
        let output_lines = wrap_text(output, chat_area.width.saturating_sub(9) as usize);
        *content_lines += output_lines.len();
        for wl in &output_lines {
            if *skip_rows > 0 {
                *skip_rows = skip_rows.saturating_sub(1);
                continue;
            }
            if *current_y >= chat_area.y + chat_area.height {
                break;
            }
            let line = format!("{}{}", output_prefix, wl.text);
            let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
            frame.render_widget(
                Paragraph::new(Line::from(vec![Span::styled(
                    line,
                    Style::default().fg(theme.assistant_fg),
                )])),
                line_rect,
            );
            *current_y += 1;
        }
    }
}

/// Render a thinking/reasoning message as a muted bordered card.
#[allow(clippy::too_many_arguments)]
fn render_thinking_card(
    frame: &mut Frame,
    chat_area: Rect,
    msg: &ChatMessage,
    mi: usize,
    selection: &SelectionState,
    theme: &Theme,
    layout_map: &mut LayoutMap,
    skip_rows: &mut usize,
    current_y: &mut u16,
    content_lines: &mut usize,
) {
    let Some(header) = msg.thinking_header() else {
        return;
    };
    let expanded = msg.thinking_expanded() == Some(true);
    let full_width = chat_area.width as usize;

    // Too narrow to render a padded region — fall back to plain blocks.
    if full_width < 8 {
        render_message_blocks(
            frame,
            chat_area,
            msg,
            mi,
            selection,
            theme,
            layout_map,
            skip_rows,
            current_y,
            content_lines,
            true,
        );
        return;
    }

    // Header band: a solid background-colored region (no border lines) with an
    // arrow that indicates the expand state. ▸ = collapsed, ▾ = expanded.
    let arrow = if expanded { "▾" } else { "▸" };
    let header_bg = theme.element_bg;
    let header_spans = vec![
        Span::styled(
            format!(" {} ", arrow),
            Style::default().bg(header_bg).fg(theme.info),
        ),
        Span::styled(
            format!(" {} ", header),
            Style::default()
                .bg(header_bg)
                .fg(theme.text_muted)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            padded_tail(full_width, arrow.width() + 2 + header.width() + 2),
            Style::default().bg(header_bg),
        ),
    ];
    *content_lines += 1;
    if *skip_rows > 0 {
        *skip_rows = skip_rows.saturating_sub(1);
    } else if *current_y < chat_area.y + chat_area.height {
        let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
        frame.render_widget(Paragraph::new(Line::from(header_spans)), line_rect);
        // Record the header region so clicks/Enter can toggle this card.
        layout_map.push(BlockRegion {
            message_idx: mi,
            block_idx: usize::MAX - 1,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
            prefix_cols: 0,
            rect: line_rect,
        });
        *current_y += 1;
    }

    // Body region: a subtly different background band, shown only when expanded.
    if expanded {
        let body_bg = theme.menu_bg;
        let indent = 3usize;
        let inner_width = full_width.saturating_sub(indent);
        for (bi, block) in msg.blocks.iter().enumerate() {
            if let Block::Text { content } = block {
                let lines = wrap_text(content, inner_width);
                *content_lines += lines.len();
                for wl in &lines {
                    if *skip_rows > 0 {
                        *skip_rows = skip_rows.saturating_sub(1);
                        continue;
                    }
                    if *current_y >= chat_area.y + chat_area.height {
                        break;
                    }
                    let sel_range = block_selection_range(selection, mi, bi);
                    let selected = matches!(line_selection(sel_range, wl), Some((s, e)) if s != e);
                    let used = indent + wl.text.width();
                    let line = Line::from(vec![
                        Span::styled(" ".repeat(indent), Style::default().bg(body_bg)),
                        Span::styled(
                            wl.text.clone(),
                            Style::default()
                                .bg(if selected { theme.selected_bg } else { body_bg })
                                .fg(if selected { theme.text } else { theme.text_muted }),
                        ),
                        Span::styled(
                            padded_tail(full_width, used),
                            Style::default().bg(body_bg),
                        ),
                    ]);
                    let line_rect = Rect::new(chat_area.x, *current_y, chat_area.width, 1);
                    frame.render_widget(Paragraph::new(line), line_rect);
                    layout_map.push(BlockRegion {
                        message_idx: mi,
                        block_idx: bi,
                        start_byte: wl.start_byte,
                        end_byte: wl.end_byte,
                        text: wl.text.clone(),
                        prefix_cols: indent as u16,
                        rect: line_rect,
                    });
                    *current_y += 1;
                }
            }
        }
    }
}

/// Produce a run of spaces that fills the rest of a full-width line so a
/// region reads as a solid colored band (the caller attaches the bg style).
fn padded_tail(full_width: usize, used: usize) -> String {
    " ".repeat(full_width.saturating_sub(used))
}

/// Draw the main chat area, recording layout info.
pub fn draw_chat(frame: &mut Frame, layout_map: &mut LayoutMap, view: ChatView<'_>) -> ChatRender {
    let ChatView {
        messages,
        scroll,
        selection,
        current_provider,
        current_model,
        current_mode,
        current_goal,
        activity,
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
            Constraint::Length(4),             // Input box (3) + hint line (1)
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
    ];
    if let Some(goal) = goal {
        header_spans.push(Span::raw("  "));
        header_spans.push(Span::styled(goal, Style::default().fg(theme.text_muted)));
    }
    let mut header_lines = vec![Line::from(header_spans)];
    if let Some((done, total, current)) = checklist {
        header_lines.push(Line::from(vec![
            Span::styled(
                format!("   Tasks {}/{}  ", done, total),
                Style::default()
                    .fg(theme.primary)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(current, Style::default().fg(theme.text_muted)),
        ]));
    }
    // Minimal header: no background fill, color/weight only (opencode-style).
    frame.render_widget(Paragraph::new(header_lines), chunks[0]);

    // 2. Chat History
    let chat_area = chunks[1];
    let mut current_y = chat_area.y;
    // Account for scroll
    let mut skip_rows = scroll as usize;
    // Total stream height, counted independently of the viewport clip so the
    // app loop can follow the bottom.
    let mut content_lines: usize = 0;

    for (mi, msg) in messages.iter().enumerate() {
        // Render message header (role label, opencode-style)
        let (glyph, label, role_color) = match msg.role {
            neenee_core::Role::User => ("┃", "You", theme.user_fg),
            neenee_core::Role::Assistant => ("┃", "neenee", theme.accent),
            neenee_core::Role::System => ("┃", "system", theme.system_fg),
            neenee_core::Role::Tool => ("┃", "tool", theme.quote_fg),
        };

        if !msg.is_tool_step() && !msg.is_thinking() {
            content_lines += 1;
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
        if msg.is_tool_step() {
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

        // Spacing between messages
        content_lines += 1;
        if skip_rows > 0 {
            skip_rows = skip_rows.saturating_sub(1);
        } else if current_y < chat_area.y + chat_area.height {
            current_y += 1;
        }
    }

    // Transient running status, inlined at the end of the neenee message
    // stream. Hidden while text is actively streaming ("responding") since the
    // streamed response is itself the feedback in that phase.
    if !activity.is_empty() && activity != "idle" && activity != "responding" {
        content_lines += 1;
        if skip_rows == 0 && current_y < chat_area.y + chat_area.height {
            let indicator = Line::from(vec![
                Span::styled(
                    " ┃ ",
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "neenee",
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  ⟳ {}", activity),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]);
            let line_rect = Rect::new(chat_area.x, current_y, chat_area.width, 1);
            frame.render_widget(Paragraph::new(indicator), line_rect);
        }
    }

    // The input box occupies the top 3 rows of the reserved bottom region;
    // the remaining 1 row is the hint line (rendered by the app loop).
    let input_rect = Rect::new(chunks[2].x, chunks[2].y, chunks[2].width, 3);
    ChatRender {
        input_rect,
        content_lines,
        view_height: chat_area.height,
    }
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
    input_rect: Rect,
    input: &str,
    cursor_display_x: u16,
    accent: Color,
    theme: &Theme,
) {
    // Minimal panel: a thick left accent bar + solid background, no prompt glyph.
    // The 3-row box uses the top and bottom rows as breathing room (non-input
    // padding); typing happens on the middle row with the native blinking caret.
    let block = panel_block(accent, theme.panel_bg);
    let inner = block.inner(input_rect);

    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(input, Style::default().fg(theme.text)),
    ]);
    frame.render_widget(
        Paragraph::new(vec![Line::from(""), line, Line::from("")]).block(block),
        input_rect,
    );

    // Caret sits on the middle inner row, after the leading padding space.
    frame.set_cursor(inner.x + 1 + cursor_display_x, inner.y + 1);
}

/// Draw the right-aligned hint line below the input box. Each entry is
/// `key description`, with the key highlighted and the description muted.
pub fn draw_hint(frame: &mut Frame, rect: Rect, hints: &[(&str, &str)], theme: &Theme) {
    let mut spans: Vec<Span> = Vec::new();
    let entries: Vec<String> = hints
        .iter()
        .map(|(key, desc)| format!("{} {}", key, desc))
        .collect();
    let total: usize = entries.iter().map(|s| s.width()).sum::<usize>() + (entries.len() * 3);
    let width = rect.width as usize;
    let lead = total.min(width);
    if lead < width {
        spans.push(Span::raw(" ".repeat(width - lead)));
    }
    for (i, ((key, _), text)) in hints.iter().zip(entries.iter()).enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", Style::default().fg(theme.text_muted)));
        }
        spans.push(Span::styled(
            format!("{} ", key),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        ));
        let desc_start = key.width() + 1;
        if desc_start < text.len() {
            spans.push(Span::styled(
                &text[desc_start..],
                Style::default().fg(theme.text_muted),
            ));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), rect);
}

/// Draw a slash-command suggestion popup anchored above the input box.
pub fn draw_suggestions(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    suggestions: &[(&str, &str)],
    selected_idx: Option<usize>,
    anchor: Rect,
    theme: &Theme,
) {
    if suggestions.is_empty() {
        return;
    }

    const MAX_VISIBLE: usize = 6;

    let visible_count = suggestions.len().min(MAX_VISIBLE);
    // +1 line for the bottom hint bar.
    let popup_height = visible_count as u16 + 2;

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
    let popup_width = inner_width + 4; // left bar + right padding

    // Position: try above the input box; if not enough room, clamp to top.
    let mut y = anchor.y.saturating_sub(popup_height);
    if y == 0 && anchor.y < popup_height {
        y = 0;
    }
    let x = anchor
        .x
        .saturating_add(2)
        .min(frame.size().width.saturating_sub(popup_width));

    let area = Rect::new(x, y, popup_width.min(frame.size().width - x), popup_height);
    frame.render_widget(Clear, area);

    let more_hint = if suggestions.len() > MAX_VISIBLE {
        format!(" … +{} more ", suggestions.len() - MAX_VISIBLE)
    } else {
        " ".to_string()
    };

    let block = RtBlock::default()
        .borders(Borders::LEFT)
        .border_type(ratatui::widgets::BorderType::Thick)
        .border_style(Style::default().fg(theme.primary))
        .style(Style::default().bg(theme.menu_bg));

    // Custom list: render the block, then items inside, plus a title line.
    let title = Line::from(vec![Span::styled(
        " Commands",
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    )]);

    let mut lines: Vec<Line> = vec![title];
    lines.extend(
        suggestions
            .iter()
            .take(MAX_VISIBLE)
            .enumerate()
            .map(|(i, (cmd, desc))| {
                let is_selected = Some(i) == selected_idx;
                let style = if is_selected {
                    Style::default()
                        .bg(theme.primary)
                        .fg(contrast_fg(theme.primary))
                } else {
                    Style::default().fg(theme.text)
                };
                let cmd_style = if is_selected {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
                };
                Line::from(vec![
                    Span::styled(" ", Style::default()),
                    Span::styled(format!("{:<width$} ", cmd, width = max_cmd), cmd_style),
                    Span::styled("· ", Style::default().fg(theme.text_muted)),
                    Span::styled(
                        *desc,
                        if is_selected {
                            style
                        } else {
                            Style::default().fg(theme.text_muted)
                        },
                    ),
                ])
            }),
    );
    lines.push(Line::from(Span::styled(
        format!("{}↑↓ navigate · Enter select · Esc close", more_hint),
        Style::default().fg(theme.text_muted),
    )));

    frame.render_widget(Paragraph::new(lines).block(block), area);
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
    theme: &Theme,
) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(72, 60, frame.size());
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![Line::from(vec![Span::styled(
        " Select Model Solution",
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    )])];

    for (i, solution) in solutions.iter().enumerate() {
        let is_current = solution.id == current_provider;
        let is_selected = i == modal_index;
        let row_bg = if is_selected {
            theme.primary
        } else {
            theme.panel_bg
        };
        let row_fg = if is_selected {
            contrast_fg(theme.primary)
        } else {
            theme.text
        };
        let base = Style::default().bg(row_bg).fg(row_fg);
        let (key_label, key_color) = match key_status.get(solution.id) {
            Some(true) => ("✓ ready", theme.success),
            Some(false) => ("✗ no key", theme.error_fg),
            None => ("", row_fg),
        };
        let key_style = if is_selected {
            Style::default().bg(row_bg).fg(contrast_fg(theme.primary))
        } else {
            Style::default().fg(key_color)
        };
        let prefix = if is_current { "● " } else { "  " };
        let dim = if is_selected {
            Style::default().bg(row_bg).fg(contrast_fg(theme.primary))
        } else {
            Style::default().fg(theme.text_muted)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", prefix), dim),
            Span::styled(
                format!("{:<14} ", solution.name),
                base.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{:<9} ", key_label), key_style),
            Span::styled(format!("{} ", solution.model), dim),
            Span::styled(format!("· {}", solution.description), dim),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑↓ navigate · Enter select/setup · k configure · Esc ",
        Style::default().fg(theme.text_muted),
    )));

    let block = panel_block(theme.primary, theme.panel_bg);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

pub fn draw_solution_input_modal(
    frame: &mut Frame,
    title: &str,
    help: &str,
    value: &str,
    masked: bool,
    theme: &Theme,
) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(60, 30, frame.size());
    frame.render_widget(Clear, area);
    let display = if masked {
        "•".repeat(value.chars().count())
    } else {
        value.to_string()
    };
    let lines = vec![
        Line::from(Span::styled(
            format!(" {}", title),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(" {}", help),
            Style::default().fg(theme.text_muted),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.primary)),
            Span::styled(display, Style::default().fg(theme.text)),
            Span::styled("▏", Style::default().fg(theme.primary)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Enter continue · Esc cancel ",
            Style::default().fg(theme.text_muted),
        )),
    ];

    let block = panel_block(theme.primary, theme.panel_bg);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw the API-key entry modal. The key itself is already masked by the caller.
pub fn draw_api_key_modal(frame: &mut Frame, provider: &str, masked_key: &str, theme: &Theme) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(56, 34, frame.size());
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::from(Span::styled(
            format!(" API key · {}", provider),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(" Key  ", Style::default().fg(theme.text_muted)),
            Span::styled(
                masked_key.to_string(),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ),
            Span::styled("▏", Style::default().fg(theme.primary)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Saved to the local config file; environment",
            Style::default().fg(theme.text_muted),
        )),
        Line::from(Span::styled(
            " variables of the same provider still win.",
            Style::default().fg(theme.text_muted),
        )),
        Line::from(""),
        Line::from(Span::styled(
            " Enter save & switch · Esc cancel ",
            Style::default().fg(theme.text_muted),
        )),
    ];

    let block = panel_block(theme.primary, theme.panel_bg);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw the history search modal.
pub fn draw_history_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    history: &[String],
    modal_index: usize,
    theme: &Theme,
) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(70, 55, frame.size());
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        " Chat History",
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    ))];

    for (i, h) in history.iter().enumerate() {
        let is_selected = i == modal_index;
        let bg = if is_selected {
            theme.primary
        } else {
            theme.panel_bg
        };
        let fg = if is_selected {
            contrast_fg(theme.primary)
        } else {
            theme.text
        };
        let num_style = if is_selected {
            Style::default().bg(bg).fg(contrast_fg(theme.primary))
        } else {
            Style::default().fg(theme.text_muted)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {:>3} ", i + 1), num_style),
            Span::styled(h, Style::default().bg(bg).fg(fg)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑↓ navigate · Enter insert · Esc close ",
        Style::default().fg(theme.text_muted),
    )));

    let block = panel_block(theme.primary, theme.panel_bg);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw a blocking tool permission request as a bottom-anchored sheet
/// (opencode-style): dimmed backdrop above, a panel with a warning-colored
/// left bar, the tool/scope/arguments detail, and a footer bar of inline
/// options where the selected one is highlighted.
pub fn draw_permission_sheet(
    frame: &mut Frame,
    request: &PermissionRequest,
    selected: usize,
    confirm_always: bool,
    theme: &Theme,
) {
    let size = frame.size();
    let input_h: u16 = 3;
    let bottom = size.height.saturating_sub(input_h);

    let arguments = serde_json::from_str::<serde_json::Value>(&request.arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| request.arguments.clone());
    let arg_lines: Vec<Line> = arguments.lines().map(Line::from).collect();

    let labels: &[&str] = if confirm_always {
        &["Confirm always", "Cancel"]
    } else {
        &["Allow once", "Always allow", "Reject"]
    };

    // Build the option footer as a single full-width element_bg line.
    let mut footer_spans: Vec<Span> = vec![Span::styled(
        " ".to_string(),
        Style::default().bg(theme.element_bg),
    )];
    for (index, label) in labels.iter().enumerate() {
        let is_reject = !confirm_always && index == 2;
        let is_selected = index == selected;
        let bg = if is_selected {
            if is_reject {
                theme.error_fg
            } else {
                theme.warning
            }
        } else {
            theme.element_bg
        };
        let fg = if is_selected {
            contrast_fg(bg)
        } else {
            theme.text
        };
        if index > 0 {
            footer_spans.push(Span::styled("  ", Style::default().bg(theme.element_bg)));
        }
        footer_spans.push(Span::styled(
            format!(" {} ", label),
            Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
        ));
    }
    // trailing hint, right-aligned-ish (just appended after options).
    footer_spans.push(Span::styled(
        "   ←→ select · enter confirm · esc reject ",
        Style::default().bg(theme.element_bg).fg(theme.text_muted),
    ));
    // Pad footer to the full width so the element bar spans the inner area.
    let inner_width = size.width.saturating_sub(1) as usize; // minus left bar
    let used: usize = footer_spans.iter().map(|s| s.content.width()).sum();
    if used < inner_width {
        footer_spans.push(Span::styled(
            " ".repeat(inner_width - used),
            Style::default().bg(theme.element_bg),
        ));
    }

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("△ ", Style::default().fg(theme.warning)),
        Span::styled(
            "Permission required",
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" Tool  ", Style::default().fg(theme.text_muted)),
        Span::styled(
            request.tool.clone(),
            Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("   Scope ", Style::default().fg(theme.text_muted)),
        Span::styled(request.scope.clone(), Style::default().fg(theme.info)),
    ]));
    lines.push(Line::from(Span::styled(
        format!(" {}", request.description),
        Style::default().fg(theme.text),
    )));
    lines.push(Line::from(Span::styled(
        "Arguments",
        Style::default().fg(theme.info).add_modifier(Modifier::BOLD),
    )));
    lines.extend(arg_lines);
    if confirm_always {
        lines.push(Line::from(Span::styled(
            " This permits the tool until neenee exits.",
            Style::default().fg(theme.warning),
        )));
    }
    lines.push(Line::from("")); // spacer
    lines.push(Line::from(footer_spans));

    // Cap height and anchor to the bottom (just above the input box).
    let max_h: u16 = 16;
    let sheet_h = (lines.len() as u16).min(max_h).min(bottom);
    let sheet_top = bottom.saturating_sub(sheet_h);

    // Dim the chat area above the sheet; leave the input visible below.
    if sheet_top > 0 {
        draw_dim_backdrop(
            frame,
            Rect::new(0, 0, size.width, sheet_top),
            theme.backdrop,
        );
    }
    let area = Rect::new(0, sheet_top, size.width, sheet_h);
    frame.render_widget(Clear, area);

    let block = panel_block(theme.warning, theme.panel_bg);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .block(block),
        area,
    );
}

/// Draw a "press Ctrl+C again to exit" toast.
pub fn draw_exit_toast(frame: &mut Frame, theme: &Theme) {
    let size = frame.size();
    let text = "press Ctrl+C again to exit";
    toast(frame, theme, text, theme.warning, size.width);
}

/// Draw the help / keybindings modal.
pub fn draw_help_modal(frame: &mut Frame, theme: &Theme) {
    draw_dim_backdrop(frame, frame.size(), theme.backdrop);
    let area = centered_rect(58, 70, frame.size());
    frame.render_widget(Clear, area);

    let key = |k: &str| {
        Span::styled(
            format!("{:<10}", k),
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )
    };
    let desc = |d: &str| Span::styled(d.to_string(), Style::default().fg(theme.text_muted));
    let section = |title: &str| {
        Span::styled(
            title.to_string(),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        )
    };
    let row = |k: &str, d: &str| Line::from(vec![key(k), desc(d)]);

    let lines = vec![
        Line::from(Span::styled(
            " Help",
            Style::default()
                .fg(theme.primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(section("General")),
        row("ctrl+p", "command palette"),
        row("enter", "send message"),
        row("esc", "interrupt / close"),
        row("ctrl+c", "copy · interrupt · quit (×2)"),
        row("↑ / ↓", "history · navigate"),
        row("tab", "accept suggestion"),
        Line::from(""),
        Line::from(section("Views & tools")),
        row("ctrl+h", "this help"),
        row("ctrl+m", "switch model"),
        row("ctrl+r", "search history"),
        row("ctrl+t", "toggle tool steps"),
        row("/", "slash commands"),
        Line::from(""),
        Line::from(section("Modes")),
        row("/mode", "build · plan"),
        row("/goal", "set a persistent goal"),
        row("/loop N", "bounded autonomous work"),
        Line::from(""),
        Line::from(desc("Drag to select · Ctrl+C or Ctrl+Shift+C to copy.")),
        Line::from(""),
        Line::from(Span::styled(
            " esc · close ",
            Style::default().fg(theme.text_muted),
        )),
    ];

    let block = panel_block(theme.primary, theme.panel_bg);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Draw a "copied to clipboard" toast.
pub fn draw_copy_toast(frame: &mut Frame, message: &str, failed: bool, theme: &Theme) {
    let size = frame.size();
    let color = if failed {
        theme.error_fg
    } else {
        theme.success
    };
    toast(frame, theme, message, color, size.width);
}

/// opencode-style toast: top-right panel with variant-colored left/right bars.
fn toast(frame: &mut Frame, theme: &Theme, message: &str, color: Color, width: u16) {
    let text = format!(" {} ", message.trim());
    // Inner width (text) capped, plus the two border columns.
    let inner_w = text.width() as u16;
    let toast_width = inner_w.min(58) + 2;
    let x = width.saturating_sub(toast_width).saturating_sub(2).max(1);
    let area = Rect::new(x, 1, toast_width, 3);

    let block = RtBlock::default()
        .borders(Borders::LEFT | Borders::RIGHT)
        .border_type(ratatui::widgets::BorderType::Thick)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(theme.panel_bg));

    let line = Line::from(Span::styled(
        text,
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    ));
    // Vertically center the single line within the 3-row panel.
    let para = Paragraph::new(vec![Line::from(""), line]);
    frame.render_widget(para.block(block), area);
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

/// Fake an alpha backdrop by filling an area with a dim solid color.
fn draw_dim_backdrop(frame: &mut Frame, area: Rect, color: Color) {
    frame.render_widget(RtBlock::default().style(Style::default().bg(color)), area);
}

/// A borderless panel with a single thick colored left bar (opencode-style).
fn panel_block(bar_color: Color, bg: Color) -> RtBlock<'static> {
    RtBlock::default()
        .borders(Borders::LEFT)
        .border_type(ratatui::widgets::BorderType::Thick)
        .border_style(Style::default().fg(bar_color))
        .style(Style::default().bg(bg))
}

/// Contrast foreground for a colored background (dark text on light fills).
fn contrast_fg(bg: Color) -> Color {
    let (r, g, b) = rgb(bg);
    let luminance = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if luminance > 140.0 {
        Color::Black
    } else {
        Color::White
    }
}

fn rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (224, 108, 117),
        Color::Green => (127, 216, 143),
        Color::Yellow => (229, 192, 123),
        Color::Blue => (137, 180, 250),
        Color::Magenta => (203, 166, 247),
        Color::Cyan => (86, 182, 194),
        Color::Gray => (128, 128, 128),
        Color::DarkGray => (64, 64, 64),
        Color::LightGreen => (127, 216, 143),
        Color::LightRed => (243, 139, 168),
        _ => (128, 128, 128),
    }
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
                let messages = vec![
                    ChatMessage::new(neenee_core::Role::User, "hi"),
                    thinking,
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
                        theme: &theme,
                    },
                );
                draw_input(f, Rect::new(0, 21, 80, 3), "hello", 5, theme.accent, &theme);
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
                draw_exit_toast(f, &theme);
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
                draw_permission_sheet(f, &request, 0, false, &theme);
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
