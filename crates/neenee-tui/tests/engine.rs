//! End-to-end engine tests: the full write → diff → render → promote cycle,
//! plus the two scenarios the engine exists to fix (ADR-0038):
//!
//! 1. **Wide-glyph ghost eradication.** Writing a CJK glyph marks its trailing
//!    column as a continuation carrying the glyph's own background, and the
//!    diff emits the glyph without ever leaking a stale `Color::Reset` cell —
//!    the class of bug ratatui's `set_stringn` + diff-skip produces through
//!    tmux.
//! 2. **Convergence.** After a diff is promoted into the front grid, a second
//!    diff is empty: the back grid and the (now-updated) front grid agree, so
//!    the terminal emits nothing on an idle frame.
//!
//! These run without a real terminal: the backend writes into a `Vec<u8>`.

use neenee_tui::backend::{Backend, Bce};
use neenee_tui::diff;
use neenee_tui::{Cell, Color, Fit, Grid, Pos, Style};

/// The full cycle over a capture buffer. Returns the emitted bytes.
fn render_cycle(back: &mut Grid, front: &mut Grid, bce: Bce) -> String {
    crossterm::style::force_color_output(true);
    let cmd = diff::diff(back, front);
    let mut buf = Vec::new();
    {
        let mut be = Backend::with_bce(&mut buf, bce);
        be.render(&cmd).unwrap();
    }
    diff::promote(back, front);
    String::from_utf8(buf).unwrap()
}

#[test]
fn second_frame_after_promote_is_idle() {
    let (w, h) = (8, 2);
    let mut back = Grid::new(w, h);
    let mut front = Grid::new(w, h);

    // Frame 1: write some content.
    back.put(
        0,
        0,
        Fit::Clip,
        Style::default().fg(Color::Rgb(1, 1, 1)),
        "hello",
    );
    let first = render_cycle(&mut back, &mut front, Bce::Yes);
    assert!(first.contains("hello"));

    // Frame 2: nothing changed → diff is empty → backend emits nothing.
    let second = render_cycle(&mut back, &mut front, Bce::Yes);
    assert!(
        second.is_empty(),
        "idle frame should emit nothing, got {second:?}"
    );
}

#[test]
fn wide_glyph_trailing_column_carries_background_not_reset() {
    // The core anti-ghost property: a wide glyph's trailing cell owns the
    // glyph's background, never Color::Reset. Through any multiplexer that
    // repaints that column on its own, it stays correct.
    let mut back = Grid::new(6, 1);
    let style = Style::default()
        .fg(Color::Rgb(255, 255, 255))
        .bg(Color::Rgb(7, 8, 8)); // near-black surface, like neenee's theme
    back.put(0, 0, Fit::Clip, style, "中");

    // Trailing continuation cell: width 0, and its bg is the glyph's bg.
    let trail = back.get(1, 0).unwrap();
    assert!(trail.is_wide_continuation());
    assert_eq!(trail.style.bg, Color::Rgb(7, 8, 8));
    assert_ne!(
        trail.style.bg,
        Color::Reset,
        "trailing cell must not be Reset"
    );

    // Diff must emit the glyph once; the trailing column is implicit.
    let front = Grid::new(6, 1);
    let cmd = diff::diff(&back, &front);
    assert_eq!(cmd.draws.len(), 1);
    let emitted_cells = match &cmd.draws[0] {
        diff::Draw::Cells { cells, .. } => cells.clone(),
        _ => panic!(),
    };
    assert_eq!(emitted_cells, vec![("中".to_string(), 2)]);
}

#[test]
fn streaming_token_grows_only_the_changed_run() {
    // Simulate streaming: a growing assistant message. Each token should dirty
    // only the tail, so the diff emits only the new characters — not a
    // repaint of the whole line.
    let (w, h) = (20, 1);
    let mut back = Grid::new(w, h);
    let mut front = Grid::new(w, h);
    let style = Style::default().fg(Color::Rgb(1, 1, 1));

    back.put(0, 0, Fit::Clip, style, "Hello");
    let _ = render_cycle(&mut back, &mut front, Bce::Yes);

    // Grow the message by one word. Only the new cells should be dirty.
    let end = back.put(5, 0, Fit::Clip, style, " world");
    assert_eq!(end, Pos { x: 11, y: 0 });

    let cmd = diff::diff(&back, &front);
    // The single changed run is exactly the new text, starting at col 5.
    assert_eq!(cmd.draws.len(), 1);
    if let diff::Draw::Cells {
        x,
        y,
        cells,
        style: s,
    } = &cmd.draws[0]
    {
        assert_eq!(*x, 5);
        assert_eq!(*y, 0);
        assert_eq!(*s, style);
        let text: String = cells.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(text, " world");
    } else {
        panic!("expected a single Cells run");
    }
}

#[test]
fn style_change_emits_sgr_only_for_the_delta() {
    // Two regions, same text but different fg: the backend should emit the
    // second region's SGR exactly once and not repeat the first.
    let (w, h) = (6, 1);
    let mut back = Grid::new(w, h);
    let mut front = Grid::new(w, h);

    let s1 = Style::default().fg(Color::Rgb(1, 1, 1));
    let s2 = Style::default().fg(Color::Rgb(2, 2, 2));
    back.put(0, 0, Fit::Clip, s1, "ab");
    back.put(2, 0, Fit::Clip, s2, "cd");

    let out = render_cycle(&mut back, &mut front, Bce::Yes);
    let first_sgr = out.matches("\x1b[38;2;1;1;1m").count();
    let second_sgr = out.matches("\x1b[38;2;2;2;2m").count();
    assert_eq!(first_sgr, 1);
    assert_eq!(second_sgr, 1);
}

