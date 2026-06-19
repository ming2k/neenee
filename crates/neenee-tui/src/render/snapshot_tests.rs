//! Snapshot baselines for tool-step rendering.
//!
//! These lock the text/layout of the tool-step card so the rendering
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

use ratatui::{backend::TestBackend, layout::Rect, Terminal};

use crate::document::{MessageKind, TranscriptMessage};
use crate::layout::LayoutMap;
use crate::selection::SelectionState;

use super::Theme;
use super::turn_artifacts::draw_tool_step_card;

/// Build a finished tool-step message with optional output and expand state.
fn tool_step(name: &str, arguments: &str, output: Option<&str>, expanded: bool) -> TranscriptMessage {
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
        *s = Some(structured);
        *exp = expanded;
    }
    m
}

/// Render `msg` as a tool-step card into a fresh `width x height` buffer and
/// return the painted grid as trimmed text rows joined by newlines.
fn render_grid(msg: &TranscriptMessage, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test backend");
    terminal
        .draw(|f| {
            let area: Rect = f.size();
            let mut layout_map = LayoutMap::default();
            let selection = SelectionState::default();
            let theme = Theme::default();
            let mut skip_rows = 0usize;
            let mut current_y = area.y;
            let mut content_lines = 0usize;
            let mut sticky = Vec::new();
            draw_tool_step_card(
                f,
                area,
                msg,
                0,
                &selection,
                &theme,
                &mut layout_map,
                &mut skip_rows,
                &mut current_y,
                &mut content_lines,
                &mut sticky,
                0,
                false,
            );
        })
        .expect("draw");

    let buf = terminal.backend().buffer();
    let bw = buf.area.width as usize;
    let mut rows: Vec<String> = Vec::with_capacity(height as usize);
    for y in 0..height as usize {
        let mut row = String::new();
        for x in 0..width as usize {
            let cell = &buf.content[y * bw + x];
            let sym: &str = cell.symbol().as_ref();
            row.push_str(sym);
        }
        rows.push(row.trim_end().to_string());
    }
    while rows.last().map_or(false, |r| r.is_empty()) {
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
/// focused on the painted card.
fn background_map(buf: &ratatui::buffer::Buffer) -> (String, String) {
    use ratatui::style::Color;
    type Bg = Option<Color>;

    let bw = buf.area.width as usize;
    let bh = buf.area.height as usize;

    let is_default = |bg: Bg| matches!(bg, None | Some(Color::Reset));

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
            let i = order.iter().position(|x| is_default(*x) == is_default(bg) && x == &bg).unwrap_or(0);
            (b'A' + i as u8) as char
        }
    };
    let fmt_color = |bg: Bg| -> String {
        match bg {
            None => "unset".to_string(),
            Some(Color::Reset) => "reset".to_string(),
            Some(Color::Rgb(r, g, b)) => format!("#{:02X}{:02X}{:02X}", r, g, b),
            Some(other) => format!("{:?}", other),
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
fn bash_expanded_renders_markers_and_output() {
    let m = tool_step(
        "bash",
        r#"{"command":"cargo test"}"#,
        Some(
            "running 3 tests\n.\n.\n.\ntest result: ok. 3 passed\nSTDOUT:\nbuilt ok\nExit 0\n",
        ),
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
    // shows the breathing dot) but partial stdout already shows under the
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
    use ratatui::{backend::TestBackend, Terminal};
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
    let backend = TestBackend::new(60, 14);
    let mut terminal = Terminal::new(backend).expect("backend");
    terminal
        .draw(|f| {
            super::draw_tool_step_detail_overlay(f, &m, 0, &Theme::default());
        })
        .expect("draw");
    let buf = terminal.backend().buffer();
    let bw = buf.area.width as usize;
    let mut rows: Vec<String> = Vec::new();
    for y in 0..buf.area.height as usize {
        let mut row = String::new();
        for x in 0..bw {
            let sym: &str = buf.content[y * bw + x].symbol().as_ref();
            row.push_str(sym);
        }
        rows.push(row.trim_end().to_string());
    }
    while rows.last().map_or(false, |r| r.is_empty()) {
        rows.pop();
    }
    insta::assert_snapshot!(rows.join("\n"));
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
        },
        true,
    );
    insta::assert_snapshot!(render_grid(&m, 80, 30));
}
