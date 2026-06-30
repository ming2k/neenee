//! Token-source report modal — an itemized "bill" of token usage per
//! provider+model, with a per-model drill-in showing the upstream-vs-estimated
//! split, every round's line items, and Anthropic prompt-cache efficiency.
//!
//! Opened by clicking the context meter in the hint bar. ↑/↓ select a line,
//! Enter opens its detail, Esc backs out / closes. The data is a live snapshot
//! of the shared `TokenSourceLedger`, so it reflects every turn booked so far
//! this session.

use neenee_core::TokenSourceReport;
use neenee_tui::{
    Color, Frame, Paragraph, {Line, Span}, {Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use super::common::placeholder;
use crate::modal::Modal;
use crate::render::Theme;
use crate::render::design::MODAL_INNER_H_PADDING;
use crate::render::primitives::{
    FooterHint, content_modal_area, content_modal_probe, modal_chrome_rows, modal_frame,
    modal_spec, render_body, render_modal_footer,
};

/// Draw the token bill (list) or, when `detail` is set, the per-model breakdown
/// for `report.rows[selected]`. `selected` is the highlighted line in the bill;
/// `scroll` drives the detail body. Returns the painted panel rect.
pub fn draw_token_report_modal(
    frame: &mut Frame,
    report: &TokenSourceReport,
    selected: usize,
    detail: bool,
    scroll: &mut usize,
    theme: &Theme,
) -> neenee_tui::Rect {
    // Probe the content width so column layout adapts to the terminal.
    let probe =
        content_modal_probe(frame, Modal::TokenReport).expect("token-report modal has geometry");
    let body_width = (probe.width as usize)
        .saturating_sub(2 * MODAL_INNER_H_PADDING as usize)
        .max(1);

    let drill = detail && !report.rows.is_empty();
    let sel = selected.min(report.rows.len().saturating_sub(1));

    let (title, body, footer): (&str, Vec<Line>, Vec<FooterHint>) = if drill {
        (
            "Token Detail",
            detail_body(report, sel, body_width, theme),
            vec![
                FooterHint::always("↑↓", "scroll"),
                FooterHint::always("Esc", "back"),
            ],
        )
    } else if report.rows.is_empty() {
        (
            "Token Bill",
            list_body(report, sel, body_width, theme),
            vec![FooterHint::always("Esc", "close")],
        )
    } else {
        (
            "Token Bill",
            list_body(report, sel, body_width, theme),
            vec![
                FooterHint::always("↑↓", "select"),
                FooterHint::always("Enter", "details"),
                FooterHint::always("Esc", "close"),
            ],
        )
    };

    // ── Size the panel to the content and paint it ──
    let spec = modal_spec(Modal::TokenReport).expect("token-report modal has geometry");
    let desired = body.len() as u16 + modal_chrome_rows(spec);
    let area = content_modal_area(frame, Modal::TokenReport, desired)
        .expect("token-report modal has geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                title,
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            )])),
            h,
        );
    }

    render_body(frame, f.body, body, scroll, None, false, theme);

    if let Some(fo) = f.footer {
        render_modal_footer(frame, fo, &footer, theme);
    }
    area
}

