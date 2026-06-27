//! History search modal.

use neenee_tui::{
    Frame, Modifier, Paragraph, Span, {Line, Style},
};

use super::common::caret_column;
use crate::tui::Modal;
use crate::tui::layout::LayoutMap;
use crate::tui::render::Theme;
use crate::tui::render::primitives::{
    FooterHint, contrast_fg, modal_area, modal_frame, render_body, render_modal_footer,
};
use unicode_width::UnicodeWidthStr;

/// Draw the history search modal.
///
/// `ranked` is the pre-computed `(original_history_index, FuzzyMatch)` list
/// produced by [`crate::tui::App::history_rows`] — passing it in avoids a second
/// fuzzy pass per frame. `modal_index` selects into `ranked`.
/// `scroll` is read AND written back so the caller's offset stays consistent
/// with the clamped body height; `follow_selection` gates whether the body
/// auto-scrolls to keep `modal_index` in view (true after navigation, false
/// once the user scrolls manually). `preview` switches the body from the
/// one-line fuzzy list to a full-text view of the selected entry (toggled by
/// Tab); `scroll` is reused as that entry's per-line scroll.
///
/// `search` selects the modal's mode. In **browse** mode (`false`) the header
/// is a plain title with a `/ to search` hint, the rows are the caller's
/// reverse-chronological list (no highlights), and there is no editable field /
/// caret. In **search** mode (`true`) the header becomes a `› <query>` filter
/// field with the real caret, and each row highlights the matched query chars.
#[allow(clippy::too_many_arguments)]
pub fn draw_history_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    history: &[String],
    query: &str,
    cursor_position: usize,
    ranked: &[(usize, crate::tui::fuzzy::FuzzyMatch)],
    modal_index: usize,
    scroll: &mut usize,
    follow_selection: bool,
    preview: bool,
    search: bool,
    theme: &Theme,
) -> neenee_tui::Rect {
    let area = modal_area(frame, Modal::HistorySearch).expect("history modal has fixed geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    let header_rect = f.header;
    if let Some(h) = header_rect {
        let title = Span::styled(
            "Input History",
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        );
        let header_line = if search {
            // Search sub-layer: the title doubles as the filter field.
            Line::from(vec![
                title,
                Span::raw("  "),
                Span::styled("› ", Style::default().fg(theme.muted())),
                Span::styled(
                    if query.is_empty() {
                        "type to fuzzy-filter"
                    } else {
                        query
                    },
                    Style::default()
                        .fg(if query.is_empty() {
                            theme.muted()
                        } else {
                            theme.fg()
                        })
                        .add_modifier(Modifier::BOLD),
                ),
            ])
        } else {
            // Browse mode: plain title plus a hint to reach search.
            Line::from(vec![
                title,
                Span::raw("  "),
                Span::styled("· / to search", Style::default().fg(theme.muted())),
            ])
        };
        frame.render_widget(Paragraph::new(header_line), h);
    }

    if preview {
        let body = preview_body(history, ranked, modal_index, theme);
        render_body(frame, f.body, body, scroll, None, true, theme);
    } else {
        let body = list_body(history, ranked, modal_index, theme, f.body.width as usize);
        let follow = if follow_selection {
            Some(modal_index)
        } else {
            None
        };
        render_body(frame, f.body, body, scroll, follow, false, theme);
    }

    if let Some(fo) = f.footer {
        let hints: &[FooterHint] = if preview {
            &[
                FooterHint::navigation("↑↓", "next entry"),
                FooterHint::secondary("Tab", "list"),
                FooterHint::primary("Enter", "insert"),
                FooterHint::always("Esc", "close"),
            ]
        } else if search {
            &[
                FooterHint::secondary("type", "filter"),
                FooterHint::navigation("↑↓", "navigate"),
                FooterHint::secondary("Tab", "preview"),
                FooterHint::primary("Enter", "insert"),
                FooterHint::always("Esc", "back"),
            ]
        } else {
            &[
                FooterHint::navigation("↑↓", "navigate"),
                FooterHint::secondary("/", "search"),
                FooterHint::secondary("Tab", "preview"),
                FooterHint::primary("Enter", "insert"),
                FooterHint::always("Esc", "close"),
            ]
        };
        render_modal_footer(frame, fo, hints, theme);
    }

    // Place the real terminal caret in the filter field (the header row, after
    // the `Input History  › ` prefix). Only in search mode — browse mode has no
    // editable field, so it shows no caret. The composer underneath is skipped
    // for this modal, so without this the caret would be absent.
    if search {
        if let Some(h) = header_rect {
            let prefix = "Input History  › ".width() as u16;
            let cursor_x = h.x + prefix + caret_column(query, cursor_position);
            let cursor_y = h.y;
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }
    area
}

/// Build the one-line-per-entry fuzzy list body. Multi-line entries are
/// collapsed to their first line with a trailing ` ↵` marker so a long prompt
/// never breaks the single-row grid; the full text is one Tab away.
fn list_body<'a>(
    history: &'a [String],
    ranked: &'a [(usize, crate::tui::fuzzy::FuzzyMatch)],
    modal_index: usize,
    theme: &Theme,
    body_width: usize,
) -> Vec<Line<'static>> {
    let mut body: Vec<Line> = Vec::new();
    if history.is_empty() {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            " (no history yet — send a message to populate this list)",
            Style::default().fg(theme.muted()),
        )));
        return body;
    }
    if ranked.is_empty() {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            " (no matches — try a shorter or different query)",
            Style::default().fg(theme.muted()),
        )));
        return body;
    }

    // Row-number prefix " 123 " = 6 columns; the " ↵" continuation marker is
    // reserved 2 columns only when actually appended.
    const ROW_NUM_COLS: usize = 6;
    for (row, (orig_idx, m)) in ranked.iter().enumerate() {
        let is_selected = row == modal_index;
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
        let num_style = if is_selected {
            Style::default().bg(bg).fg(contrast_fg(theme.brand()))
        } else {
            Style::default().fg(theme.muted())
        };
        let base_style = Style::default().bg(bg).fg(fg);
        let matched_style = if is_selected {
            Style::default()
                .bg(bg)
                .fg(contrast_fg(theme.brand()))
                .add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
                .bg(bg)
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD)
        };

        let raw = history.get(*orig_idx).map(String::as_str).unwrap_or("");
        // Collapse to a single line: take the first physical line and mark
        // continuation so a multi-line prompt reads as one row. The highlight
        // positions (computed against `raw`) map onto the first line since any
        // character past the first `\n` is dropped before truncation.
        let (first_line, multiline) = match raw.find('\n') {
            Some(i) => (&raw[..i], true),
            None => (raw, false),
        };
        // Reserve room for the continuation glyph before truncating so it
        // never lands outside the panel edge.
        let reserve = if multiline { 2 } else { 0 };
        let entry_max = body_width.saturating_sub(ROW_NUM_COLS + reserve);
        let entry = super::common::truncate_ellipsis(first_line, entry_max);
        let matched: std::collections::HashSet<usize> = m
            .positions
            .iter()
            .copied()
            .filter(|&p| p <= first_line.len())
            .collect();

        let mut spans: Vec<Span> = Vec::with_capacity(entry.chars().count() + 2);
        spans.push(Span::styled(format!(" {:>3} ", row + 1), num_style));
        // Re-derive the char index of each kept character within `first_line`
        // (== the kept prefix of `entry`) since `positions` are char indices
        // into `raw`, and the first line is a prefix of `raw`.
        for (char_idx, c) in entry.chars().enumerate() {
            let style = if matched.contains(&char_idx) {
                matched_style
            } else {
                base_style
            };
            spans.push(Span::styled(c.to_string(), style));
        }
        if multiline {
            spans.push(Span::styled(" ↵", Style::default().bg(bg).fg(num_style.fg)));
        }
        body.push(Line::from(spans));
    }
    body
}

