//! Simpler showcases: provider picker, model editor, history search, sessions
//! picker, session-context modal, activity modal, help, and toasts.
//!
//! These share the [`common::run_showcase`] runner; each is its own small
//! state struct + key handler. Several are navigation-only (up/down/tab).

use std::cell::Cell;
use std::collections::HashMap;
use std::io;

use crossterm::event::KeyCode;

use neenee_core::{
    McpServerInfo, ModelInfo, PermissionRuleInfo, ProviderPickerRow, ProviderPickerSnapshot,
    SessionContextSnapshot, SessionOverview, SkillInfo, ToolInfo, mcp::McpConnectionStatus,
};
use neenee_core::{Pursuit, TodoId, TodoItem, TodoList, TodoStatus};

use crate::showcase::common::{self, ShowAction};
use crate::tui::fuzzy;
use crate::tui::layout::LayoutMap;
use crate::tui::render::Theme;
use crate::tui::render::{
    ActivityModalView, draw_activity_modal, draw_armed_toast, draw_copy_toast, draw_help_modal,
    draw_history_modal, draw_model_editor, draw_models_modal, draw_session_modal,
    draw_sessions_modal,
};
use crate::tui::{ActivityTab, PROVIDERS};

// ─────────────────────────── provider picker ──────────────────────────────

struct ProviderState {
    index: usize,
    query: String,
    cursor: usize,
    picker: ProviderPickerSnapshot,
    key_status: HashMap<String, bool>,
    solutions: Vec<crate::tui::ProviderPreset>,
}

