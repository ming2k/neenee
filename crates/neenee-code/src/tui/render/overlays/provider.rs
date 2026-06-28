//! Flat model picker and API-key / model-id editor modals.

use std::collections::HashMap;

use neenee_tui::{
    Frame, Paragraph, {Line, Span}, {Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use crate::tui::layout::LayoutMap;

use super::common::{caret_column, truncate_ellipsis};
use crate::tui::render::Theme;
use crate::tui::render::primitives::{
    FooterHint, contrast_fg, modal_area, modal_frame, render_body, render_modal_footer,
};
use crate::tui::{Modal, PROVIDERS, RankedModel};

/// Draw the flat model picker — a single searchable list of every
/// `(provider, model)` pair. Mirrors the input-history modal's two-mode design:
///
/// - **browse** (`search == false`): a plain ranked list (favorites → last-used
///   → name), no editable field, `/` enters search, `*` favorites the row's
///   provider and `e` opens its editor;
/// - **search** (`search == true`): the header becomes a `› <query>` field with
///   the real caret and each row highlights the matched query characters.
///
/// `ranked` is the pre-computed [`RankedModel`] list (from
/// [`crate::tui::App::models_filtered`]); `modal_index` selects into it.
/// `key_status` maps provider id → key-ready for the readiness glyph. `scroll`
/// is read and written back so the offset stays consistent with the clamped
/// body height; `follow_selection` keeps `modal_index` in view after navigation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_models_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    ranked: &[RankedModel],
    current_provider: &str,
    current_model: &str,
    modal_index: usize,
    key_status: &HashMap<String, bool>,
    query: &str,
    cursor_position: usize,
    scroll: &mut usize,
    follow_selection: bool,
    search: bool,
    theme: &Theme,
) -> neenee_tui::Rect {
    let area = modal_area(frame, Modal::Provider).expect("model picker modal has fixed geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    let header_rect = f.header;
    if let Some(h) = header_rect {
        let title = Span::styled(
            "Models",
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

    let body = list_body(
        ranked,
        current_provider,
        current_model,
        key_status,
        modal_index,
        theme,
        f.body.width as usize,
    );
    let follow = if follow_selection {
        Some(modal_index)
    } else {
        None
    };
    render_body(frame, f.body, body, scroll, follow, false, theme);

    if let Some(fo) = f.footer {
        let hints: &[FooterHint] = if search {
            &[
                FooterHint::secondary("type", "filter"),
                FooterHint::navigation("↑↓", "navigate"),
                FooterHint::primary("Enter", "activate"),
                FooterHint::always("Esc", "back"),
            ]
        } else {
            &[
                FooterHint::navigation("↑↓", "navigate"),
                FooterHint::secondary("/", "search"),
                FooterHint::primary("Enter", "activate"),
                FooterHint::secondary("*", "favorite"),
                FooterHint::secondary("e", "edit"),
                FooterHint::always("Esc", "close"),
            ]
        };
        render_modal_footer(frame, fo, hints, theme);
    }

    // The real terminal caret only exists in search mode — browse mode has no
    // editable field. Place it in the header filter field after `Models  › `.
    if search {
        if let Some(h) = header_rect {
            let prefix = "Models  › ".width() as u16;
            let cursor_x = h.x + prefix + caret_column(query, cursor_position);
            let cursor_y = h.y;
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }
    area
}

/// Build the one-line-per-model list body. Each row is
/// `★ ● <key> <model display>  <provider>`, with the model name bold and the
/// provider name dimmed; in search mode the fuzzy-matched characters of the
/// label are underlined/bolded. The match positions index into each row's
/// `label`, so they map directly onto the rendered characters.
fn list_body(
    ranked: &[RankedModel],
    current_provider: &str,
    current_model: &str,
    key_status: &HashMap<String, bool>,
    modal_index: usize,
    theme: &Theme,
    body_width: usize,
) -> Vec<Line<'static>> {
    let mut body: Vec<Line> = Vec::new();
    if ranked.is_empty() {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            " (no matches — try a shorter or different query)",
            Style::default().fg(theme.muted()),
        )));
        return body;
    }

    for (row, rm) in ranked.iter().enumerate() {
        let solution = PROVIDERS[rm.provider_idx];
        let is_current = solution.id == current_provider && rm.model == current_model;
        let is_selected = row == modal_index;
        let bg = if is_selected {
            theme.brand()
        } else {
            theme.panel()
        };
        let sel_fg = contrast_fg(theme.brand());

        let star = if rm.favorite { "★ " } else { "  " };
        let dot = if is_current { "● " } else { "  " };
        let (key_label, key_color) = match key_status.get(solution.id) {
            Some(true) => ("✓ ", theme.ok()),
            Some(false) => ("✗ ", theme.err()),
            None => ("  ", theme.muted()),
        };

        let star_style = if is_selected {
            Style::default().bg(bg).fg(sel_fg)
        } else if rm.favorite {
            Style::default().fg(theme.warn())
        } else {
            Style::default().fg(theme.muted())
        };
        let dim_style = if is_selected {
            Style::default().bg(bg).fg(sel_fg)
        } else {
            Style::default().fg(theme.muted())
        };
        let key_style = if is_selected {
            Style::default().bg(bg).fg(sel_fg)
        } else {
            Style::default().fg(key_color)
        };
        // Model-name characters: bold; provider-name characters: dim.
        let model_style = if is_selected {
            Style::default()
                .bg(bg)
                .fg(sel_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD)
        };
        let provider_style = dim_style;
        let matched_style = if is_selected {
            Style::default()
                .bg(bg)
                .fg(sel_fg)
                .add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
                .bg(bg)
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD)
        };

        // Fixed prefix: star(2) + dot(2) + key(2) = 6 columns.
        const PREFIX_COLS: usize = 6;
        let label_max = body_width.saturating_sub(PREFIX_COLS).max(1);
        let label = truncate_ellipsis(&rm.label, label_max);

        let matched: std::collections::HashSet<usize> =
            rm.m.as_ref()
                .map(|m| m.positions.iter().copied().collect())
                .unwrap_or_default();

        let mut spans: Vec<Span> = Vec::with_capacity(label.chars().count() + 3);
        spans.push(Span::styled(format!(" {star}"), star_style));
        spans.push(Span::styled(dot.to_string(), dim_style));
        spans.push(Span::styled(key_label.to_string(), key_style));
        for (char_idx, c) in label.chars().enumerate() {
            let style = if matched.contains(&char_idx) {
                matched_style
            } else if char_idx < rm.model_w {
                model_style
            } else {
                provider_style
            };
            spans.push(Span::styled(c.to_string(), style));
        }
        body.push(Line::from(spans));
    }
    body
}

