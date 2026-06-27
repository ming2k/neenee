//! Transcript showcase — full-screen fixtures for the production transcript
//! renderer.
//!
//! This complements component-level modal showcases by exercising the screen
//! surface that users spend most time in: ordinary chat, markdown, tables,
//! wide glyphs, notices, thinking traces, and long scrollback.

use std::cell::Cell;
use std::io;

use crossterm::event::{KeyCode, KeyModifiers};
use neenee_core::{Role, ToolOutput};

use crate::showcase::common::{self, ShowAction};
use crate::tui::document::{NoticeSeverity, TranscriptMessage};
use crate::tui::layout::LayoutMap;
use crate::tui::render::{Theme, TranscriptView, draw_transcript};
use crate::tui::selection::SelectionState;

const SCENARIO_COUNT: usize = 3;

struct TranscriptState {
    scenario: usize,
    scroll: Cell<u16>,
    messages: Cell<Vec<TranscriptMessage>>,
}

pub fn run() -> io::Result<()> {
    let theme = Theme::default();
    let mut state = TranscriptState {
        scenario: 0,
        scroll: Cell::new(0),
        messages: Cell::new(build_scenario(0)),
    };

    common::run_showcase(
        &mut state,
        |f, s| {
            let mut layout_map = LayoutMap::new();
            let selection = SelectionState::None;
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
                    activity: "showcase render check",
                    spinner_phase: (scroll as usize) % 8,
                    input: "Resize the terminal, scroll, or switch fixtures...",
                    byte_cursor: "Resize the terminal, scroll, or switch fixtures...".len(),
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
            s.messages.set(messages);

            let max_scroll = render
                .content_lines
                .saturating_sub(render.view_height as usize);
            if (scroll as usize) > max_scroll {
                s.scroll.set(max_scroll as u16);
            }

            let title = format!(
                " transcript · [{}/{}] {} · Tab=next · ↑↓ scroll · Ctrl+T toggle · q/Esc quit",
                s.scenario + 1,
                SCENARIO_COUNT,
                SCENARIO_LABELS[s.scenario],
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
                KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let mut messages = s.messages.take();
                    let any_collapsed = messages.iter().any(|m| {
                        m.tool_step_expanded() == Some(false)
                            || m.thinking_expanded() == Some(false)
                    });
                    for message in &mut messages {
                        if message.tool_step_expanded().is_some() {
                            message.pin_tool_step_expanded(any_collapsed);
                        }
                        if message.thinking_expanded().is_some() {
                            message.pin_thinking_expanded(any_collapsed);
                        }
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
    "chat chrome + notice + thinking",
    "markdown table/code/CJK wrapping",
    "long scrollback resize stress",
];

fn build_scenario(idx: usize) -> Vec<TranscriptMessage> {
    match idx {
        0 => chat_flow(),
        1 => markdown_stress(),
        _ => long_scrollback(),
    }
}

fn chat_flow() -> Vec<TranscriptMessage> {
    let mut thinking = TranscriptMessage::thinking(
        "Read the TUI renderer path, compare modal and transcript background ownership, \
         then decide whether the retained grid needs a full-surface repaint.",
    );
    thinking.set_thinking_duration(1_260);
    thinking.set_thinking_expanded(false);

    let mut grep = TranscriptMessage::tool_step(
        "grep_bg",
        "grep",
        r#"{"pattern":"theme.surface","path":"crates/neenee-code/src/tui/render"}"#,
    );
    let grep_out = ToolOutput::Matches {
        pattern: "theme.surface".into(),
        lines: vec![
            "render/mod.rs:230: frame.render_widget(... bg(theme.surface()))".into(),
            "showcase/common.rs: draw_app_background(...)".into(),
        ],
    };
    grep.finish_tool_step("grep_bg", grep_out.to_text(), grep_out, 44);
    grep.set_tool_step_expanded(false);

    vec![
        TranscriptMessage::new(
            Role::User,
            "Resize the showcase window and check whether the old right edge leaves stale cells.",
        ),
        thinking,
        grep,
        TranscriptMessage::notice(
            NoticeSeverity::Warning,
            "Showcase fixtures should repaint the entire app surface before drawing partial modal chrome.",
        ),
        TranscriptMessage::new(
            Role::Assistant,
            "The transcript renderer already fills the whole frame. The standalone modal showcases need the same background ownership before drawing their focused component.",
        )
        .with_attribution("anthropic", "claude-sonnet-4-5"),
    ]
}

fn markdown_stress() -> Vec<TranscriptMessage> {
    vec![
        TranscriptMessage::new(
            Role::User,
            "Render a dense answer with a table, code block, lists, and CJK wide characters.",
        ),
        TranscriptMessage::new(
            Role::Assistant,
            r#"# Renderer checklist

- Full-width app background survives terminal resize.
- Inline `code` keeps its own surface.
- CJK and emoji-width neighbours stay aligned: 你好，世界 · 渲染测试 · カタカナ.

| Case | Expected | Status |
| :--- | :------: | -----: |
| resize larger | new cells use app background | pass |
| resize smaller | stale tails disappear | pass |
| wide glyph | trailing cell keeps background | pass |

```rust
fn repaint(frame: &mut Frame, theme: &Theme) {
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.surface())),
        frame.area(),
    );
}
```

> The important invariant is that every visible cell belongs to the TUI.
"#,
        )
        .with_attribution("openai", "gpt-4.1"),
    ]
}

fn long_scrollback() -> Vec<TranscriptMessage> {
    let mut messages = Vec::new();
    messages.push(TranscriptMessage::notice(
        NoticeSeverity::Info,
        "Long scrollback fixture: resize while scrolled, then jump between narrow and wide widths.",
    ));
    for idx in 1..=18 {
        messages.push(TranscriptMessage::new(
            Role::User,
            format!(
                "Fixture prompt #{idx}: verify wrapping with a deliberately long line that should flow cleanly across the transcript band."
            ),
        ));
        messages.push(
            TranscriptMessage::new(
                Role::Assistant,
                format!(
                    "Response #{idx}: the retained grid should repaint only dirty rows, but the rendered surface must still look complete after resize. This paragraph includes mixed width text: ASCII, 中文宽字符, and symbols -> [] {{}}."
                ),
            )
            .with_attribution("mock", "showcase-model"),
        );
    }
    messages
}

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
    use super::*;

    #[test]
    fn all_scenarios_have_messages() {
        assert_eq!(SCENARIO_LABELS.len(), SCENARIO_COUNT);
        for idx in 0..SCENARIO_COUNT {
            assert!(
                !build_scenario(idx).is_empty(),
                "scenario {idx} should be renderable"
            );
        }
    }

    #[test]
    fn markdown_stress_covers_wide_text_and_code() {
        let messages = markdown_stress();
        let body = messages
            .iter()
            .map(|message| message.raw.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("你好，世界"));
        assert!(body.contains("```rust"));
        assert!(body.contains("| Case | Expected | Status |"));
    }

    #[test]
    fn long_scrollback_is_large_enough_to_scroll() {
        assert!(long_scrollback().len() > 24);
    }
}