/// The bill: one selectable line per provider+model, plus a grand total.
fn list_body<'a>(
    report: &TokenSourceReport,
    sel: usize,
    body_width: usize,
    theme: &Theme,
) -> Vec<Line<'a>> {
    let mut body: Vec<Line> = Vec::new();

    if report.rows.is_empty() {
        body.push(placeholder(
            "No token usage recorded yet — send a message first.",
            true,
            theme.muted(),
        ));
        return body;
    }

    const TOTAL_W: usize = 12;
    const SRC_W: usize = 11;
    // 2 leading marker cols + 2 single-space gaps.
    let name_budget = body_width.saturating_sub(TOTAL_W + SRC_W + 4).max(12);

    // Header row.
    body.push(Line::from(vec![
        Span::styled(
            format!("  {:<w$}", "Provider / Model", w = name_budget),
            Style::default().fg(theme.muted()),
        ),
        Span::styled(
            format!("{:>w$}", "Tokens", w = TOTAL_W),
            Style::default().fg(theme.muted()),
        ),
        Span::styled(
            format!(" {:>w$}", "Source", w = SRC_W),
            Style::default().fg(theme.muted()),
        ),
    ]));
    body.push(rule(body_width, theme));

    // One selectable line per provider+model.
    for (i, row) in report.rows.iter().enumerate() {
        let selected = i == sel;
        let marker = if selected { "> " } else { "  " };
        let label = truncate_str(&format!("{} · {}", row.provider, row.model), name_budget);
        let (src_text, src_color) = source_label(
            row.totals.reported_tokens,
            row.totals.estimated_tokens,
            theme,
        );
        let name_style = if selected {
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg())
        };
        body.push(Line::from(vec![
            Span::styled(
                format!("{marker}{:<w$}", label, w = name_budget),
                name_style,
            ),
            Span::styled(
                format!("{:>w$}", fmt_tokens(row.totals.total()), w = TOTAL_W),
                Style::default().fg(theme.fg()),
            ),
            Span::styled(
                format!(" {:>w$}", src_text, w = SRC_W),
                Style::default().fg(src_color),
            ),
        ]));
    }

    // Grand-total line.
    body.push(rule(body_width, theme));
    body.push(Line::from(vec![
        Span::styled(
            format!("  {:<w$}", "Total", w = name_budget),
            Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "{:>w$}",
                fmt_tokens(report.grand_total.total()),
                w = TOTAL_W
            ),
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {:>w$}", "", w = SRC_W), Style::default()),
    ]));

    body.push(Line::from(""));
    body.push(Line::from(Span::styled(
        "Source: % real = share of tokens from the provider's usage object (vs. local estimate).",
        Style::default().fg(theme.muted()),
    )));
    body.push(Line::from(Span::styled(
        "Select a line and press Enter to see its rounds and cache efficiency.",
        Style::default().fg(theme.muted()),
    )));
    body
}

