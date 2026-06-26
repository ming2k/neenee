//! Tiny shared render helpers: viewport math, modal centering/recess, panel
//! chrome, and color arithmetic. Kept in one place so the per-component
//! modules do not need to depend on each other for these primitives.

use crate::tui::app::Recess;
use neenee_tui::{
    Constraint, Direction, Frame, Layout, Line, Margin, Rect, {Block as RtBlock, Clear, Paragraph},
    {Color, Style},
};

use super::Theme;
use super::design::{MODAL_INNER_H_PADDING, MODAL_INNER_V_PADDING, PANEL_BAR_INSET, SCROLLBAR_GAP};

/// Global viewport margin. Only vertical breathing room (1 cell top and
/// bottom) is reserved; horizontally every component spans the full terminal
/// width.
pub(super) const VIEWPORT_H_MARGIN: u16 = 0;
pub(super) const VIEWPORT_V_MARGIN: u16 = 1;

/// The usable area after reserving the global viewport margins (1 cell top
/// and bottom). The full `frame.size()` is only used to paint the app
/// background and the modal backdrop.
pub(super) fn viewport_rect(frame: &Frame) -> Rect {
    frame.area().inner(neenee_tui::Margin {
        horizontal: VIEWPORT_H_MARGIN,
        vertical: VIEWPORT_V_MARGIN,
    })
}

pub(super) fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Recess the live surface behind a modal, per its [`Recess`] policy.
///
/// A terminal cannot alpha-blend, so the event loop calls this exactly once
/// per frame *after* the transcript and chrome are drawn and *before* the
/// centered modal panel — which then overpaints its own crisp area on top.
/// The three policies:
///
/// - [`Recess::None`] leaves the surface untouched (lightweight floats such as
///   Question / Permission that never take over).
/// - [`Recess::Dim`] darkens every cell in place by [`Theme::modal_dim_factor`]
///   so the background stays visible for context while the modal reads as the
///   focal layer. This replaces the old opaque full-screen fill: context no
///   longer vanishes behind a modal.
/// - [`Recess::Takeover`] clears + fills with [`Theme::backdrop`], fully
///   occluding the surface for a context switch (session selection).
///
/// [`Theme::modal_dim_factor`]: Theme::modal_dim_factor
pub fn recess_backdrop(frame: &mut Frame, recess: Recess, theme: &Theme) {
    match recess {
        Recess::None => {}
        Recess::Dim => dim_surface(frame, theme.modal_dim_factor()),
        Recess::Takeover => {
            let area = frame.area();
            frame.render_widget(Clear, area);
            frame.render_widget(
                RtBlock::default().style(Style::default().bg(theme.backdrop())),
                area,
            );
        }
    }
}

/// Darken the whole frame buffer in place by scaling each cell's RGB channels
/// toward black by `factor` (0.0 = invisible, 1.0 = unchanged). This is the
/// "dim-recess" effect: the surface is rendered normally first, then every
/// cell is multiplied by `factor`, so context stays visible while clearly
/// receding behind the modal drawn on top.
///
/// Only [`Color::Rgb`] is scaled (the entire palette is RGB, so this covers
/// every painted cell); named / Reset colors are left untouched so the dim is
/// additive rather than lossy where they appear.
fn dim_surface(frame: &mut Frame, factor: f32) {
    let buffer = frame.buffer_mut();
    for cell in buffer.content.iter_mut() {
        cell.fg = scale_color(cell.fg, factor);
        cell.bg = scale_color(cell.bg, factor);
        cell.style.fg = cell.fg;
        cell.style.bg = cell.bg;
    }
}

/// Multiply an RGB color's channels by `factor`, clamped to `[0, 1]`.
fn scale_color(color: Color, factor: f32) -> Color {
    let f = factor.clamp(0.0, 1.0);
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(
            (r as f32 * f).round() as u8,
            (g as f32 * f).round() as u8,
            (b as f32 * f).round() as u8,
        ),
        other => other,
    }
}

/// A borderless panel with a single thick colored left bar (opencode-style).
pub(super) fn panel_block(bar_color: Color, bg: Color) -> RtBlock<'static> {
    RtBlock::default()
        .borders(neenee_tui::Borders::LEFT)
        .border_type(neenee_tui::BorderType::Thick)
        .border_style(Style::default().fg(bar_color))
        .style(Style::default().bg(bg))
}

