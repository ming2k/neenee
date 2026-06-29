//! Two-stage provider/model picker, the API-key / model-id editor, and the
//! custom-provider editor modals.

use std::collections::HashMap;

use neenee_tui::{
    Frame, Paragraph, {Line, Span}, {Modifier, Style},
};
use unicode_width::UnicodeWidthStr;

use crate::tui::layout::LayoutMap;

use super::common::{caret_column, truncate_ellipsis};
use crate::tui::render::Theme;
use crate::tui::render::primitives::{
    FooterHint, modal_area, modal_frame, render_body, render_modal_footer,
};
use crate::tui::{Modal, RankedModel, RankedProvider};

/// Draw the **two-stage** provider/model picker. Mirrors the input-history
/// modal's two-mode (browse/search) design within each stage:
///
/// - **stage 1** (`picker_provider == None`): a ranked *provider* list
///   (favorites → last-used → name). Each row shows the provider, its key-ready
///   glyph, and its active model — plus a `· N ›` count badge for multi-model
///   providers. Enter drills into a multi-model provider (→ stage 2) or
///   activates a single-model one; `*` favorites and `e` edits the row's key.
/// - **stage 2** (`picker_provider == Some(row_idx)`): the model sub-list for
///   the snapshot row at `row_idx`. Enter activates the highlighted model; for a
///   custom provider, `d` removes a model and a trailing "＋ Add model" row adds
///   one. Esc returns to stage 1.
///
/// Within either stage, `/` enters search (the header becomes a `› <query>`
/// field with the real caret and rows highlight the matched characters).
/// `providers` / `models` are the pre-computed stage rows (only the active
/// stage's is non-empty); `modal_index` selects into the active stage. `scroll`
/// is read and written back so the offset stays consistent with the clamped
/// body height; `follow_selection` keeps `modal_index` in view after navigation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_models_modal(
    frame: &mut Frame,
    _layout_map: &mut LayoutMap,
    providers: &[RankedProvider],
    models: &[RankedModel],
    picker_provider: Option<usize>,
    picker_provider_name: Option<&str>,
    stage2_custom: bool,
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

    // Stage 2's title is the provider name with a "‹ back" affordance; stage 1
    // is simply "Providers".
    let title_text: String = match picker_provider_name {
        Some(name) => format!("{name} ‹ back"),
        None => "Providers".to_string(),
    };

    let header_rect = f.header;
    if let Some(h) = header_rect {
        let title = Span::styled(
            title_text.clone(),
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

    // Stage-2 model rows map 1:1 to `modal_index`; stage-1 inserts non-selectable
    // group headers, so the body builder reports the selected row's visual line.
    let (body, follow_line) = if picker_provider.is_some() {
        (
            model_list_body(
                models,
                current_provider,
                current_model,
                stage2_custom,
                modal_index,
                theme,
                f.body.width as usize,
            ),
            modal_index,
        )
    } else {
        provider_list_body(
            providers,
            current_provider,
            key_status,
            modal_index,
            theme,
            f.body.width as usize,
        )
    };
    let follow = if follow_selection {
        Some(follow_line)
    } else {
        None
    };
    render_body(frame, f.body, body, scroll, follow, false, theme);

    if let Some(fo) = f.footer {
        // Stage-2 browse on a custom provider exposes `d` to remove the
        // highlighted model (built-in model lists are curated, so no remove).
        let stage2_custom_browse: &[FooterHint] = if stage2_custom {
            &[
                FooterHint::navigation("↑↓", "navigate"),
                FooterHint::secondary("/", "search"),
                FooterHint::primary("Enter", "activate"),
                FooterHint::secondary("d", "remove"),
                FooterHint::always("Esc", "back"),
            ]
        } else {
            &[
                FooterHint::navigation("↑↓", "navigate"),
                FooterHint::secondary("/", "search"),
                FooterHint::primary("Enter", "activate"),
                FooterHint::always("Esc", "back"),
            ]
        };
        let hints: &[FooterHint] = match (picker_provider.is_some(), search) {
            // Stage 2 (model sub-list): Esc returns to the provider list.
            (true, true) => &[
                FooterHint::secondary("type", "filter"),
                FooterHint::navigation("↑↓", "navigate"),
                FooterHint::primary("Enter", "activate"),
                FooterHint::always("Esc", "back"),
            ],
            (true, false) => stage2_custom_browse,
            // Stage 1 (provider list): Enter opens/activates, Esc closes.
            (false, true) => &[
                FooterHint::secondary("type", "filter"),
                FooterHint::navigation("↑↓", "navigate"),
                FooterHint::primary("Enter", "select"),
                FooterHint::always("Esc", "back"),
            ],
            (false, false) => &[
                FooterHint::navigation("↑↓", "navigate"),
                FooterHint::secondary("/", "search"),
                FooterHint::primary("Enter", "select"),
                FooterHint::secondary("*", "favorite"),
                FooterHint::secondary("e", "edit"),
                FooterHint::always("Esc", "close"),
            ],
        };
        render_modal_footer(frame, fo, hints, theme);
    }

    // The real terminal caret only exists in search mode — browse mode has no
    // editable field. Place it in the header filter field after `<title>  › `.
    if search && let Some(h) = header_rect {
        let prefix = format!("{title_text}  › ").width() as u16;
        let cursor_x = h.x + prefix + caret_column(query, cursor_position);
        let cursor_y = h.y;
        frame.set_cursor_position((cursor_x, cursor_y));
    }
    area
}

/// Build the **stage-1** provider list body. Rows are grouped under dim
/// `Built-in` / `Custom` section headers (non-selectable). Each row is
/// `★ › <provider…>  <model · N ›>`, the provider name padded to a shared
/// column so the model column lines up. The `›` marks the cursor; the current
/// provider's name is underlined. Returns the body lines and the *visual*
/// line index of the selected selectable row (`modal_index`), since the headers
/// offset the 1:1 mapping the scroll-follow relies on.
fn provider_list_body(
    providers: &[RankedProvider],
    current_provider: &str,
    _key_status: &HashMap<String, bool>,
    modal_index: usize,
    theme: &Theme,
    body_width: usize,
) -> (Vec<Line<'static>>, usize) {
    // Fixed prefix: star(2) + marker(2) = 4 columns. The name occupies a
    // shared column (longest name, capped) so every suffix starts at the same x.
    const PREFIX_COLS: usize = 4;
    let avail = body_width.saturating_sub(PREFIX_COLS).max(1);
    let longest_name = providers.iter().map(|p| p.name.width()).max().unwrap_or(0);
    // Leave room for at least a short suffix; clamp the name column so wide
    // terminals don't push the model far to the right.
    let name_col = longest_name.clamp(1, avail.saturating_sub(10).max(1)).min(28);

    let header_line = |label: &str| {
        Line::from(Span::styled(
            format!(" {label}"),
            Style::default()
                .fg(theme.muted())
                .add_modifier(Modifier::BOLD),
        ))
    };

    let mut body: Vec<Line> = Vec::new();
    let mut selected_visual = 0usize;
    let mut prev_builtin: Option<bool> = None;
    for (sel, rp) in providers.iter().enumerate() {
        // Section header at each group boundary (built-ins first, then custom).
        if prev_builtin != Some(rp.builtin) {
            body.push(header_line(if rp.builtin { "Built-in" } else { "Custom" }));
            prev_builtin = Some(rp.builtin);
        }
        if sel == modal_index {
            selected_visual = body.len();
        }

        let is_current = rp.id == current_provider;
        let is_selected = sel == modal_index;
        let g = RowGlyphs::new(theme, is_selected, rp.favorite, is_current);

        // Suffix: the active model's display name, plus a `· N ›` count badge
        // that hints the row drills into the model list. Multi-model providers
        // always drill; a custom (user-defined) provider drills too — even with
        // a single model — because its stage-2 list is the only surface where
        // models can be added/removed. Built-in single-model presets activate
        // directly, so they show no badge.
        let model_name = crate::tui::model_display_name(&rp.model);
        let drills = rp.is_multi_model() || !rp.builtin;
        let suffix = if drills {
            format!("{model_name} · {} ›", rp.models.len())
        } else {
            model_name
        };

        // Pad / truncate the name to the shared column so suffixes align.
        let name = truncate_ellipsis(&rp.label, name_col);
        let pad = name_col.saturating_sub(name.width());

        let matched = match_set(rp.m.as_ref());
        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled(format!(" {}", g.star), g.star_style));
        spans.push(Span::styled(g.marker.to_string(), g.dim_style));
        for (char_idx, c) in name.chars().enumerate() {
            let style = if matched.contains(&char_idx) {
                g.matched_style
            } else {
                g.name_style
            };
            spans.push(Span::styled(c.to_string(), style));
        }
        // Pad to the name column, then the aligned dim suffix.
        spans.push(Span::styled(
            format!("{}  {suffix}", " ".repeat(pad)),
            g.dim_style,
        ));
        body.push(Line::from(spans));
    }

    // Trailing synthetic "＋ Add provider" row (selectable index == providers.len()).
    if modal_index == providers.len() {
        selected_visual = body.len();
    }
    let add_selected = modal_index == providers.len();
    let add_style = if add_selected {
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.brand())
    };
    body.push(Line::from(Span::styled(" ＋ Add provider", add_style)));
    (body, selected_visual)
}

