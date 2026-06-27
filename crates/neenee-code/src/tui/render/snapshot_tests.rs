//! Snapshot baselines for tool-step rendering.
//!
//! These lock the text/layout of the tool step so the rendering
//! refactor (RenderCtx extraction, and later redesign steps) can be verified
//! behavior-preserving. Snapshots capture the painted grid (cell symbols per
//! row, trailing whitespace trimmed) at a fixed terminal size.
//!
//! Regenerate baselines after an intentional visual change:
//!
//! ```sh
//! INSTA_UPDATE=always cargo test -p neenee-tui render::snapshot_tests
//! ```

#![cfg(test)]

use neenee_tui::Rect;

use crate::tui::document::{MessageKind, TranscriptMessage};
use crate::tui::layout::LayoutMap;
use crate::tui::selection::SelectionState;

use super::Theme;
use super::disclosure::draw_tool_step;

/// Build a finished tool-step message with optional output and expand state.
fn tool_step(
    name: &str,
    arguments: &str,
    output: Option<&str>,
    expanded: bool,
) -> TranscriptMessage {
    let mut m = TranscriptMessage::tool_step("call_test", name, arguments);
    if let MessageKind::ToolStep {
        output: out,
        expanded: exp,
        ..
    } = &mut m.kind
    {
        *out = output.map(str::to_string);
        *exp = expanded;
    }
    m
}

/// Like [`tool_step`] but also sets the structured payload + terminal status,
/// mirroring what `finish_tool_step` produces in production.
fn tool_step_structured(
    name: &str,
    arguments: &str,
    structured: neenee_core::ToolOutput,
    expanded: bool,
) -> TranscriptMessage {
    let text = structured.to_text();
    let mut m = TranscriptMessage::tool_step("call_test", name, arguments);
    m.finish_tool_step("call_test", text, structured, 0);
    if let MessageKind::ToolStep { expanded: exp, .. } = &mut m.kind {
        *exp = expanded;
    }
    m
}

/// A still-running tool step carrying a partial structured payload, mirroring
/// what `push_tool_stream` produces mid-stream (status stays `Running`).
fn tool_step_streaming(
    name: &str,
    arguments: &str,
    structured: neenee_core::ToolOutput,
    expanded: bool,
) -> TranscriptMessage {
    let mut m = TranscriptMessage::tool_step("call_test", name, arguments);
    if let MessageKind::ToolStep {
        structured: s,
        expanded: exp,
        ..
    } = &mut m.kind
    {
        *s = Some(Box::new(structured));
        *exp = expanded;
    }
    m
}

/// Render `msg` as a tool step into a fresh `width x height` buffer and
/// return the painted grid as trimmed text rows joined by newlines.
fn render_grid(msg: &TranscriptMessage, width: u16, height: u16) -> String {
    let mut terminal = neenee_tui::TestTerminal::new(width, height);
    terminal.draw(|f| {
        let area: Rect = f.area();
        let mut layout_map = LayoutMap::default();
        let selection = SelectionState::default();
        let theme = Theme::default();
        let mut skip_rows = 0usize;
        let mut current_y = area.y;
        let mut content_lines = 0usize;
        let mut sticky = Vec::new();
        draw_tool_step(
            f,
            area,
            msg,
            0,
            &selection,
            None,
            &theme,
            &mut layout_map,
            &mut skip_rows,
            &mut current_y,
            &mut content_lines,
            &mut sticky,
            false,
            false,
        );
    });

    let buf = terminal.buffer();
    let bw = buf.area().width as usize;
    let mut rows: Vec<String> = Vec::with_capacity(height as usize);
    for y in 0..height as usize {
        let mut row = String::new();
        for x in 0..width as usize {
            let cell = &buf.content[y * bw + x];
            let sym: &str = cell.symbol();
            row.push_str(sym);
        }
        rows.push(row.trim_end().to_string());
    }
    while rows.last().is_some_and(|r| r.is_empty()) {
        rows.pop();
    }
    let grid = rows.join("\n");

    // Style layer: a compact run-length map of the background per row, plus a
    // legend. Text snapshots can't see color, so this makes palette/banding
    // changes (Step 2) visible and reviewable. Symbols are assigned per-frame
    // in first-appearance order; `.` is the terminal default.
    let (bgmap, legend) = background_map(buf);
    if bgmap.is_empty() {
        grid
    } else {
        format!("{grid}\n\nbackgrounds:\n{legend}\n{bgmap}")
    }
}