/// Content rect inside a [`panel_block`]: starts one column right of the left
/// `┃` bar and reserves a matching column on the right, so the panel's
/// content is symmetric and a long line never touches either edge. Callers
/// paint [`panel_block`] bare over the full `area` for the chrome, then
/// render content into this rect — the left-bar-panel counterpart to how
/// [`modal_frame`] insets the borderless modal family via
/// `MODAL_INNER_H_PADDING`.
pub(super) fn panel_inner(area: Rect) -> Rect {
    area.inner(Margin {
        horizontal: PANEL_BAR_INSET,
        vertical: 0,
    })
}

/// Section rects produced by [`modal_frame`]: the header and footer are
/// `Option`al (omitted when the modal asked for none), and `body` is always
/// present and flexes to fill whatever the header/footer leave behind.
pub(super) struct ModalFrame {
    pub header: Option<Rect>,
    pub body: Rect,
    pub footer: Option<Rect>,
}

/// Paint the unified modal chrome and split the content area into sections.
///
/// Every centered modal goes through this so the panel style lives in one
/// place: a borderless solid-bg panel (no `┃` left bar) with
/// `MODAL_INNER_H_PADDING`/`MODAL_INNER_V_PADDING` inner padding, then a
/// vertical split into optional `header` (1 row) / `body` (flex) / optional
/// 1-row gap + `footer` (1 row). The caller renders its own header / body /
/// footer content into the returned rects.
pub(super) fn modal_frame(
    frame: &mut Frame,
    area: Rect,
    bg: Color,
    header: bool,
    footer: bool,
) -> ModalFrame {
    frame.render_widget(Clear, area);
    frame.render_widget(RtBlock::default().style(Style::default().bg(bg)), area);
    let inner = area.inner(Margin {
        horizontal: MODAL_INNER_H_PADDING,
        vertical: MODAL_INNER_V_PADDING,
    });

    // Tagged constraints so we can map split chunks back to sections:
    // 0 = header, 4 = gap after header, 1 = body, 2 = gap before footer,
    // 3 = footer. Both gaps are 1 row so the body always sits one blank line
    // below the header and one above the footer — regardless of which sections
    // a modal asks for.
    let mut tagged: Vec<(u8, Constraint)> = Vec::new();
    if header {
        tagged.push((0, Constraint::Length(1)));
        tagged.push((4, Constraint::Length(1)));
    }
    tagged.push((1, Constraint::Min(0)));
    if footer {
        tagged.push((2, Constraint::Length(1)));
        tagged.push((3, Constraint::Length(1)));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(tagged.iter().map(|(_, c)| *c))
        .split(inner);

    let mut out = ModalFrame {
        header: None,
        body: inner,
        footer: None,
    };
    for (i, (tag, _)) in tagged.iter().enumerate() {
        match tag {
            0 => out.header = Some(chunks[i]),
            1 => out.body = chunks[i],
            3 => out.footer = Some(chunks[i]),
            _ => {}
        }
    }
    out
}

/// Render a modal body with shared scroll mechanics. The `scroll` offset is
/// clamped to `[0, content_lines - visible]` (so it can never drift past the
/// last line) and, when `follow` is `Some(idx)`, nudged so row `idx` stays on
/// screen — that's how list modals keep their selection visible without a
/// separate scroll cursor. The body is rendered with `.scroll()` so anything
/// past the visible window is clipped rather than silently truncated.
pub(super) fn render_body(
    frame: &mut Frame,
    body_rect: Rect,
    lines: Vec<Line<'static>>,
    scroll: &mut usize,
    follow: Option<usize>,
    wrap: bool,
    theme: &Theme,
) {
    let visible = body_rect.height as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    *scroll = (*scroll).min(max_scroll);
    if let Some(idx) = follow {
        if visible > 0 {
            if idx < *scroll {
                *scroll = idx;
            } else if idx >= *scroll + visible {
                *scroll = idx.saturating_sub(visible.saturating_sub(1));
            }
        }
    }

    let mut para = Paragraph::new(lines).scroll(*scroll as u16, 0);
    if wrap {
        para = para.wrap(neenee_tui::Wrap { trim: false });
    }
    frame.render_widget(para, body_rect);

    // Scroll indicator: a one-cell scrollbar in the right margin showing
    // whether more content lies above and/or below the window. Only drawn
    // when content overflows the body height.
    draw_scrollbar(frame, body_rect, *scroll, max_scroll, theme);
}

/// Draw a minimal one-column scrollbar in the body's rightmost column when
/// the content overflows. Shows a thumb whose vertical position reflects the
/// `scroll / max_scroll` ratio, plus `▲` / `▼` caps when more content lies
/// above / below. The thumb uses `theme.muted()`; the caps use `theme.dim()`
/// so the bar reads as a subtle affordance, not a focal element.
fn draw_scrollbar(frame: &mut Frame, body: Rect, scroll: usize, max_scroll: usize, theme: &Theme) {
    if max_scroll == 0 || body.width == 0 || body.height < 2 {
        return;
    }
    let h = body.height as usize;
    // Thumb height scales with the visible-to-total ratio, floored at 1.
    let thumb_h = (h * h / (max_scroll + h)).max(1).min(h) as u16;
    let track = h as u16;
    let track_top = body.y;
    let track_x = body.x + body.width + SCROLLBAR_GAP;

    let more_above = scroll > 0;
    let more_below = scroll < max_scroll;

    // Caps (only when there is content in that direction). Coordinates are
    // within `body`, which is inside the buffer, so direct content indexing
    // is safe.
    let buf = frame.buffer_mut();
    let buf_area = buf.area();
    if more_above {
        let cell = cell_at_index(buf, buf_area, track_x, track_top);
        cell.set_symbol("▲");
        cell.set_fg(theme.dim());
    }
    if more_below {
        let cell = cell_at_index(buf, buf_area, track_x, track_top + track - 1);
        cell.set_symbol("▼");
        cell.set_fg(theme.dim());
    }

    // Thumb position within the open track (between the two caps).
    let open_top = if more_above { 1 } else { 0 };
    let open_bottom = track as i32 - if more_below { 1 } else { 0 };
    let open_h = (open_bottom - open_top).max(1) as u16;
    let ratio = if max_scroll > 0 {
        scroll as f32 / max_scroll as f32
    } else {
        0.0
    };
    let thumb_y =
        track_top + open_top as u16 + (ratio * (open_h.saturating_sub(thumb_h)) as f32) as u16;

    for i in 0..thumb_h {
        let y = thumb_y + i;
        if y < track_top + track {
            let cell = cell_at_index(buf, buf_area, track_x, y);
            cell.set_symbol(" ");
            cell.set_bg(theme.muted());
        }
    }
}

/// Index a buffer cell by absolute (x, y) via direct `content` indexing.
/// The caller guarantees the coordinate lies inside `area`.
#[allow(deprecated)]
fn cell_at_index(buf: &mut neenee_tui::Grid, area: Rect, x: u16, y: u16) -> &mut neenee_tui::Cell {
    let idx = (y as usize - area.y as usize) * area.width as usize + (x as usize - area.x as usize);
    let cell = &mut buf.content[idx];
    cell.set_skip(false);
    let _ = cell;
    cell
}

/// Contrast foreground for a colored background (dark text on light fills).
pub(super) fn contrast_fg(bg: Color) -> Color {
    let (r, g, b) = rgb(bg);
    let luminance = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    if luminance > 140.0 {
        Color::Black
    } else {
        Color::White
    }
}

pub(super) fn rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (224, 108, 117),
        Color::Green => (127, 216, 143),
        Color::Yellow => (229, 192, 123),
        Color::Blue => (137, 180, 250),
        Color::Magenta => (203, 166, 247),
        Color::Cyan => (86, 182, 194),
        Color::Gray => (128, 128, 128),
        Color::DarkGray => (64, 64, 64),
        Color::LightGreen => (127, 216, 143),
        Color::LightRed => (243, 139, 168),
        _ => (128, 128, 128),
    }
}