/// The drill-in for one provider+model: source split, cache efficiency, and a
/// per-round line-item table.
fn detail_body<'a>(
    report: &TokenSourceReport,
    sel: usize,
    body_width: usize,
    theme: &Theme,
) -> Vec<Line<'a>> {
    let row = &report.rows[sel];
    let t = &row.totals;
    let mut body: Vec<Line> = Vec::new();

    body.push(Line::from(Span::styled(
        format!("{} · {}", row.provider, row.model),
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD),
    )));
    body.push(Line::from(""));

    let total = t.total().max(1);
    let pct_real = (t.reported_tokens as f64 / total as f64 * 100.0).round() as i64;
    body.push(kv(
        "Reported (upstream)",
        &format!("{}  ({pct_real}% of total)", fmt_tokens(t.reported_tokens)),
        theme.ok(),
        theme,
    ));
    body.push(kv(
        "Estimated (local)",
        &fmt_tokens(t.estimated_tokens),
        if t.estimated_tokens > 0 {
            theme.warn()
        } else {
            theme.muted()
        },
        theme,
    ));
    body.push(kv(
        "Input / Output",
        &format!(
            "{} / {}",
            fmt_tokens(t.prompt_tokens),
            fmt_tokens(t.completion_tokens)
        ),
        theme.fg(),
        theme,
    ));

    if t.cache_read_tokens > 0 || t.cache_write_tokens > 0 {
        // Hit-rate = cache-read / (cache-read + reported uncached input). The
        // uncached input is the reported total minus the two cache portions.
        let uncached = (t.reported_tokens - t.cache_read_tokens - t.cache_write_tokens).max(0);
        let denom = (t.cache_read_tokens + uncached).max(1) as f64;
        let hit = (t.cache_read_tokens as f64 / denom * 100.0).round() as i64;
        body.push(kv(
            "Cache read / write",
            &format!(
                "{} / {}",
                fmt_tokens(t.cache_read_tokens),
                fmt_tokens(t.cache_write_tokens)
            ),
            theme.ok(),
            theme,
        ));
        body.push(kv(
            "Cache hit-rate",
            &format!("{hit}%  (cache_control breakpoints landing)"),
            if hit >= 50 { theme.ok() } else { theme.muted() },
            theme,
        ));
    }

    body.push(Line::from(""));
    body.push(Line::from(Span::styled(
        "Per-round line items",
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD),
    )));
    body.push(rule(body_width, theme));
    body.push(Line::from(Span::styled(
        format!(
            "{:>3}  {:<9}{:>9}{:>9}{:>9}   {:<13}",
            "#", "Source", "Input", "Output", "Total", "Cache r/w"
        ),
        Style::default().fg(theme.muted()),
    )));

    if row.rounds.is_empty() {
        body.push(placeholder("No per-round detail.", true, theme.muted()));
    }
    for (i, r) in row.rounds.iter().enumerate() {
        let (src, src_color) = if r.reported {
            ("upstream", theme.ok())
        } else {
            ("local", theme.warn())
        };
        let (input, output) = if r.reported {
            (fmt_tokens(r.prompt_tokens), fmt_tokens(r.completion_tokens))
        } else {
            ("—".to_string(), "—".to_string())
        };
        let cache = if r.cache_read_tokens > 0 || r.cache_write_tokens > 0 {
            format!(
                "{}/{}",
                fmt_tokens(r.cache_read_tokens),
                fmt_tokens(r.cache_write_tokens)
            )
        } else {
            "—".to_string()
        };
        body.push(Line::from(vec![
            Span::styled(
                format!("{:>3}  ", i + 1),
                Style::default().fg(theme.muted()),
            ),
            Span::styled(format!("{:<9}", src), Style::default().fg(src_color)),
            Span::styled(
                format!("{:>9}{:>9}{:>9}", input, output, fmt_tokens(r.total_tokens)),
                Style::default().fg(theme.fg()),
            ),
            Span::styled(
                format!("   {:<13}", cache),
                Style::default().fg(theme.muted()),
            ),
        ]));
    }

    body.push(Line::from(""));
    body.push(Line::from(Span::styled(
        "Upstream = authoritative provider usage; local = char-class estimate.",
        Style::default().fg(theme.muted()),
    )));
    body
}

/// A full-width horizontal rule line.
fn rule<'a>(w: usize, theme: &Theme) -> Line<'a> {
    Line::from(Span::styled(
        "─".repeat(w),
        Style::default().fg(theme.muted()),
    ))
}

/// A muted `key` + colored `value` line for the detail summary.
fn kv<'a>(k: &str, v: &str, vcolor: Color, theme: &Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{:<22}", k), Style::default().fg(theme.muted())),
        Span::styled(v.to_string(), Style::default().fg(vcolor)),
    ])
}

/// The "Source" cell for a bill line: how much of this row is authoritative.
fn source_label(reported: i64, estimated: i64, theme: &Theme) -> (String, Color) {
    let total = (reported + estimated).max(1);
    if reported > 0 && estimated == 0 {
        ("100% real".to_string(), theme.ok())
    } else if reported > 0 {
        let pct = (reported as f64 / total as f64 * 100.0).round() as i64;
        (format!("{pct}% real"), theme.warn())
    } else {
        ("estimated".to_string(), theme.muted())
    }
}

/// Format a token count with a `k`/`M` suffix for compactness in narrow columns.
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
fn truncate_str(s: &str, max: usize) -> String {
    if s.width() <= max {
        s.to_string()
    } else if max <= 1 {
        "…".to_string()
    } else {
        let mut out = s.chars().take(max.saturating_sub(1)).collect::<String>();
        out.push('…');
        out
    }
}
