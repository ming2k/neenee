//! Permission sheet (inline) and question modal.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block as RtBlock, Clear, Paragraph, Wrap},
};

use neenee_core::{PermissionRequest, UserQuestionRequest};

use crate::tui::render::Theme;
use crate::tui::render::primitives::{
    centered_rect, contrast_fg, modal_frame, panel_block, viewport_rect,
};
use unicode_width::UnicodeWidthStr;

// The permission sheet renders inline, replacing the composer (input box)
// area. Collapsed it shows a one-line summary plus the action footer;
// expanding "Details" grows the body upward into the transcript.
const PERMISSION_H_PADDING: u16 = 2;
const PERMISSION_TOP_PADDING: u16 = 1;
const PERMISSION_FOOTER_HEIGHT: u16 = 1;
const PERMISSION_BODY_FOOTER_GAP: u16 = 1;
/// Max body rows in the collapsed (summary-only) state.
const PERMISSION_COLLAPSED_BODY_CAP: u16 = 2;
/// Max body rows when "Details" is expanded; the rest is scrollable.
const PERMISSION_MAX_BODY_ROWS: u16 = 14;

/// options; the user navigates with ↑/↓, selects with Space or Enter, and
/// submits with Enter. Multi-select questions use checkboxes; single-select
/// uses radio buttons. A numbered digit key (1-9) jumps directly to an option.
const OTHER_OPTION_LABEL: &str = "Other";
const OTHER_OPTION_PLACEHOLDER: &str = "Type your own answer";