/// Build the **stage-2** model list body for the drilled-into provider. Each row
/// is `› <model display>`, the model name bold; the current model is
/// underlined. In search mode the fuzzy-matched characters are highlighted.
fn model_list_body(
    models: &[RankedModel],
    current_provider: &str,
    current_model: &str,
    stage2_custom: bool,
    modal_index: usize,
    theme: &Theme,
    body_width: usize,
) -> Vec<Line<'static>> {
    if models.is_empty() && !stage2_custom {
        return empty_body(theme);
    }
    let mut body: Vec<Line> = Vec::new();
    for (row, rm) in models.iter().enumerate() {
        let is_current = rm.provider_id == current_provider && rm.model == current_model;
        let is_selected = row == modal_index;
        // Favorite is provider-level; stage 2 lists one provider's models, so the
        // per-row star is suppressed here to keep the model list uncluttered.
        let g = RowGlyphs::new(theme, is_selected, false, is_current);

        // Prefix: marker(2) + indent(2) = 4 columns.
        const PREFIX_COLS: usize = 4;
        let label_max = body_width.saturating_sub(PREFIX_COLS).max(1);
        let label = truncate_ellipsis(&rm.label, label_max);

        let matched = match_set(rm.m.as_ref());
        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled(format!("  {}", g.marker), g.dim_style));
        for (char_idx, c) in label.chars().enumerate() {
            let style = if matched.contains(&char_idx) {
                g.matched_style
            } else {
                g.name_style
            };
            spans.push(Span::styled(c.to_string(), style));
        }
        body.push(Line::from(spans));
    }
    // Custom providers gain a trailing synthetic "＋ Add model" row (index ==
    // model count) so the user can append models after creating the provider.
    if stage2_custom {
        let add_selected = modal_index == models.len();
        let add_style = if add_selected {
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.brand())
        };
        body.push(Line::from(Span::styled(" ＋ Add model", add_style)));
    }
    body
}

