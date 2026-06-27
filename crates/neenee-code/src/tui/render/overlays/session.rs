//! Sessions picker and the session-context dashboard modal.

use std::collections::HashMap;

use neenee_tui::{
    Frame, Paragraph, {Line, Span}, {Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use super::common::{McpRow, relative_time_compact, truncate_ellipsis};
use crate::tui::render::Theme;
use crate::tui::render::design::{MODAL_BODY_LEADING_INDENT, MODAL_INNER_H_PADDING};
use crate::tui::render::primitives::{
    centered_rect, centered_rect_h, contrast_fg, modal_frame, render_body, viewport_rect,
};

/// Draw the sessions picker: each row shows the session overview plus its
/// creation and last-interaction times. Enter opens the selected session.
pub fn draw_sessions_modal(
    frame: &mut Frame,
    sessions: &[neenee_core::SessionOverview],
    selected: usize,
    theme: &Theme,
) {
    let area = centered_rect(80, 64, viewport_rect(frame));
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Sessions",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    let body_width = f.body.width as usize;
    let mut body: Vec<Line> = Vec::new();

    if sessions.is_empty() {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            "No previous sessions yet.",
            Style::default().fg(theme.muted()),
        )));
    }

    for (i, session) in sessions.iter().enumerate() {
        let is_selected = i == selected;
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
        let muted = if is_selected {
            contrast_fg(theme.brand())
        } else {
            theme.muted()
        };
        let badge = if session.active { "● " } else { "  " };
        // Drop the message count (low signal) and use compact relative times
        // (no "ago" suffix) so the meta column stays narrow and predictable.
        let meta = format!(
            "created {} · active {}",
            relative_time_compact(session.created_at),
            relative_time_compact(session.updated_at),
        );
        let meta_w = meta.width();
        // Guarantee a fixed gutter between the two columns by giving the
        // overview a width budget of `body_width - meta_w - gutter`, then
        // truncating it with an ellipsis when it overflows. That way a long
        // overview never crowds the meta column, and the gutter is constant
        // row-to-row instead of whatever slack is left over.
        const COL_GUTTER: usize = 2;
        let badge_w = badge.width();
        let col1_budget = body_width.saturating_sub(meta_w + COL_GUTTER);
        let overview = truncate_ellipsis(&session.overview, col1_budget.saturating_sub(badge_w));
        let left = format!("{}{}", badge, overview);
        let left_w = left.width();
        let pad = body_width.saturating_sub(left_w + meta_w);
        let spans = vec![
            Span::styled(left, Style::default().bg(bg).fg(fg)),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
            Span::styled(meta, Style::default().bg(bg).fg(muted)),
        ];
        body.push(Line::from(spans));
    }

    render_body(frame, f.body, body, &mut 0, Some(selected), false, theme);

    if let Some(fo) = f.footer {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "↑↓ navigate · Enter open · d delete · Esc close",
                Style::default().fg(theme.muted()),
            ))),
            fo,
        );
    }
}

