//! Nudge sub-page of the config manager modal.
//!
//! Reached from [`super::config`] by selecting the "Nudge" row. Shows the
//! master `enabled` switch and the four tunable thresholds (`window`,
//! `threshold`, `escalate_at`, `path_threshold`). `Space` toggles the
//! enabled flag; `←`/`→` adjust the selected threshold; `Esc` returns to
//! the config root. Edits are sent as `AgentRequest::UpdateNudgeConfig` and
//! the harness replies with `AgentResponse::NudgeConfigUpdated`, which
//! re-seeds the snapshot the modal reads.

use neenee_core::NudgeConfig;
use neenee_tui::{
    Frame, Paragraph, {Line, Span}, {Modifier, Style},
};

use crate::modal::Modal;
use crate::render::Theme;
use crate::render::design::MODAL_INNER_H_PADDING;
use crate::render::primitives::{
    FooterHint, content_modal_area, content_modal_probe, contrast_fg, modal_chrome_rows,
    modal_frame, modal_spec, render_body, render_modal_footer,
};

/// Row index of the `enabled` toggle in the field list. `Space` only toggles
/// when this row is selected; threshold rows respond to `←`/`→` instead.
pub const ROW_ENABLED: usize = 0;
/// Row index of `window`. (Followed by threshold, escalate_at, path_threshold.)
pub const ROW_WINDOW: usize = 1;
pub const ROW_THRESHOLD: usize = 2;
pub const ROW_ESCALATE_AT: usize = 3;
pub const ROW_PATH_THRESHOLD: usize = 4;

/// Total number of rows in the nudge sub-page (enabled + 4 thresholds).
pub const ROW_COUNT: usize = 5;

/// Draw the nudge sub-page modal. `modal_index` is the selection cursor;
/// `config` is the live snapshot from the harness. The caller sends
/// `AgentRequest::UpdateNudgeConfig` when the user edits a field; the
/// harness reply re-seeds the snapshot so this renderer always reads the
/// authoritative state.
pub fn draw_config_nudge_modal(
    frame: &mut Frame,
    config: &NudgeConfig,
    modal_index: usize,
    scroll: &mut usize,
    theme: &Theme,
) -> neenee_tui::Rect {
    let probe =
        content_modal_probe(frame, Modal::ConfigNudge).expect("config nudge modal geometry");
    let body_width = (probe.width as usize)
        .saturating_sub(2 * MODAL_INNER_H_PADDING as usize)
        .max(1);

    let mut body: Vec<Line> = Vec::new();
    let mut selected_line: Option<usize> = None;

    // A one-line description of the whole sub-page, rendered before the
    // field list. Muted, not selectable.
    body.push(Line::from(Span::styled(
        "Steers the model out of read-loops: injects a hidden nudge, then \
         hard-blocks the looping read. Default is off.",
        Style::default().fg(theme.muted()),
    )));
    body.push(Line::from(""));

    // ── field rows ──
    const GUTTER_W: usize = 2;
    const PREFIX_W: usize = GUTTER_W + 2; // gutter + glyph
    let name_col = 16usize;
    let val_col = 8usize;

    let rows: [(String, String); ROW_COUNT] = [
        (
            "enabled".to_string(),
            if config.enabled {
                "on".to_string()
            } else {
                "off".to_string()
            },
        ),
        ("window".to_string(), config.window.to_string()),
        ("threshold".to_string(), config.threshold.to_string()),
        ("escalate_at".to_string(), config.escalate_at.to_string()),
        (
            "path_threshold".to_string(),
            config.path_threshold.to_string(),
        ),
    ];

    for (i, (name, val)) in rows.iter().enumerate() {
        let row_idx = i; // 0-based within the field list
        let is_sel = row_idx == modal_index;
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
        let desc = field_hint(name);
        let desc_budget = body_width
            .saturating_sub(PREFIX_W + name_col + val_col + 4)
            .max(1);
        let desc_truncated = if desc.len() > desc_budget {
            &desc[..desc_budget.saturating_sub(1)]
        } else {
            desc
        };
        let pad =
            body_width.saturating_sub(PREFIX_W + name_col + val_col + 4 + desc_truncated.len());
        if is_sel {
            selected_line = Some(body.len());
        }
        // For the enabled row, render a [on]/[off] badge instead of a bare
        // value, so the toggle affordance is visually distinct from a number.
        let val_display = if name == "enabled" {
            format!("[{val}]")
        } else {
            format!(" {val}")
        };
        body.push(Line::from(vec![
            Span::styled(" ".repeat(GUTTER_W), Style::default().bg(bg)),
            Span::styled(format!("{glyph} "), Style::default().bg(bg).fg(fg)),
            Span::styled(
                format!("{:<w$}", name, w = name_col),
                Style::default().bg(bg).fg(fg),
            ),
            Span::styled(
                format!("{:>w$}", val_display, w = val_col),
                Style::default().bg(bg).fg(dim),
            ),
            Span::styled(
                format!("  {desc_truncated}"),
                Style::default().bg(bg).fg(dim),
            ),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        ]));
    }

    let spec = modal_spec(Modal::ConfigNudge).expect("config nudge modal geometry");
    let desired = body.len() as u16 + modal_chrome_rows(spec);
    let area = content_modal_area(frame, Modal::ConfigNudge, desired)
        .expect("config nudge modal geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("← ", Style::default().fg(theme.muted())),
                Span::styled(
                    "Nudge",
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
        let hints: &[FooterHint] = if modal_index == ROW_ENABLED {
            &[
                FooterHint::navigation("↑↓", "select"),
                FooterHint::primary("Space", "toggle"),
                FooterHint::always("Esc", "back"),
            ]
        } else {
            &[
                FooterHint::navigation("↑↓", "select"),
                FooterHint::primary("←→", "adjust"),
                FooterHint::always("Esc", "back"),
            ]
        };
        render_modal_footer(frame, fo, hints, theme);
    }
    area
}

/// Short per-field hint shown to the right of the value.
fn field_hint(name: &str) -> &'static str {
    match name {
        "enabled" => "master switch",
        "window" => "sliding-window size (recent read rounds)",
        "threshold" => "exact-signature occurrences to trip a nudge",
        "escalate_at" => "escalate Inject → Block at this count",
        "path_threshold" => "same-file many-offsets occurrences to trip",
        _ => "",
    }
}

/// Apply a ±1 delta to the threshold at `row_index` in the nudge sub-page.
/// Row 0 (`enabled`) is not a threshold and must be excluded by the caller.
/// Each threshold is clamped to `>= 1` so a value of 0 never disables the
/// guard silently (use the `enabled` switch for that).
pub fn apply_threshold_delta(config: &mut NudgeConfig, row_index: usize, delta: i32) {
    let clamp = |v: u32, d: i32| (v as i32 + d).max(1) as u32;
    let clamp_usize = |v: usize, d: i32| (v as i32 + d).max(1) as usize;
    match row_index {
        ROW_WINDOW => config.window = clamp_usize(config.window, delta),
        ROW_THRESHOLD => config.threshold = clamp(config.threshold, delta),
        ROW_ESCALATE_AT => config.escalate_at = clamp(config.escalate_at, delta),
        ROW_PATH_THRESHOLD => config.path_threshold = clamp(config.path_threshold, delta),
        _ => {}
    }
}
