//! Simpler showcases: provider picker, model editor, history search, sessions
//! picker, activity modal, help, and toasts.
//!
//! These share the [`common::run_showcase`] runner; each is its own small
//! state struct + key handler. Several are navigation-only (up/down/tab).

use std::cell::Cell;
use std::collections::HashMap;
use std::io;

use crossterm::event::KeyCode;

use neenee_core::{ProviderPickerRow, ProviderPickerSnapshot, SessionOverview};
use neenee_core::{Pursuit, TodoId, TodoItem, TodoList, TodoStatus};

use crate::showcase::common::{self, ShowAction};
use crate::tui::ActivityTab;
use crate::tui::fuzzy;
use crate::tui::layout::LayoutMap;
use crate::tui::render::Theme;
use crate::tui::render::{
    ActivityModalView, draw_activity_modal, draw_armed_toast, draw_copy_toast, draw_help_modal,
    draw_history_modal, draw_model_editor, draw_models_modal, draw_sessions_modal,
};

// ─────────────────────────── provider picker ──────────────────────────────

struct ProviderState {
    index: usize,
    query: String,
    cursor: usize,
    scroll: usize,
    search: bool,
    picker: ProviderPickerSnapshot,
    key_status: HashMap<String, bool>,
}

pub fn provider() -> io::Result<()> {
    let theme = Theme::default();
    let mk = |id: &str, name: &str, models: &[&str], fav: bool, key: bool| ProviderPickerRow {
        id: id.to_string(),
        name: name.to_string(),
        model: models.first().copied().unwrap_or("").to_string(),
        models: models.iter().map(|m| m.to_string()).collect(),
        model_info: Vec::new(),
        builtin: true,
        protocol: String::new(),
        base_url: String::new(),
        key_ready: key,
        favorite: fav,
        last_used_ms: fav.then_some(1_700_000_000_000),
    };
    let picker = ProviderPickerSnapshot {
        default_id: "anthropic".into(),
        rows: vec![
            mk("openai", "OpenAI", &["gpt-4o", "gpt-4o-mini"], false, false),
            mk(
                "anthropic",
                "Anthropic",
                &["claude-opus-4-8", "claude-sonnet-4-6"],
                true,
                true,
            ),
            mk("kimi-code", "Kimi Code", &["kimi-k2.7-code"], false, true),
        ],
    };
    let key_status: HashMap<String, bool> = picker
        .rows
        .iter()
        .map(|r| (r.id.clone(), r.key_ready))
        .collect();

    let mut state = ProviderState {
        index: 0,
        query: String::new(),
        cursor: 0,
        scroll: 0,
        search: false,
        picker,
        key_status,
    };

    common::run_showcase(
        &mut state,
        |f, s| {
            let title = format!(
                " model picker · {} providers · / to search · q/Ctrl+C=quit",
                s.picker.rows.len(),
            );
            let hint = " ↑↓ navigate · Enter select · / search · Esc back/quit ";
            common::draw_with_chrome(f, &title, hint, &theme, |f| {
                let mut lm = LayoutMap::new();
                let query = if s.search { s.query.trim() } else { "" };
                let ranked = crate::tui::providers_filtered_from(&s.picker, query);
                // The draw closure borrows state immutably; follow-selection
                // re-anchors the scroll each frame, so a frame-local offset is
                // sufficient for the showcase.
                let mut scroll = s.scroll;
                draw_models_modal(
                    f,
                    &mut lm,
                    &ranked,
                    &[],
                    None,
                    None,
                    false,
                    &s.picker.default_id,
                    "",
                    s.index,
                    &s.key_status,
                    &s.query,
                    s.cursor,
                    &mut scroll,
                    true,
                    s.search,
                    &theme,
                );
            });
        },
        |s, key| -> ShowAction {
            match key.code {
                KeyCode::Esc => {
                    // Two-stage Esc, mirroring the real picker: search → browse,
                    // then browse → quit.
                    if s.search {
                        s.search = false;
                        s.query.clear();
                        s.cursor = 0;
                        s.index = 0;
                        return ShowAction::Continue;
                    }
                    ShowAction::Exit
                }
                KeyCode::Up => {
                    if s.index > 0 {
                        s.index -= 1;
                    }
                    ShowAction::Continue
                }
                KeyCode::Down => {
                    s.index += 1;
                    ShowAction::Continue
                }
                KeyCode::Char('/') if !s.search => {
                    s.search = true;
                    s.index = 0;
                    ShowAction::Continue
                }
                KeyCode::Backspace if s.search => {
                    if s.cursor > 0 {
                        s.cursor -= 1;
                        s.query.remove(s.cursor);
                    }
                    s.index = 0;
                    ShowAction::Continue
                }
                KeyCode::Char(c) if s.search => {
                    s.query.insert(s.cursor, c);
                    s.cursor += 1;
                    s.index = 0;
                    ShowAction::Continue
                }
                _ => ShowAction::Continue,
            }
        },
    )
}

