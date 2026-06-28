//! Permission sheet (inline) and question modal.

use neenee_tui::{
    Frame, Rect, {Block as RtBlock, Clear, Paragraph}, {Line, Span}, {Modifier, Style},
};

use neenee_core::{PermissionRequest, UserQuestionRequest};

use crate::tui::Modal;
use crate::tui::layout::{ModalHitMap, PermissionActionHit, QuestionOptionHit};
use crate::tui::render::Theme;
use crate::tui::render::primitives::{
    FooterHint, contrast_fg, modal_area, modal_footer_text, modal_frame, panel_block, render_body,
    render_modal_footer,
};
use crate::tui::render::text_layout::wrap_text;
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
/// shows no marker at all — the highlight *is* the selection (it moves live
/// with ↑/↓ and a digit jump), so pressing Enter submits the highlighted row
/// directly. A numbered digit key (1-9) jumps directly to an option.
const OTHER_OPTION_LABEL: &str = "Other";

#[allow(clippy::too_many_arguments)] // modal draw fns thread many context args by nature
pub fn draw_question_modal(
    frame: &mut Frame,
    hit_map: &mut ModalHitMap,
    request: &UserQuestionRequest,
    current_question: usize,
    selected: &[Vec<usize>],
    other_text: &[String],
    highlighted: usize,
    scroll: &mut usize,
    follow_highlight: bool,
    theme: &Theme,
) -> neenee_tui::Rect {
    let area = modal_area(frame, Modal::Question).expect("question modal has fixed geometry");
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

    let mut body_lines: Vec<Line> = Vec::new();
    let mut option_rows: Vec<(usize, usize, usize)> = Vec::new();
    let body_width = f.body.width as usize;
    let mut highlighted_row = None;
    if let Some(q) = question {
        if let Some(header) = &q.header {
            push_wrapped_styled(
                &mut body_lines,
                "",
                "",
                header,
                Style::default()
                    .fg(theme.info())
                    .add_modifier(Modifier::BOLD),
                body_width,
            );
        }
        push_wrapped_styled(
            &mut body_lines,
            "",
            "",
            &q.question,
            Style::default().fg(theme.fg()),
            body_width,
        );
        body_lines.push(Line::from(""));

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
            let row = body_lines.len();
            if is_highlighted {
                highlighted_row = Some(row);
            }
            let start = body_lines.len();
            render_question_option(
                &mut body_lines,
                i,
                &option.label,
                option.description.as_deref(),
                is_selected,
                is_highlighted,
                q.multi_select,
                body_width,
                theme,
            );
            option_rows.push((i, start, body_lines.len()));
        }

        let row = body_lines.len();
        if other_highlighted {
            highlighted_row = Some(row);
        }
        let other_start = body_lines.len();
        render_question_option(
            &mut body_lines,
            other_index,
            OTHER_OPTION_LABEL,
            None,
            q_selected.is_some_and(|s| s.contains(&other_index)),
            other_highlighted,
            q.multi_select,
            body_width,
            theme,
        );
        if other_highlighted {
            push_wrapped_styled(
                &mut body_lines,
                "     ",
                "     ",
                &format!("{}{}", other_text_value, "█"),
                Style::default().fg(theme.brand()),
                body_width,
            );
        }
        option_rows.push((other_index, other_start, body_lines.len()));
    }

    // Auto-follow the highlight only while navigating (the default after open /
    // ↑↓ / digit-jump); a manual wheel/page scroll clears the flag so the user
    // can browse a long question or option list without the body snapping back
    // to the cursor. Mirrors the session / history modals.
    let follow = if follow_highlight {
        highlighted_row
    } else {
        None
    };
    render_body(frame, f.body, body_lines, scroll, follow, false, theme);
    record_question_hits(hit_map, f.body, &option_rows, *scroll);

    if let Some(fo) = f.footer {
        // Single-select is live (the highlight is the selection), so there is
        // no "select" action to advertise — Space is a no-op there. Only
        // multi-select offers the Space toggle.
        let mut hints = vec![
            FooterHint::navigation("↑↓", "navigate"),
            FooterHint::navigation("wheel/Pg", "scroll"),
            FooterHint::primary("Enter", "submit"),
        ];
        if question.is_some_and(|q| q.multi_select) {
            hints.push(FooterHint::secondary("Space", "select"));
        }
        hints.push(FooterHint::secondary("1-9", "jump"));
        hints.push(FooterHint::always("Esc", "cancel"));
        render_modal_footer(frame, fo, &hints, theme);
    }
    area
}