/// Greedy word-wrap of `text` into lines no wider than `width` display columns.
/// Used to pre-split prose (the model description, an MCP failure reason) so the
/// dashboard's content height can be measured exactly before the modal is sized.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in text.split_whitespace() {
        let ww = word.width();
        if cur_w == 0 {
            cur.push_str(word);
            cur_w = ww;
        } else if cur_w + 1 + ww <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + ww;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_w = ww;
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Draw the session-context modal: a single scrollable **dashboard** of the
/// live session — no tabs.
///
/// Layout: a borderless solid-bg panel sized to its content (clamped to the
/// viewport), split into **header** (`Session` title), **body**, and **footer**
/// (keybinding hints). The body stacks four labelled sections top to bottom —
/// `MODEL`, `MCP`, `SKILLS`, `TOOLS` — each a brand-colored heading (with a
/// count) over indented rows, so the whole session reads at a glance instead of
/// hiding three-quarters of it behind tab switches.
///
/// `session_context` is the latest snapshot from the harness; when `None` (still
/// loading) the Model section falls back to the App-level `provider` / `model` /
/// `key_status` and MCP falls back to `mcp_statuses`, while Skills / Tools show a
/// "Loading…" placeholder.
///
/// Only the `TOOLS` rows are interactive: `modal_index` is the selection cursor
/// over the tool list (the sole toggleable surface), the selected row takes the
/// brand highlight, and the body auto-scrolls to keep it in view while
/// `follow_selection` is latched (cleared the moment the user scrolls manually).
/// `scroll` is read AND written back, clamped to the body's real height.
#[allow(clippy::too_many_arguments)]
pub fn draw_session_modal(
    frame: &mut Frame,
    provider: &str,
    model: &str,
    key_status: &HashMap<String, bool>,
    mcp_statuses: &[(String, neenee_core::mcp::McpConnectionStatus)],
    session_context: Option<&neenee_core::SessionContextSnapshot>,
    modal_index: usize,
    scroll: &mut usize,
    follow_selection: bool,
    theme: &Theme,
) {
    const MODAL_W: u16 = 76;
    const MIN_H: u16 = 9;
    let viewport = viewport_rect(frame);

    // Width is independent of height, so probe a full-height rect first to learn
    // the body's content width, then build the dashboard, then size the panel to
    // the content (clamped) so there is no slab of dead space below it.
    let probe = centered_rect(MODAL_W, 100, viewport);
    let body_width = (probe.width as usize)
        .saturating_sub(2 * MODAL_INNER_H_PADDING as usize)
        .max(1);

    let indent = " ".repeat(MODAL_BODY_LEADING_INDENT);
    let row_budget = body_width.saturating_sub(MODAL_BODY_LEADING_INDENT);

    // Section heading: bold brand label, optionally suffixed with a `· N` count.
    let heading = |title: &str, count: Option<usize>| -> Line<'static> {
        let text = match count {
            Some(n) => format!("{title}  ·  {n}"),
            None => title.to_string(),
        };
        Line::from(Span::styled(
            text,
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        ))
    };
    let blank = || Line::from("");
    let muted_row = |text: String| -> Line<'static> {
        Line::from(Span::styled(
            format!("{indent}{text}"),
            Style::default().fg(theme.muted()),
        ))
    };

    let mut body: Vec<Line> = Vec::new();
    // Body line index of the selected tool row, for follow-scroll.
    let mut selected_line: Option<usize> = None;

    // ── MODEL ──
    body.push(heading("MODEL", None));
    let (p_name, m_display, m_id, window, key_ok, desc, caps) = match session_context {
        Some(s) => (
            s.model.provider.clone(),
            s.model.display_name.clone(),
            s.model.model.clone(),
            s.model.context_window,
            s.model.api_key_ready,
            s.model.description.clone(),
            s.model.capabilities.clone(),
        ),
        None => {
            let solution = crate::tui::PROVIDERS.iter().find(|x| x.id == provider);
            (
                provider.to_string(),
                crate::tui::model_display_name(model),
                model.to_string(),
                crate::tui::provider_context_window(provider),
                key_status
                    .get(&provider.to_lowercase())
                    .copied()
                    .unwrap_or(false),
                solution
                    .map(|s| s.description.to_string())
                    .unwrap_or_default(),
                Vec::new(),
            )
        }
    };
    // Name (bold) · provider (muted) on one row.
    body.push(Line::from(vec![
        Span::raw(indent.clone()),
        Span::styled(
            truncate_ellipsis(&m_display, row_budget.saturating_sub(p_name.width() + 3)),
            Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  ·  {p_name}"), Style::default().fg(theme.muted())),
    ]));
    // Model id (dim), only when it differs from the display name.
    if m_id != m_display {
        body.push(Line::from(Span::styled(
            format!("{indent}{}", truncate_ellipsis(&m_id, row_budget)),
            Style::default().fg(theme.dim()),
        )));
    }
    // Meta: context · key state · capabilities, on one mixed-color row.
    let window_label = if window >= 1_000_000 {
        format!("{:.1}M context", window as f64 / 1_000_000.0)
    } else if window >= 1_000 {
        format!("{:.0}K context", window as f64 / 1_000.0)
    } else {
        "context unknown".to_string()
    };
    let (key_mark, key_fg, key_word) = if key_ok {
        ("✓", theme.ok(), "key ready")
    } else {
        ("✗", theme.err(), "no key")
    };
    let mut meta: Vec<Span> = vec![
        Span::raw(indent.clone()),
        Span::styled(window_label, Style::default().fg(theme.fg())),
        Span::styled("  ·  ", Style::default().fg(theme.dim())),
        Span::styled(
            format!("{key_mark} {key_word}"),
            Style::default().fg(key_fg),
        ),
    ];
    if !caps.is_empty() {
        meta.push(Span::styled("  ·  ", Style::default().fg(theme.dim())));
        meta.push(Span::styled(
            caps.join(" · "),
            Style::default().fg(theme.muted()),
        ));
    }
    body.push(Line::from(meta));
    if !desc.is_empty() {
        for line in wrap_words(&desc, row_budget) {
            body.push(muted_row(line));
        }
    }

    // ── MCP ──
    let mcp_rows: Vec<(String, McpRow)> = match session_context {
        Some(s) => s
            .mcp
            .iter()
            .map(|srv| {
                let row = if srv.disabled {
                    McpRow::disabled()
                } else if let Some(reason) = srv.failure.as_ref() {
                    McpRow::failed(reason.clone())
                } else if srv.connected {
                    McpRow::connected(srv.tool_names.clone())
                } else {
                    McpRow::disabled()
                };
                (srv.name.clone(), row)
            })
            .collect(),
        None => mcp_statuses
            .iter()
            .map(|(name, status)| {
                use neenee_core::mcp::McpConnectionStatus;
                let row = match status {
                    McpConnectionStatus::Connected { tools } => {
                        McpRow::connected(vec![format!("+{tools} more")])
                    }
                    McpConnectionStatus::Disabled => McpRow::disabled(),
                    McpConnectionStatus::Failed(r) => McpRow::failed(r.clone()),
                };
                (name.clone(), row)
            })
            .collect(),
    };
    // Shared name-column width so the MCP / SKILLS / TOOLS rows line their
    // second column (status / description / source) up into one tidy table,
    // capped so a single long name can't push it off-screen.
    // Geometry of every list row: indent + 2-col selection gutter + glyph(1) +
    // space(1) + name(name_col) + 2-col gap + second column.
    const GUTTER_W: usize = 2;
    let prefix_w = GUTTER_W + 2 + 2; // gutter + "glyph " + trailing gap
    let name_col = session_context
        .map(|s| {
            s.mcp
                .iter()
                .map(|m| m.name.width())
                .chain(s.skills.iter().map(|k| k.name.width()))
                .chain(s.tools.iter().map(|t| t.name.width()))
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
        .clamp(8, row_budget.saturating_sub(prefix_w + 8).max(8))
        .min(28);
    let second_col = row_budget.saturating_sub(prefix_w + name_col).max(1);
    let gutter = " ".repeat(GUTTER_W);

    body.push(blank());
    body.push(heading("MCP", Some(mcp_rows.len())));
    if mcp_rows.is_empty() {
        body.push(muted_row("No MCP servers configured.".to_string()));
    } else {
        for (name, row) in &mcp_rows {
            let (word, color, glyph) = row.dashboard_summary(theme);
            body.push(Line::from(vec![
                Span::raw(format!("{indent}{gutter}")),
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(
                    format!("{:<w$}  ", truncate_ellipsis(name, name_col), w = name_col),
                    Style::default().fg(theme.fg()),
                ),
                Span::styled(
                    truncate_ellipsis(&word, second_col),
                    Style::default().fg(color),
                ),
            ]));
        }
    }

    // ── SKILLS ──
    let skills = session_context.map(|s| s.skills.as_slice()).unwrap_or(&[]);
    body.push(blank());
    body.push(heading("SKILLS", session_context.map(|_| skills.len())));
    if session_context.is_none() {
        body.push(muted_row("Loading…".to_string()));
    } else if skills.is_empty() {
        body.push(muted_row("No skills loaded.".to_string()));
    } else {
        for skill in skills {
            let (glyph, color) = if skill.enabled {
                ("●", theme.ok())
            } else {
                ("○", theme.muted())
            };
            body.push(Line::from(vec![
                Span::raw(format!("{indent}{gutter}")),
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(
                    format!(
                        "{:<w$}  ",
                        truncate_ellipsis(&skill.name, name_col),
                        w = name_col
                    ),
                    Style::default().fg(theme.fg()),
                ),
                Span::styled(
                    truncate_ellipsis(&skill.description, second_col),
                    Style::default().fg(theme.muted()),
                ),
            ]));
        }
    }

    // ── TOOLS (interactive) ──
    let tools = session_context.map(|s| s.tools.as_slice()).unwrap_or(&[]);
    body.push(blank());
    body.push(heading("TOOLS", session_context.map(|_| tools.len())));
    if session_context.is_none() {
        body.push(muted_row("Loading…".to_string()));
    } else if tools.is_empty() {
        body.push(muted_row("No tools available.".to_string()));
    } else {
        for (i, tool) in tools.iter().enumerate() {
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
            let glyph_color = if is_sel {
                fg
            } else if tool.enabled {
                theme.ok()
            } else {
                theme.muted()
            };
            let glyph = if tool.enabled { "●" } else { "○" };
            let state = if tool.enabled { "on" } else { "off" };
            let name = truncate_ellipsis(&tool.name, name_col);
            // `[state]` pinned to the row's right edge; the source fills the gap
            // between the name column and that badge.
            let badge = format!("[{state}]");
            let src = truncate_ellipsis(&tool.source, second_col.saturating_sub(badge.width() + 1));
            // Left-packed content: gutter + glyph + name col + source.
            let left_w = GUTTER_W + 2 + name_col + 2 + src.width();
            let pad = body_width.saturating_sub(MODAL_BODY_LEADING_INDENT + left_w + badge.width());
            let caret = if is_sel { "▸ " } else { "  " };
            if is_sel {
                selected_line = Some(body.len());
            }
            body.push(Line::from(vec![
                Span::styled(indent.clone(), Style::default().bg(bg)),
                Span::styled(caret, Style::default().bg(bg).fg(fg)),
                Span::styled(format!("{glyph} "), Style::default().bg(bg).fg(glyph_color)),
                Span::styled(
                    format!("{:<w$}  ", name, w = name_col),
                    Style::default().bg(bg).fg(fg),
                ),
                Span::styled(src, Style::default().bg(bg).fg(dim)),
                Span::styled(" ".repeat(pad), Style::default().bg(bg)),
                Span::styled(badge, Style::default().bg(bg).fg(dim)),
            ]));
        }
    }

    // ── Size the panel to the content and paint it ──
    // chrome = v-padding(2) + header(1) + gap(1) + gap(1) + footer(1) = 6 rows.
    const CHROME: u16 = 6;
    let desired = body.len() as u16 + CHROME;
    let max_h = (viewport.height as f32 * 0.82) as u16;
    let height = desired.clamp(MIN_H, max_h.max(MIN_H));
    let area = centered_rect_h(MODAL_W, height, viewport);
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Session",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    let body_rect = f.body;
    let visible = body_rect.height as usize;
    let content_lines = body.len();
    let has_tools = session_context
        .map(|s| !s.tools.is_empty())
        .unwrap_or(false);
    let follow = if has_tools && follow_selection {
        selected_line
    } else {
        None
    };
    render_body(frame, body_rect, body, scroll, follow, false, theme);

    if let Some(fo) = f.footer {
        let mut hint = String::new();
        if has_tools {
            hint.push_str("↑↓ select · Space toggle · ");
        } else if content_lines > visible {
            hint.push_str("↑↓ scroll · ");
        }
        hint.push_str("Esc close");
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(theme.muted()),
            ))),
            fo,
        );
    }
}
