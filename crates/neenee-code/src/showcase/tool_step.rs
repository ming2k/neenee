//! Tool-step showcase — renders the **parallel tools** transcript in isolation.
//!
//! `neenee showcase tool-step` paints a fixed conversation whose turn is a
//! batch of tool calls, pumped through the *production* `draw_transcript`
//! renderer. It is the live, interactive fixture for the tool-step TUI:
//! spacing (collapsed steps stack flush; expanded bodies own their padding),
//! per-tool body renderers (read → code, grep → matches, bash → shell), and
//! the lifecycle colour grammar (running / ok / failed / cancelled).
//!
//! ## Scenarios
//!
//! The showcase cycles through three scenarios so each spacing/layout rule is
//! visible on its own:
//!
//! 1. **flush batch** — a parallel fan-out of collapsed read/grep calls. The
//!    headers stack with *no* blank rows between them: a batch of parallel
//!    tool calls reads as one compact log block.
//! 2. **expanded body** — one call expanded mid-batch. Its body is padded one
//!    row from its own header (top) and one row from the next header
//!    (bottom); the collapsed neighbours around it stay flush.
//! 3. **lifecycles** — one call per lifecycle (ok / failed / running /
//!    cancelled) so the header-colour grammar is legible, including a failed
//!    step (which auto-expands to surface its error) and a running step
//!    (which carries the accent while in flight).
//!
//! ## Keys
//!
//! `Tab` / `→` cycle scenarios · `↑`/`↓` (or `j`/`k`) scroll · `Ctrl+T`
//! toggles every step's disclosure · `Esc` / `q` quits.

use std::cell::Cell;
use std::io;

use crossterm::event::KeyCode;

use neenee_core::ToolOutput;

use crate::showcase::common::{self, ShowAction};
use crate::tui::document::TranscriptMessage;
use crate::tui::layout::LayoutMap;
use crate::tui::render::{Theme, TranscriptView, draw_transcript};
use crate::tui::selection::SelectionState;

/// Number of fixture scenarios (cycled with `Tab` / `→`).
const SCENARIO_COUNT: usize = 3;

struct ToolStepState {
    scenario: usize,
    // `scroll` needs interior mutability: the renderer clamps it while
    // `run_showcase` only hands the render closure a `&State`.
    scroll: Cell<u16>,
    /// Live transcript for the active scenario. Held in a `Cell` and swapped
    /// out for mutation (scenario switch / Ctrl+T toggle), since the render
    /// closure sees the state by shared reference.
    messages: Cell<Vec<TranscriptMessage>>,
}

pub fn run() -> io::Result<()> {
    let theme = Theme::default();
    let mut state = ToolStepState {
        scenario: 0,
        scroll: Cell::new(0),
        messages: Cell::new(build_scenario(0)),
    };

    common::run_showcase(
        &mut state,
        |f, s| {
            let mut layout_map = LayoutMap::new();
            let selection = SelectionState::None;
            // Take the live messages out of the cell for the render pass, then
            // put them back so the next pass (and the key handler) still see
            // them. The render closure only holds `&s`, so the cell is the
            // borrow-constrained home for the mutable batch.
            let messages = s.messages.take();
            let scroll = s.scroll.get();
            let render = draw_transcript(
                f,
                &mut layout_map,
                TranscriptView {
                    messages: &messages,
                    scroll,
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
                    height_cache: None,
                },
            );
            s.messages.set(messages);
            // Clamp scroll to the content height so the last scenario line is
            // always reachable but the view never over-scrolls past it.
            let max_scroll = render
                .content_lines
                .saturating_sub(render.view_height as usize);
            if (scroll as usize) > max_scroll {
                s.scroll.set(max_scroll as u16);
            }

            // Title bar: scenario index + label, so the active fixture is
            // obvious without reading the whole transcript.
            let title = format!(
                " tool-step · [{}/{}] {} · Tab=next · Ctrl+T=toggle · q/Esc=quit",
                s.scenario + 1,
                SCENARIO_COUNT,
                SCENARIO_LABELS[s.scenario]
            );
            draw_title(f, &title, &theme);
        },
        |s, key| -> ShowAction {
            match key.code {
                KeyCode::Esc => ShowAction::Exit,
                KeyCode::Tab | KeyCode::Right => {
                    s.scenario = (s.scenario + 1) % SCENARIO_COUNT;
                    s.messages.set(build_scenario(s.scenario));
                    s.scroll.set(0);
                    ShowAction::Continue
                }
                KeyCode::Left => {
                    s.scenario = (s.scenario + SCENARIO_COUNT - 1) % SCENARIO_COUNT;
                    s.messages.set(build_scenario(s.scenario));
                    s.scroll.set(0);
                    ShowAction::Continue
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    s.scroll.set(s.scroll.get().saturating_add(1));
                    ShowAction::Continue
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    s.scroll.set(s.scroll.get().saturating_sub(1));
                    ShowAction::Continue
                }
                KeyCode::Char('t')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    // Mirror the app's Ctrl+T: toggle every step's disclosure
                    // (any collapsed → expand all; else collapse all).
                    let mut messages = s.messages.take();
                    let any_collapsed = messages
                        .iter()
                        .any(|m| m.tool_step_expanded() == Some(false));
                    for m in &mut messages {
                        m.pin_tool_step_expanded(any_collapsed);
                    }
                    s.messages.set(messages);
                    s.scroll.set(0);
                    ShowAction::Continue
                }
                _ => ShowAction::Continue,
            }
        },
    )
}