/// The "no matches" placeholder body shared by both stages.
fn empty_body(theme: &Theme) -> Vec<Line<'static>> {
    vec![
        Line::from(""),
        Line::from(Span::styled(
            " (no matches — try a shorter or different query)",
            Style::default().fg(theme.muted()),
        )),
    ]
}

/// Char indices the fuzzy match highlights, as a set for O(1) per-char lookup.
fn match_set(m: Option<&crate::tui::fuzzy::FuzzyMatch>) -> std::collections::HashSet<usize> {
    m.map(|m| m.positions.iter().copied().collect())
        .unwrap_or_default()
}

/// The shared per-row glyphs and styles for a picker line, computed once from
/// the row's selected / favorite / current state so both stage bodies render
/// consistently.
///
/// There is no selection *background* and no current-model *glyph dot*. State
/// is conveyed purely through text styling, so the rows stay flat and quiet:
/// - **Selected** (cursor): a leading `›` marker plus brand-colored bold text.
/// - **Current** (the live provider/model): the name is underlined. This reads
///   as "the one that's running" without reserving a fixed glyph column.
/// - **Favorite**: a `★` star in the warning tone.
/// When a row is both selected and current, both cues apply (marker + brand
/// color + underline).
struct RowGlyphs {
    star: &'static str,
    marker: &'static str,
    star_style: Style,
    dim_style: Style,
    name_style: Style,
    matched_style: Style,
}