pub fn provider() -> io::Result<()> {
    let theme = Theme::default();
    let mut rows = Vec::new();
    for p in PROVIDERS.iter() {
        rows.push(ProviderPickerRow {
            id: p.id.to_string(),
            key_ready: p.id != "openai",
            favorite: p.id == "anthropic",
            last_used_ms: if p.id == "anthropic" {
                Some(1_700_000_000_000)
            } else {
                None
            },
        });
    }
    let picker = ProviderPickerSnapshot {
        default_id: "anthropic".into(),
        rows,
    };
    let key_status: HashMap<String, bool> = picker
        .rows
        .iter()
        .map(|r| (r.id.clone(), r.key_ready))
        .collect();
    let solutions: Vec<_> = PROVIDERS.to_vec();

    let mut state = ProviderState {
        index: 0,
        query: String::new(),
        cursor: 0,
        picker,
        key_status,
        solutions,
    };

    common::run_showcase(
        &mut state,
        |f, s| {
            let title = format!(
                " provider picker · {} providers · type to filter · q/Ctrl+C=quit",
                s.solutions.len(),
            );
            let hint = " ↑↓ navigate · Enter select · type to filter · Esc clear/quit ";
            common::draw_with_chrome(f, &title, hint, &theme, |f| {
                let mut lm = LayoutMap::new();
                draw_models_modal(
                    f,
                    &mut lm,
                    &s.solutions,
                    &s.picker.default_id,
                    s.index,
                    &s.key_status,
                    &s.picker,
                    &s.query,
                    s.cursor,
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
                KeyCode::Up if s.query.is_empty() => {
                    if s.index > 0 {
                        s.index -= 1;
                    }
                    ShowAction::Continue
                }
                KeyCode::Down if s.query.is_empty() => {
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

// ──────────────────────────── model editor ────────────────────────────────

struct ModelEditorState {
    field: u8, // 0 = API key, 1 = model id
    key_buf: String,
    model_buf: String,
    input: String, // the focused field's live value
    cursor: usize,
}

pub fn model_editor() -> io::Result<()> {
    let theme = Theme::default();
    let mut state = ModelEditorState {
        field: 0,
        key_buf: String::new(),
        model_buf: String::from("gpt-4o"),
        input: String::new(),
        cursor: 0,
    };

    common::run_showcase(
        &mut state,
        |f, s| {
            let field_name = if s.field == 0 { "API key" } else { "Model id" };
            let title =
                format!(" model editor · focused: {field_name} · Tab switch field · q/Ctrl+C=quit");
            let hint = " type to edit · Tab switch field · Esc quit ";
            common::draw_with_chrome(f, &title, hint, &theme, |f| {
                draw_model_editor(
                    f,
                    "OpenAI",
                    s.field,
                    &s.key_buf,
                    &s.model_buf,
                    &s.input,
                    s.cursor,
                    &theme,
                );
            });
        },
        |s, key| -> ShowAction {
            match key.code {
                KeyCode::Tab => {
                    // Commit the focused field to its buf, swap focus.
                    if s.field == 0 {
                        s.key_buf = s.input.clone();
                        s.input = s.model_buf.clone();
                    } else {
                        s.model_buf = s.input.clone();
                        s.input = s.key_buf.clone();
                    }
                    s.cursor = s.input.chars().count();
                    s.field = if s.field == 0 { 1 } else { 0 };
                    ShowAction::Continue
                }
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

// ──────────────────────── session-context modal ───────────────────────────

struct SessionState {
    snapshot: SessionContextSnapshot,
    index: usize,
    scroll: Cell<usize>,
    key_status: HashMap<String, bool>,
    mcp_statuses: Vec<(String, McpConnectionStatus)>,
}

pub fn session() -> io::Result<()> {
    let theme = Theme::default();
    let snapshot = SessionContextSnapshot {
        model: ModelInfo {
            provider: "anthropic".into(),
            model: "claude-sonnet-4-5".into(),
            display_name: "Claude Sonnet 4.5".into(),
            context_window: 200_000,
            api_key_ready: true,
            description: "Anthropic Claude Sonnet 4.5".into(),
            capabilities: vec!["tool calling".into(), "vision".into()],
        },
        tools: vec![
            ToolInfo {
                name: "bash".into(),
                description: "run a shell command".into(),
                enabled: true,
                source: "builtin".into(),
            },
            ToolInfo {
                name: "edit".into(),
                description: "edit a file".into(),
                enabled: true,
                source: "builtin".into(),
            },
            ToolInfo {
                name: "mcp__fs__read_file".into(),
                description: "read a file (MCP)".into(),
                enabled: false,
                source: "mcp:fs".into(),
            },
        ],
        permissions: vec![
            PermissionRuleInfo {
                tool: "bash".into(),
                scope: "*".into(),
            },
            PermissionRuleInfo {
                tool: "read".into(),
                scope: "src/**".into(),
            },
        ],
        skills: vec![SkillInfo {
            name: "rust-expert".into(),
            description: "Rust development help".into(),
            version: Some("1.0.0".into()),
            enabled: true,
            source: "repo".into(),
            tags: vec!["rust".into()],
        }],
        mcp: vec![McpServerInfo {
            name: "fs".into(),
            connected: true,
            disabled: false,
            failure: None,
            tool_names: vec!["read_file".into(), "write_file".into()],
        }],
    };
    let mut state = SessionState {
        snapshot,
        index: 0,
        scroll: Cell::new(0),
        key_status: HashMap::new(),
        mcp_statuses: vec![("fs".into(), McpConnectionStatus::Connected { tools: 2 })],
    };

    common::run_showcase(
        &mut state,
        |f, s| {
            let title = " session-context dashboard · q/Ctrl+C=quit";
            let hint = " ↑↓ select tool · Space toggle · Esc quit ";
            common::draw_with_chrome(f, title, hint, &theme, |f| {
                let mut scroll = s.scroll.get();
                draw_session_modal(
                    f,
                    "anthropic",
                    "claude-sonnet-4-5",
                    &s.key_status,
                    &s.mcp_statuses,
                    Some(&s.snapshot),
                    s.index,
                    &mut scroll,
                    true,
                    &theme,
                );
                s.scroll.set(scroll);
            });
        },
        |s, key| -> ShowAction {
            let tools = s.snapshot.tools.len().max(1);
            match key.code {
                KeyCode::Esc => ShowAction::Exit,
                KeyCode::Up => {
                    s.index = if s.index == 0 { tools - 1 } else { s.index - 1 };
                    ShowAction::Continue
                }
                KeyCode::Down => {
                    s.index = (s.index + 1) % tools;
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
                        turn_count: 3,
                        current_round: 2,
                        review_alert: "",
                        current_model: "claude-sonnet-4-5",
                        turn_started_at: Some(s.started),
                        activity: "running subagent · exploring the codebase",
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
