//! Activity modal: section-specific overview of the current turn, pursuit, or todos.
//!
//! Each section is opened independently by clicking the corresponding segment
//! on the activity bar — there is no tab strip or Left/Right cycling.

use neenee_tui::{
    Frame, Paragraph, {Line, Span}, {Modifier, Style},
};

use super::common::todo_status_glyph_color;
use crate::tui::Modal;
use crate::tui::render::Theme;
use crate::tui::render::design::{MODAL_BODY_LEADING_INDENT, MODAL_TITLE_META_GAP};
use crate::tui::render::primitives::{
    FooterHint, modal_area, modal_frame, render_body, render_modal_footer,
};
use crate::tui::render::text_layout::{indented_wrapped_lines, wrap_text};

/// Inputs for [`draw_activity_modal`]. Carries everything the old always-pinned
/// pursuit bar, plan panel, and activity bar used to show, gathered into one
/// overlay so the footer is a single line. Fields are `Option`al so the modal
/// simply omits a section when there is nothing to report.
pub struct ActivityModalView<'a> {
    /// Which section to show (Activity or Todos). Each section is opened
    /// independently by clicking the corresponding segment on the activity bar.
    pub active_tab: crate::tui::ActivityTab,
    /// Active pursuit, if any. Shown as an objective line plus one row per
    /// checklist item.
    pub pursuit: Option<&'a neenee_core::Pursuit>,
    /// Live unified task list, if any. Shown as a header (done/total) plus
    /// one row per item with a status glyph.
    pub todos: Option<&'a neenee_core::TodoList>,
    /// The current turn's user prompt, if any. Shown in the Activity tab.
    pub user_prompt: Option<&'a str>,
    /// Harness turn counter (`turn N`).
    pub turn_count: u64,
    /// Current tool round within the turn (1-indexed; `0` before the first
    /// model request).
    pub current_round: u64,
    /// Session-review alert (ADR-0016), or empty when inactive.
    pub review_alert: &'a str,
    /// Display id of the currently active model.
    pub current_model: &'a str,
    /// Wall-clock instant the current turn started, or `None` between turns.
    pub turn_started_at: Option<std::time::Instant>,

    pub activity: &'a str,
}