const SCENARIO_LABELS: [&str; SCENARIO_COUNT] = [
    "flush batch — parallel collapsed calls stack with no gaps",
    "expanded body — padded from its header + the next; neighbours flush",
    "lifecycles — ok / failed (auto-expand) / running / cancelled",
];

/// Build the fixture transcript for scenario `idx`.
fn build_scenario(idx: usize) -> Vec<TranscriptMessage> {
    match idx {
        0 => flush_batch(),
        1 => expanded_body(),
        _ => lifecycles(),
    }
}

// ── scenarios ─────────────────────────────────────────────────────────────

/// A parallel fan-out of collapsed calls. The headers must stack flush (no
/// blank rows between them) — the rule this showcase exercises.
fn flush_batch() -> Vec<TranscriptMessage> {
    vec![
        read("call_a", "src/lib.rs", true),
        read("call_b", "src/main.rs", true),
        grep("call_c", "ToolStep", "src"),
        read("call_d", "README.md", false),
    ]
}

/// One call expanded mid-batch. Its body is padded one row from its own
/// header and one from the next; the collapsed neighbours stay flush.
fn expanded_body() -> Vec<TranscriptMessage> {
    vec![
        read("call_a", "src/lib.rs", true),      // collapsed
        grep_expanded("call_b", "foo", "src"),   // expanded mid-batch
        read("call_c", "src/main.rs", true),     // collapsed
        read("call_d", "tests/basic.rs", false), // collapsed
    ]
}

/// One call per lifecycle, so the header-colour grammar is legible: Ok (calm),
/// Failed (auto-expands to surface the error), Running (accent in flight),
/// Cancelled (muted, collapsed).
fn lifecycles() -> Vec<TranscriptMessage> {
    vec![
        read("call_ok", "src/lib.rs", true),
        bash_failed("call_fail"),
        running("call_run"),
        cancelled("call_cancel"),
    ]
}

// ── fixture builders ──────────────────────────────────────────────────────