// ──────────────────────────── model editor ────────────────────────────────

struct ModelEditorState {
    input: String, // the live API-key value
    cursor: usize,
}

pub fn model_editor() -> io::Result<()> {
    let theme = Theme::default();
    let mut state = ModelEditorState {
        input: String::new(),
        cursor: 0,
    };

    common::run_showcase(
        &mut state,
        |f, s| {
            let title = " key editor · API key · q/Ctrl+C=quit".to_string();
            let hint = " type to edit · Enter save · Esc quit ";
            common::draw_with_chrome(f, &title, hint, &theme, |f| {
                draw_model_editor(f, "OpenAI", &s.input, s.cursor, true, 0, None, None, &theme);
            });
        },
        |s, key| -> ShowAction {
            match key.code {
                KeyCode::Esc => ShowAction::Exit,
                KeyCode::Backspace => {
                    if s.cursor > 0 {
                        s.cursor -= 1;
                        s.input.remove(s.cursor);
                    }
                    ShowAction::Continue
                }
                KeyCode::Char(c) => {
                    s.input.insert(s.cursor, c);
                    s.cursor += 1;
                    ShowAction::Continue
                }
                _ => ShowAction::Continue,
            }
        },
    )
}

// ──────────────────────────── history search ──────────────────────────────

struct HistoryState {
    history: Vec<String>,
    query: String,
    cursor: usize,
    index: usize,
}

pub fn history() -> io::Result<()> {
    let theme = Theme::default();
    let history: Vec<String> = vec![
        "Refactor the renderer into overlay modules".into(),
        "Fix the tool_call_id routing bug".into(),
        "Add a question modal MVU extraction".into(),
        "Wire the showcase subcommand into main".into(),
        "How does the permission sheet scroll work?".into(),
        "cargo test -p neenee-code snapshot_tests".into(),
        "Update the README with the new showcase command".into(),
        "Why does the activity bar hide during streaming?".into(),
    ];
    let mut state = HistoryState {
        history,
        query: String::new(),
        cursor: 0,
        index: 0,
    };

    common::run_showcase(
        &mut state,
        |f, s| {
            let ranked = fuzzy::rank(&s.history, &s.query);
            let index = s.index.min(ranked.len().saturating_sub(1));
            let title = format!(
                " history search · {} entries · type to fuzzy-filter · q/Ctrl+C=quit",
                s.history.len(),
            );
            let hint = " type to filter · ↑↓ navigate · Esc clear/quit ";
            common::draw_with_chrome(f, &title, hint, &theme, |f| {
                let mut lm = LayoutMap::new();
                let mut scroll = 0;
                draw_history_modal(
                    f,
                    &mut lm,
                    &s.history,
                    &s.query,
                    s.cursor,
                    &ranked,
                    index,
                    &mut scroll,
                    true,
                    false,
                    true,
                    &theme,
                );
            });
        },
        |s, key| -> ShowAction {
            match key.code {
                KeyCode::Esc => {
                    if s.query.is_empty() {
                        return ShowAction::Exit;
                    }
                    s.query.clear();
                    s.cursor = 0;
                    s.index = 0;
                    ShowAction::Continue
                }
                KeyCode::Up => {
                    if s.index > 0 {
                        s.index -= 1;
                    }
                    ShowAction::Continue
                }
                KeyCode::Down => {
                    s.index += 1;
                    ShowAction::Continue
                }
                KeyCode::Backspace => {
                    if s.cursor > 0 {
                        s.cursor -= 1;
                        s.query.remove(s.cursor);
                    }
                    s.index = 0;
                    ShowAction::Continue
                }
                KeyCode::Char(c) => {
                    s.query.insert(s.cursor, c);
                    s.cursor += 1;
                    s.index = 0;
                    ShowAction::Continue
                }
                _ => ShowAction::Continue,
            }
        },
    )
}