/// The Activity modal: a scrollable overview of a single section (Activity or
/// Todos). The active section is determined by which activity-bar segment the
/// user clicked — there is no tab strip inside the modal.
pub fn draw_activity_modal(
    frame: &mut Frame,
    view: ActivityModalView<'_>,
    scroll: &mut usize,
    theme: &Theme,
) -> neenee_tui::Rect {
    let ActivityModalView {
        active_tab,
        pursuit,
        todos,
        user_prompt,
        turn_count,
        current_round,
        review_alert,
        current_model,
        turn_started_at,
        activity,
    } = view;

    let area = modal_area(frame, Modal::Activity).expect("activity modal has fixed geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    let muted = theme.muted();

    // ── Header: section title, plus a trailing meta counter for Todos ──
    // The Todos `done/total` counter sits beside the title instead of being
    // re-emitted as a second "Todos" body line, so the label shows once.
    if let Some(h) = f.header {
        let mut header_spans: Vec<Span<'static>> = vec![Span::styled(
            active_tab.title(),
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        )];
        if let crate::tui::ActivityTab::Todos = active_tab {
            if let Some(list) = todos.filter(|l| !l.items.is_empty()) {
                use neenee_core::TodoStatus;
                let done = list.count(TodoStatus::Completed);
                let total = list.items.len();
                header_spans.push(Span::styled(
                    format!("{}{done}/{total}", " ".repeat(MODAL_TITLE_META_GAP)),
                    Style::default().fg(muted),
                ));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(header_spans)), h);
    }

    let mut lines: Vec<Line<'static>> = Vec::new();

    match active_tab {
        crate::tui::ActivityTab::Activity => {
            let mut have_section = false;

            // ── Pursuit ──
            if let Some(pursuit) = pursuit {
                have_section = true;
                lines.push(Line::from(vec![Span::styled(
                    "Pursuit",
                    Style::default()
                        .fg(theme.brand())
                        .add_modifier(Modifier::BOLD),
                )]));
                let objective_label = if pursuit.is_complete {
                    "✓ complete · ".to_string()
                } else {
                    String::new()
                };
                // Pre-wrap so a long objective's continuation rows inherit the
                // leading indent (render_body no longer soft-wraps this modal).
                let objective = format!("{}{}", objective_label, pursuit.objective);
                lines.extend(indented_wrapped_lines(
                    &objective,
                    MODAL_BODY_LEADING_INDENT,
                    f.body.width as usize,
                    Style::default().fg(theme.fg()),
                ));
            }

            // ── Prompt (current turn's user message) ──
            if let Some(prompt) = user_prompt.filter(|p| !p.is_empty()) {
                if have_section {
                    lines.push(Line::from(""));
                }
                have_section = true;
                lines.push(Line::from(vec![Span::styled(
                    "Prompt",
                    Style::default()
                        .fg(theme.brand())
                        .add_modifier(Modifier::BOLD),
                )]));
                // Container primitive: pre-wrap at `body_width - indent` and
                // emit one indented `Line` per visual row. This is the fix for
                // the bug where only the first logical line of a multi-line
                // prompt was indented — every row (explicit `\n` *and*
                // width-induced continuation) now inherits the block indent,
                // because the indent is a geometry property of the block, not
                // a span painted on a single logical line. `render_body` runs
                // with wrapping disabled below.
                lines.extend(indented_wrapped_lines(
                    prompt,
                    MODAL_BODY_LEADING_INDENT,
                    f.body.width as usize,
                    Style::default().fg(theme.fg()),
                ));
            }

            // ── Status (always shown) ──
            if have_section {
                lines.push(Line::from(""));
            }
            let idle = activity.is_empty() || activity == "idle";
            lines.push(Line::from(vec![Span::styled(
                "Status",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            )]));

            if turn_count > 0 {
                // Build the structured detail as one string so the container
                // helper can pre-wrap it as a unit — a long model name or
                // locale-dependent elapsed string would otherwise overflow the
                // body's right edge (render_body no longer soft-wraps here).
                let mut detail = format!("turn {}", turn_count);
                if current_round >= 1 {
                    detail.push_str(" · ");
                    detail.push_str(&format!("round {}", current_round));
                }
                if !current_model.is_empty() {
                    detail.push_str(" · ");
                    detail.push_str(&crate::tui::model_display_name(current_model));
                }
                if let Some(started) = turn_started_at {
                    detail.push_str(" · ");
                    detail.push_str(&crate::tui::render::chrome::format_elapsed(
                        started.elapsed(),
                    ));
                }
                lines.extend(indented_wrapped_lines(
                    &detail,
                    MODAL_BODY_LEADING_INDENT,
                    f.body.width as usize,
                    Style::default().fg(muted),
                ));
            }

            let status_style = if idle {
                Style::default().fg(muted)
            } else {
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::ITALIC)
            };
            let status_label = if idle {
                "idle".to_string()
            } else {
                activity.to_string()
            };
            lines.extend(indented_wrapped_lines(
                &status_label,
                MODAL_BODY_LEADING_INDENT,
                f.body.width as usize,
                status_style,
            ));
            if !review_alert.is_empty() {
                lines.extend(indented_wrapped_lines(
                    &format!("⚠ {review_alert}"),
                    MODAL_BODY_LEADING_INDENT,
                    f.body.width as usize,
                    Style::default().fg(theme.warn()),
                ));
            }
        }

        crate::tui::ActivityTab::Todos => {
            if let Some(list) = todos.filter(|l| !l.items.is_empty()) {
                // Hanging indent: the status glyph leads the first visual row;
                // continuation rows align under the content, not the glyph.
                // The content column is `indent + glyph(1) + space(1)`, and the
                // content is pre-wrapped at `body_width - content_column` so a
                // long task description wraps cleanly instead of spilling past
                // the body's right edge.
                let glyph_col = MODAL_BODY_LEADING_INDENT + 1;
                let content_col = glyph_col + 1;
                let body_w = f.body.width as usize;
                let content_wrap_w = body_w.saturating_sub(content_col).max(1);
                for item in &list.items {
                    let glyph_color = todo_status_glyph_color(item.status, theme, muted);
                    let glyph = item.status.glyph();
                    let wrapped = wrap_text(&item.content, content_wrap_w);
                    // wrap_text yields nothing only for empty input; render an
                    // empty todo as a glyph + blank content row regardless.
                    if wrapped.is_empty() {
                        let row = Line::from(vec![
                            Span::styled(" ".repeat(MODAL_BODY_LEADING_INDENT), Style::default()),
                            Span::styled(glyph, Style::default().fg(glyph_color)),
                            Span::styled(" ", Style::default()),
                        ]);
                        lines.push(row);
                        continue;
                    }
                    for (i, wl) in wrapped.iter().enumerate() {
                        if i == 0 {
                            lines.push(Line::from(vec![
                                Span::styled(
                                    " ".repeat(MODAL_BODY_LEADING_INDENT),
                                    Style::default(),
                                ),
                                Span::styled(glyph, Style::default().fg(glyph_color)),
                                Span::styled(" ", Style::default()),
                                Span::styled(wl.text.clone(), Style::default().fg(theme.fg())),
                            ]));
                        } else {
                            lines.push(Line::from(vec![
                                Span::styled(" ".repeat(content_col), Style::default()),
                                Span::styled(wl.text.clone(), Style::default().fg(theme.fg())),
                            ]));
                        }
                    }
                }
            } else {
                lines.push(Line::from(Span::styled(
                    "No todos.",
                    Style::default().fg(muted),
                )));
            }
        }
    }

    // Wrapping is disabled: every wrappable block above was pre-wrapped by the
    // `indented_wrapped_lines` / `wrap_text` container primitives, which emit
    // one already-indented `Line` per visual row. A second wrap pass here
    // would mangle the pre-sized budgets and re-introduce the continuation-
    // row indent bug (the whole reason the pre-wrap path exists).
    render_body(frame, f.body, lines, scroll, None, false, theme);

    if let Some(footer) = f.footer {
        render_modal_footer(
            frame,
            footer,
            &[
                FooterHint::navigation("↑↓", "scroll"),
                FooterHint::always("Esc", "close"),
            ],
            theme,
        );
    }
    area
}
