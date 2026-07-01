//! Debug inspector modal — a read-only dry-run snapshot of the request the
//! next turn would send (`/debug context`, dev-only). Three top-level sections
//! (Model & Context, Tools, Messages) browsed with `↑/↓`; `Enter` drills into
//! a section's detail body (scrollable). Esc / outside-click closes.
//!
//! Modelled directly on [`super::token_report`]: the same two-level
//! (list → drill-in) shape, the same content-sized panel + chrome pipeline,
//! and the same `render_body` scroll contract. The snapshot data comes from
//! [`neenee_core::DebugSnapshot`] carried by
//! [`neenee_core::AgentResponse::DebugSnapshot`].

use neenee_core::DebugSnapshot;
use neenee_tui::{Frame, {Line, Span}, {Modifier, Style}};
use unicode_width::UnicodeWidthChar;

use super::common::placeholder;
use crate::modal::Modal;
use crate::render::Theme;
use crate::render::design::MODAL_INNER_H_PADDING;
use crate::render::primitives::{
    FooterHint, content_modal_area, content_modal_probe, modal_chrome_rows, modal_frame,
    modal_spec, render_body, render_modal_footer,
};

/// Which section is drilled into. `None` ⇒ the section list.
pub type DebugDetail = Option<DebugSection>;

/// The three top-level sections of the debug inspector. Order is the display
/// order in the section list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugSection {
    Model,
    Tools,
    Messages,
}

impl DebugSection {
    pub const ALL: [DebugSection; 3] = [DebugSection::Model, DebugSection::Tools, DebugSection::Messages];

    /// Display label for both the list marker and the drill-in title.
    pub fn label(self) -> &'static str {
        match self {
            DebugSection::Model => "Model & Context",
            DebugSection::Tools => "Tools",
            DebugSection::Messages => "Messages",
        }
    }

    fn count(self, snapshot: &DebugSnapshot) -> usize {
        match self {
            DebugSection::Model => 0, // summary, no count badge
            DebugSection::Tools => snapshot.tools.len(),
            DebugSection::Messages => snapshot.messages.len(),
        }
    }

    fn badge(self, snapshot: &DebugSnapshot) -> Option<String> {
        match self {
            DebugSection::Model => None,
            DebugSection::Tools | DebugSection::Messages => Some(format!("{}", self.count(snapshot))),
        }
    }
}

/// Draw the section list, or — when `detail` is `Some(section)` — that
/// section's drill-in body. `section_cursor` is the highlighted row in the
/// list; `detail_scroll` drives the drill-in body. Returns the painted panel
/// rect (for outside-click dismissal registration).
pub fn draw_debug_modal(
    frame: &mut Frame,
    snapshot: &DebugSnapshot,
    section_cursor: usize,
    detail: DebugDetail,
    detail_scroll: &mut usize,
    theme: &Theme,
) -> neenee_tui::Rect {
    let probe = content_modal_probe(frame, Modal::Debug).expect("debug modal has geometry");
    let body_width =
        (probe.width as usize).saturating_sub(2 * MODAL_INNER_H_PADDING as usize).max(1);

    let (title, body, footer): (&str, Vec<Line>, Vec<FooterHint>) = match detail {
        Some(section) => (
            section.label(),
            detail_body(snapshot, section, body_width, theme),
            vec![
                FooterHint::always("↑↓", "scroll"),
                FooterHint::always("Esc", "back"),
            ],
        ),
        None => (
            "Debug · Context Snapshot",
            list_body(snapshot, section_cursor, body_width, theme),
            vec![
                FooterHint::always("↑↓", "select"),
                FooterHint::always("Enter", "details"),
                FooterHint::always("Esc", "close"),
            ],
        ),
    };

    // ── Size the panel to the content and paint it ──
    let spec = modal_spec(Modal::Debug).expect("debug modal has geometry");
    let desired = body.len() as u16 + modal_chrome_rows(spec);
    let area = content_modal_area(frame, Modal::Debug, desired).expect("debug modal has geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            neenee_tui::Paragraph::new(Line::from(vec![Span::styled(
                title,
                Style::default().fg(theme.brand()).add_modifier(Modifier::BOLD),
            )])),
            h,
        );
    }

    render_body(frame, f.body, body, detail_scroll, None, false, theme);

    if let Some(fo) = f.footer {
        render_modal_footer(frame, fo, &footer, theme);
    }
    area
}