impl RowGlyphs {
    fn new(theme: &Theme, is_selected: bool, favorite: bool, is_current: bool) -> Self {
        // Selection is a text "ring", not a background fill: the selected row
        // borrows the brand tone (the same color every interactive affordance
        // uses) so it lifts off the panel without darkening its surroundings.
        let select_fg = if is_selected {
            theme.brand()
        } else {
            theme.muted()
        };
        let dim_style = Style::default().fg(select_fg);
        let star_style = if is_selected {
            Style::default().fg(theme.brand())
        } else if favorite {
            Style::default().fg(theme.warn())
        } else {
            Style::default().fg(theme.muted())
        };
        // The name: bold always; brand-colored when selected; underlined when
        // it is the current provider/model (the "live" cue), so current and
        // selected are independently readable.
        let mut name_style = if is_selected {
            Style::default().fg(theme.brand())
        } else {
            Style::default().fg(theme.fg())
        };
        name_style = name_style.add_modifier(Modifier::BOLD);
        if is_current {
            name_style = name_style.add_modifier(Modifier::UNDERLINED);
        }
        let mut matched_style = Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD);
        if is_selected || is_current {
            matched_style = matched_style.add_modifier(Modifier::UNDERLINED);
        }
        Self {
            star: if favorite { "★ " } else { "  " },
            marker: if is_selected { "› " } else { "  " },
            star_style,
            dim_style,
            name_style,
            matched_style,
        }
    }
}