/// Build the full-text preview body for the focused entry. The entry is laid
/// out verbatim (one `Line` per physical line) with the fuzzy-match positions
/// highlighted on whichever lines they fall; ↑/↓ move to the next entry and
/// the renderer re-anchors its own scroll to the top.
fn preview_body<'a>(
    history: &'a [String],
    ranked: &'a [(usize, crate::tui::fuzzy::FuzzyMatch)],
    modal_index: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let Some((orig_idx, m)) = ranked.get(modal_index) else {
        return vec![Line::from(Span::styled(
            " (no entry selected)",
            Style::default().fg(theme.muted()),
        ))];
    };
    let raw = history.get(*orig_idx).map(String::as_str).unwrap_or("");
    let matched: std::collections::HashSet<usize> = m.positions.iter().copied().collect();

    let body_style = Style::default().fg(theme.fg());
    let matched_style = Style::default()
        .fg(theme.brand())
        .add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line> = Vec::new();
    let mut char_idx = 0usize;
    for line in raw.split('\n') {
        let mut spans: Vec<Span> = Vec::with_capacity(line.chars().count());
        for c in line.chars() {
            let style = if matched.contains(&char_idx) {
                matched_style
            } else {
                body_style
            };
            spans.push(Span::styled(c.to_string(), style));
            char_idx += 1;
        }
        lines.push(Line::from(spans));
        char_idx += 1; // the consumed `\n`
    }
    lines
}
