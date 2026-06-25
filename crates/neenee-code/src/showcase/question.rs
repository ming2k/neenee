//! Question (ask_user) modal showcase — two fixtures the user can Tab
//! through: single-select (radio) and multi-select (checkbox). Both
//! exercise the synthetic "Other" free-text row and option descriptions.

use std::io;

use crossterm::event::KeyCode;

use neenee_core::{UserQuestion, UserQuestionOption, UserQuestionRequest};

use crate::showcase::common::{self, ShowAction};
use crate::tui::question_model::{QuestionAction, QuestionEffect, QuestionModel};
use crate::tui::render::{Theme, draw_question_modal};

/// Showcase fixture set the user can cycle through with `Tab`.
fn fixtures() -> Vec<UserQuestionRequest> {
    vec![
        // Fixture 0: single-select (radio), two labeled options with
        // descriptions. First option is pre-selected by default.
        UserQuestionRequest {
            id: "single".into(),
            questions: vec![UserQuestion {
                header: Some("Error handling".into()),
                question: "Which error handling crate should we use?".into(),
                options: vec![
                    UserQuestionOption {
                        label: "anyhow (Recommended)".into(),
                        description: Some(
                            "Simple, context-rich errors for application code.".into(),
                        ),
                    },
                    UserQuestionOption {
                        label: "thiserror".into(),
                        description: Some("Structured, typed errors for library code.".into()),
                    },
                ],
                multi_select: false,
            }],
        },
        // Fixture 1: multi-select (checkbox), three options with
        // descriptions. None selected by default; Space toggles each on/off.
        UserQuestionRequest {
            id: "multi".into(),
            questions: vec![UserQuestion {
                header: Some("Features".into()),
                question: "Which features should be enabled?".into(),
                options: vec![
                    UserQuestionOption {
                        label: "Telemetry".into(),
                        description: Some(
                            "Anonymous usage metrics sent to a collector endpoint.".into(),
                        ),
                    },
                    UserQuestionOption {
                        label: "Caching".into(),
                        description: Some("In-memory cache for repeated read-only queries.".into()),
                    },
                    UserQuestionOption {
                        label: "Rate limiting".into(),
                        description: Some("Per-client throttle to protect upstream APIs.".into()),
                    },
                ],
                multi_select: true,
            }],
        },
    ]
}

struct State {
    fx: Vec<UserQuestionRequest>,
    idx: usize,
    model: QuestionModel,
    last_result: Option<String>,
    scroll: usize,
}

pub fn run() -> io::Result<()> {
    let fx = fixtures();
    let mut state = State {
        idx: 0,
        model: QuestionModel::open(fx[0].clone()),
        last_result: None,
        scroll: 0,
        fx,
    };
    let theme = Theme::default();

    common::run_showcase(
        &mut state,
        |f, s| {
            let title = format!(
                " question modal · fixture {}/{} · Tab=next fixture · q/Ctrl+C=quit",
                s.idx + 1,
                s.fx.len(),
            );
            let mut hint =
                " ↑↓ navigate · Space select · Enter submit · 1-9 jump · Esc cancel ".to_string();
            if let Some(r) = &s.last_result {
                hint = format!("{r}     {hint}");
            }
            common::draw_with_chrome(f, &title, &hint, &theme, |f| {
                let mut scroll = s.scroll;
                draw_question_modal(
                    f,
                    s.model.request(),
                    s.model.current(),
                    s.model.selected(),
                    s.model.other_text(),
                    s.model.highlight(),
                    &mut scroll,
                    &theme,
                );
            });
        },
        |s, key| -> ShowAction {
            if key.code == KeyCode::Tab {
                s.idx = (s.idx + 1) % s.fx.len();
                s.model = QuestionModel::open(s.fx[s.idx].clone());
                s.last_result = None;
                s.scroll = 0;
                return ShowAction::Continue;
            }
            let action = match key.code {
                KeyCode::Up => QuestionAction::Up,
                KeyCode::Down => QuestionAction::Down,
                KeyCode::Char(' ') => QuestionAction::Toggle,
                KeyCode::Char(c @ '1'..='9') => {
                    QuestionAction::Select(c.to_digit(10).expect("digit") as usize)
                }
                KeyCode::Backspace => QuestionAction::Backspace,
                KeyCode::Enter => QuestionAction::Submit,
                KeyCode::Esc => QuestionAction::Cancel,
                KeyCode::Char(c) => QuestionAction::InsertChar(c),
                _ => return ShowAction::Continue,
            };
            let effects = s.model.update_mut(action);
            for effect in &effects {
                match effect {
                    QuestionEffect::Reply { answers, .. } => {
                        let text = answers
                            .iter()
                            .map(|a| format!("[{}]", a.join(", ")))
                            .collect::<Vec<_>>()
                            .join(" ");
                        s.last_result = Some(format!("submitted → {text}"));
                    }
                    QuestionEffect::Closed { .. } => {
                        if !effects
                            .iter()
                            .any(|e| matches!(e, QuestionEffect::Reply { .. }))
                        {
                            s.last_result = Some("cancelled".into());
                        }
                        return ShowAction::Exit;
                    }
                }
            }
            ShowAction::Continue
        },
    )
}
