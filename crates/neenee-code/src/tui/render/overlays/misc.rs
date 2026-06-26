//! History search, tool-step detail, help, plan preview, and toast modals.

use neenee_tui::{
    Block as RtBlock, Borders, Clear, Color, Frame, Modifier, Paragraph, Rect, Style, {Line, Span},
};

use super::common::caret_column;
use crate::tui::layout::LayoutMap;
use crate::tui::render::Theme;
use crate::tui::render::primitives::{
    centered_rect, contrast_fg, modal_frame, panel_block, panel_inner, render_body, viewport_rect,
};
use unicode_width::UnicodeWidthStr;

/// Draw the history search modal.
///
/// `query` is the fuzzy query the user is typing into the (borrowed) input
/// box; `ranked` is the pre-computed `(original_history_index, FuzzyMatch)`
/// list produced by [`crate::tui::App::history_filtered`] — passing it in avoids a
/// second fuzzy pass per frame. `modal_index` selects into `ranked`.
///
/// Each result line highlights the matched characters of the query so the
/// user can see why an entry surfaced. Empty query → show everything with no
/// highlights; query with no matches → "no matches" placeholder.
#[allow(clippy::too_many_arguments)]
pub fn draw_history_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    history: &[String],
    query: &str,
    cursor_position: usize,
    ranked: &[(usize, crate::tui::fuzzy::FuzzyMatch)],
    modal_index: usize,
    theme: &Theme,
) {
    let area = centered_rect(70, 55, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

    let header_rect = f.header;
    if let Some(h) = header_rect {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "Input History",
                    Style::default()
                        .fg(theme.brand())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled("❯ ", Style::default().fg(theme.muted())),
                Span::styled(
                    if query.is_empty() {
                        "type to fuzzy-filter"
                    } else {
                        query
                    },
                    Style::default()
                        .fg(if query.is_empty() {
                            theme.muted()
                        } else {
                            theme.fg()
                        })
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            h,
        );
    }

    let mut body: Vec<Line> = Vec::new();
    if history.is_empty() {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            " (no history yet — send a message to populate this list)",
            Style::default().fg(theme.muted()),
        )));
    } else if ranked.is_empty() {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            " (no matches — try a shorter or different query)",
            Style::default().fg(theme.muted()),
        )));
    } else {
        for (row, (orig_idx, m)) in ranked.iter().enumerate() {
            let is_selected = row == modal_index;
            let bg = if is_selected {
                theme.brand()
            } else {
                theme.panel()
            };
            let fg = if is_selected {
                contrast_fg(theme.brand())
            } else {
                theme.fg()
            };
            let num_style = if is_selected {
                Style::default().bg(bg).fg(contrast_fg(theme.brand()))
            } else {
                Style::default().fg(theme.muted())
            };
            let base_style = Style::default().bg(bg).fg(fg);
            let matched_style = if is_selected {
                Style::default()
                    .bg(bg)
                    .fg(contrast_fg(theme.brand()))
                    .add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default()
                    .bg(bg)
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD)
            };

            let entry = history.get(*orig_idx).map(String::as_str).unwrap_or("");
            let mut spans: Vec<Span> = Vec::with_capacity(entry.chars().count() + 1);
            spans.push(Span::styled(format!(" {:>3} ", row + 1), num_style));
            let matched: std::collections::HashSet<usize> = m.positions.iter().copied().collect();
            for (char_idx, c) in entry.chars().enumerate() {
                let style = if matched.contains(&char_idx) {
                    matched_style
                } else {
                    base_style
                };
                spans.push(Span::styled(c.to_string(), style));
            }
            body.push(Line::from(spans));
        }
    }
    render_body(frame, f.body, body, &mut 0, Some(modal_index), false, theme);

    if let Some(fo) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "type to filter · ↑↓ navigate · Enter insert · Esc close",
                Style::default().fg(theme.muted()),
            ))),
            fo,
        );
    }

    // Place the real terminal caret in the filter field (the header row, after
    // the `Input History  ❯ ` prefix). The composer underneath is skipped for
    // this modal, so without this the caret would be absent.
    if let Some(h) = header_rect {
        let prefix = "Input History  ❯ ".width() as u16;
        let cursor_x = h.x + prefix + caret_column(query, cursor_position);
        let cursor_y = h.y;
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

pub fn draw_armed_toast(frame: &mut Frame, message: &str, theme: &Theme) {
    let size = frame.area();
    toast(frame, theme, message, theme.warn(), size.width);
}

/// Draw the help / keybindings modal.
/// Full-output detail overlay for a focused tool step (ADR-0001 Step 8). Shows
/// the step's complete output in a centered, scrollable panel so a long result
/// can be inspected without scrolling the whole transcript. Shell output is
/// broken into `$ command`, stdout, stderr (in `error_fg`), and an exit footer
pub fn draw_tool_step_detail_overlay(
    frame: &mut Frame,
    msg: &crate::tui::document::TranscriptMessage,
    scroll: u16,
    theme: &Theme,
) {
    use crate::tui::document::MessageKind;
    let area = centered_rect(92, 84, viewport_rect(frame));
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(summary) = msg.tool_step_summary() {
        lines.push(Line::from(Span::styled(
            summary,
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    }

    let body_style = Style::default().fg(theme.fg());
    let stderr_style = Style::default().fg(theme.err());
    let marker_style = Style::default()
        .fg(theme.warn())
        .add_modifier(Modifier::BOLD);
    match &msg.kind {
        MessageKind::ToolStep { structured, .. }
            if matches!(
                structured.as_deref(),
                Some(neenee_core::ToolOutput::Shell { .. })
            ) =>
        {
            let MessageKind::ToolStep { structured, .. } = &msg.kind else {
                unreachable!()
            };
            let neenee_core::ToolOutput::Shell {
                command,
                stdout,
                stderr,
                exit,
                truncated,
            } = structured.as_deref().expect("guarded by match guard")
            else {
                unreachable!()
            };
            if !command.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("$ {}", command),
                    Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
                )));
            }
            for line in stdout.trim_end_matches(&['\r', '\n'][..]).split('\n') {
                lines.push(Line::from(Span::styled(line.to_string(), body_style)));
            }
            if !stderr.is_empty() {
                for line in stderr.trim_end_matches(&['\r', '\n'][..]).split('\n') {
                    lines.push(Line::from(Span::styled(line.to_string(), stderr_style)));
                }
            }
            if *truncated {
                lines.push(Line::from(Span::styled(
                    "[output truncated]".to_string(),
                    marker_style,
                )));
            }
            if let Some(code) = exit.filter(|c| *c != 0) {
                lines.push(Line::from(Span::styled(
                    format!("exit {}", code),
                    marker_style,
                )));
            }
        }
        MessageKind::ToolStep {
            output: Some(output),
            ..
        } => {
            for line in output.split('\n') {
                lines.push(Line::from(Span::styled(line.to_string(), body_style)));
            }
        }
        _ => {}
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " ↑/↓ or wheel scroll · esc close ",
        Style::default().fg(theme.muted()),
    )));

    // Paint the panel chrome (bg + brand `┃` left bar) bare, then render the
    // content into `panel_inner` so a long line reserves the bar's mirrored
    // right gutter instead of running into the panel's right edge — the same
    // symmetric-inset contract the permission sheet and `modal_frame` use.
    frame.render_widget(panel_block(theme.brand(), theme.panel()), area);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll(scroll, 0)
            .wrap(neenee_tui::Wrap { trim: false }),
        panel_inner(area),
    );
}