/// Draw the provider key editor: a single **API key** field. The model is chosen
/// from the picker's stage-2 list, so it is not edited here. `input` is the live
/// API-key value borrowed from the composer line.
pub fn draw_model_editor(
    frame: &mut Frame,
    title: &str,
    input: &str,
    cursor_position: usize,
    theme: &Theme,
) -> neenee_tui::Rect {
    let area =
        modal_area(frame, Modal::ModelEditor).expect("model editor modal has fixed geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

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

    let body = vec![Line::from(vec![
        Span::styled(
            format!(" {:<8}", "API key"),
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            if input.is_empty() {
                "enter key…".to_string()
            } else {
                input.to_string()
            },
            Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
        ),
    ])];
    let body_rect = f.body;
    render_body(frame, body_rect, body, &mut 0, None, false, theme);

    if let Some(fo) = f.footer {
        render_modal_footer(
            frame,
            fo,
            &[
                FooterHint::primary("Enter", "save"),
                FooterHint::always("Esc", "cancel"),
            ],
            theme,
        );
    }

    // Caret on the single API-key row, after its label.
    let prefix = format!(" {:<8}", "API key");
    let cursor_x = body_rect.x + prefix.width() as u16 + caret_column(input, cursor_position);
    let cursor_y = body_rect.y;
    frame.set_cursor_position((cursor_x, cursor_y));
    area
}

/// Render the suggestion dropdown shared by the filter fields: up to a few
/// matches, the highlighted one marked `›` in the brand tone, windowed around the
/// highlight so a long list stays navigable. An empty list shows a `(no match)`
/// hint (Enter then uses the typed text).
fn suggestion_lines(suggestions: &[String], highlight: usize, theme: &Theme) -> Vec<Line<'static>> {
    const MAX: usize = 6;
    if suggestions.is_empty() {
        return vec![Line::from(Span::styled(
            "    (no match)".to_string(),
            Style::default().fg(theme.muted()),
        ))];
    }
    let start = if highlight >= MAX {
        highlight + 1 - MAX
    } else {
        0
    };
    suggestions
        .iter()
        .enumerate()
        .skip(start)
        .take(MAX)
        .map(|(i, s)| {
            let (marker, style) = if i == highlight {
                (
                    " › ",
                    Style::default()
                        .fg(theme.brand())
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("   ", Style::default().fg(theme.muted()))
            };
            Line::from(Span::styled(format!("{marker}{s}"), style))
        })
        .collect()
}

/// Draw the add-model overlay for a custom provider: a Model **filter** field
/// (type to filter) plus the matching suggestion list. `↑/↓` move the highlight;
/// Enter adds the highlighted model (or the typed id when nothing matches).
pub fn draw_add_model_editor(
    frame: &mut Frame,
    provider_name: &str,
    suggestions: &[String],
    suggest_index: usize,
    input: &str,
    cursor_position: usize,
    theme: &Theme,
) -> neenee_tui::Rect {
    let area = modal_area(frame, Modal::AddModel).expect("add-model modal has fixed geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    if let Some(h) = f.header {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("＋ Add model · {provider_name}"),
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    const LABEL_W: usize = 8;
    let label_span = Span::styled(
        format!(" {:<LABEL_W$}", "Model"),
        Style::default()
            .fg(theme.brand())
            .add_modifier(Modifier::BOLD),
    );
    let value_span = if input.is_empty() {
        Span::styled(
            "type to filter…".to_string(),
            Style::default().fg(theme.muted()),
        )
    } else {
        Span::styled(
            input.to_string(),
            Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD),
        )
    };
    let mut body = vec![Line::from(vec![label_span, value_span]), Line::from("")];
    body.extend(suggestion_lines(suggestions, suggest_index, theme));

    let body_rect = f.body;
    render_body(frame, body_rect, body, &mut 0, None, false, theme);

    if let Some(fo) = f.footer {
        render_modal_footer(
            frame,
            fo,
            &[
                FooterHint::secondary("type", "filter"),
                FooterHint::navigation("↑↓", "navigate"),
                FooterHint::primary("Enter", "add"),
                FooterHint::always("Esc", "cancel"),
            ],
            theme,
        );
    }

    // Caret on the filter field.
    let prefix = format!(" {:<LABEL_W$}", "Model");
    let cursor_x = body_rect.x + prefix.width() as u16 + caret_column(input, cursor_position);
    frame.set_cursor_position((cursor_x, body_rect.y));
    area
}

/// Everything [`draw_custom_provider_editor`] renders, bundled so the call site
/// stays readable.
pub struct CustomEditorView<'a> {
    /// Focused field index: Name=0, Protocol=1, Base URL=2, Token=3, Model=4.
    pub field: u8,
    /// Edit mode hides the Model field (models are managed in stage 2).
    pub editing: bool,
    pub name_buf: &'a str,
    pub base_url_buf: &'a str,
    pub token_buf: &'a str,
    /// Display label of the committed protocol (shown when Protocol is unfocused).
    pub protocol_label: &'a str,
    /// Display name of the committed model (shown when Model is unfocused).
    pub model_display: &'a str,
    /// Suggestions for the focused filter field (Protocol or Model); empty for
    /// the plain text fields.
    pub suggestions: &'a [String],
    pub suggest_index: usize,
    /// Title for the suggestion block (`"Protocol"` / `"Model"`), or `""`.
    pub suggest_title: &'a str,
    /// The focused field's live value (text buffer, or filter query for the
    /// Protocol / Model fields).
    pub input: &'a str,
    pub cursor_position: usize,
}

