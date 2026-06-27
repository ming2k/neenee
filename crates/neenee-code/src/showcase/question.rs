//! Question (ask_user) modal showcase — two fixtures the user can Tab
//! through: single-select (radio) and multi-select (checkbox). Both
//! exercise the synthetic "Other" free-text row and option descriptions.

use std::cell::{Cell, RefCell};
use std::io;

use crossterm::event::KeyCode;

use neenee_core::{UserQuestion, UserQuestionOption, UserQuestionRequest};

use crate::showcase::common::{self, ShowAction, ShowEvent};
use crate::tui::layout::ModalHitMap;
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
    scroll: Cell<usize>,
    follow_highlight: Cell<bool>,
    hit_map: RefCell<ModalHitMap>,
}

pub fn run() -> io::Result<()> {
    let fx = fixtures();
    let mut state = State {
        idx: 0,
        model: QuestionModel::open(fx[0].clone()),
        last_result: None,
        scroll: Cell::new(0),
        follow_highlight: Cell::new(true),
        hit_map: RefCell::new(ModalHitMap::new()),
        fx,
    };
    let theme = Theme::default();

    common::run_showcase_events(
        &mut state,
        |f, s| {
            let title = format!(
                " question modal · fixture {}/{} · Tab=next fixture · q/Ctrl+C=quit",
                s.idx + 1,
                s.fx.len(),
            );
            let mut hint =
                " ↑↓ navigate · wheel/Pg scroll · Space select · Enter submit · 1-9 jump · Esc cancel "
                    .to_string();
            if let Some(r) = &s.last_result {
                hint = format!("{r}     {hint}");
            }
            common::draw_with_chrome(f, &title, &hint, &theme, |f| {
                let mut hit_map = s.hit_map.borrow_mut();
                hit_map.clear();
                let mut scroll = s.scroll.get();
                draw_question_modal(
                    f,
                    &mut hit_map,
                    s.model.request(),
                    s.model.current(),
                    s.model.selected(),
                    s.model.other_text(),
                    s.model.highlight(),
                    &mut scroll,
                    s.follow_highlight.get(),
                    &theme,
                );
                s.scroll.set(scroll);
            });
        },
        |s, event| -> ShowAction {
            let key = match event {
                ShowEvent::Click { x, y } => {
                    let hit = { s.hit_map.borrow().question_option_at(x, y) };
                    if let Some(hit) = hit {
                        s.follow_highlight.set(true);
                        let effects = s
                            .model
                            .update_mut(QuestionAction::Select(hit.option_index + 1));
                        if apply_effects(s, &effects) {
                            return ShowAction::Exit;
                        }
                    }
                    return ShowAction::Continue;
                }
                ShowEvent::ScrollUp => {
                    s.follow_highlight.set(false);
                    s.scroll.set(s.scroll.get().saturating_sub(4));
                    return ShowAction::Continue;
                }
                ShowEvent::ScrollDown => {
                    s.follow_highlight.set(false);
                    s.scroll.set(s.scroll.get().saturating_add(4));
                    return ShowAction::Continue;
                }
                ShowEvent::Key(key) => key,
            };
            if key.code == KeyCode::Tab {
                s.idx = (s.idx + 1) % s.fx.len();
                s.model = QuestionModel::open(s.fx[s.idx].clone());
                s.last_result = None;
                s.scroll.set(0);
                s.follow_highlight.set(true);
                return ShowAction::Continue;
            }
            let action = match key.code {
                KeyCode::Up => QuestionAction::Up,
                KeyCode::Down => QuestionAction::Down,
                KeyCode::PageUp => {
                    s.follow_highlight.set(false);
                    s.scroll.set(s.scroll.get().saturating_sub(8));
                    return ShowAction::Continue;
                }
                KeyCode::PageDown => {
                    s.follow_highlight.set(false);
                    s.scroll.set(s.scroll.get().saturating_add(8));
                    return ShowAction::Continue;
                }
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
            if matches!(
                action,
                QuestionAction::Up | QuestionAction::Down | QuestionAction::Select(_)
            ) {
                s.follow_highlight.set(true);
            }
            let effects = s.model.update_mut(action);
            if apply_effects(s, &effects) {
                return ShowAction::Exit;
            }
            ShowAction::Continue
        },
    )
}

fn apply_effects(s: &mut State, effects: &[QuestionEffect]) -> bool {
    let mut should_exit = false;
    for effect in effects {
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
                should_exit = true;
            }
        }
    }
    should_exit
}