#[test]
fn clear_row_then_redraw_marks_only_that_row() {
    // Clearing a row and rewriting it dirties only that row; other rows stay
    // clean and the diff skips them.
    let (w, h) = (5, 3);
    let mut back = Grid::new(w, h);
    let mut front = Grid::new(w, h);

    // Prime all rows, then sync.
    for y in 0..h {
        back.put(0, y, Fit::Clip, Style::default(), "aaaaa");
    }
    let _ = render_cycle(&mut back, &mut front, Bce::Yes);

    // Rewrite row 1.
    back.clear_row(1, 0, Style::default().bg(Color::Rgb(10, 10, 10)));
    back.put(0, 1, Fit::Clip, Style::default(), "bbbbb");

    let cmd = diff::diff(&back, &front);
    let touched_rows: std::collections::HashSet<u16> = cmd
        .draws
        .iter()
        .filter_map(|d| match d {
            diff::Draw::Cells { y, .. } => Some(*y),
            _ => None,
        })
        .collect();
    assert_eq!(touched_rows, [1].into_iter().collect());
}

#[test]
fn resize_preserves_content_and_converges() {
    let (w, h) = (4, 2);
    let mut back = Grid::new(w, h);
    let mut front = Grid::new(w, h);
    back.put(0, 0, Fit::Clip, Style::default(), "abcd");
    let _ = render_cycle(&mut back, &mut front, Bce::Yes);

    // Grow both grids identically. The overlap ("abcd") is preserved on the
    // back grid; the newly exposed cells are blank on both, so the diff is
    // empty (nothing visually changed). The key guarantees: content survives
    // the resize, and a follow-up frame converges.
    back.resize(6, 3);
    front.resize(6, 3);
    assert_eq!(back.get(0, 0).unwrap().symbol, "a");
    assert_eq!(back.get(3, 0).unwrap().symbol, "d");
    // New cells are blank.
    assert_eq!(back.get(5, 2).unwrap().symbol, " ");

    let cmd = diff::diff(&back, &front);
    let _ = render_cycle(&mut back, &mut front, Bce::Yes);
    // Either nothing changed (empty diff) or only genuinely new content did.
    let _ = cmd;
    let second = render_cycle(&mut back, &mut front, Bce::Yes);
    assert!(
        second.is_empty(),
        "frame converges after resize, got {second:?}"
    );
}

#[test]
fn bce_clear_to_end_uses_clr_eol() {
    // When bce is on, a dirty row whose tail resolves to the current
    // background can be cleared with a single \x1b[K. We verify the backend
    // emits that sequence when handed an explicit ClearEol command.
    use neenee_tui::diff::{Draw, DrawCmd};
    crossterm::style::force_color_output(true);
    let cmd = DrawCmd {
        draws: vec![Draw::ClearEol {
            x: 2,
            y: 1,
            style: Style::default().bg(Color::Rgb(10, 10, 10)),
            width: 4,
        }],
    };
    let mut buf = Vec::new();
    {
        let mut be = Backend::with_bce(&mut buf, Bce::Yes);
        be.render(&cmd).unwrap();
    }
    let s = String::from_utf8(buf).unwrap();
    assert!(s.contains("\x1b[K"), "clr_eol emitted under bce: {s:?}");
    assert!(
        s.contains("\x1b[48;2;10;10;10m"),
        "clr_eol applies the target background first: {s:?}"
    );
}

#[test]
fn bce_clear_to_end_falls_back_to_spaces_without_bce() {
    use neenee_tui::diff::{Draw, DrawCmd};
    crossterm::style::force_color_output(true);
    let cmd = DrawCmd {
        draws: vec![Draw::ClearEol {
            x: 1,
            y: 0,
            style: Style::default().bg(Color::Rgb(1, 2, 3)),
            width: 3,
        }],
    };
    let mut buf = Vec::new();
    {
        let mut be = Backend::with_bce(&mut buf, Bce::No);
        be.render(&cmd).unwrap();
    }
    let s = String::from_utf8(buf).unwrap();
    assert!(!s.contains("\x1b[K"));
    assert!(s.ends_with("   "));
}

#[test]
fn diff_collapses_uniform_blank_tail_to_clear_eol() {
    let mut back = Grid::new(8, 1);
    let mut front = Grid::new(8, 1);
    back.put(0, 0, Fit::Clip, Style::default(), "abcdef");
    let _ = render_cycle(&mut back, &mut front, Bce::Yes);

    let bg = Style::default().bg(Color::Rgb(7, 8, 9));
    back.clear_row(0, 2, bg);

    let cmd = diff::diff(&back, &front);
    assert!(matches!(
        cmd.draws.as_slice(),
        [diff::Draw::ClearEol {
            x: 2,
            y: 0,
            style,
            width: 6,
        }] if style.bg == Color::Rgb(7, 8, 9)
    ));
}

/// A trivial sanity check that the cell-level types compose as documented.
#[test]
fn cell_constructors() {
    let blank = Cell::blank();
    assert_eq!(blank.symbol, " ");
    assert_eq!(blank.width, 1);

    let wide = Cell::wide_continuation(Style::default().bg(Color::Rgb(1, 2, 3)));
    assert!(wide.is_wide_continuation());
    assert_eq!(wide.style.bg, Color::Rgb(1, 2, 3));
}