pub fn draw_question_modal(
    frame: &mut Frame,
    request: &UserQuestionRequest,
    current_question: usize,
    selected: &[Vec<usize>],
    other_text: &[String],
    highlighted: usize,
    theme: &Theme,
) {
    let area = centered_rect(78, 70, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

    let question = request.questions.get(current_question);
    let total = request.questions.len();

    if let Some(h) = f.header {
        let title = if total > 1 {
            format!("Question {}/{}", current_question + 1, total)
        } else {
            "Question".to_string()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                title,
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    let mut body: Vec<Line> = Vec::new();
    if let Some(q) = question {
        if let Some(header) = &q.header {
            body.push(Line::from(vec![Span::styled(
                format!(" {}", header),
                Style::default()
                    .fg(theme.info())
                    .add_modifier(Modifier::BOLD),
            )]));
        }
        body.push(Line::from(vec![Span::styled(
            format!(" {}", q.question),
            Style::default().fg(theme.fg()),
        )]));
        body.push(Line::from(""));

        let q_selected = selected.get(current_question);
        let other_index = q.options.len();
        let other_highlighted = highlighted == other_index;
        let other_text_value = other_text
            .get(current_question)
            .map(String::as_str)
            .unwrap_or("");

        for (i, option) in q.options.iter().enumerate() {
            let is_selected = q_selected.is_some_and(|s| s.contains(&i));
            let is_highlighted = i == highlighted;
            render_question_option(
                &mut body,
                i,
                &option.label,
                option.description.as_deref(),
                is_selected,
                is_highlighted,
                q.multi_select,
                theme,
            );
        }

        render_question_option(
            &mut body,
            other_index,
            OTHER_OPTION_LABEL,
            None,
            q_selected.is_some_and(|s| s.contains(&other_index)),
            other_highlighted,
            q.multi_select,
            theme,
        );
        if other_highlighted {
            let display = if other_text_value.is_empty() {
                OTHER_OPTION_PLACEHOLDER
            } else {
                other_text_value
            };
            body.push(Line::from(vec![
                Span::styled("   ", Style::default().fg(theme.fg())),
                Span::styled(
                    format!("{} {}", "▏", display),
                    Style::default()
                        .fg(if other_text_value.is_empty() {
                            theme.muted()
                        } else {
                            theme.fg()
                        })
                        .add_modifier(Modifier::UNDERLINED),
                ),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), f.body);

    if let Some(fo) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "↑↓ navigate · Space toggle · 1-9 jump · Enter submit · Esc cancel",
                Style::default().fg(theme.muted()),
            ))),
            fo,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn render_question_option(
    lines: &mut Vec<Line>,
    index: usize,
    label: &str,
    description: Option<&str>,
    is_selected: bool,
    is_highlighted: bool,
    multi_select: bool,
    theme: &Theme,
) {
    let marker = if multi_select {
        if is_selected { "[x]" } else { "[ ]" }
    } else {
        if is_selected { "●" } else { "○" }
    };
    let number = if index < 9 {
        format!("{}.", index + 1)
    } else {
        " ".to_string()
    };
    let focus_style = if is_highlighted {
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.muted())
    };
    let marker_style = if is_selected {
        Style::default().fg(theme.ok()).add_modifier(Modifier::BOLD)
    } else {
        focus_style
    };
    let text_style = if is_highlighted {
        Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg())
    };
    let focus = if is_highlighted { "❯" } else { " " };

    let label_line = Line::from(vec![
        Span::styled(format!("{} {:>2} ", focus, number), focus_style),
        Span::styled(format!("{} ", marker), marker_style),
        Span::styled(label.to_string(), text_style),
    ]);
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines.push(label_line);

    if let Some(desc) = description {
        let desc_style = Style::default().fg(theme.dim());
        let indent = if multi_select { "         " } else { "       " };
        lines.push(Line::from(vec![
            Span::styled(indent.to_string(), desc_style),
            Span::styled(desc.to_string(), desc_style),
        ]));
    }
}

/// Draw a blocking tool permission request inline, replacing the composer
/// (input box) area. The transcript above stays visible and scrollable.
///
/// Collapsed (the default) the sheet is a one-line summary — the tool name
/// plus its scope (the specific path/command being touched) — followed by a
/// footer of inline actions. Selecting "Details" expands the body upward to
/// reveal the full description and arguments without leaving the prompt.
#[allow(clippy::too_many_arguments)]
pub fn draw_permission_sheet(
    frame: &mut Frame,
    request: &PermissionRequest,
    selected: usize,
    confirm_always: bool,
    show_details: bool,
    scroll: usize,
    input_rect: Rect,
    theme: &Theme,
) -> usize {
    let area_bottom = input_rect.y + input_rect.height;

    let arguments = serde_json::from_str::<serde_json::Value>(&request.arguments)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| request.arguments.clone());
    let scope_meaningful = !request.scope.is_empty() && request.scope != "*";

    // Header line: human-friendly label (falling back to the raw tool name
    // for safety), plus the concrete scope (path/command) when it adds
    // information. The scope is the single most useful detail, so it earns a
    // spot in the collapsed summary; everything else hides behind "Details".
    // The left bar carries the severity cue.
    let label = if request.label.is_empty() {
        request.tool.clone()
    } else {
        request.label.clone()
    };
    let mut header = vec![Span::styled(
        label,
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD),
    )];
    if confirm_always {
        header.push(Span::styled(
            " — always allow until exit?",
            Style::default().fg(theme.fg()),
        ));
    } else if scope_meaningful {
        header.push(Span::styled("  ", Style::default()));
        header.push(Span::styled(
            request.scope.clone(),
            Style::default().fg(theme.info()),
        ));
    }

    let mut body_lines: Vec<Line> = Vec::new();
    body_lines.push(Line::from(header));

    if confirm_always {
        body_lines.push(Line::from(Span::styled(
            "Grants this tool until neenee exits.",
            Style::default().fg(theme.muted()),
        )));
    } else if show_details {
        body_lines.push(Line::from(""));
        body_lines.push(Line::from(Span::styled(
            request.description.clone(),
            Style::default().fg(theme.fg()),
        )));
        body_lines.push(Line::from(""));
        body_lines.push(Line::from(Span::styled(
            "Arguments",
            Style::default()
                .fg(theme.info())
                .add_modifier(Modifier::BOLD),
        )));
        body_lines.extend(
            arguments
                .lines()
                .map(|line| Line::from(line).style(Style::default().fg(theme.code_text()))),
        );
    }

    let fixed = PERMISSION_TOP_PADDING + PERMISSION_BODY_FOOTER_GAP + PERMISSION_FOOTER_HEIGHT;
    let content_w = input_rect
        .width
        .saturating_sub(1 + 2 * PERMISSION_H_PADDING)
        .max(1);
    let body_total_rows: usize = body_lines
        .iter()
        .map(|line| {
            let width: usize = line.spans.iter().map(|span| span.content.width()).sum();
            width.max(1).div_ceil(content_w as usize)
        })
        .sum();

    // How tall the body may grow. Collapsed stays compact; expanded climbs
    // into the transcript but never past the top of the viewport.
    let body_cap: u16 = if confirm_always {
        body_total_rows.min(2).min(u16::MAX as usize) as u16
    } else if show_details {
        let room = area_bottom.saturating_sub(fixed).max(1);
        PERMISSION_MAX_BODY_ROWS.min(room)
    } else {
        PERMISSION_COLLAPSED_BODY_CAP
    };
    let body_h = (body_total_rows as u16).min(body_cap);
    let max_scroll = body_total_rows.saturating_sub(body_h as usize);
    let body_scroll = scroll.min(max_scroll);

    let desired_h = fixed + body_h;
    // Fill the composer slot when collapsed (so it reads as a drop-in
    // replacement for the input box); grow above it when expanded.
    let sheet_h = desired_h.max(input_rect.height).min(area_bottom).max(1);
    let sheet_top = area_bottom.saturating_sub(sheet_h);
    let area = Rect::new(input_rect.x, sheet_top, input_rect.width, sheet_h);

    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(theme.warn(), theme.panel()), area);

    let content_x = area.x + 1 + PERMISSION_H_PADDING;
    let body_area = Rect::new(
        content_x,
        area.y + PERMISSION_TOP_PADDING,
        content_w,
        body_h,
    );
    frame.render_widget(
        Paragraph::new(body_lines)
            .scroll((body_scroll.min(u16::MAX as usize) as u16, 0))
            .wrap(ratatui::widgets::Wrap { trim: false }),
        body_area,
    );

    let footer_y = area
        .y
        .saturating_add(sheet_h)
        .saturating_sub(PERMISSION_FOOTER_HEIGHT);
    let footer_band = Rect::new(
        area.x + 1,
        footer_y,
        area.width.saturating_sub(1),
        PERMISSION_FOOTER_HEIGHT,
    );
    frame.render_widget(
        RtBlock::default().style(Style::default().bg(theme.raised())),
        footer_band,
    );

    let details_label = if show_details { "Hide" } else { "Details" };
    let labels: Vec<&str> = if confirm_always {
        vec!["Confirm always", "Cancel"]
    } else {
        vec!["Allow once", "Always allow", "Reject", details_label]
    };

    let mut footer_spans: Vec<Span> = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        let is_cancel = confirm_always && index == 1;
        let is_reject = !confirm_always && index == 2;
        let is_selected = index == selected;
        let bg = if is_selected {
            if is_reject || is_cancel {
                theme.err()
            } else {
                theme.brand()
            }
        } else {
            theme.raised()
        };
        let fg = if is_selected {
            contrast_fg(bg)
        } else {
            theme.fg()
        };
        if index > 0 {
            footer_spans.push(Span::styled("  ", Style::default().bg(theme.raised())));
        }
        footer_spans.push(Span::styled(
            format!(" {} ", label),
            Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
        ));
    }
    let hint = if confirm_always {
        " ←→ select · Enter confirm · Esc back "
    } else if max_scroll > 0 {
        " ↑↓ scroll details · ←→ select · Enter · Esc reject "
    } else {
        " ←→ select · Enter · Esc reject "
    };
    let hint_width = hint.width();
    let footer_width = content_w as usize;
    let used: usize = footer_spans.iter().map(|s| s.content.width()).sum();
    if used + hint_width <= footer_width {
        footer_spans.push(Span::styled(
            " ".repeat(footer_width - used - hint_width),
            Style::default().bg(theme.raised()),
        ));
        footer_spans.push(Span::styled(
            hint,
            Style::default().bg(theme.raised()).fg(theme.muted()),
        ));
    } else if used < footer_width {
        footer_spans.push(Span::styled(
            " ".repeat(footer_width - used),
            Style::default().bg(theme.raised()),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(footer_spans)),
        Rect::new(content_x, footer_y, content_w, PERMISSION_FOOTER_HEIGHT),
    );
    max_scroll
}
