//! Model-View-Update core for the question (ask_user) modal.
//!
//! This is the single source of truth for the question modal's *interaction*
//! state and its *reply computation*, extracted out of the 2000-line
//! `event_loop.rs` match so the entire state machine can be unit-tested in
//! isolation — no terminal, no async, no agent channel.
//!
//! ## Architecture
//!
//! - **Model** ([`QuestionModel`]): the complete, self-contained state of an
//!   open question modal — the request, which question is active, the
//!   highlighted option, the per-question selected indices, and the per-question
//!   "Other" free-text. It is plain data: clone it, inspect it, render it.
//! - **View**: already pure and lives in `render::draw_question_modal`, which
//!   reads straight off the model via the [`QuestionModel`] accessors.
//! - **Update** ([`QuestionModel::update`]): the pure state transition. It
//!   takes an [`QuestionAction`] (an input-event already mapped by `input.rs`)
//!   and returns `(updated model, Vec<QuestionEffect>)`. It performs **no I/O**:
//!   every side effect — replying to the agent, advancing the queue — is
//!   described as a [`QuestionEffect`] value that the event loop executes.
//!
//! Because `update` is pure, a test can feed a script of keystrokes and assert
//! both the final model *and* the emitted effects, then render each
//! intermediate state to a snapshot. That is the "see the interaction" debug
//! loop the old inline arms made impossible.

use neenee_core::{UserQuestion, UserQuestionRequest};

/// The "Other" free-text option label emitted in a reply when the user
/// selected the synthetic "Other" row but left its text field blank — matches
/// the original inline behavior in `event_loop.rs`.
const OTHER_LABEL: &str = "Other";

/// Index reserved for the synthetic "Other" free-text option, which always
/// sits one past the last real option of a question.
fn other_index(q: &UserQuestion) -> usize {
    q.options.len()
}

/// Total selectable rows for a question: the real options plus the "Other"
/// row. Clamped to ≥ 1 so the up/down wrap modulo never divides by zero.
fn option_rows(q: &UserQuestion) -> usize {
    q.options.len() + 1
}

/// A reduced input-event the modal cares about.
///
/// This mirrors the `input::InputAction::Question*` variants but is its own
/// enum so the model has zero coupling to the terminal input layer: tests
/// construct actions directly, and the event loop is the one place that
/// translates `InputAction` → `QuestionAction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionAction {
    /// Move the highlight up, wrapping past the top to the last row.
    Up,
    /// Move the highlight down, wrapping past the last row to the top.
    Down,
    /// Toggle/select the highlighted row (Space or Enter on a row).
    Toggle,
    /// Jump the highlight to the 1-based `n`-th row AND select it.
    Select(usize),
    /// Type a character into the "Other" field (no-op unless "Other" is
    /// highlighted for the active question).
    InsertChar(char),
    /// Delete the last character from the "Other" field.
    Backspace,
    /// Submit all answers (Enter when not toggling a row).
    Submit,
    /// Cancel the modal (Esc).
    Cancel,
}

/// A side effect the event loop must perform after an update. The pure
/// `update` function never touches the channel or the queue — it only
/// *describes* what should happen, and the loop carries it out. This is what
/// makes the state transition testable without a live agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionEffect {
    /// Send the computed answers back to the agent. Carries the request id,
    /// one array of selected option labels per question, and the optional
    /// subagent parent tool-call id the reply must be tagged with for routing.
    Reply {
        request_id: String,
        answers: Vec<Vec<String>>,
    },
    /// The modal closed (submit or cancel). The loop should drop the current
    /// request from the pending queue and, if the queue is now empty, clear
    /// the modal.
    Closed { request_id: String },
}

/// The self-contained state of an open question modal.
///
/// Built from a [`UserQuestionRequest`] via [`QuestionModel::open`]. Held by
/// `App` (replacing the four scattered `question_*` fields) for the lifetime
/// of the modal and consumed back into `None` on close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionModel {
    /// The original request (id + questions). Kept by value so `submit` can
    /// compute replies and the renderer can read questions without borrowing.
    request: UserQuestionRequest,
    /// Which question the user is currently answering (0-based). Multiple
    /// questions are presented one at a time; this is the page cursor.
    current: usize,
    /// Which option row is highlighted for *the active question* (0-based,
    /// where `options.len()` is the synthetic "Other" row).
    highlight: usize,
    /// Per-question selected option indices. Parallels `request.questions`.
    /// Multi-select questions may hold several; single-select holds at most one.
    selected: Vec<Vec<usize>>,
    /// Per-question free text for the "Other" row. Parallels
    /// `request.questions`; only meaningful when that row is selected.
    other_text: Vec<String>,
}