/// A finished, Ok `read_file` step. `present` controls whether the payload
/// carries real content (rendering a code block when expanded) or is empty.
fn read(id: &str, path: &str, present: bool) -> TranscriptMessage {
    let args = format!(r#"{{"path":"{path}"}}"#);
    let text = if present {
        format!("// {path}\nfn entry() {{}}\n")
    } else {
        String::new()
    };
    let structured = ToolOutput::Code {
        lang: Some("rs".into()),
        text,
        start_line: 1,
        prefix: None,
        suffix: None,
    };
    let mut m = TranscriptMessage::tool_step(id, "read_file", &args);
    m.finish_tool_step(id, structured.to_text(), structured, 42);
    m.set_tool_step_expanded(false);
    m
}

/// A finished, Ok `grep` step, collapsed.
fn grep(id: &str, pattern: &str, path: &str) -> TranscriptMessage {
    let args = format!(r#"{{"pattern":"{pattern}","path":"{path}"}}"#);
    let structured = ToolOutput::Matches {
        pattern: pattern.into(),
        lines: vec![
            format!("{path}/a.rs:10:1:    let x = {pattern};"),
            format!("{path}/b.rs:7:1:    // {pattern} here"),
            format!("{path}/b.rs:22:1:    {pattern}_ref()"),
        ],
    };
    let mut m = TranscriptMessage::tool_step(id, "grep", &args);
    m.finish_tool_step(id, structured.to_text(), structured, 18);
    m.set_tool_step_expanded(false);
    m
}

/// A finished, Ok `grep` step, expanded (to show the body-padding rule).
fn grep_expanded(id: &str, pattern: &str, path: &str) -> TranscriptMessage {
    let mut m = grep(id, pattern, path);
    m.pin_tool_step_expanded(true);
    m
}

/// A failed `bash` step — `finish_tool_step` derives the `Failed` status from
/// `ToolOutput::is_error()` (non-zero exit). Pinned open so the error body is
/// always visible regardless of density.
fn bash_failed(id: &str) -> TranscriptMessage {
    let args = r#"{"command":"cargo test"}"#;
    let structured = ToolOutput::Shell {
        command: "cargo test".into(),
        stdout: "running 3 tests\n...\ntest result: FAILED. 1 passed; 2 failed".into(),
        stderr: "error[E0599]: no method named `missing`".into(),
        lines: Vec::new(),
        exit: Some(1),
        truncated: false,
    };
    let mut m = TranscriptMessage::tool_step(id, "bash", args);
    m.finish_tool_step(id, structured.to_text(), structured, 1200);
    m.pin_tool_step_expanded(true);
    m
}

/// A still-running step: created but never finished, so its status stays
/// `Running` and its header carries the accent.
fn running(id: &str) -> TranscriptMessage {
    let mut m = TranscriptMessage::tool_step(id, "bash", r#"{"command":"cargo build"}"#);
    m.set_tool_step_expanded(false);
    m
}

/// A cancelled step: created then cancelled, so its status is `Cancelled` and
/// it renders collapsed + muted.
fn cancelled(id: &str) -> TranscriptMessage {
    let mut m = TranscriptMessage::tool_step(id, "bash", r#"{"command":"sleep 60"}"#);
    m.cancel_tool_step(id);
    m.set_tool_step_expanded(false);
    m
}

// ── title bar ─────────────────────────────────────────────────────────────

/// A single muted line pinned to the top of the terminal. The transcript
/// renderer already owns the full frame (it paints the app background), so
/// this draws *over* the top row after the transcript pass.
fn draw_title(f: &mut neenee_tui::Frame, text: &str, theme: &Theme) {
    use neenee_tui::{Line, Rect, Span, Style};
    let area = f.area();
    let row = Rect::new(area.x, area.y, area.width, 1);
    let line = Line::from(Span::styled(text, Style::default().fg(theme.muted())));
    f.render_widget(
        neenee_tui::Paragraph::new(line).style(Style::default().bg(theme.surface())),
        row,
    );
}

#[cfg(test)]
mod tests {
    //! Fixture contracts: assert each scenario builds the expected message
    //! shape (kind, status, disclosure) so the showcase stays a *testable*
    //! case, not just a manually-run viewer. These guard the data the
    //! production renderer draws, independent of the raw-mode loop.

    use super::*;
    use crate::tui::document::ToolStepStatus;

    #[test]
    fn flush_batch_is_all_collapsed_tool_steps() {
        let msgs = flush_batch();
        assert_eq!(msgs.len(), 4, "flush batch has four parallel calls");
        assert!(msgs.iter().all(|m| m.is_tool_step()));
        assert!(
            msgs.iter().all(|m| m.tool_step_expanded() == Some(false)),
            "every step must be collapsed so the headers stack flush"
        );
        assert!(
            msgs.iter()
                .all(|m| m.tool_step_status() == Some(ToolStepStatus::Ok)),
            "flush batch is a successful fan-out"
        );
    }

    #[test]
    fn expanded_body_has_exactly_one_open_step_mid_batch() {
        let msgs = expanded_body();
        assert_eq!(msgs.len(), 4);
        let open = msgs
            .iter()
            .filter(|m| m.tool_step_expanded() == Some(true))
            .count();
        assert_eq!(open, 1, "exactly one step expanded mid-batch");
        // The expanded step is at index 1, flanked by collapsed neighbours.
        assert_eq!(msgs[1].tool_step_expanded(), Some(true));
        assert_eq!(msgs[0].tool_step_expanded(), Some(false));
        assert_eq!(msgs[2].tool_step_expanded(), Some(false));
    }

    #[test]
    fn lifecycles_cover_each_terminal_state_plus_running() {
        let msgs = lifecycles();
        let statuses: Vec<_> = msgs.iter().filter_map(|m| m.tool_step_status()).collect();
        assert_eq!(
            statuses,
            vec![
                ToolStepStatus::Ok,
                ToolStepStatus::Failed,
                ToolStepStatus::Running,
                ToolStepStatus::Cancelled,
            ],
            "one step per lifecycle, in display order"
        );
        // The failed step must be expanded so the error body is visible.
        assert_eq!(msgs[1].tool_step_expanded(), Some(true));
        // Running + cancelled + ok-default stay collapsed.
        assert_eq!(msgs[0].tool_step_expanded(), Some(false));
        assert_eq!(msgs[2].tool_step_expanded(), Some(false));
        assert_eq!(msgs[3].tool_step_expanded(), Some(false));
    }

    #[test]
    fn build_scenario_indexes_match_labels() {
        // Guards that `run()`'s SCENARIO_LABELS and `build_scenario` agree on
        // ordering — a silent drift here would label the wrong transcript.
        assert_eq!(SCENARIO_LABELS.len(), SCENARIO_COUNT);
        for idx in 0..SCENARIO_COUNT {
            assert!(
                build_scenario(idx).iter().all(|m| m.is_tool_step()),
                "scenario {idx} must be a pure tool-step transcript"
            );
        }
    }
}
