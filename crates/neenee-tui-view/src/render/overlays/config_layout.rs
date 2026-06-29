//! Transcript layout sub-page of the config manager modal.
//!
//! Reached from [`super::config`] by selecting the "Layout" row. Lists the
//! layout strategies; the active one is marked and highlighted. `Space` /
//! `Enter` applies the selected strategy — sent as
//! `AgentRequest::UpdateTuiLayout`, persisted to `config.toml`, and the
//! harness replies with `AgentResponse::TuiLayoutUpdated` which re-seeds
//! `App::transcript_layout`(App::transcript_layout). `Esc`
//! returns to the config root.

use neenee_tui::{
    Frame, Paragraph, {Line, Span}, {Modifier, Style},
};

use crate::modal::Modal;
use crate::render::Theme;
use crate::render::design::MODAL_INNER_H_PADDING;
use crate::render::layout::Strategy;
use crate::render::primitives::{
    FooterHint, content_modal_area, content_modal_probe, contrast_fg, modal_chrome_rows,
    modal_frame, modal_spec, render_body, render_modal_footer,
};

/// One selectable layout strategy + its human description.
struct LayoutOption {
    /// The canonical config-string this row maps to (written verbatim to
    /// `config.toml`). Matches [`Strategy::from_config`]'s accepted spellings.
    config_value: &'static str,
    label: &'static str,
    description: &'static str,
}

/// The static option list. **Order matters**: `modal_index` selects by
/// position, and `apply_index` maps an index back to a `config_value`.
fn options() -> [LayoutOption; 2] {
    [
        LayoutOption {
            config_value: "compact",
            label: "Compact",
            description: "Original flush stack: tight gaps, batched tool calls",
        },
        LayoutOption {
            config_value: "round_band",
            label: "Round-band",
            description: "Each tool round grouped under a labelled header",
        },
    ]
}

/// The canonical config-string for the option at `index`, for the apply path.
pub fn config_value_at(index: usize) -> Option<&'static str> {
    options().get(index).map(|o| o.config_value)
}

/// Number of selectable rows.
pub const ROW_COUNT: usize = 2;

/// Draw the layout sub-page modal. `modal_index` is the selection cursor;
/// `current` is the live [`Strategy`] from `App.transcript_layout`, used to
/// mark the active option. The caller sends `AgentRequest::UpdateTuiLayout`
/// when the user applies a choice.
pub fn draw_config_layout_modal(
    frame: &mut Frame,
    current: Strategy,
    modal_index: usize,
    scroll: &mut usize,
    theme: &Theme,
) -> neenee_tui::Rect {
    let probe =
        content_modal_probe(frame, Modal::ConfigLayout).expect("config layout modal geometry");
    let body_width = (probe.width as usize)
        .saturating_sub(2 * MODAL_INNER_H_PADDING as usize)
        .max(1);

    let mut body: Vec<Line> = Vec::new();
    let mut selected_line: Option<usize> = None;

    // One-line description of the sub-page, rendered before the option list.
    // Muted, not selectable.
    body.push(Line::from(Span::styled(
        "How the transcript arranges tool rounds. Round-band groups each \
         model request under a header so the history reads as discrete steps.",
        Style::default().fg(theme.muted()),
    )));
    body.push(Line::from(""));

    const GUTTER_W: usize = 2;
    const PREFIX_W: usize = GUTTER_W + 2; // gutter + glyph

    let current_value = match current {
        Strategy::Compact => "compact",
        Strategy::RoundBand => "round_band",
    };

    for (i, opt) in options().iter().enumerate() {
        let is_sel = i == modal_index;
        let is_active = opt.config_value == current_value;
        let bg = if is_sel { theme.brand() } else { theme.panel() };
        let fg = if is_sel {
            contrast_fg(theme.brand())
        } else {
            theme.fg()
        };
        let dim = if is_sel {
            contrast_fg(theme.brand())
        } else {
            theme.muted()
        };
        let glyph = if is_sel { "▸" } else { " " };
        let mark = if is_active { "● " } else { "○ " };

        let label_w = 12usize;
        let desc_budget = body_width
            .saturating_sub(PREFIX_W + label_w + 2 + mark.len())
            .max(1);
        let desc = opt.description;
        let desc_truncated = if desc.len() > desc_budget {
            &desc[..desc_budget.saturating_sub(1)]
        } else {
            desc
        };
        let pad =
            body_width.saturating_sub(PREFIX_W + label_w + 2 + mark.len() + desc_truncated.len());

        if is_sel {
            selected_line = Some(body.len());
        }
        body.push(Line::from(vec![
            Span::styled(" ".repeat(GUTTER_W), Style::default().bg(bg)),
            Span::styled(format!("{glyph} "), Style::default().bg(bg).fg(fg)),
            Span::styled(
                format!("{:<w$}", opt.label, w = label_w),
                Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(mark, Style::default().bg(bg).fg(dim)),
            Span::styled(desc_truncated, Style::default().bg(bg).fg(dim)),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        ]));
    }

    // Footnote: what the active setting currently is.
    body.push(Line::from(""));
    body.push(Line::from(Span::styled(
        format!("Active: {}", current_value),
        Style::default().fg(theme.muted()),
    )));

    let spec = modal_spec(Modal::ConfigLayout).expect("config layout modal geometry");
    let desired = body.len() as u16 + modal_chrome_rows(spec);
    let area = content_modal_area(frame, Modal::ConfigLayout, desired)
        .expect("config layout modal geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Configuration › ", Style::default().fg(theme.muted())),
                Span::styled(
                    "Layout",
                    Style::default()
                        .fg(theme.brand())
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            h,
        );
    }

    let body_rect = f.body;
    let follow = selected_line;
    render_body(frame, body_rect, body, scroll, follow, false, theme);

    if let Some(fo) = f.footer {
        render_modal_footer(
            frame,
            fo,
            &[
                FooterHint::navigation("↑↓", "select"),
                FooterHint::primary("Enter/Space", "apply"),
                FooterHint::always("Esc", "back"),
            ],
            theme,
        );
    }
    area
}