fn record_question_hits(
    hit_map: &mut ModalHitMap,
    body: Rect,
    option_rows: &[(usize, usize, usize)],
    scroll: usize,
) {
    if body.width == 0 || body.height == 0 {
        return;
    }
    let visible_top = scroll;
    let visible_bottom = scroll + body.height as usize;
    for &(option_index, start, end) in option_rows {
        let top = start.max(visible_top);
        let bottom = end.max(start + 1).min(visible_bottom);
        if top >= bottom {
            continue;
        }
        hit_map.push_question_option(QuestionOptionHit {
            option_index,
            rect: Rect::new(
                body.x,
                body.y + (top - visible_top) as u16,
                body.width,
                (bottom - top) as u16,
            ),
        });
    }
}

fn push_wrapped_styled(
    lines: &mut Vec<Line>,
    first_prefix: &str,
    continuation_prefix: &str,
    text: &str,
    style: Style,
    body_width: usize,
) {
    let first_width = first_prefix.width();
    let continuation_width = continuation_prefix.width();
    let wrap_width = body_width
        .saturating_sub(first_width.max(continuation_width))
        .max(1);
    let wrapped = wrap_text(text, wrap_width);
    if wrapped.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            first_prefix.to_string(),
            Style::default(),
        )]));
        return;
    }

    for (idx, wrapped_line) in wrapped.into_iter().enumerate() {
        let prefix = if idx == 0 {
            first_prefix
        } else {
            continuation_prefix
        };
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), Style::default()),
            Span::styled(wrapped_line.text, style),
        ]));
    }
}

#[allow(clippy::too_many_arguments)]
fn render_question_option(
    lines: &mut Vec<Line>,
    _index: usize,
    label: &str,
    description: Option<&str>,
    is_selected: bool,
    is_highlighted: bool,
    multi_select: bool,
    body_width: usize,
    theme: &Theme,
) {
    // No row-number prefix and no `❯` focus glyph — the hover is signalled
    // purely by the font color of the highlighted row (brand tone + bold),
    // not by a background band.
    //
    // Marker policy:
    // - Multi-select: a `[x]`/`[ ]` checkbox, because selection is a separate
    //   toggle set from the highlight — the checkbox is the only way to tell
    //   a *selected* row from a merely *hovered* one.
    // - Single-select: no marker. The highlight is *live* (it moves with
    //   ↑/↓ and commits immediately), so the highlighted row is, by
    //   definition, the selected one. Showing a radio dot would be redundant
    //   with the brand-colored highlight and would imply a two-step
    //   (mark-then-confirm) flow that does not exist.
    let (marker, marker_style) = if multi_select {
        let m = if is_selected { "[x]" } else { "[ ]" };
        let style = if is_selected {
            Style::default().fg(theme.ok()).add_modifier(Modifier::BOLD)
        } else if is_highlighted {
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted())
        };
        (m, style)
    } else {
        // Single-select: no marker. Use an empty span styled as the muted
        // baseline so the line keeps the same indentation rhythm as the
        // multi-select rows.
        ("", Style::default().fg(theme.muted()))
    };

    let text_style = if is_highlighted {
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.fg())
    };

    // Keep the label column aligned whether or not a marker is shown: the
    // prefix reserves a fixed-width slot ("  [x] " for multi-select, "    "
    // for the marker-less single-select) so single- and multi-select rows
    // line up the same.
    let first_prefix = format!("  {} ", marker);
    let continuation_prefix = "     ";
    push_wrapped_styled_with_prefix_style(
        lines,
        &first_prefix,
        continuation_prefix,
        label,
        marker_style,
        text_style,
        body_width,
    );

    if let Some(desc) = description {
        let desc_style = if is_highlighted {
            Style::default().fg(theme.brand())
        } else {
            Style::default().fg(theme.dim())
        };
        push_wrapped_styled(lines, "     ", "     ", desc, desc_style, body_width);
    }
}