/// Draw the custom-provider editor: a Name / Protocol / Base URL / Token (/ Model)
/// form. Protocol and Model are **type-to-filter** fields — focusing one borrows
/// the input line as a filter and renders a suggestion dropdown below the form;
/// `↑/↓` move the highlight (committed live). The Token is masked unless focused.
/// In edit mode the Model field is hidden (models are managed in the stage-2
/// list) and the header reads `Edit · <name>`.
pub fn draw_custom_provider_editor(
    view: CustomEditorView<'_>,
    frame: &mut Frame,
    theme: &Theme,
) -> neenee_tui::Rect {
    let CustomEditorView {
        field,
        editing,
        name_buf,
        base_url_buf,
        token_buf,
        protocol_label,
        model_display,
        suggestions,
        suggest_index,
        suggest_title,
        input,
        cursor_position,
    } = view;

    let area = modal_area(frame, Modal::CustomProvider)
        .expect("custom provider editor modal has fixed geometry");
    let f = modal_frame(frame, area, theme.panel(), true, true);

    const LABEL_W: usize = 9;
    let field_label = |label: &str, focused: bool| {
        let style = if focused {
            Style::default()
                .fg(theme.brand())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted())
        };
        Span::styled(format!(" {label:<LABEL_W$}"), style)
    };
    let value_style = |focused: bool| {
        if focused {
            Style::default().fg(theme.fg()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted())
        }
    };
    let placeholder = |val: String, focused: bool, hint: &str| -> Span<'static> {
        if val.is_empty() && focused {
            Span::styled(hint.to_string(), Style::default().fg(theme.muted()))
        } else {
            Span::styled(val, value_style(focused))
        }
    };
    // A filter field shows the live query (caret) when focused, else its
    // committed value.
    let filter_row = |label: &str, focused: bool, committed: &str, hint: &str| {
        let value = if focused {
            placeholder(input.to_string(), true, hint)
        } else {
            Span::styled(committed.to_string(), value_style(false))
        };
        Line::from(vec![field_label(label, focused), value])
    };
    let text_row = |idx: u8, label: &str, buf: &str, hint: &str, mask: bool| {
        let raw = if field == idx {
            input.to_string()
        } else if mask {
            "•".repeat(buf.chars().count())
        } else {
            buf.to_string()
        };
        Line::from(vec![
            field_label(label, field == idx),
            placeholder(raw, field == idx, hint),
        ])
    };

    if let Some(h) = f.header {
        let title = if editing {
            format!("Edit · {name_buf}")
        } else {
            "＋ Add provider".to_string()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                title,
                Style::default()
                    .fg(theme.brand())
                    .add_modifier(Modifier::BOLD),
            ))),
            h,
        );
    }

    let mut body = vec![
        text_row(0, "Name", name_buf, "e.g. My Relay", false),
        filter_row("Protocol", field == 1, protocol_label, "type to filter…"),
        text_row(
            2,
            "Base URL",
            base_url_buf,
            "https://relay.example.com/v1/chat/completions",
            false,
        ),
        text_row(
            3,
            "Token",
            token_buf,
            if editing {
                "blank = keep existing"
            } else {
                "API key (blank for local)"
            },
            true,
        ),
    ];
    // Model row only in create mode (edit manages models in the stage-2 list).
    if !editing {
        body.push(filter_row(
            "Model",
            field == 4,
            model_display,
            "type to filter…",
        ));
    }
    // Suggestion dropdown for the focused filter field.
    if matches!(field, 1 | 4) {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            format!(" {suggest_title} matches"),
            Style::default().fg(theme.muted()),
        )));
        body.extend(suggestion_lines(suggestions, suggest_index, theme));
    }

    let body_rect = f.body;
    render_body(frame, body_rect, body, &mut 0, None, false, theme);

    if let Some(fo) = f.footer {
        render_modal_footer(
            frame,
            fo,
            &[
                FooterHint::secondary("Tab", "field"),
                FooterHint::navigation("↑↓", "choose"),
                FooterHint::primary("Enter", "save"),
                FooterHint::always("Esc", "cancel"),
            ],
            theme,
        );
    }

    // Caret on the focused field's row (every field borrows the input line —
    // plain text for Name/URL/Token, the filter query for Protocol/Model).
    let row = field as u16; // Name=0, Protocol=1, URL=2, Token=3, Model=4
    let prefix_w = 1 + LABEL_W as u16; // leading space + padded label
    let cursor_x = body_rect.x + prefix_w + caret_column(input, cursor_position);
    let cursor_y = body_rect.y + row;
    frame.set_cursor_position((cursor_x, cursor_y));
    area
}