// ──────────────────────────── sessions picker ─────────────────────────────

struct SessionsState {
    sessions: Vec<SessionOverview>,
    index: usize,
}

pub fn sessions() -> io::Result<()> {
    let theme = Theme::default();
    let sessions: Vec<SessionOverview> = vec![
        SessionOverview {
            id: "abc123".into(),
            overview: "Refactor the renderer into overlay modules".into(),
            created_at: now_ms() - 3_600_000,
            updated_at: now_ms() - 600_000,
            message_count: 12,
            active: true,
        },
        SessionOverview {
            id: "def456".into(),
            overview: "Fix the tool_call_id routing bug".into(),
            created_at: now_ms() - 86_400_000,
            updated_at: now_ms() - 43_200_000,
            message_count: 4,
            active: false,
        },
        SessionOverview {
            id: "ghi789".into(),
            overview: "Add the question modal MVU extraction".into(),
            created_at: now_ms() - 172_800_000,
            updated_at: now_ms() - 172_800_000,
            message_count: 28,
            active: false,
        },
    ];
    let mut state = SessionsState { sessions, index: 0 };

    common::run_showcase(
        &mut state,
        |f, s| {
            let index = s.index.min(s.sessions.len().saturating_sub(1));
            let title = format!(
                " sessions picker · {} sessions · q/Ctrl+C=quit",
                s.sessions.len()
            );
            let hint = " ↑↓ navigate · Esc quit ";
            common::draw_with_chrome(f, &title, hint, &theme, |f| {
                draw_sessions_modal(f, &s.sessions, index, &theme);
            });
        },
        |s, key| -> ShowAction {
            match key.code {
                KeyCode::Esc => ShowAction::Exit,
                KeyCode::Up => {
                    if s.index > 0 {
                        s.index -= 1;
                    }
                    ShowAction::Continue
                }
                KeyCode::Down => {
                    s.index += 1;
                    ShowAction::Continue
                }
                _ => ShowAction::Continue,
            }
        },
    )
}

// ──────────────────────────── activity modal ──────────────────────────────

struct ActivityState {
    pursuit: Pursuit,
    todos: TodoList,
    tab: ActivityTab,
    scroll: Cell<usize>,
    started: std::time::Instant,
}