/// Compact per-row background run-length map + legend for a rendered buffer.
/// Skips rows that are entirely the terminal default so the output stays
/// focused on the painted step.
fn background_map(buf: &neenee_tui::Grid) -> (String, String) {
    use neenee_tui::Color;
    type Bg = Color;

    let bw = buf.area().width as usize;
    let bh = buf.area().height as usize;

    let is_default = |bg: Bg| bg == Color::Reset;

    // Distinct bg colors in first-appearance order.
    let mut order: Vec<Bg> = Vec::new();
    for y in 0..bh {
        for x in 0..bw {
            let bg = buf.content[y * bw + x].style().bg;
            if !order.contains(&bg) {
                order.push(bg);
            }
        }
    }
    let sym_of = |bg: Bg| -> char {
        if is_default(bg) {
            '.'
        } else {
            let i = order
                .iter()
                .position(|x| is_default(*x) == is_default(bg) && x == &bg)
                .unwrap_or(0);
            (b'A' + i as u8) as char
        }
    };
    let fmt_color = |bg: Bg| -> String {
        match bg {
            Color::Reset => "reset".to_string(),
            Color::Rgb(r, g, b) => format!("#{:02X}{:02X}{:02X}", r, g, b),
            other => format!("{:?}", other),
        }
    };
    let legend = order
        .iter()
        .map(|&bg| format!("{}={}", sym_of(bg), fmt_color(bg)))
        .collect::<Vec<_>>()
        .join("  ");

    let mut lines: Vec<String> = Vec::new();
    for y in 0..bh {
        let mut runs: Vec<(char, usize)> = Vec::new();
        for x in 0..bw {
            let s = sym_of(buf.content[y * bw + x].style().bg);
            match runs.last_mut() {
                Some((last, n)) if *last == s => *n += 1,
                _ => runs.push((s, 1)),
            }
        }
        while matches!(runs.last(), Some(('.', _))) {
            runs.pop();
        }
        if runs.is_empty() {
            continue;
        }
        let line = runs
            .iter()
            .map(|(s, n)| format!("{}{}", s, n))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(format!("{}: {}", y, line));
    }
    (lines.join("\n"), legend)
}

#[test]
fn read_file_expanded_renders_code_block() {
    let m = tool_step(
        "read_file",
        r#"{"path":"src/lib.rs"}"#,
        Some("fn main() {\n    let x = 1;\n}\n"),
        true,
    );
    insta::assert_snapshot!(render_grid(&m, 80, 40));
}

#[test]
fn read_file_with_offset_numbers_from_start_line() {
    // A structured `Code` carrying `start_line: 100` (as `read_file` emits
    // when called with `offset: 100`) must number the gutter 100, 101, … —
    // not restart at 1. Also locks that the gutter column widens to fit
    // three-digit line numbers instead of overflowing.
    let m = tool_step_structured(
        "read_file",
        r#"{"path":"src/lib.rs","offset":100}"#,
        neenee_core::ToolOutput::Code {
            lang: Some("rs".into()),
            text: "fn a() {}\nfn b() {}\nfn c() {}\n".into(),
            start_line: 100,
            prefix: None,
            suffix: None,
        },
        true,
    );
    insta::assert_snapshot!(render_grid(&m, 80, 40));
}

#[test]
fn bash_expanded_renders_markers_and_output() {
    let m = tool_step(
        "bash",
        r#"{"command":"cargo test"}"#,
        Some("running 3 tests\n.\n.\n.\ntest result: ok. 3 passed\nSTDOUT:\nbuilt ok\nExit 0\n"),
        true,
    );
    insta::assert_snapshot!(render_grid(&m, 80, 40));
}

#[test]
fn bash_expanded_renders_structured_shell() {
    // Structured Shell: stdout and stderr render as separate color bands and a
    // non-zero exit produces an `exit N` footer (the failure is also reflected
    // in the header status glyph). This is the path bash takes in production
    // post-ADR-0001; the marker-sniffing test above covers the legacy fallback.
    let m = tool_step_structured(
        "bash",
        r#"{"command":"cargo test"}"#,
        neenee_core::ToolOutput::Shell {
            command: "cargo test".into(),
            stdout: "running 3 tests\n...\ntest result: ok. 3 passed".into(),
            stderr: "warning: unused import".into(),
            exit: Some(1),
            truncated: false,
        },
        true,
    );
    insta::assert_snapshot!(render_grid(&m, 80, 40));
}

