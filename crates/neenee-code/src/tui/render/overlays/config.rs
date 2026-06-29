//! Config manager modal — the root settings overlay.
//!
//! Opened via the `/config` slash command. Lists the configurable categories
//! (Nudge, …) as selectable rows; `Enter` / `Space` drills into a category's
//! sub-page ([`super::config_nudge`] for the Nudge sub-page). `Esc` closes.
//!
//! The category list is static for now — as more configurable surfaces are
//! added (compaction, hooks, permissions defaults, …), they each get a row
//! here and a dedicated sub-page module under `overlays/`.

use neenee_tui::{
    Frame, Paragraph, {Line, Span}, {Modifier, Style},
};

use crate::tui::Modal;
use crate::tui::render::Theme;
use crate::tui::render::primitives::{
    FooterHint, content_modal_area, content_modal_probe, contrast_fg, modal_chrome_rows,
    modal_frame, modal_spec, render_body, render_modal_footer,
};

/// One configurable category row in the config root modal.
struct ConfigCategory {
    label: &'static str,
    description: &'static str,
}

/// The static category list. As more configurable surfaces are added, append
/// here and create a matching sub-page module.
///
/// **Index matters**: the `ConfigActivate` handler dispatches on `modal_index`
/// (0 = Nudge, 1 = Layout). Keep this order in sync with that match.
fn categories() -> Vec<ConfigCategory> {
    vec![
        ConfigCategory {
            label: "Nudge",
            description: "Read-loop guard: thresholds and master switch",
        },
        ConfigCategory {
            label: "Layout",
            description: "Transcript round grouping & spacing",
        },
    ]
}

/// Draw the config root modal: a centered, dismissable, selectable list of
/// configurable categories. Each row shows the category name, a short
/// description, and a `›` drill-in affordance. `Enter` / `Space` opens the
/// selected category's sub-page; `Esc` closes.
pub fn draw_config_modal(
    frame: &mut Frame,
    modal_index: usize,
    scroll: &mut usize,
    theme: &Theme,
) -> neenee_tui::Rect {
    let probe = content_modal_probe(frame, Modal::Config).expect("config modal has geometry");
    let body_width = (probe.width as usize)
        .saturating_sub(2 * crate::tui::render::design::MODAL_INNER_H_PADDING as usize)
        .max(1);

    let cats = categories();

    let mut body: Vec<Line> = Vec::new();
    let mut selected_line: Option<usize> = None;

    const GUTTER_W: usize = 2;
    const PREFIX_W: usize = GUTTER_W + 2; // gutter + glyph
    let label_col = 12usize;

    for (i, cat) in cats.iter().enumerate() {
        let is_sel = i == modal_index;
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
        let desc = cat.description;
        let desc_budget = body_width.saturating_sub(PREFIX_W + label_col + 2).max(1);
        let desc_truncated = if desc.len() > desc_budget {
            &desc[..desc_budget.saturating_sub(1)]
        } else {
            desc
        };
        let pad = body_width.saturating_sub(PREFIX_W + label_col + 2 + desc_truncated.len());
        if is_sel {
            selected_line = Some(body.len());
        }
        body.push(Line::from(vec![
            Span::styled(" ".repeat(GUTTER_W), Style::default().bg(bg)),
            Span::styled(format!("{glyph} "), Style::default().bg(bg).fg(fg)),
            Span::styled(
                format!("{:<w$}", cat.label, w = label_col),
                Style::default().bg(bg).fg(fg),
            ),
            Span::styled(
                format!("  {desc_truncated}"),
                Style::default().bg(bg).fg(dim),
            ),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        ]));
    }

    let spec = modal_spec(Modal::Config).expect("config modal has geometry");
    let desired = body.len() as u16 + modal_chrome_rows(spec);
    let area =
        content_modal_area(frame, Modal::Config, desired).expect("config modal has geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Configuration",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
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
                FooterHint::primary("Enter", "open"),
                FooterHint::always("Esc", "close"),
            ],
            theme,
        );
    }
    area
}