// ── Section list ────────────────────────────────────────────────────────

fn list_body<'a>(
    snapshot: &DebugSnapshot,
    sel: usize,
    body_width: usize,
    theme: &Theme,
) -> Vec<Line<'a>> {
    let mut body: Vec<Line> = Vec::new();

    // One-line summary of the capture at the top.
    body.push(summary_line(snapshot, body_width, theme));
    body.push(rule(body_width, theme));

    for (i, section) in DebugSection::ALL.into_iter().enumerate() {
        let selected = i == sel;
        let marker = if selected { "> " } else { "  " };
        let name_style = if selected {
            Style::default().fg(theme.brand()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg())
        };
        let label = section.label();
        let badge = section.badge(snapshot);
        let mut spans = vec![Span::styled(format!("{marker}{label}"), name_style)];
        if let Some(b) = badge {
            spans.push(Span::styled(
                pad_left(body_width, marker.len() + label.len(), b.len(), " "),
                Style::default().fg(theme.muted()),
            ));
            spans.push(Span::styled(b, Style::default().fg(theme.muted())));
        }
        body.push(Line::from(spans));
    }

    body.push(rule(body_width, theme));
    body.push(Line::from(vec![Span::styled(
        format!(
            "Full JSON snapshot persisted to {}",
            truncate_str(&snapshot.file_path, body_width.saturating_sub(33))
        ),
        Style::default().fg(theme.muted()),
    )]));
    body
}

/// The one-line capture summary shown at the top of the section list:
/// provider/model, ~tokens of window (pressure%), message & tool counts.
fn summary_line<'a>(snapshot: &DebugSnapshot, _w: usize, theme: &Theme) -> Line<'a> {
    let window = if snapshot.context_window_tokens > 0 {
        format!("{}", snapshot.context_window_tokens)
    } else {
        "?".to_string()
    };
    Line::from(vec![
        Span::styled(
            format!("{} · {}", snapshot.provider, snapshot.model),
            Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled("~", Style::default().fg(theme.muted())),
        Span::styled(
            format!("{} tokens", snapshot.estimated_tokens),
            Style::default().fg(theme.fg()),
        ),
        Span::styled(
            format!(" of {} ({}%)", window, snapshot.pressure_pct),
            Style::default().fg(theme.muted()),
        ),
    ])
}

// ── Drill-in bodies ─────────────────────────────────────────────────────

fn detail_body<'a>(
    snapshot: &DebugSnapshot,
    section: DebugSection,
    body_width: usize,
    theme: &Theme,
) -> Vec<Line<'a>> {
    match section {
        DebugSection::Model => model_body(snapshot, body_width, theme),
        DebugSection::Tools => tools_body(snapshot, body_width, theme),
        DebugSection::Messages => messages_body(snapshot, body_width, theme),
    }
}

fn model_body<'a>(snapshot: &DebugSnapshot, body_width: usize, theme: &Theme) -> Vec<Line<'a>> {
    let mut body: Vec<Line> = Vec::new();
    let mut kv = |k: &str, v: String| {
        body.push(Line::from(vec![
            Span::styled(format!("{:<18}", k), Style::default().fg(theme.muted())),
            Span::styled(v, Style::default().fg(theme.fg())),
        ]));
    };
    kv("Provider", snapshot.provider.clone());
    kv("Model", snapshot.model.clone());
    kv("Session", snapshot.session_id.clone());
    kv(
        "Context window",
        if snapshot.context_window_tokens > 0 {
            format!("{} tokens", snapshot.context_window_tokens)
        } else {
            "unknown".to_string()
        },
    );
    kv("Estimated tokens", format!("{}", snapshot.estimated_tokens));
    kv("Estimated bytes", format!("{}", snapshot.estimated_bytes));
    kv("Pressure", format!("{}%", snapshot.pressure_pct));
    kv(
        "Pursuit",
        snapshot
            .pursuit
            .as_ref()
            .map(|p| format!("armed · {}", truncate_str(&p.objective, 48)))
            .unwrap_or_else(|| "none".to_string()),
    );
    kv("Captured", snapshot.timestamp.clone());
    body.push(rule(body_width, theme));
    body.push(Line::from(vec![Span::styled(
        "The full request JSON (messages + tool schemas) is in the persisted file.",
        Style::default().fg(theme.muted()),
    )]));
    body
}