#[cfg(test)]
mod tests {
    //! `panel_inner` is the symmetric-inset contract for the left-bar-panel
    //! family. Lock its geometry directly so a long overlay line can never
    //! kiss the panel's right edge regardless of terminal width.
    use super::*;
    use neenee_tui::Rect;

    #[test]
    fn panel_inner_insets_symmetrically_around_left_bar() {
        // A 10-wide panel: the `┃` bar owns the first column, content starts
        // one column in (clear of the bar) and ends one column short of the
        // right edge (the bar's mirrored gutter).
        let area = Rect::new(2, 3, 10, 5);
        let inner = panel_inner(area);
        assert_eq!(inner.x, 3, "content starts right after the ┃ bar");
        assert_eq!(inner.width, 8, "10 − 2 (left bar + right gutter)");
        assert_eq!(inner.y, 3);
        assert_eq!(inner.height, 5, "no vertical inset");
        // Content's right edge is exactly one short of the panel's right edge.
        assert_eq!(inner.x + inner.width, area.x + area.width - 1);
    }

    #[test]
    fn panel_inner_clamps_without_underflow() {
        // A panel too narrow for the bar + gutter collapses to an empty rect
        // at the panel's origin rather than underflowing the width.
        let inner = panel_inner(Rect::new(0, 0, 1, 1));
        assert_eq!(inner, Rect::new(0, 0, 0, 0));
    }
}