#[test]
fn grep_expanded_renders_grouped_matches() {
    let m = tool_step(
        "grep",
        r#"{"pattern":"foo","path":"src"}"#,
        Some("src/a.rs:10:let foo = 1;\nsrc/a.rs:22:foo();\nsrc/b.rs:5:foo,"),
        true,
    );
    insta::assert_snapshot!(render_grid(&m, 80, 40));
}

#[test]
fn edit_file_expanded_renders_diff() {
    let m = tool_step(
        "edit_file",
        r#"{"path":"a.rs","old_string":"let x = 1;","new_string":"let x = 2;"}"#,
        Some("Edited a.rs"),
        true,
    );
    insta::assert_snapshot!(render_grid(&m, 80, 30));
}

#[test]
fn edit_file_prose_diff_suppresses_noisy_word_highlights() {
    let m = tool_step(
        "edit_file",
        r#"{"path":"docs/explanation/tui.md","old_string":"Because the frame is a pure function of state, anything that changes state — a streamed token, a permission request, a mouse drag — shows up on the very next frame with no manual invalidation.","new_string":"Because the frame is a pure function of state, diff compares the back grid against the front grid and walks only the dirty rows from each row's dirty column."}"#,
        Some("Edited docs/explanation/tui.md"),
        true,
    );
    insta::assert_snapshot!(render_grid(&m, 100, 30));
}

#[test]
fn edit_file_multihunk_interleaves_changes() {
    // Two separated single-token edits: the LCS diff must interleave
    // context/remove/add per hunk rather than all-remove-then-all-add.
    let m = tool_step(
        "edit_file",
        r#"{"path":"a.rs","old_string":"fn one() {\n    return 1;\n}\n\nfn two() {\n    return 2;\n}\n","new_string":"fn one() {\n    return 10;\n}\n\nfn two() {\n    return 20;\n}\n"}"#,
        Some("Edited a.rs"),
        true,
    );
    insta::assert_snapshot!(render_grid(&m, 80, 40));
}

#[test]
fn edit_file_distant_hunks_show_ellipsis_with_range_header() {
    // Two changes separated by 10 context lines — well past the 2×3=6
    // overlap window. An ellipsis row must appear between the two hunks
    // with a centred ⋮ gutter and a @@ range header in theme-info colour.
    let old = concat!(
        "line  1\nline  2\nCHANGE\n",
        "line  4\nline  5\nline  6\nline  7\nline  8\nline  9\n",
        "line 10\nline 11\nline 12\nline 13\n",
        "CHANGE\nline 15\nline 16\n",
    );
    let new = concat!(
        "line  1\nline  2\nchange\n",
        "line  4\nline  5\nline  6\nline  7\nline  8\nline  9\n",
        "line 10\nline 11\nline 12\nline 13\n",
        "change\nline 15\nline 16\n",
    );
    let args = format!(
        r#"{{"path":"a.rs","old_string":{},"new_string":{}}}"#,
        serde_json::to_string(old).unwrap(),
        serde_json::to_string(new).unwrap(),
    );
    let m = tool_step("edit_file", &args, Some("Edited a.rs"), true);
    insta::assert_snapshot!(render_grid(&m, 80, 40));
}

#[test]
fn list_dir_expanded_renders_listing() {
    let m = tool_step(
        "list_dir",
        r#"{"path":"."}"#,
        Some("src/\ntests/\nCargo.toml\nREADME.md"),
        true,
    );
    insta::assert_snapshot!(render_grid(&m, 80, 30));
}

#[test]
fn bash_running_streams_live_preview() {
    // A long-running bash command mid-stream: status is still Running (header
    // shows a steady `info` accent) but partial stdout already shows under the
    // header via the structured Shell, instead of freezing on a spinner.
    let m = tool_step_streaming(
        "bash",
        r#"{"command":"cargo build"}"#,
        neenee_core::ToolOutput::Shell {
            command: "cargo build".into(),
            stdout: "Compiling neenee-core v0.1.0\nCompiling neenee-tui v0.1.0".into(),
            stderr: String::new(),
            exit: None,
            truncated: false,
        },
        false,
    );
    insta::assert_snapshot!(render_grid(&m, 80, 24));
}

