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
    rows.join("\n")
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
fn bash_collapsed_renders_preview() {
    let m = tool_step(
        "bash",
        r#"{"command":"echo hi"}"#,
        Some("hi\nExit 0\n"),
        false,
    );
    insta::assert_snapshot!(render_grid(&m, 80, 24));
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
        None,
        true,
    );
    insta::assert_snapshot!(render_grid(&m, 80, 30));
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
