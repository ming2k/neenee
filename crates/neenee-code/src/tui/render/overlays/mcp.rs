//! MCP manager modal — the interactive MCP-server-list surface.
//!
//! Distinct from [`super::session`] (the read-only MODEL/MCP/SKILLS dashboard)
//! and [`super::tools`] (the per-tool toggle), this is the centered, dismissable
//! overlay opened via the `/mcp` slash command. It lists every configured MCP
//! server with its connection status (connected / disabled / failed) and tool
//! count, with per-row actions: `Space` connects/disconnects the server for the
//! session, and `r` reconnects it. Data comes from the session-context
//! snapshot's `mcp` pane (the same snapshot `/session` and `/tools` use).

use neenee_tui::{
    Frame, Paragraph, {Line, Span}, {Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use super::common::{placeholder, truncate_ellipsis};
use crate::tui::Modal;
use crate::tui::render::Theme;
use crate::tui::render::design::MODAL_INNER_H_PADDING;
use crate::tui::render::primitives::{
    FooterHint, content_modal_area, content_modal_probe, contrast_fg, modal_chrome_rows,
    modal_frame, modal_spec, render_body, render_modal_footer,
};

/// Draw the MCP manager modal: a centered, dismissable, selectable list of the
/// configured MCP servers. Each row shows a status glyph, the server name, a
/// status detail (`N tools` / `disabled` / `failed: …`), and an `[on]`/`[off]`
/// badge. `Space` toggles the selected server; `r` reconnects it. The harness
/// replies with a fresh snapshot that re-renders the list.
pub fn draw_mcp_modal(
    frame: &mut Frame,
    session_context: Option<&neenee_core::SessionContextSnapshot>,
    modal_index: usize,
    scroll: &mut usize,
    follow_selection: bool,
    theme: &Theme,
) -> neenee_tui::Rect {
    // Width is independent of height: probe a full-height rect for the content
    // width, build the list, then size the panel to the content (clamped).
    let probe = content_modal_probe(frame, Modal::Mcp).expect("mcp modal has geometry");
    let body_width = (probe.width as usize)
        .saturating_sub(2 * MODAL_INNER_H_PADDING as usize)
        .max(1);

    let servers = session_context.map(|s| s.mcp.as_slice()).unwrap_or(&[]);

    let mut body: Vec<Line> = Vec::new();
    let mut selected_line: Option<usize> = None;

    if session_context.is_none() {
        body.push(placeholder("Loading MCP servers…", false, theme.muted()));
    } else if servers.is_empty() {
        body.push(placeholder(
            "No MCP servers configured.",
            true,
            theme.muted(),
        ));
    } else {
        const GUTTER_W: usize = 2;
        const PREFIX_W: usize = GUTTER_W + 2; // gutter + "glyph "
        let name_col = servers
            .iter()
            .map(|s| s.name.width())
            .max()
            .unwrap_or(0)
            .clamp(8, 24);
        let badge_w = "[off]".width();
        let detail_budget = body_width
            .saturating_sub(PREFIX_W + name_col + badge_w + 4)
            .max(1);

        for (i, server) in servers.iter().enumerate() {
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

            // Glyph + status detail derive from the connection tri-state. A
            // disabled server reads "off"; connected and failed are both
            // enabled intents, distinguished by glyph and detail.
            let (glyph, glyph_color, state, detail) = if server.disabled {
                ("○", theme.muted(), "off", "disabled".to_string())
            } else if server.connected {
                (
                    "●",
                    theme.ok(),
                    "on",
                    format!(
                        "{} tool{}",
                        server.tool_names.len(),
                        if server.tool_names.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                )
            } else {
                (
                    "✕",
                    theme.err(),
                    "on",
                    format!(
                        "failed: {}",
                        server.failure.as_deref().unwrap_or("not connected")
                    ),
                )
            };
            let glyph_color = if is_sel { fg } else { glyph_color };

            let name = truncate_ellipsis(&server.name, name_col);
            let detail = truncate_ellipsis(&detail, detail_budget);
            let badge = format!("[{state}]");
            let left_w = GUTTER_W + 2 + name_col + 2 + detail.width();
            let pad = body_width.saturating_sub(left_w + badge_w);
            if is_sel {
                selected_line = Some(body.len());
            }
            body.push(Line::from(vec![
                Span::styled(" ".repeat(GUTTER_W), Style::default().bg(bg)),
                Span::styled(format!("{glyph} "), Style::default().bg(bg).fg(glyph_color)),
                Span::styled(
                    format!("{:<w$}  ", name, w = name_col),
                    Style::default().bg(bg).fg(fg),
                ),
                Span::styled(detail, Style::default().bg(bg).fg(dim)),
                Span::styled(" ".repeat(pad), Style::default().bg(bg)),
                Span::styled(badge, Style::default().bg(bg).fg(dim)),
            ]));
        }
    }

    // ── Size the panel to the content and paint it ──
    let spec = modal_spec(Modal::Mcp).expect("mcp modal has geometry");
    let desired = body.len() as u16 + modal_chrome_rows(spec);
    let area = content_modal_area(frame, Modal::Mcp, desired).expect("mcp modal has geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                "MCP servers",
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            )])),
            h,
        );
    }

    let body_rect = f.body;
    let visible = body_rect.height as usize;
    let content_lines = body.len();
    let has_servers = session_context.map(|s| !s.mcp.is_empty()).unwrap_or(false);
    let follow = if has_servers && follow_selection {
        selected_line
    } else {
        None
    };
    render_body(frame, body_rect, body, scroll, follow, false, theme);

    if let Some(fo) = f.footer {
        let hints: &[FooterHint] = if has_servers {
            &[
                FooterHint::navigation("↑↓", "select"),
                FooterHint::primary("Space", "toggle"),
                FooterHint::primary("r", "reconnect"),
                FooterHint::always("Esc", "close"),
            ]
        } else if content_lines > visible {
            &[
                FooterHint::navigation("↑↓", "scroll"),
                FooterHint::always("Esc", "close"),
            ]
        } else {
            &[FooterHint::always("Esc", "close")]
        };
        render_modal_footer(frame, fo, hints, theme);
    }
    area
}