impl QuestionModel {
    /// Initialize a model from a freshly-arriving request, applying the
    /// default-selection rule: multi-select questions start with nothing
    /// selected, single-select questions start with the first option selected.
    /// The highlight begins on the first row.
    pub fn open(request: UserQuestionRequest) -> Self {
        let selected = request
            .questions
            .iter()
            .map(|q| if q.multi_select { Vec::new() } else { vec![0] })
            .collect();
        let other_text = request.questions.iter().map(|_| String::new()).collect();
        Self {
            request,
            current: 0,
            highlight: 0,
            selected,
            other_text,
        }
    }

    // ── Accessors used by the renderer and by tests ──────────────────────

    pub fn request(&self) -> &UserQuestionRequest {
        &self.request
    }
    pub fn current(&self) -> usize {
        self.current
    }
    pub fn highlight(&self) -> usize {
        self.highlight
    }
    pub fn selected(&self) -> &[Vec<usize>] {
        &self.selected
    }
    pub fn other_text(&self) -> &[String] {
        &self.other_text
    }

    /// The active question, or `None` if the model is somehow empty. The
    /// renderer and update logic treat the non-empty case as the norm.
    fn active_question(&self) -> Option<&UserQuestion> {
        self.request.questions.get(self.current)
    }

    /// Compute the reply answers from the current selections. One array of
    /// option labels per question. The synthetic "Other" index resolves to its
    /// typed text, or the literal `"Other"` when left blank — exactly the
    /// legacy `event_loop.rs` behavior.
    pub fn compute_answers(&self) -> Vec<Vec<String>> {
        self.request
            .questions
            .iter()
            .enumerate()
            .map(|(q_idx, q)| {
                let other_idx = other_index(q);
                let other_text = self.other_text.get(q_idx).cloned().unwrap_or_default();
                self.selected
                    .get(q_idx)
                    .map(|sel| {
                        sel.iter()
                            .map(|&opt_idx| {
                                if opt_idx == other_idx {
                                    if other_text.is_empty() {
                                        OTHER_LABEL.to_string()
                                    } else {
                                        other_text.clone()
                                    }
                                } else {
                                    q.options
                                        .get(opt_idx)
                                        .map(|o| o.label.clone())
                                        .unwrap_or_default()
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect()
    }

    /// The pure state transition. Consumes the model and an action, returns
    /// the next model plus any effects to perform. The model is taken by value
    /// so the caller (`App`) replaces its field cleanly with the return value;
    /// within a no-op action the model is returned unchanged.
    ///
    /// Actions are ignored gracefully when they don't apply (e.g. typing into
    /// "Other" while a real option is highlighted) — this mirrors the original
    /// arms, which silently no-oped in the same situations rather than erroring.
    pub fn update(mut self, action: QuestionAction) -> (Self, Vec<QuestionEffect>) {
        // No active question means there is nothing to do; bail unchanged.
        let Some(q) = self.active_question() else {
            return (self, Vec::new());
        };
        let rows = option_rows(q);
        let q = self.current;
        let multi = self
            .request
            .questions
            .get(q)
            .map(|x| x.multi_select)
            .unwrap_or(false);

        let effects = match action {
            QuestionAction::Up => {
                self.highlight = if self.highlight == 0 {
                    rows.saturating_sub(1)
                } else {
                    self.highlight - 1
                };
                Vec::new()
            }
            QuestionAction::Down => {
                self.highlight = (self.highlight + 1) % rows.max(1);
                Vec::new()
            }
            QuestionAction::Toggle => {
                self.toggle(self.highlight, q, multi);
                Vec::new()
            }
            QuestionAction::Select(n) => {
                if n > 0 && n <= rows {
                    self.highlight = n - 1;
                    self.toggle(n - 1, q, multi);
                }
                Vec::new()
            }
            QuestionAction::InsertChar(c) => {
                let other_idx = self.other_index_of(q);
                if self.highlight == other_idx {
                    if let Some(text) = self.other_text.get_mut(q) {
                        text.push(c);
                    }
                }
                Vec::new()
            }
            QuestionAction::Backspace => {
                let other_idx = self.other_index_of(q);
                if self.highlight == other_idx {
                    if let Some(text) = self.other_text.get_mut(q) {
                        text.pop();
                    }
                }
                Vec::new()
            }
            QuestionAction::Submit => {
                let request_id = self.request.id.clone();
                let answers = self.compute_answers();
                vec![
                    QuestionEffect::Reply {
                        request_id: request_id.clone(),
                        answers,
                    },
                    QuestionEffect::Closed { request_id },
                ]
            }
            QuestionAction::Cancel => {
                let request_id = self.request.id.clone();
                vec![QuestionEffect::Closed { request_id }]
            }
        };

        (self, effects)
    }

    /// Shared toggle logic for `Toggle` and `Select`. Multi-select removes the
    /// index if already present (and sorts for stable ordering), otherwise
    /// appends; single-select replaces. Bound-checks the question slot.
    fn toggle(&mut self, idx: usize, q: usize, multi: bool) {
        let Some(sel) = self.selected.get_mut(q) else {
            return;
        };
        if multi {
            if let Some(pos) = sel.iter().position(|&x| x == idx) {
                sel.remove(pos);
            } else {
                sel.push(idx);
                sel.sort();
            }
        } else {
            sel.clear();
            sel.push(idx);
        }
    }

    /// In-place variant of [`update`](Self::update) for callers that hold the
    /// model behind a mutable reference (e.g. inside a borrow-constrained
    /// showcase loop). Applies the transition in place and returns just the
    /// effects. Equivalent to taking the value, calling `update`, and writing
    /// it back.
    #[cfg(debug_assertions)]
    pub fn update_mut(&mut self, action: QuestionAction) -> Vec<QuestionEffect> {
        // We need to move `self` into `update`, so clone the request as a
        // stand-in, swap, then write the result back. This is cheap because the
        // model is small (a request + a few indices/vecs).
        let placeholder = self.request.clone();
        let me = std::mem::replace(self, QuestionModel::open(placeholder));
        let (next, effects) = me.update(action);
        *self = next;
        effects
    }

    /// The synthetic "Other" index for question `q` (its real option count).
    fn other_index_of(&self, q: usize) -> usize {
        self.request
            .questions
            .get(q)
            .map(|x| x.options.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    //! Pure state-machine tests: feed a script of actions, assert the model
    //! and the emitted effects. No terminal, no async, no agent.

    use super::*;
    use neenee_core::{UserQuestion, UserQuestionOption, UserQuestionRequest};

    /// A single-select question with two labeled options.
    fn single_select_req() -> UserQuestionRequest {
        UserQuestionRequest {
            id: "q1".into(),
            questions: vec![UserQuestion {
                header: Some("Style".into()),
                question: "Which error handling crate?".into(),
                options: vec![
                    UserQuestionOption {
                        label: "anyhow".into(),
                        description: Some("Simple".into()),
                    },
                    UserQuestionOption {
                        label: "thiserror".into(),
                        description: Some("Structured".into()),
                    },
                ],
                multi_select: false,
            }],
        }
    }

    /// A multi-select question with two labeled options.
    fn multi_select_req() -> UserQuestionRequest {
        UserQuestionRequest {
            id: "q2".into(),
            questions: vec![UserQuestion {
                header: None,
                question: "Which features?".into(),
                options: vec![
                    UserQuestionOption {
                        label: "a".into(),
                        description: None,
                    },
                    UserQuestionOption {
                        label: "b".into(),
                        description: None,
                    },
                    UserQuestionOption {
                        label: "c".into(),
                        description: None,
                    },
                ],
                multi_select: true,
            }],
        }
    }

    fn two_question_req() -> UserQuestionRequest {
        UserQuestionRequest {
            id: "q3".into(),
            questions: vec![
                UserQuestion {
                    header: None,
                    question: "first?".into(),
                    options: vec![UserQuestionOption {
                        label: "x".into(),
                        description: None,
                    }],
                    multi_select: false,
                },
                UserQuestion {
                    header: None,
                    question: "second?".into(),
                    options: vec![
                        UserQuestionOption {
                            label: "y".into(),
                            description: None,
                        },
                        UserQuestionOption {
                            label: "z".into(),
                            description: None,
                        },
                    ],
                    multi_select: false,
                },
            ],
        }
    }

    // ── Open / defaults ──────────────────────────────────────────────────

    #[test]
    fn single_select_open_defaults_to_first_option_selected() {
        let m = QuestionModel::open(single_select_req());
        assert_eq!(m.current(), 0);
        assert_eq!(m.highlight(), 0);
        assert_eq!(m.selected(), &[vec![0]]); // first option pre-selected
        assert!(m.other_text().iter().all(String::is_empty));
    }

    #[test]
    fn multi_select_open_defaults_to_none_selected() {
        let m = QuestionModel::open(multi_select_req());
        assert_eq!(m.selected(), &[Vec::<usize>::new()]);
    }

    // ── Navigation (Up/Down) ─────────────────────────────────────────────

    #[test]
    fn down_wraps_around() {
        let mut m = QuestionModel::open(single_select_req());
        // 2 options + Other = 3 rows. 0 -> 1 -> 2 -> 0.
        m = m.update(QuestionAction::Down).0;
        assert_eq!(m.highlight(), 1);
        m = m.update(QuestionAction::Down).0;
        assert_eq!(m.highlight(), 2);
        m = m.update(QuestionAction::Down).0;
        assert_eq!(m.highlight(), 0);
    }

    #[test]
    fn up_wraps_around() {
        let mut m = QuestionModel::open(single_select_req());
        m = m.update(QuestionAction::Up).0; // 0 -> 2
        assert_eq!(m.highlight(), 2);
        m = m.update(QuestionAction::Up).0;
        assert_eq!(m.highlight(), 1);
    }

    #[test]
    fn navigation_emits_no_effects() {
        let m = QuestionModel::open(single_select_req());
        let (_, eff) = m.update(QuestionAction::Down);
        assert!(eff.is_empty());
    }

    // ── Toggle / select (single-select) ──────────────────────────────────

    #[test]
    fn single_select_toggle_replaces_selection() {
        let m = QuestionModel::open(single_select_req());
        let m = m.update(QuestionAction::Down).0; // highlight -> 1 (thiserror)
        let (m, eff) = m.update(QuestionAction::Toggle);
        assert_eq!(m.selected(), &[vec![1]]);
        assert!(eff.is_empty());
    }

    #[test]
    fn single_select_toggle_same_option_keeps_it() {
        // Toggling the already-selected index in single-select replaces with
        // itself (clears then pushes) — so it stays selected.
        let m = QuestionModel::open(single_select_req());
        let (m, _) = m.update(QuestionAction::Toggle); // highlight 0 already selected
        assert_eq!(m.selected(), &[vec![0]]);
    }

    #[test]
    fn digit_select_jumps_and_selects() {
        let m = QuestionModel::open(single_select_req());
        let (m, _) = m.update(QuestionAction::Select(2)); // 1-based -> index 1
        assert_eq!(m.highlight(), 1);
        assert_eq!(m.selected(), &[vec![1]]);
    }

    #[test]
    fn digit_select_out_of_range_is_ignored() {
        let m = QuestionModel::open(single_select_req()); // 3 rows
        let (m, _) = m.update(QuestionAction::Select(9));
        assert_eq!(m.highlight(), 0); // unchanged
        assert_eq!(m.selected(), &[vec![0]]); // unchanged
    }

    // ── Toggle / select (multi-select) ───────────────────────────────────

    #[test]
    fn multi_select_toggle_adds_and_sorts() {
        let m = QuestionModel::open(multi_select_req()); // rows 0,1,2 + Other(3)
        let m = m.update(QuestionAction::Select(3)).0; // select index 2 ("c")
        let m = m.update(QuestionAction::Select(1)).0; // select index 0 ("a")
        assert_eq!(m.selected(), &[vec![0, 2]]); // sorted
    }

    #[test]
    fn multi_select_toggle_removes_if_present() {
        let m = QuestionModel::open(multi_select_req());
        let m = m.update(QuestionAction::Select(1)).0; // add 0
        assert_eq!(m.selected(), &[vec![0]]);
        let (m, _) = m.update(QuestionAction::Select(1)); // toggle off 0
        assert_eq!(m.selected(), &[Vec::<usize>::new()]);
    }

    // ── "Other" free text ────────────────────────────────────────────────

    #[test]
    fn insert_char_only_works_when_other_highlighted() {
        let mut m = QuestionModel::open(single_select_req()); // highlight 0
        let (m_, _) = m.update(QuestionAction::InsertChar('h')); // not on Other
        m = m_;
        assert_eq!(m.other_text(), &[""]);

        // Move to Other (index 2) then type.
        m = m.update(QuestionAction::Select(3)).0; // 1-based 3 -> index 2 (Other)
        let (m_, _) = m.update(QuestionAction::InsertChar('h'));
        let (m_, _) = m_.update(QuestionAction::InsertChar('i'));
        m = m_;
        assert_eq!(m.other_text(), &["hi"]);
    }

    #[test]
    fn backspace_deletes_last_char() {
        let m = QuestionModel::open(single_select_req());
        let m = m.update(QuestionAction::Select(3)).0; // focus Other
        let m = m.update(QuestionAction::InsertChar('a')).0;
        let m = m.update(QuestionAction::InsertChar('b')).0;
        let (m, _) = m.update(QuestionAction::Backspace);
        assert_eq!(m.other_text(), &["a"]);
    }

    #[test]
    fn backspace_when_not_on_other_is_noop() {
        let m = QuestionModel::open(single_select_req()); // highlight 0
        let (m, _) = m.update(QuestionAction::Backspace);
        assert_eq!(m.other_text(), &[""]);
    }

    // ── Submit / cancel effects ──────────────────────────────────────────

    #[test]
    fn submit_emits_reply_and_closed_with_labels() {
        let m = QuestionModel::open(single_select_req());
        let (m, eff) = m.update(QuestionAction::Submit);
        assert_eq!(
            eff,
            vec![
                QuestionEffect::Reply {
                    request_id: "q1".into(),
                    answers: vec![vec!["anyhow".to_string()]], // default selection kept
                },
                QuestionEffect::Closed {
                    request_id: "q1".into()
                },
            ]
        );
        // Model is returned (the event loop drops it via the Closed effect).
        assert_eq!(m.current(), 0);
    }

    #[test]
    fn submit_with_other_blank_emits_literal_other() {
        let m = QuestionModel::open(single_select_req());
        let m = m.update(QuestionAction::Select(3)).0; // select Other (index 2)
        let (_, eff) = m.update(QuestionAction::Submit);
        assert_eq!(
            eff,
            vec![
                QuestionEffect::Reply {
                    request_id: "q1".into(),
                    answers: vec![vec!["Other".to_string()]],
                },
                QuestionEffect::Closed {
                    request_id: "q1".into()
                },
            ]
        );
    }

    #[test]
    fn submit_with_other_text_emits_the_text() {
        let m = QuestionModel::open(single_select_req());
        let m = m.update(QuestionAction::Select(3)).0;
        let m = m.update(QuestionAction::InsertChar('c')).0;
        let m = m.update(QuestionAction::InsertChar('u')).0;
        let m = m.update(QuestionAction::InsertChar('s')).0;
        let m = m.update(QuestionAction::InsertChar('t')).0;
        let m = m.update(QuestionAction::InsertChar('o')).0;
        let m = m.update(QuestionAction::InsertChar('m')).0;
        let (_, eff) = m.update(QuestionAction::Submit);
        assert_eq!(
            eff,
            vec![
                QuestionEffect::Reply {
                    request_id: "q1".into(),
                    answers: vec![vec!["custom".to_string()]],
                },
                QuestionEffect::Closed {
                    request_id: "q1".into()
                },
            ]
        );
    }

    #[test]
    fn cancel_emits_only_closed_no_reply() {
        let m = QuestionModel::open(single_select_req());
        let (_, eff) = m.update(QuestionAction::Cancel);
        assert_eq!(
            eff,
            vec![QuestionEffect::Closed {
                request_id: "q1".into()
            }]
        );
    }

    // ── Multi-question answers ───────────────────────────────────────────

    #[test]
    fn compute_answers_covers_all_questions() {
        let m = QuestionModel::open(two_question_req());
        // Defaults: q0 -> [x] (index 0), q1 -> [y] (index 0).
        assert_eq!(
            m.compute_answers(),
            vec![vec!["x".to_string()], vec!["y".to_string()]]
        );
    }

    // ── Single-select: switch selection back and forth ──────────────────

    #[test]
    fn single_select_switches_selection_on_repeated_toggle() {
        // Select "thiserror" (1), then switch back to "anyhow" (0). The
        // single-select contract is "replace", so each toggle overwrites.
        let m = QuestionModel::open(single_select_req());
        let m = m.update(QuestionAction::Select(2)).0; // -> thiserror (idx 1)
        assert_eq!(m.selected(), &[vec![1]]);
        let m = m.update(QuestionAction::Select(1)).0; // -> anyhow (idx 0)
        assert_eq!(m.selected(), &[vec![0]]);
    }

    #[test]
    fn single_select_arrow_then_space_replaces() {
        // The keyboard path: Down to highlight "thiserror", Space to confirm.
        let m = QuestionModel::open(single_select_req());
        let m = m.update(QuestionAction::Down).0; // highlight 1
        let (m, eff) = m.update(QuestionAction::Toggle); // Space
        assert_eq!(m.selected(), &[vec![1]]);
        assert!(eff.is_empty(), "selecting emits no effect");
    }

    // ── Single-select: discontinuous keystrokes (move, then pick) ────────

    #[test]
    fn single_select_move_then_move_again_without_selecting_keeps_default() {
        // Navigating with arrows alone never changes the selection — the
        // default stays until the user explicitly Space/selects a row.
        let m = QuestionModel::open(single_select_req()); // anyhow selected
        let m = m.update(QuestionAction::Down).0; // highlight thiserror
        let m = m.update(QuestionAction::Down).0; // highlight Other
        let m = m.update(QuestionAction::Up).0; // back to thiserror
        assert_eq!(m.highlight(), 1);
        assert_eq!(m.selected(), &[vec![0]], "arrows never select");
    }

    #[test]
    fn single_select_arrow_then_space_then_arrow_again() {
        // Discontinuous: move away from the default, confirm with Space, then
        // keep moving — the committed selection must persist.
        let m = QuestionModel::open(single_select_req());
        let m = m.update(QuestionAction::Down).0; // highlight thiserror
        let m = m.update(QuestionAction::Toggle).0; // Space -> select it
        assert_eq!(m.selected(), &[vec![1]]);
        let m = m.update(QuestionAction::Down).0; // move to Other
        assert_eq!(m.highlight(), 2);
        assert_eq!(m.selected(), &[vec![1]], "selection persists after moving");
    }

    #[test]
    fn single_select_jump_back_and_forth_between_two() {
        // Select option 2, then 1, then 2 again via digit keys — the
        // single-select slot replaces each time.
        let m = QuestionModel::open(single_select_req());
        let m = m.update(QuestionAction::Select(2)).0; // thiserror
        assert_eq!(m.selected(), &[vec![1]]);
        assert_eq!(m.highlight(), 1);
        let m = m.update(QuestionAction::Select(1)).0; // anyhow
        assert_eq!(m.selected(), &[vec![0]]);
        assert_eq!(m.highlight(), 0);
        let m = m.update(QuestionAction::Select(2)).0; // thiserror again
        assert_eq!(m.selected(), &[vec![1]]);
    }

    // ── Multi-select: add several, deselect, reselect ───────────────────

    #[test]
    fn multi_select_select_all_three_then_deselect_middle() {
        // Pick all three real options, then toggle the middle one off.
        let m = QuestionModel::open(multi_select_req());
        let m = m.update(QuestionAction::Select(1)).0; // a
        let m = m.update(QuestionAction::Select(2)).0; // b
        let m = m.update(QuestionAction::Select(3)).0; // c
        assert_eq!(m.selected(), &[vec![0, 1, 2]]);
        let (m, _) = m.update(QuestionAction::Select(2)); // deselect b
        assert_eq!(m.selected(), &[vec![0, 2]]);
    }

    #[test]
    fn multi_select_toggle_same_option_twice_is_idempotent() {
        // Toggle on then off returns to empty.
        let m = QuestionModel::open(multi_select_req());
        let m = m.update(QuestionAction::Select(1)).0;
        assert_eq!(m.selected(), &[vec![0]]);
        let (m, _) = m.update(QuestionAction::Select(1));
        assert_eq!(m.selected(), &[Vec::<usize>::new()]);
        let (m, _) = m.update(QuestionAction::Select(1)); // reselect
        assert_eq!(m.selected(), &[vec![0]]);
    }

    #[test]
    fn multi_select_plus_other_text_all_in_reply() {
        // Select two real options AND "Other" with typed text; the reply
        // must list all three labels in index order.
        let m = QuestionModel::open(multi_select_req());
        let m = m.update(QuestionAction::Select(1)).0; // a (idx 0)
        let m = m.update(QuestionAction::Select(3)).0; // c (idx 2)
        let m = m.update(QuestionAction::Select(4)).0; // Other (idx 3)
        let m = m.update(QuestionAction::InsertChar('z')).0;
        assert_eq!(m.selected(), &[vec![0, 2, 3]]);
        let (_, eff) = m.update(QuestionAction::Submit);
        assert_eq!(
            eff,
            vec![
                QuestionEffect::Reply {
                    request_id: "q2".into(),
                    answers: vec![vec!["a".to_string(), "c".to_string(), "z".to_string()]],
                },
                QuestionEffect::Closed {
                    request_id: "q2".into()
                },
            ]
        );
    }

    // ── "Other" edge cases ───────────────────────────────────────────────

    #[test]
    fn multi_select_other_blank_in_reply_emits_literal_other() {
        // Selecting "Other" with no typed text resolves to "Other" in the
        // reply, even alongside real selections.
        let m = QuestionModel::open(multi_select_req());
        let m = m.update(QuestionAction::Select(1)).0; // a
        let m = m.update(QuestionAction::Select(4)).0; // Other (blank)
        let (_, eff) = m.update(QuestionAction::Submit);
        assert_eq!(
            eff,
            vec![
                QuestionEffect::Reply {
                    request_id: "q2".into(),
                    answers: vec![vec!["a".to_string(), "Other".to_string()]],
                },
                QuestionEffect::Closed {
                    request_id: "q2".into()
                },
            ]
        );
    }

    #[test]
    fn typing_into_other_does_not_affect_a_real_option_highlight() {
        // When a real option is highlighted, InsertChar/Backspace are no-ops
        // on the other_text field (they never write there).
        let m = QuestionModel::open(single_select_req()); // highlight 0
        let (m, _) = m.update(QuestionAction::InsertChar('x'));
        let (m, _) = m.update(QuestionAction::Backspace);
        assert_eq!(m.other_text(), &[""]);
    }

    // ── Multi-question (paged) interaction ───────────────────────────────

    #[test]
    fn two_question_reply_carries_one_array_per_question() {
        // Open a two-question request and submit without changing selections:
        // both single-select questions keep their default first option.
        // The reply carries one answer array per question, in question order.
        let m = QuestionModel::open(two_question_req());
        let (_, eff) = m.update(QuestionAction::Submit);
        // q0 single-select defaults [x]; q1 single-select defaults [y].
        assert_eq!(
            eff,
            vec![
                QuestionEffect::Reply {
                    request_id: "q3".into(),
                    answers: vec![vec!["x".to_string()], vec!["y".to_string()]],
                },
                QuestionEffect::Closed {
                    request_id: "q3".into()
                },
            ]
        );
    }

    #[test]
    fn cancel_discards_pending_selections() {
        // Even after making selections, Cancel emits only Closed — no reply,
        // so the agent sees the question was dismissed without an answer.
        let m = QuestionModel::open(multi_select_req());
        let m = m.update(QuestionAction::Select(1)).0;
        let m = m.update(QuestionAction::Select(2)).0;
        assert_eq!(m.selected(), &[vec![0, 1]]);
        let (_, eff) = m.update(QuestionAction::Cancel);
        assert_eq!(
            eff,
            vec![QuestionEffect::Closed {
                request_id: "q2".into()
            }]
        );
    }

    // ── Full interaction script (regression) ─────────────────────────────

    #[test]
    fn full_script_multi_select_then_other_then_submit() {
        // A realistic script: open a multi-select, pick two, switch to Other,
        // type a custom answer, submit — and assert both final model and the
        // emitted reply in one go. This is the "see the interaction" test the
        // old inline arms could never express.
        let m = QuestionModel::open(multi_select_req());
        let m = m.update(QuestionAction::Select(1)).0; // "a"
        let m = m.update(QuestionAction::Down).0; // highlight -> "b"
        let m = m.update(QuestionAction::Toggle).0; // toggle "b" on
        let m = m.update(QuestionAction::Select(4)).0; // Other (1-based 4 -> idx 3)
        let m = m.update(QuestionAction::InsertChar('z')).0;
        let (m, eff) = m.update(QuestionAction::Submit);

        assert_eq!(m.selected(), &[vec![0, 1, 3]]); // a, b, and Other
        assert_eq!(m.other_text(), &["z".to_string()]);
        assert_eq!(
            eff,
            vec![
                QuestionEffect::Reply {
                    request_id: "q2".into(),
                    answers: vec![vec!["a".to_string(), "b".to_string(), "z".to_string()]],
                },
                QuestionEffect::Closed {
                    request_id: "q2".into()
                },
            ]
        );
    }

    // ── Rendering film: see the interaction, frame by frame ──────────────
    //
    // The pure `update` lets a test feed a *script* of actions and render every
    // intermediate state. Each frame is snapshotted, so `cargo insta review`
    // reads like a flip-book of the modal responding to keystrokes — the
    // "can I see this component behave correctly?" debug loop the old inline
    // arms made impossible. Regenerate after an intentional visual change:
    //   INSTA_UPDATE=always cargo test -p neenee-code question_modal_film

    /// Render a question model into a trimmed grid of cell symbols at a fixed
    /// size, mirroring `render::snapshot_tests::render_grid` but for the modal
    /// instead of a tool step. Returns only the painted rows so the snapshot
    /// stays compact and diffable.
    fn render_question_grid(model: &QuestionModel, width: u16, height: u16) -> String {
        use crate::tui::render::{Theme, draw_question_modal};

        let mut terminal = neenee_tui::TestTerminal::new(width, height);
        terminal.draw(|f| {
            let mut scroll = 0;
            draw_question_modal(
                f,
                model.request(),
                model.current(),
                model.selected(),
                model.other_text(),
                model.highlight(),
                &mut scroll,
                &Theme::default(),
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

    /// A flip-book: open a multi-select, then walk through a realistic
    /// keystroke script, snapshotting the rendered modal at each step. The
    /// combined snapshot is the whole interaction as text — review it to see
    /// the highlight move, selections toggle, and the "Other" field fill in.
    #[test]
    fn question_modal_film_multiselect_interaction() {
        let m = QuestionModel::open(multi_select_req());
        let mut film = String::new();
        film.push_str("=== open ===\n");
        film.push_str(&render_question_grid(&m, 72, 26));

        // ↓ highlight moves to "b" (row 1)
        let m = m.update(QuestionAction::Down).0;
        film.push_str("\n\n=== down → highlight 'b' ===\n");
        film.push_str(&render_question_grid(&m, 72, 26));

        // Space toggles "b" on
        let m = m.update(QuestionAction::Toggle).0;
        film.push_str("\n\n=== space → toggle 'b' selected ===\n");
        film.push_str(&render_question_grid(&m, 72, 26));

        // 1 selects "a"
        let m = m.update(QuestionAction::Select(1)).0;
        film.push_str("\n\n=== '1' → select 'a' ===\n");
        film.push_str(&render_question_grid(&m, 72, 26));

        // ↓ to "Other" (index 3), type "custom"
        let m = m.update(QuestionAction::Select(4)).0;
        let m = m.update(QuestionAction::InsertChar('c')).0;
        let m = m.update(QuestionAction::InsertChar('u')).0;
        let m = m.update(QuestionAction::InsertChar('s')).0;
        let m = m.update(QuestionAction::InsertChar('t')).0;
        film.push_str("\n\n=== '4' + 'custom' → focus 'Other', type ===\n");
        film.push_str(&render_question_grid(&m, 72, 26));

        insta::assert_snapshot!(film);
    }

    /// Single-select film: the radio-dot selection replacing on jump, and the
    /// highlight ring moving with arrow keys.
    #[test]
    fn question_modal_film_single_select_jump() {
        let m = QuestionModel::open(single_select_req());
        let mut film = String::new();
        film.push_str("=== open (first selected by default) ===\n");
        film.push_str(&render_question_grid(&m, 64, 16));

        // ↓ to "thiserror", Space replaces the selection
        let m = m.update(QuestionAction::Down).0;
        let m = m.update(QuestionAction::Toggle).0;
        film.push_str("\n\n=== down + space → select 'thiserror' ===\n");
        film.push_str(&render_question_grid(&m, 64, 16));

        insta::assert_snapshot!(film);
    }

    /// Multi-select film: pick all three, then deselect the middle one — the
    /// checkbox markers flip from `[x]` to `[ ]` and back, and the highlight
    /// ring follows the cursor through `↑`/`↓`.
    #[test]
    fn question_modal_film_multiselect_checkboxes() {
        let m = QuestionModel::open(multi_select_req());
        let mut film = String::new();
        film.push_str("=== open (none selected) ===\n");
        film.push_str(&render_question_grid(&m, 72, 26));

        // 1, 2, 3 → select a, b, c
        let m = m.update(QuestionAction::Select(1)).0;
        let m = m.update(QuestionAction::Select(2)).0;
        let m = m.update(QuestionAction::Select(3)).0;
        film.push_str("\n\n=== 1,2,3 → select all three ===\n");
        film.push_str(&render_question_grid(&m, 72, 26));

        // 2 again → deselect b (the middle checkbox flips off)
        let m = m.update(QuestionAction::Select(2)).0;
        film.push_str("\n\n=== '2' again → deselect 'b' ===\n");
        film.push_str(&render_question_grid(&m, 72, 26));

        insta::assert_snapshot!(film);
    }

    /// Multi-page film: open a two-question request, make a selection, then
    /// submit — the footer shows "Question 1/2" and the title advances. This
    /// pins the paged header rendering.
    #[test]
    fn question_modal_film_two_question_header() {
        let m = QuestionModel::open(two_question_req());
        let mut film = String::new();
        film.push_str("=== open: Question 1/2 ===\n");
        film.push_str(&render_question_grid(&m, 64, 16));

        insta::assert_snapshot!(film);
    }
}