fn push_wrapped_styled_with_prefix_style(
    lines: &mut Vec<Line>,
    first_prefix: &str,
    continuation_prefix: &str,
    text: &str,
    first_prefix_style: Style,
    text_style: Style,
    body_width: usize,
) {
    let first_width = first_prefix.width();
    let continuation_width = continuation_prefix.width();
    let wrap_width = body_width
        .saturating_sub(first_width.max(continuation_width))
        .max(1);
    let wrapped = wrap_text(text, wrap_width);
    if wrapped.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            first_prefix.to_string(),
            first_prefix_style,
        )]));
        return;
    }

    for (idx, wrapped_line) in wrapped.into_iter().enumerate() {
        if idx == 0 {
            lines.push(Line::from(vec![
                Span::styled(first_prefix.to_string(), first_prefix_style),
                Span::styled(wrapped_line.text, text_style),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(continuation_prefix.to_string(), Style::default()),
                Span::styled(wrapped_line.text, text_style),
            ]));
        }
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
    hit_map: &mut ModalHitMap,
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
    hit_map.set_permission_sheet(area);

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
            .scroll(body_scroll.min(u16::MAX as usize) as u16, 0)
            .wrap(neenee_tui::Wrap { trim: false }),
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
    let mut action_x = content_x;
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
            action_x = action_x.saturating_add(2);
        }
        let text = format!(" {} ", label);
        let width = text.width().min(u16::MAX as usize) as u16;
        hit_map.push_permission_action(PermissionActionHit {
            action_index: index,
            rect: Rect::new(action_x, footer_y, width, PERMISSION_FOOTER_HEIGHT),
        });
        footer_spans.push(Span::styled(
            text,
            Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
        ));
        action_x = action_x.saturating_add(width);
    }
    let hints: &[FooterHint] = if confirm_always {
        &[
            FooterHint::navigation("←→", "select"),
            FooterHint::primary("Enter", "confirm"),
            FooterHint::always("Esc", "back"),
        ]
    } else if max_scroll > 0 {
        &[
            FooterHint::navigation("↑↓", "scroll"),
            FooterHint::navigation("←→", "select"),
            FooterHint::primary("Enter", "confirm"),
            FooterHint::always("Esc", "reject"),
        ]
    } else {
        &[
            FooterHint::navigation("←→", "select"),
            FooterHint::primary("Enter", "confirm"),
            FooterHint::always("Esc", "reject"),
        ]
    };
    let footer_width = content_w as usize;
    let used: usize = footer_spans.iter().map(|s| s.content.width()).sum();
    let hint = modal_footer_text(hints, footer_width.saturating_sub(used));
    let hint_width = hint.width();
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

#[cfg(test)]
mod tests {
    use super::*;
    use neenee_core::{UserQuestion, UserQuestionOption};

    #[test]
    fn question_modal_records_option_hit_boxes() {
        let request = UserQuestionRequest {
            id: "q".into(),
            questions: vec![UserQuestion {
                header: None,
                question: "Pick one".into(),
                options: vec![
                    UserQuestionOption {
                        label: "A".into(),
                        description: None,
                    },
                    UserQuestionOption {
                        label: "B".into(),
                        description: Some("Second option".into()),
                    },
                ],
                multi_select: false,
            }],
        };
        let mut terminal = neenee_tui::TestTerminal::new(80, 24);
        let mut hit_map = ModalHitMap::new();
        terminal.draw(|frame| {
            let mut scroll = 0;
            draw_question_modal(
                frame,
                &mut hit_map,
                &request,
                0,
                &[vec![0]],
                &[String::new()],
                0,
                &mut scroll,
                true,
                &Theme::default(),
            );
        });

        assert!(find_question_hit(&hit_map, 80, 24, 0));
        assert!(find_question_hit(&hit_map, 80, 24, 1));
        assert!(find_question_hit(&hit_map, 80, 24, 2));
    }

    #[test]
    fn permission_sheet_records_footer_action_hit_boxes() {
        let request = PermissionRequest {
            id: "p".into(),
            tool: "bash".into(),
            label: "bash".into(),
            description: "Run a command".into(),
            arguments: r#"{"command":"cargo test"}"#.into(),
            scope: "*".into(),
        };
        let mut terminal = neenee_tui::TestTerminal::new(80, 24);
        let mut hit_map = ModalHitMap::new();
        terminal.draw(|frame| {
            let rect = Rect::new(0, 16, 80, 8);
            let _ = draw_permission_sheet(
                frame,
                &mut hit_map,
                &request,
                0,
                false,
                false,
                0,
                rect,
                &Theme::default(),
            );
        });

        for action_index in 0..4 {
            assert!(
                find_permission_hit(&hit_map, 80, 24, action_index),
                "missing permission action {action_index}"
            );
        }
    }

    fn find_question_hit(map: &ModalHitMap, width: u16, height: u16, option_index: usize) -> bool {
        (0..height).any(|y| {
            (0..width).any(|x| {
                map.question_option_at(x, y)
                    .is_some_and(|hit| hit.option_index == option_index)
            })
        })
    }

    fn find_permission_hit(
        map: &ModalHitMap,
        width: u16,
        height: u16,
        action_index: usize,
    ) -> bool {
        (0..height).any(|y| {
            (0..width).any(|x| {
                map.permission_action_at(x, y)
                    .is_some_and(|hit| hit.action_index == action_index)
            })
        })
    }
}