#[test]
fn tool_step_detail_overlay_renders_full_shell_output() {
    let m = tool_step_structured(
        "bash",
        r#"{"command":"cargo test"}"#,
        neenee_core::ToolOutput::Shell {
            command: "cargo test".into(),
            stdout: "running 3 tests\n...\ntest result: ok. 3 passed".into(),
            stderr: "warning: unused import".into(),
            exit: Some(0),
            truncated: false,
        },
        false,
    );
    let mut terminal = neenee_tui::TestTerminal::new(60, 14);
    terminal.draw(|f| {
        super::draw_tool_step_detail_overlay(f, &m, 0, &Theme::default());
    });
    let buf = terminal.buffer();
    let bw = buf.area().width as usize;
    let mut rows: Vec<String> = Vec::new();
    for y in 0..buf.area().height as usize {
        let mut row = String::new();
        for x in 0..bw {
            let sym: &str = buf.content[y * bw + x].symbol();
            row.push_str(sym);
        }
        rows.push(row.trim_end().to_string());
    }
    while rows.last().is_some_and(|r| r.is_empty()) {
        rows.pop();
    }
    insta::assert_snapshot!(rows.join("\n"));
}

#[test]
fn tool_step_detail_overlay_keeps_right_gutter_clear_on_long_lines() {
    // The detail overlay's left `┃` bar must have a mirrored right gutter
    // (panel_inner): a stdout line long enough to fill the panel never runs
    // its text into the panel's last column, no matter the terminal width.
    let long = "x".repeat(512);
    let m = tool_step_structured(
        "bash",
        r#"{"command":"cargo test"}"#,
        neenee_core::ToolOutput::Shell {
            command: "cargo test".into(),
            stdout: long,
            stderr: String::new(),
            exit: Some(0),
            truncated: false,
        },
        false,
    );
    // Recompute the overlay's panel rect with the exact primitives it uses,
    // so the assertion tracks the real geometry rather than a hard-coded guess.
    let mut panel = Rect::default();
    let mut terminal = neenee_tui::TestTerminal::new(60, 14);
    terminal.draw(|f| {
        panel = super::primitives::modal_area(f, crate::tui::Modal::ToolStepDetail)
            .expect("tool detail modal has fixed geometry");
        super::draw_tool_step_detail_overlay(f, &m, 0, &Theme::default());
    });
    let buf = terminal.buffer();
    let bw = buf.area().width as usize;
    let gutter_col = (panel.x + panel.width - 1) as usize; // bar's mirrored gutter
    let content_end = (panel.x + panel.width - 2) as usize; // last content column

    let mut saw_content = false;
    for y in 0..panel.height as usize {
        let gutter = buf.content[y * bw + gutter_col].symbol();
        assert_eq!(
            gutter, " ",
            "right gutter col {gutter_col} on row {y} must stay clear, got {gutter:?}"
        );
        if buf.content[y * bw + content_end].symbol() == "x" {
            saw_content = true;
        }
    }
    assert!(
        saw_content,
        "expected the long line to render into the panel's last content column"
    );
}

#[test]
fn edit_file_diff_renders_from_structured_patch() {
    // The diff now comes from the ToolOutput::Patch payload (old/new), not
    // from re-parsing the tool arguments.
    let m = tool_step_structured(
        "edit_file",
        r#"{"path":"a.rs","old_string":"let x = 1;","new_string":"let x = 2;"}"#,
        neenee_core::ToolOutput::Patch {
            path: "a.rs".into(),
            op: neenee_core::PatchOp::Edit,
            old: "let x = 1;".into(),
            new: "let x = 2;".into(),
            start_line: 0,
        },
        true,
    );
    insta::assert_snapshot!(render_grid(&m, 80, 30));
}

// ── Tool-step batch spacing (ADR-0001, "spacing belongs to the body") ──
//
// Collapsed tool steps stack flush — no blank row between adjacent headers —
// so a batch of parallel/sequential tool calls reads as one compact log block.
// Only an expanded body is padded: one row above (its own header) and one row
// below (the next step's header). These tests render the full transcript
// (`draw_transcript`, which owns the inter-message gap) so the suppression
// logic itself is exercised — the single-step `render_grid` helper above only
// draws one step and so cannot observe inter-step spacing.