pub fn draw_help_modal(frame: &mut Frame, scroll: &mut usize, theme: &Theme) {
    let area = centered_rect(58, 70, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Help",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    let key = |k: &str| {
        Span::styled(
            format!("{:<10}", k),
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        )
    };
    let desc = |d: &str| Span::styled(d.to_string(), Style::default().fg(theme.muted()));
    let section = |title: &str| {
        Span::styled(
            title.to_string(),
            Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
        )
    };
    let row = |k: &str, d: &str| Line::from(vec![key(k), desc(d)]);

    let body = vec![
        Line::from(section("General")),
        row("ctrl+p", "command palette"),
        row("enter", "send message"),
        row("alt+enter", "insert newline (ctrl+j)"),
        row("esc", "interrupt (×2) / close"),
        row("ctrl+c", "copy · clear input · quit (×2)"),
        Line::from(""),
        Line::from(section("Line editing")),
        row("ctrl+a / ctrl+e", "caret to line start / end"),
        row("ctrl+b", "move back one char (←)"),
        row("home / end", "caret to line start / end"),
        row("ctrl+u / ctrl+k", "delete to line start / end"),
        row("ctrl+w", "delete previous word"),
        row("alt+backspace", "delete previous word"),
        row("alt+d", "delete next word"),
        row("ctrl+← / ctrl+→", "move word back / forward"),
        row("alt+b / alt+f", "move word back / forward"),
        Line::from(""),
        Line::from(section("Transcript focus")),
        Line::from(desc(
            "No modes: typing always lands in the prompt. Ctrl+↑/↓ highlights",
        )),
        Line::from(desc(
            "a step; the highlight tells you which keys act on it.",
        )),
        row("ctrl+↑ / ctrl+↓", "focus a step (nearest first)"),
        row("↑ / ↓", "while focused: cycle steps"),
        row("enter", "open the focused step"),
        row("esc", "clear the focus"),
        Line::from(""),
        Line::from(section("Views & tools")),
        row("ctrl+h", "this help"),
        row("/session", "session context"),
        row("ctrl+m", "switch model"),
        row("ctrl+r", "search history"),
        row("ctrl+t", "toggle tool steps"),
        row("/", "slash commands"),
        Line::from(""),
        Line::from(section("Modes")),
        row("/pursue", "pursue a pursuit until it is met"),
        Line::from(""),
        Line::from(desc("Drag to select · Ctrl+C or Ctrl+Shift+C to copy.")),
    ];
    render_body(frame, f.body, body, scroll, None, true, theme);

    if let Some(fo) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "↑/↓ scroll · esc close",
                Style::default().fg(theme.muted()),
            ))),
            fo,
        );
    }
}

pub fn draw_copy_toast(frame: &mut Frame, message: &str, failed: bool, theme: &Theme) {
    let size = frame.area();
    let color = if failed { theme.err() } else { theme.ok() };
    toast(frame, theme, message, color, size.width);
}

pub(crate) fn toast(frame: &mut Frame, theme: &Theme, message: &str, color: Color, width: u16) {
    let text = format!(" {} ", message.trim());
    // Inner width (text) capped, plus the two border columns.
    let inner_w = text.width() as u16;
    let toast_width = inner_w.min(58) + 2;
    let x = width.saturating_sub(toast_width).saturating_sub(2).max(1);
    let area = Rect::new(x, 1, toast_width, 3);

    let block = RtBlock::default()
        .borders(Borders::LEFT | Borders::RIGHT)
        .border_type(neenee_tui::BorderType::Thick)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(theme.panel()));

    let line = Line::from(Span::styled(
        text,
        Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
    ));
    // Vertically center the single line within the 3-row panel.
    let para = Paragraph::new(vec![Line::from(""), line]);
    frame.render_widget(para.block(block), area);
}