/// Draw the unified provider editor Two fields — API key
/// (masked) and model id — with `Tab` cycling focus. The composer input line
/// is borrowed for the focused field's value; `key_buf` / `model_buf` hold the
/// other field while it is unfocused. `field` is `0` for key, `1` for model id.
#[allow(clippy::too_many_arguments)]
pub fn draw_model_editor(
    frame: &mut Frame,
    title: &str,
    field: u8,
    key_buf: &str,
    model_buf: &str,
    input: &str,
    cursor_position: usize,
    theme: &Theme,
) -> neenee_tui::Rect {
    let area =
        modal_area(frame, Modal::ModelEditor).expect("model editor modal has fixed geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    // The focused field's value lives in `input`; the unfocused one in its buf.
    let (key_display, model_display): (String, String) = if field == 0 {
        (input.to_string(), model_buf.to_string())
    } else {
        // Mask the key whenever it is not being actively edited.
        ("•".repeat(key_buf.chars().count()), input.to_string())
    };

    let field_label = |label: &str, focused: bool| {
        if focused {
            Span::styled(
                format!(" {:<8}", label),
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!(" {:<8}", label), Style::default().fg(theme.muted()))
        }
    };
    let value_style = |focused: bool| {
        if focused {
            Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted())
        }
    };

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("Edit · {}", title),
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    let body = vec![
        Line::from(vec![
            field_label("API key", field == 0),
            Span::styled(
                if key_display.is_empty() && field == 0 {
                    "enter key…".to_string()
                } else {
                    key_display.clone()
                },
                value_style(field == 0),
            ),
        ]),
        Line::from(vec![
            field_label("Model id", field == 1),
            Span::styled(
                if model_display.is_empty() && field == 1 {
                    "enter model id…".to_string()
                } else {
                    model_display.clone()
                },
                value_style(field == 1),
            ),
        ]),
    ];
    let body_rect = f.body;
    render_body(frame, body_rect, body, &mut 0, None, false, theme);

    if let Some(fo) = f.footer {
        render_modal_footer(
            frame,
            fo,
            &[
                FooterHint::secondary("Tab", "switch field"),
                FooterHint::primary("Enter", "save"),
                FooterHint::always("Esc", "cancel"),
            ],
            theme,
        );
    }

    // Place the terminal caret on the focused field's value row, after its
    // label. API key is body line 0, Model id is body line 1 (the gap below
    // the header is provided by `modal_frame`, so the body starts at the top
    // of the body rect).
    let prefix = format!(" {:<8}", if field == 0 { "API key" } else { "Model id" });
    let cursor_x = body_rect.x + prefix.width() as u16 + caret_column(input, cursor_position);
    let cursor_y = body_rect.y + if field == 0 { 0 } else { 1 };
    frame.set_cursor_position((cursor_x, cursor_y));
    area
}
