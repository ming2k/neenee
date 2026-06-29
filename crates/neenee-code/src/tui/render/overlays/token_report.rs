//! Token-source report modal — a read-only breakdown of how many tokens each
//! provider+model reported authoritatively (upstream `usage`) vs. how many were
//! filled in by the local char-class estimator.
//!
//! Opened by clicking the context meter in the hint bar. The data is a live
//! snapshot of the shared [`TokenSourceLedger`], so it reflects every turn
//! booked so far this session.

use neenee_core::TokenSourceReport;
use neenee_tui::{
    Frame, Paragraph, {Line, Span}, {Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use super::common::placeholder;
use crate::tui::Modal;
use crate::tui::render::Theme;
use crate::tui::render::design::MODAL_INNER_H_PADDING;
use crate::tui::render::primitives::{
    FooterHint, content_modal_area, content_modal_probe, modal_chrome_rows, modal_frame,
    modal_spec, render_body, render_modal_footer,
};

/// Draw the token-source report modal: a centered, dismissable, read-only
/// table. Each row is one provider+model with its reported-token count,
/// estimated-token count, and a percentage. A summary row shows the grand
/// total. Esc / outside-click closes.
pub fn draw_token_report_modal(
    frame: &mut Frame,
    report: &TokenSourceReport,
    scroll: &mut usize,
    theme: &Theme,
) -> neenee_tui::Rect {
    // Probe the content width so column layout adapts to the terminal.
    let probe =
        content_modal_probe(frame, Modal::TokenReport).expect("token-report modal has geometry");
    let body_width = (probe.width as usize)
        .saturating_sub(2 * MODAL_INNER_H_PADDING as usize)
        .max(1);

    let mut body: Vec<Line> = Vec::new();

    if report.rows.is_empty() {
        body.push(placeholder(
            "No token usage recorded yet — send a message first.",
            true,
            theme.muted(),
        ));
    } else {
        // Column layout: provider/model (flex) | reported (14) | estimated (14) | pct (7)
        const REPORTED_W: usize = 14;
        const ESTIMATED_W: usize = 14;
        const PCT_W: usize = 7;
        let name_budget = body_width
            .saturating_sub(REPORTED_W + ESTIMATED_W + PCT_W + 6)
            .max(12);

        // Header row.
        body.push(Line::from(vec![
            Span::styled(
                format!("{:<w$}", "Provider / Model", w = name_budget),
                Style::default().fg(theme.muted()),
            ),
            Span::styled(
                format!("{:>w$}", "Reported", w = REPORTED_W),
                Style::default().fg(theme.muted()),
            ),
            Span::styled(
                format!("{:>w$}", "Estimated", w = ESTIMATED_W),
                Style::default().fg(theme.muted()),
            ),
            Span::styled(
                format!("{:>w$}", "% Real", w = PCT_W),
                Style::default().fg(theme.muted()),
            ),
        ]));
        body.push(Line::from(Span::styled(
            "─".repeat(body_width),
            Style::default().fg(theme.muted()),
        )));

        // One row per provider+model.
        for row in &report.rows {
            let total = row.totals.total().max(1);
            let pct_real =
                (row.totals.reported_tokens as f64 / total as f64 * 100.0).round() as i64;
            let label = format!("{} · {}", row.provider, row.model);
            let label = truncate_str(&label, name_budget);

            // Color the percentage: green when fully reported, yellow when
            // mixed, muted/red when all estimated.
            let pct_color = if row.totals.reported_tokens > 0 && row.totals.estimated_tokens == 0 {
                theme.ok()
            } else if row.totals.reported_tokens > 0 {
                theme.warn()
            } else {
                theme.muted()
            };

            body.push(Line::from(vec![
                Span::styled(
                    format!("{:<w$}", label, w = name_budget),
                    Style::default().fg(theme.fg()),
                ),
                Span::styled(
                    format!(
                        "{:>w$}",
                        fmt_tokens(row.totals.reported_tokens),
                        w = REPORTED_W
                    ),
                    Style::default().fg(theme.ok()),
                ),
                Span::styled(
                    format!(
                        "{:>w$}",
                        fmt_tokens(row.totals.estimated_tokens),
                        w = ESTIMATED_W
                    ),
                    Style::default().fg(if row.totals.estimated_tokens > 0 {
                        theme.warn()
                    } else {
                        theme.muted()
                    }),
                ),
                Span::styled(
                    format!("{:>w$}%", pct_real, w = PCT_W.saturating_sub(1)),
                    Style::default().fg(pct_color),
                ),
            ]));
        }

        // Grand-total summary.
        body.push(Line::from(Span::styled(
            "─".repeat(body_width),
            Style::default().fg(theme.muted()),
        )));
        let g_total = report.grand_total.total().max(1);
        let g_pct =
            (report.grand_total.reported_tokens as f64 / g_total as f64 * 100.0).round() as i64;
        body.push(Line::from(vec![
            Span::styled(
                format!("{:<w$}", "Total", w = name_budget),
                Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{:>w$}",
                    fmt_tokens(report.grand_total.reported_tokens),
                    w = REPORTED_W
                ),
                Style::default().fg(theme.ok()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{:>w$}",
                    fmt_tokens(report.grand_total.estimated_tokens),
                    w = ESTIMATED_W
                ),
                Style::default()
                    .fg(if report.grand_total.estimated_tokens > 0 {
                        theme.warn()
                    } else {
                        theme.muted()
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:>w$}%", g_pct, w = PCT_W.saturating_sub(1)),
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        // Explanatory footer lines inside the body.
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            "Reported = authoritative counts from the provider's usage object.",
            Style::default().fg(theme.muted()),
        )));
        body.push(Line::from(Span::styled(
            "Estimated = local char-class heuristic (provider reported no usage).",
            Style::default().fg(theme.muted()),
        )));
    }

    // ── Size the panel to the content and paint it ──
    let spec = modal_spec(Modal::TokenReport).expect("token-report modal has geometry");
    let desired = body.len() as u16 + modal_chrome_rows(spec);
    let area = content_modal_area(frame, Modal::TokenReport, desired)
        .expect("token-report modal has geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                "Token Source Report",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            )])),
            h,
        );
    }

    render_body(frame, f.body, body, scroll, None, false, theme);

    if let Some(fo) = f.footer {
        render_modal_footer(frame, fo, &[FooterHint::always("Esc", "close")], theme);
    }
    area
}

/// Format a token count with a `k`/`M` suffix for compactness in narrow
/// columns.
fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Truncate a string to fit a column width, appending an ellipsis when cut.
/// (A local copy of `common::truncate_ellipsis` to avoid a borrow mismatch on
/// `name_budget` since that helper takes `&str` widths differently.)
fn truncate_str(s: &str, max: usize) -> String {
    if s.width() <= max {
        s.to_string()
    } else if max <= 1 {
        "…".to_string()
    } else {
        // Byte-truncate near the width budget, then clamp with the ellipsis.
        let mut out = s.chars().take(max.saturating_sub(1)).collect::<String>();
        out.push('…');
        out
    }
}