/// Render `steps` (already finalized tool-step messages) through the full
/// transcript pipeline and return the painted grid as trimmed rows. Unlike
/// [`render_grid`], this exercises `draw_transcript` so the message-level
/// spacing between consecutive steps is captured. Backgrounds are omitted:
/// these tests are about row counts, not palette.
fn render_transcript_grid(steps: &[TranscriptMessage], width: u16, height: u16) -> String {
    use super::{Theme, TranscriptView, draw_transcript};
    use crate::tui::layout::LayoutMap;

    let theme = Theme::default();
    let selection = SelectionState::default();
    let mut terminal = neenee_tui::TestTerminal::new(width, height);
    let mut layout_map = LayoutMap::new();
    terminal.draw(|f| {
        let _ = draw_transcript(
            f,
            &mut layout_map,
            TranscriptView {
                messages: steps,
                scroll: 0,
                selection: &selection,
                cell_selection: None,
                activity: "",
                spinner_phase: 0,
                input: "",
                byte_cursor: 0,
                chrome_hidden: false,
                subagent_bar: None,
                side_banner: None,
                pursuit: None,
                todos: None,
                review_alert: String::new(),
                turn_started_at: None,
                hovered_step: None,
                focused_target: None,
                logo: None,
                theme: &theme,
            },
        );
    });

    let buf = terminal.buffer();
    let bw = buf.area().width as usize;
    let mut rows: Vec<String> = Vec::with_capacity(height as usize);
    for y in 0..height as usize {
        let mut row = String::new();
        for x in 0..width as usize {
            row.push_str(buf.content[y * bw + x].symbol());
        }
        rows.push(row.trim_end().to_string());
    }
    while rows.last().is_some_and(|r| r.is_empty()) {
        rows.pop();
    }
    rows.join("\n")
}

/// A batch of collapsed tool steps renders with no blank rows between headers.
#[test]
fn collapsed_tool_steps_stack_flush() {
    let steps = vec![
        tool_step_structured(
            "read_file",
            r#"{"path":"a.rs"}"#,
            neenee_core::ToolOutput::Code {
                lang: None,
                text: "x".into(),
                start_line: 1,
                prefix: None,
                suffix: None,
            },
            false,
        ),
        tool_step_structured(
            "read_file",
            r#"{"path":"b.rs"}"#,
            neenee_core::ToolOutput::Code {
                lang: None,
                text: "y".into(),
                start_line: 1,
                prefix: None,
                suffix: None,
            },
            false,
        ),
        tool_step_structured(
            "read_file",
            r#"{"path":"c.rs"}"#,
            neenee_core::ToolOutput::Code {
                lang: None,
                text: "z".into(),
                start_line: 1,
                prefix: None,
                suffix: None,
            },
            false,
        ),
    ];
    let grid = render_transcript_grid(&steps, 60, 12);
    // The three headers must be adjacent: no blank row between any pair. Each
    // header carries the disclosure marker (`+` collapsed) somewhere in the
    // line, so locate their row indices and assert they are consecutive.
    let header_idx: Vec<usize> = grid
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains("Read ") && (l.contains('+') || l.contains('-')))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(header_idx.len(), 3, "expected three Read headers:\n{grid}");
    assert_eq!(
        header_idx[1] - header_idx[0],
        1,
        "first two collapsed headers must be flush (no blank row):\n{grid}"
    );
    assert_eq!(
        header_idx[2] - header_idx[1],
        1,
        "last two collapsed headers must be flush (no blank row):\n{grid}"
    );
}

/// An expanded body is padded one row from its own header (top) and one row
/// from the next header (bottom); collapsed neighbours around it stay flush.
#[test]
fn expanded_body_pads_itself_neighbours_stay_flush() {
    let steps = vec![
        tool_step_structured(
            "read_file",
            r#"{"path":"a.rs"}"#,
            neenee_core::ToolOutput::Code {
                lang: None,
                text: "x".into(),
                start_line: 1,
                prefix: None,
                suffix: None,
            },
            false, // collapsed — flush against the next step's header
        ),
        tool_step_structured(
            "grep",
            r#"{"pattern":"foo","path":"src"}"#,
            neenee_core::ToolOutput::Matches {
                pattern: "foo".into(),
                lines: vec!["src/a.rs:10:1:foo".into(), "src/b.rs:5:1:foo".into()],
            },
            true, // expanded — body padded above + below
        ),
        tool_step_structured(
            "read_file",
            r#"{"path":"c.rs"}"#,
            neenee_core::ToolOutput::Code {
                lang: None,
                text: "z".into(),
                start_line: 1,
                prefix: None,
                suffix: None,
            },
            false, // collapsed — flush below the expanded body's trailing gap
        ),
    ];
    insta::assert_snapshot!(render_transcript_grid(&steps, 60, 16));
}