fn tools_body<'a>(snapshot: &DebugSnapshot, body_width: usize, theme: &Theme) -> Vec<Line<'a>> {
    if snapshot.tools.is_empty() {
        return vec![placeholder("No tools resolved for the active model.", true, theme.muted())];
    }
    let mut body: Vec<Line> = Vec::new();
    let count_w = 4usize;
    let variant_w = 10usize;
    let name_budget = body_width
        .saturating_sub(2 /* marker */ + variant_w + 2 /* gaps */ + count_w)
        .max(12);
    // Header.
    body.push(Line::from(vec![
        Span::styled(format!("  {:<w$}", "Tool", w = name_budget), muted(theme)),
        Span::styled(format!("{:<w$}", "Variant", w = variant_w), muted(theme)),
        Span::styled(format!("{:>w$}", "Desc", w = count_w), muted(theme)),
    ]));
    body.push(rule(body_width, theme));
    for tool in &snapshot.tools {
        let name = truncate_str(&tool.name, name_budget);
        let variant = truncate_str(&tool.variant, variant_w);
        let desc = truncate_str(&tool.description, 48);
        body.push(Line::from(vec![
            Span::styled(format!("  {name:<w$}", w = name_budget), Style::default().fg(theme.fg())),
            Span::styled(format!("{variant:<w$}", w = variant_w), Style::default().fg(theme.muted())),
            Span::styled(desc, Style::default().fg(theme.dim())),
        ]));
    }
    body
}

fn messages_body<'a>(snapshot: &DebugSnapshot, body_width: usize, theme: &Theme) -> Vec<Line<'a>> {
    if snapshot.messages.is_empty() {
        return vec![placeholder("No messages in the context window.", true, theme.muted())];
    }
    let mut body: Vec<Line> = Vec::new();
    let idx_w = format!("{}", snapshot.messages.len().saturating_sub(1)).len().max(2);
    let role_w = 9usize;
    let tok_w = 6usize;
    let summary_budget = body_width
        .saturating_sub(idx_w + 2 + role_w + 2 + tok_w + 2)
        .max(16);
    // Header.
    body.push(Line::from(vec![
        Span::styled(format!("{:>w$}  ", "#", w = idx_w), muted(theme)),
        Span::styled(format!("{:<w$}  ", "Role", w = role_w), muted(theme)),
        Span::styled(format!("{:>w$}  ", "Tok", w = tok_w), muted(theme)),
        Span::styled("Content", muted(theme)),
    ]));
    body.push(rule(body_width, theme));
    for m in &snapshot.messages {
        let role_style = if m.hidden {
            Style::default().fg(theme.muted())
        } else {
            Style::default().fg(theme.fg())
        };
        let summary = truncate_str(&m.summary, summary_budget);
        body.push(Line::from(vec![
            Span::styled(format!("{:>w$}  ", m.index, w = idx_w), muted(theme)),
            Span::styled(format!("{:<w$}  ", m.role, w = role_w), role_style),
            Span::styled(format!("{:>w$}  ", m.tokens, w = tok_w), muted(theme)),
            Span::styled(summary, role_style),
        ]));
    }
    body
}

// ── small helpers (local copies matching token_report's) ────────────────

fn muted(theme: &Theme) -> Style {
    Style::default().fg(theme.muted())
}

fn rule<'a>(w: usize, theme: &Theme) -> Line<'a> {
    Line::from(vec![Span::styled(
        "─".repeat(w.min(200)),
        Style::default().fg(theme.dim()),
    )])
}

fn truncate_str(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut width = 0usize;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(1).max(1);
        if width + cw > max {
            break;
        }
        width += cw;
        out.push(ch);
    }
    if out.chars().count() < s.chars().count() {
        // Only ellipsize when we actually cut.
        out.push('…');
    }
    out
}

/// Right-pad so `label` is followed by enough spaces to reach `badge`, when
/// the row's total fits within `body_width`. Used to push a count badge to the
/// right edge of the list row.
fn pad_left(body_width: usize, used: usize, badge_w: usize, fill: &str) -> String {
    let target = body_width
        .saturating_sub(used + badge_w)
        .min(80);
    fill.repeat(target)
}