pub fn activity() -> io::Result<()> {
    let theme = Theme::default();
    let pursuit = Pursuit {
        objective: "Land the component showcase framework".into(),
        is_complete: false,
    };
    let todos = TodoList {
        items: vec![
            TodoItem {
                id: TodoId(1),
                content: "Restructure showcase into a directory module".into(),
                status: TodoStatus::Completed,
                created_at: 0,
                updated_at: 0,
            },
            TodoItem {
                id: TodoId(2),
                content: "Implement permission sheet showcase".into(),
                status: TodoStatus::InProgress,
                created_at: 0,
                updated_at: 0,
            },
            TodoItem {
                id: TodoId(3),
                content: "Wire all modals into the dispatcher".into(),
                status: TodoStatus::Pending,
                created_at: 0,
                updated_at: 0,
            },
            TodoItem {
                id: TodoId(4),
                content: "Verify build + clippy".into(),
                status: TodoStatus::Pending,
                created_at: 0,
                updated_at: 0,
            },
        ],
        ..Default::default()
    };
    let mut state = ActivityState {
        pursuit,
        todos,
        tab: ActivityTab::Activity,
        scroll: Cell::new(0),
        started: std::time::Instant::now(),
    };

    common::run_showcase(
        &mut state,
        |f, s| {
            let title = " activity modal · q/Ctrl+C=quit";
            let hint = " ←→ / Tab cycle tabs · ↑↓ scroll · Esc quit ";
            common::draw_with_chrome(f, title, hint, &theme, |f| {
                let mut scroll = s.scroll.get();
                draw_activity_modal(
                    f,
                    ActivityModalView {
                        active_tab: s.tab,
                        pursuit: Some(&s.pursuit),
                        todos: Some(&s.todos),
                        user_prompt: Some("Build a showcase for all TUI components"),
                        round_count: 3,
                        current_turn: 2,
                        review_alert: "",
                        current_model: "claude-sonnet-4-5",
                        turn_started_at: Some(s.started),
                        activity: "running envoy · exploring the codebase",
                    },
                    &mut scroll,
                    &theme,
                );
                s.scroll.set(scroll);
            });
        },
        |s, key| -> ShowAction {
            match key.code {
                KeyCode::Esc => ShowAction::Exit,
                KeyCode::Left | KeyCode::Char('h') => {
                    s.tab = crate::tui::ActivityTab::Activity;
                    s.scroll.set(0);
                    ShowAction::Continue
                }
                KeyCode::Right | KeyCode::Tab | KeyCode::Char('l') => {
                    s.tab = crate::tui::ActivityTab::Todos;
                    s.scroll.set(0);
                    ShowAction::Continue
                }
                KeyCode::Up => {
                    if s.scroll.get() > 0 {
                        s.scroll.set(s.scroll.get().saturating_sub(1));
                    }
                    ShowAction::Continue
                }
                KeyCode::Down => {
                    s.scroll.set(s.scroll.get() + 1);
                    ShowAction::Continue
                }
                _ => ShowAction::Continue,
            }
        },
    )
}

// ──────────────────────────── help + toast ────────────────────────────────

pub fn help() -> io::Result<()> {
    let theme = Theme::default();
    let mut state = ();
    common::run_showcase(
        &mut state,
        |f, _| {
            common::draw_with_chrome(
                f,
                " help · keybindings · q/Esc=quit",
                " Esc quit ",
                &theme,
                |f| {
                    let mut scroll = 0;
                    draw_help_modal(f, &mut scroll, &theme);
                },
            );
        },
        |_, key| match key.code {
            KeyCode::Esc => ShowAction::Exit,
            _ => ShowAction::Continue,
        },
    )
}

struct ToastState {
    idx: usize,
}

pub fn toast() -> io::Result<()> {
    let theme = Theme::default();
    let variants: [(&str, bool); 3] = [
        ("copied to clipboard", false),
        ("clipboard read failed", true),
        ("press Ctrl+C again to exit", false), // armed uses a different fn
    ];
    let mut state = ToastState { idx: 0 };

    common::run_showcase(
        &mut state,
        |f, s| {
            let (msg, failed) = variants[s.idx];
            let title = format!(
                " toast · variant {}/{} · Tab=next · q/Ctrl+C=quit",
                s.idx + 1,
                variants.len()
            );
            let hint = " Tab next · Esc quit ";
            common::draw_with_chrome(f, &title, hint, &theme, |f| {
                if s.idx == variants.len() - 1 {
                    draw_armed_toast(f, msg, &theme);
                } else {
                    draw_copy_toast(f, msg, failed, &theme);
                }
            });
        },
        |s, key| match key.code {
            KeyCode::Tab => {
                s.idx = (s.idx + 1) % variants.len();
                ShowAction::Continue
            }
            KeyCode::Esc => ShowAction::Exit,
            _ => ShowAction::Continue,
        },
    )
}

// ────────────────────────────── helpers ───────────────────────────────────

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
